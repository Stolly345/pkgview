//! PS3/PSP `.pkg` parsing, decryption and extraction. No UI dependencies.
//!
//! Format and crypto mirror the original PkgView (`pkgmgr.vb`); see README "PKG format".

use aes::cipher::{generic_array::GenericArray, BlockEncrypt, KeyInit};
use aes::Aes128;
use sha1::{Digest, Sha1};
use std::collections::{BTreeSet, HashSet};
use std::fs::{self, File};
use std::io::{self, BufWriter, Write};
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

pub const PS3_KEY: [u8; 16] = [
    0x2E, 0x7B, 0x71, 0xD7, 0xC9, 0xC9, 0xA1, 0x4E, 0xA3, 0x22, 0x1F, 0x18, 0x88, 0x28, 0xB8, 0xF8,
];
pub const PSP_KEY: [u8; 16] = [
    0x07, 0xF2, 0xC6, 0x82, 0x90, 0xB5, 0x0D, 0x2C, 0x33, 0x81, 0x8D, 0x70, 0x9B, 0x60, 0xE6, 0x2B,
];

pub const TYPE_DEBUG_PS3: u32 = 0x0000_0001;
pub const TYPE_RETAIL_PS3: u32 = 0x8000_0001;
pub const TYPE_DEBUG_PSP: u32 = 0x0000_0002;
pub const TYPE_RETAIL_PSP: u32 = 0x8000_0002;

const HEADER_LEN: usize = 0x80;
const FOOTER_LEN: u64 = 32;
const CHUNK: usize = 1 << 20;
/// Largest entry `read_entry` will load into memory (PARAM.SFO / ICON0.PNG are tiny).
const MAX_IN_MEMORY: u64 = 16 << 20;

fn bad(msg: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg.into())
}

fn be32(b: &[u8], o: usize) -> u32 {
    u32::from_be_bytes(b[o..o + 4].try_into().unwrap())
}

fn be64(b: &[u8], o: usize) -> u64 {
    u64::from_be_bytes(b[o..o + 8].try_into().unwrap())
}

/// Big-endian add of `n` into a counter of any width, wrapping on overflow.
pub fn add_be(buf: &mut [u8], n: u64) {
    let mut carry = n;
    for b in buf.iter_mut().rev() {
        if carry == 0 {
            break;
        }
        let s = *b as u64 + (carry & 0xFF);
        *b = s as u8;
        carry = (carry >> 8) + (s >> 8);
    }
}

#[derive(Debug, Clone)]
pub struct Header {
    pub pkg_type: u32,
    pub metadata_offset: u32,
    pub metadata_count: u32,
    pub header_size: u32,
    pub item_count: u32,
    pub total_size: u64,
    pub data_offset: u64,
    pub data_size: u64,
    pub content_id: String,
    pub qa_digest: [u8; 16],
    pub data_riv: [u8; 16],
    /// The 20 bytes at EOF-32.
    pub sha1: [u8; 20],
}

impl Header {
    fn parse(raw: &[u8; HEADER_LEN], sha1: [u8; 20]) -> Header {
        let id = &raw[0x30..0x60];
        let id = &id[..id.iter().position(|&c| c == 0).unwrap_or(id.len())];
        Header {
            pkg_type: be32(raw, 0x04),
            metadata_offset: be32(raw, 0x08),
            metadata_count: be32(raw, 0x0C),
            header_size: be32(raw, 0x10),
            item_count: be32(raw, 0x14),
            total_size: be64(raw, 0x18),
            data_offset: be64(raw, 0x20),
            data_size: be64(raw, 0x28),
            content_id: String::from_utf8_lossy(id).into_owned(),
            qa_digest: raw[0x60..0x70].try_into().unwrap(),
            data_riv: raw[0x70..0x80].try_into().unwrap(),
            sha1,
        }
    }

    pub fn type_label(&self) -> &'static str {
        match self.pkg_type {
            TYPE_DEBUG_PS3 => "Debug",
            TYPE_RETAIL_PS3 => "Retail",
            TYPE_DEBUG_PSP => "Debug PSX/PSP",
            TYPE_RETAIL_PSP => "Retail PSX/PSP",
            _ => "Unknown",
        }
    }

    /// Tree root, as the original: `UP0001-NPUB12345_00-…` → `NPUB12345`.
    pub fn root_folder(&self) -> String {
        match self.content_id.split('-').nth(1) {
            Some(p) if p.len() > 3 && p.is_char_boundary(p.len() - 3) => {
                p[..p.len() - 3].to_string()
            }
            _ => {
                let s = self.content_id.trim().replace(['/', '\\'], "_");
                if s.is_empty() || s == "." || s == ".." {
                    "pkg".into()
                } else {
                    s
                }
            }
        }
    }

    /// Field / hex / value rows for the properties dialog and `pkgview info`.
    /// Original PkgView labels are kept in parentheses where they differ.
    pub fn rows(&self) -> Vec<[String; 3]> {
        let h32 = |v: u32| format!("0x{v:08X}");
        let h64 = |v: u64| format!("0x{v:016X}");
        vec![
            [
                "PKG type".into(),
                h32(self.pkg_type),
                self.type_label().into(),
            ],
            [
                "Metadata offset (orig: Header size)".into(),
                h32(self.metadata_offset),
                self.metadata_offset.to_string(),
            ],
            [
                "Metadata count (orig: Unknown 1)".into(),
                h32(self.metadata_count),
                self.metadata_count.to_string(),
            ],
            [
                "Header size (orig: Section 2 offset)".into(),
                h32(self.header_size),
                round_bytes(self.header_size as u64),
            ],
            [
                "Number of files".into(),
                h32(self.item_count),
                self.item_count.to_string(),
            ],
            [
                "Total size (orig: Filesize)".into(),
                h64(self.total_size),
                round_bytes(self.total_size),
            ],
            [
                "Data offset".into(),
                h64(self.data_offset),
                self.data_offset.to_string(),
            ],
            [
                "Data size".into(),
                h64(self.data_size),
                round_bytes(self.data_size),
            ],
        ]
    }
}

pub fn hex_spaced(b: &[u8]) -> String {
    b.iter()
        .map(|x| format!("{x:02X}"))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Like the original `RoundBytes`: 1024-based, up to 2 decimals.
pub fn round_bytes(n: u64) -> String {
    const UNITS: [&str; 4] = ["KB", "MB", "GB", "TB"];
    if n < 1024 {
        return format!("{n} bytes");
    }
    let mut v = n as f64 / 1024.0;
    let mut u = 0;
    while v >= 1024.0 && u < UNITS.len() - 1 {
        v /= 1024.0;
        u += 1;
    }
    let s = format!("{v:.2}");
    format!(
        "{} {}",
        s.trim_end_matches('0').trim_end_matches('.'),
        UNITS[u]
    )
}

pub fn thousands(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    out
}

fn now_hms() -> String {
    // SAFETY: plain libc calls with valid pointers.
    let tm = unsafe {
        let t = libc::time(std::ptr::null_mut());
        let mut tm: libc::tm = std::mem::zeroed();
        libc::localtime_r(&t, &mut tm);
        tm
    };
    format!("{:02}:{:02}:{:02}", tm.tm_hour, tm.tm_min, tm.tm_sec)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Cipher {
    Debug,
    Ps3,
    Psp,
}

enum Keystream {
    /// 64-byte SHA-1 counter built from qa_digest.
    Debug([u8; 64]),
    /// AES-128 over a 128-bit counter starting at data_riv.
    Aes(Box<Aes128>, [u8; 16]),
}

impl Keystream {
    fn new(h: &Header, c: Cipher) -> Keystream {
        match c {
            Cipher::Debug => {
                let mut k = [0u8; 64];
                k[0..8].copy_from_slice(&h.qa_digest[0..8]);
                k[8..16].copy_from_slice(&h.qa_digest[0..8]);
                k[16..24].copy_from_slice(&h.qa_digest[8..16]);
                k[24..32].copy_from_slice(&h.qa_digest[8..16]);
                Keystream::Debug(k)
            }
            Cipher::Ps3 => Keystream::Aes(Box::new(Aes128::new(&PS3_KEY.into())), h.data_riv),
            Cipher::Psp => Keystream::Aes(Box::new(Aes128::new(&PSP_KEY.into())), h.data_riv),
        }
    }

    /// XOR `buf` (multiple of 16) with the keystream from data-section block `first` on.
    /// The counter is computed directly, so any offset is O(1) (the original stepped it block by block).
    fn apply(&self, first: u64, buf: &mut [u8]) {
        debug_assert!(buf.len() % 16 == 0);
        match self {
            Keystream::Debug(k) => {
                for (i, blk) in buf.chunks_exact_mut(16).enumerate() {
                    let mut ctr = *k;
                    add_be(&mut ctr, first + i as u64);
                    let h = Sha1::digest(ctr);
                    blk.iter_mut().zip(h.iter()).for_each(|(b, k)| *b ^= k);
                }
            }
            Keystream::Aes(aes, riv) => {
                let mut ks: Vec<_> = (0..buf.len() as u64 / 16)
                    .map(|i| {
                        let mut c = *riv;
                        add_be(&mut c, first + i);
                        GenericArray::from(c)
                    })
                    .collect();
                aes.encrypt_blocks(&mut ks);
                for (blk, k) in buf.chunks_exact_mut(16).zip(&ks) {
                    blk.iter_mut().zip(k.iter()).for_each(|(b, k)| *b ^= k);
                }
            }
        }
    }
}

#[derive(Debug, Clone)]
pub struct Entry {
    /// `/`-separated, validated to stay inside the output dir.
    pub path: String,
    #[allow(dead_code)] // only read by tests (checks the original's packed-names assumption)
    pub name_offset: u32,
    pub offset: u64,
    pub size: u64,
    pub flags: u32,
}

impl Entry {
    pub fn is_dir(&self) -> bool {
        self.flags & 0xFF == 0x04
    }
    pub fn psp_key(&self) -> bool {
        self.flags >> 24 == 0x90
    }
    pub fn name(&self) -> &str {
        self.path.rsplit('/').next().unwrap_or(&self.path)
    }
}

/// Untrusted names: no absolute paths, `.`/`..`, NUL or backslashes.
fn sanitize(raw: &[u8]) -> io::Result<String> {
    let s = String::from_utf8_lossy(raw);
    let s = s.trim_end_matches('\0').trim_end_matches('/');
    let ok = !s.is_empty()
        && !s.starts_with('/')
        && !s.contains(['\0', '\\'])
        && s.split('/').all(|c| !c.is_empty() && c != "." && c != "..");
    if ok {
        Ok(s.to_string())
    } else {
        Err(bad(format!("unsafe file name in pkg: {s:?}")))
    }
}

/// What to do when an output file already exists.
#[allow(dead_code)] // Ask/Skip reach the GUI and CLI in 2.4.0
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Overwrite {
    Ask,
    Skip,
    Replace,
    Rename,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ExtractOptions {
    pub overwrite: Overwrite,
    /// Put everything under `<dir>/<pkg name>/`.
    pub pkg_folder: bool,
    /// Keep `<root>/<path>`; false = every file directly in the output folder.
    pub keep_paths: bool,
}

impl Default for ExtractOptions {
    /// The original's behaviour.
    fn default() -> Self {
        ExtractOptions {
            overwrite: Overwrite::Replace,
            pkg_folder: true,
            keep_paths: true,
        }
    }
}

/// `name (1).ext`, `name (2).ext`, …: the first that neither exists nor is in `taken`.
fn free_name(target: &Path, taken: &HashSet<PathBuf>) -> PathBuf {
    let stem = target.file_stem().unwrap_or_default().to_string_lossy();
    let ext = target.extension().map(|e| e.to_string_lossy());
    (1..)
        .map(|n| {
            target.with_file_name(match &ext {
                Some(ext) => format!("{stem} ({n}).{ext}"),
                None => format!("{stem} ({n})"),
            })
        })
        .find(|c| !c.exists() && !taken.contains(c))
        .unwrap()
}

/// `*` = any run of characters, `?` = one character; the whole of `s` must match.
fn glob_match(pat: &[char], s: &[char]) -> bool {
    let (mut p, mut i) = (0, 0);
    let mut star: Option<(usize, usize)> = None; // (pattern pos after `*`, input pos it matched up to)
    while i < s.len() {
        if p < pat.len() && (pat[p] == '?' || pat[p] == s[i]) {
            p += 1;
            i += 1;
        } else if p < pat.len() && pat[p] == '*' {
            star = Some((p + 1, i));
            p += 1;
        } else if let Some((sp, si)) = star {
            // Let the last `*` swallow one more character and retry.
            p = sp;
            i = si + 1;
            star = Some((sp, si + 1));
        } else {
            return false;
        }
    }
    pat[p..].iter().all(|&c| c == '*')
}

/// Every `*.pkg` (any case) under `dir`, sorted. Symlinked folders are not followed (no loops).
pub fn find_pkgs(dir: &Path) -> Vec<PathBuf> {
    let mut out = vec![];
    let mut todo = vec![dir.to_path_buf()];
    while let Some(d) = todo.pop() {
        let Ok(rd) = fs::read_dir(&d) else { continue };
        for e in rd.flatten() {
            let p = e.path();
            match e.file_type() {
                Ok(t) if t.is_dir() => todo.push(p),
                Ok(_)
                    if p.extension().is_some_and(|x| x.eq_ignore_ascii_case("pkg"))
                        && p.is_file() =>
                {
                    out.push(p)
                }
                _ => {}
            }
        }
    }
    out.sort();
    out
}

/// Output folder name for a pkg: its file name without `.pkg`.
pub fn pkg_name(path: &Path) -> String {
    path.file_stem()
        .map_or("pkg".into(), |s| s.to_string_lossy().into_owned())
}

pub enum Progress<'a> {
    File(&'a Entry),
    Bytes(u64),
}

pub struct Pkg {
    file: File,
    len: u64,
    pub header: Header,
    pub entries: Vec<Entry>,
}

impl Pkg {
    pub fn open(path: &Path) -> io::Result<Pkg> {
        let file = File::open(path)?; // read-only (the original opened ReadWrite)
        let len = file.metadata()?.len();
        let not_pkg = || bad("This file is not supported.");
        if len < HEADER_LEN as u64 + FOOTER_LEN {
            return Err(not_pkg());
        }
        let mut raw = [0u8; HEADER_LEN];
        file.read_exact_at(&mut raw, 0)?;
        if raw[0..4] != *b"\x7FPKG" {
            return Err(not_pkg());
        }
        let mut sha1 = [0u8; 20];
        file.read_exact_at(&mut sha1, len - FOOTER_LEN)?;
        let header = Header::parse(&raw, sha1);
        // ponytail: type 2 (debug PSP) was never supported by the original and is untested here.
        if !matches!(
            header.pkg_type,
            TYPE_DEBUG_PS3 | TYPE_RETAIL_PS3 | TYPE_RETAIL_PSP
        ) {
            return Err(bad("This pkg is not supported."));
        }
        let mut pkg = Pkg {
            file,
            len,
            header,
            entries: vec![],
        };
        pkg.entries = pkg.read_table()?;
        Ok(pkg)
    }

    fn table_cipher(&self) -> Cipher {
        match self.header.pkg_type {
            TYPE_DEBUG_PS3 => Cipher::Debug,
            TYPE_RETAIL_PSP => Cipher::Psp,
            _ => Cipher::Ps3,
        }
    }

    pub fn cipher_for(&self, e: &Entry) -> Cipher {
        // Deviation: the original used the PSP AES key for 0x90 entries even in debug pkgs; we keep SHA-1.
        if self.header.pkg_type == TYPE_DEBUG_PS3 {
            Cipher::Debug
        } else if e.psp_key() {
            Cipher::Psp
        } else {
            Cipher::Ps3
        }
    }

    /// Read `buf.len()` (multiple of 16) bytes at data-section offset `rel` (16-aligned) and decrypt.
    fn read_decrypt(&self, c: Cipher, rel: u64, buf: &mut [u8]) -> io::Result<()> {
        let abs = self
            .header
            .data_offset
            .checked_add(rel)
            .filter(|a| {
                a.checked_add(buf.len() as u64)
                    .is_some_and(|end| end <= self.len)
            })
            .ok_or_else(|| bad("pkg points past end of file (truncated or corrupt)"))?;
        self.file.read_exact_at(buf, abs)?;
        Keystream::new(&self.header, c).apply(rel / 16, buf);
        Ok(())
    }

    fn read_table(&self) -> io::Result<Vec<Entry>> {
        let n = self.header.item_count as u64;
        if n == 0 {
            return Ok(vec![]);
        }
        let avail = self.len.saturating_sub(self.header.data_offset);
        if n * 32 > avail {
            return Err(bad("item table is larger than the file"));
        }
        let tc = self.table_cipher();
        let mut first = [0u8; 32];
        self.read_decrypt(tc, 0, &mut first)?;
        // The first entry's data offset = size of records + names.
        let table_len = be64(&first, 8);
        if table_len < n * 32 || table_len > avail {
            return Err(bad("corrupt item table"));
        }
        let mut table = vec![0u8; table_len.next_multiple_of(16) as usize];
        self.read_decrypt(tc, 0, &mut table)?;
        // Mirrors the original: in retail PSP pkgs the records use the PSP key, but names of
        // non-0x90 entries are encrypted with the PS3 key, so decrypt the region a second time.
        let ps3_copy = if self.header.pkg_type == TYPE_RETAIL_PSP {
            let mut t = vec![0u8; table.len()];
            self.read_decrypt(Cipher::Ps3, 0, &mut t)?;
            Some(t)
        } else {
            None
        };

        let mut entries = Vec::with_capacity(n as usize);
        for r in table[..n as usize * 32].chunks_exact(32) {
            let (name_offset, name_len) = (be32(r, 0), be32(r, 4) as usize);
            let flags = be32(r, 24);
            let src = match &ps3_copy {
                Some(t) if flags >> 24 != 0x90 => t,
                _ => &table,
            };
            let raw = (name_offset as usize)
                .checked_add(name_len)
                .and_then(|end| src.get(name_offset as usize..end))
                .ok_or_else(|| bad("file name outside item table"))?;
            let e = Entry {
                path: sanitize(raw)?,
                name_offset,
                offset: be64(r, 8),
                size: be64(r, 16),
                flags,
            };
            if !e.is_dir() && e.size > 0 {
                let end = e
                    .size
                    .checked_next_multiple_of(16)
                    .and_then(|s| s.checked_add(e.offset))
                    .and_then(|s| s.checked_add(self.header.data_offset));
                if e.offset % 16 != 0 || end.is_none_or(|end| end > self.len) {
                    return Err(bad(format!("{}: data lies outside the file", e.path)));
                }
            }
            entries.push(e);
        }
        Ok(entries)
    }

    /// Direct subfolders and files of `folder` ("" = root). Folders implied by file paths count too.
    // ponytail: O(entries) per call; index by folder if pkgs with 100k+ entries show up.
    pub fn children(&self, folder: &str) -> (Vec<String>, Vec<usize>) {
        let mut dirs = BTreeSet::new();
        let mut files = vec![];
        for (i, e) in self.entries.iter().enumerate() {
            let rest = if folder.is_empty() {
                e.path.as_str()
            } else {
                match e
                    .path
                    .strip_prefix(folder)
                    .and_then(|r| r.strip_prefix('/'))
                {
                    Some(r) => r,
                    None => continue,
                }
            };
            match rest.split_once('/') {
                Some((d, _)) => {
                    dirs.insert(d.to_string());
                }
                None if e.is_dir() => {
                    dirs.insert(rest.to_string());
                }
                None => files.push(i),
            }
        }
        (dirs.into_iter().collect(), files)
    }

    /// Indices of entries equal to or below any of `paths` ("" selects everything).
    pub fn select(&self, paths: &[&str]) -> Vec<usize> {
        (0..self.entries.len())
            .filter(|&i| {
                let p = &self.entries[i].path;
                paths.iter().any(|s| {
                    s.is_empty() || p == s || p.strip_prefix(s).is_some_and(|r| r.starts_with('/'))
                })
            })
            .collect()
    }

    /// Entries matching `query`, case-insensitively: a substring of the path, or, if the query has `*`/`?`,
    /// a glob over the whole path or the name (`*` also crosses `/`).
    pub fn search(&self, query: &str) -> Vec<usize> {
        let q: Vec<char> = query.to_lowercase().chars().collect();
        let glob = q.contains(&'*') || q.contains(&'?');
        let qs: String = q.iter().collect();
        (0..self.entries.len())
            .filter(|&i| {
                let e = &self.entries[i];
                let path = e.path.to_lowercase();
                if !glob {
                    return path.contains(&qs);
                }
                let name: Vec<char> = e.name().to_lowercase().chars().collect();
                glob_match(&q, &path.chars().collect::<Vec<_>>()) || glob_match(&q, &name)
            })
            .collect()
    }

    pub fn find(&self, name: &str) -> Option<&Entry> {
        self.entries
            .iter()
            .filter(|e| !e.is_dir() && e.name().eq_ignore_ascii_case(name))
            .min_by_key(|e| e.path.matches('/').count())
    }

    /// TITLE, TITLE_ID, APP_VER and CATEGORY from PARAM.SFO, if there is one.
    pub fn sfo_summary(&self) -> Vec<(String, String)> {
        let Some(e) = self.find("PARAM.SFO") else {
            return vec![];
        };
        let all = self
            .read_entry(e)
            .ok()
            .and_then(|b| parse_sfo(&b))
            .unwrap_or_default();
        all.into_iter()
            .filter(|(k, _)| ["TITLE", "TITLE_ID", "APP_VER", "CATEGORY"].contains(&k.as_str()))
            .collect()
    }

    pub fn read_entry(&self, e: &Entry) -> io::Result<Vec<u8>> {
        if e.size > MAX_IN_MEMORY {
            return Err(bad("entry too large to load into memory"));
        }
        let mut buf = vec![0u8; e.size.next_multiple_of(16) as usize];
        self.read_decrypt(self.cipher_for(e), e.offset, &mut buf)?;
        buf.truncate(e.size as usize);
        Ok(buf)
    }

    /// Extract `indices` into `<dir>/<pkg_name>/<root>/…` and write `<pkg_name>.log` next to it, like
    /// the original (`opts` can drop the `<pkg_name>` level or flatten). Returns Ok(false) if cancelled;
    /// a cancelled or failed file is deleted.
    pub fn extract(
        &self,
        pkg_name: &str,
        indices: &[usize],
        dir: &Path,
        opts: &ExtractOptions,
        cancel: &AtomicBool,
        mut progress: impl FnMut(Progress),
    ) -> io::Result<bool> {
        let base = if opts.pkg_folder {
            dir.join(pkg_name)
        } else {
            dir.to_path_buf()
        };
        let root = base.join(self.header.root_folder());
        fs::create_dir_all(if opts.keep_paths { &root } else { &base })?;
        let mut log = BufWriter::new(File::create(base.join(format!("{pkg_name}.log")))?);
        // Files written by this run: a second entry landing on one of them (flattening) is renamed.
        let mut taken = HashSet::new();
        for &i in indices {
            let e = &self.entries[i];
            progress(Progress::File(e));
            let mut target = match (e.is_dir(), opts.keep_paths) {
                (true, false) => continue,
                (_, true) => root.join(&e.path),
                (false, false) => base.join(e.name()),
            };
            let rel = |t: &Path| t.strip_prefix(&base).unwrap_or(t).display().to_string();
            let r = if e.is_dir() {
                fs::create_dir_all(&target).map(|_| true)
            } else {
                let policy = if taken.contains(&target) {
                    Overwrite::Rename
                } else if target.exists() {
                    opts.overwrite
                } else {
                    Overwrite::Replace
                };
                match policy {
                    Overwrite::Skip => {
                        writeln!(log, "[{}] {} SKIPPED (exists)", now_hms(), rel(&target))?;
                        progress(Progress::Bytes(e.size));
                        continue;
                    }
                    Overwrite::Rename => target = free_name(&target, &taken),
                    // ponytail: Ask is answered by the GUI in a later version; until then it replaces.
                    Overwrite::Ask | Overwrite::Replace => {}
                }
                taken.insert(target.clone());
                self.extract_file(e, &target, cancel, &mut progress)
            };
            match r {
                Ok(true) if e.is_dir() => {}
                Ok(true) => writeln!(
                    log,
                    "[{}] {} {} bytes OK",
                    now_hms(),
                    rel(&target),
                    thousands(e.size)
                )?,
                Ok(false) => {
                    writeln!(log, "[{}] Extraction cancelled.", now_hms())?;
                    log.flush()?;
                    return Ok(false);
                }
                Err(err) => {
                    writeln!(log, "[{}] {} FAILED", now_hms(), rel(&target))?;
                    writeln!(log, "[{}] Extraction cancelled.", now_hms())?;
                    log.flush()?;
                    return Err(io::Error::new(err.kind(), format!("{}: {err}", e.path)));
                }
            }
        }
        writeln!(log, "[{}] Extraction completed.", now_hms())?;
        log.flush()?;
        Ok(true)
    }

    fn extract_file(
        &self,
        e: &Entry,
        target: &Path,
        cancel: &AtomicBool,
        progress: &mut impl FnMut(Progress),
    ) -> io::Result<bool> {
        if let Some(p) = target.parent() {
            fs::create_dir_all(p)?;
        }
        let c = self.cipher_for(e);
        let mut out = BufWriter::new(File::create(target)?);
        let res = (|| {
            let mut buf = vec![0u8; CHUNK];
            let mut done = 0u64;
            while done < e.size {
                if cancel.load(Ordering::Relaxed) {
                    return Ok(false);
                }
                let n = (e.size - done).min(CHUNK as u64) as usize;
                // Last partial block: decrypt the whole block, write only what belongs to the file.
                self.read_decrypt(c, e.offset + done, &mut buf[..n.next_multiple_of(16)])?;
                out.write_all(&buf[..n])?;
                done += n as u64;
                progress(Progress::Bytes(n as u64));
            }
            out.flush()?;
            Ok(true)
        })();
        if !matches!(res, Ok(true)) {
            drop(out);
            let _ = fs::remove_file(target);
        }
        res
    }

    /// Compare the trailing digest with SHA-1 of everything before the last 32 bytes (checked on real pkgs).
    pub fn verify_sha1(&self) -> io::Result<bool> {
        let end = self.len - FOOTER_LEN;
        let mut h = Sha1::new();
        let mut buf = vec![0u8; CHUNK];
        let mut pos = 0;
        while pos < end {
            let n = (end - pos).min(CHUNK as u64) as usize;
            self.file.read_exact_at(&mut buf[..n], pos)?;
            h.update(&buf[..n]);
            pos += n as u64;
        }
        Ok(h.finalize()[..] == self.header.sha1)
    }
}

/// PARAM.SFO, all little-endian: magic `\0PSF`, version, key_table_start, data_table_start, count;
/// then 16-byte index entries: key_off u16, fmt u16 (0x0404 = u32, else UTF-8), len u32, max u32, data_off u32.
pub fn parse_sfo(b: &[u8]) -> Option<Vec<(String, String)>> {
    let le16 = |o: usize| Some(u16::from_le_bytes(b.get(o..o + 2)?.try_into().ok()?) as usize);
    let le32 = |o: usize| Some(u32::from_le_bytes(b.get(o..o + 4)?.try_into().ok()?) as usize);
    if b.get(0..4)? != b"\0PSF" {
        return None;
    }
    let (keys, data, n) = (le32(8)?, le32(12)?, le32(16)?);
    let mut out = vec![];
    for i in 0..n.min(1024) {
        let o = 20 + i * 16;
        let (key_off, fmt, len, data_off) = (le16(o)?, le16(o + 2)?, le32(o + 4)?, le32(o + 12)?);
        let key = b.get(keys.checked_add(key_off)?..)?;
        let key = &key[..key.iter().position(|&c| c == 0)?];
        let d = b.get(data.checked_add(data_off)?..)?.get(..len)?;
        let val = if fmt == 0x0404 {
            u32::from_le_bytes(d.get(..4)?.try_into().ok()?).to_string()
        } else {
            String::from_utf8_lossy(d)
                .trim_end_matches('\0')
                .to_string()
        };
        out.push((String::from_utf8_lossy(key).into_owned(), val));
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::atomic::AtomicUsize;

    fn tmp(tag: &str) -> PathBuf {
        static N: AtomicUsize = AtomicUsize::new(0);
        let d = std::env::temp_dir().join(format!(
            "pkgview-test-{}-{}-{tag}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    fn noise(len: usize, seed: u64) -> Vec<u8> {
        let mut x = seed
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        (0..len)
            .map(|_| {
                x = x
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
                (x >> 33) as u8
            })
            .collect()
    }

    /// (path, Some(data) = file / None = dir, flags high byte)
    type Item = (&'static str, Option<Vec<u8>>, u32);

    /// Test-only pkg writer: header, records, names packed and 16-aligned, data, SHA-1 footer.
    /// Encryption is the same XOR keystream as decryption.
    fn build(pkg_type: u32, items: &[Item]) -> Vec<u8> {
        let n = items.len();
        let mut name_offs = vec![];
        let mut cur = n * 32;
        for (p, _, _) in items {
            name_offs.push(cur);
            cur += p.len().next_multiple_of(16);
        }
        let mut data_offs = vec![];
        for (_, d, _) in items {
            data_offs.push(cur);
            cur += d.as_ref().map_or(0, |d| d.len().next_multiple_of(16));
        }
        let mut sec = vec![0u8; cur];
        for (i, (p, d, hi)) in items.iter().enumerate() {
            let r = &mut sec[i * 32..i * 32 + 32];
            r[0..4].copy_from_slice(&(name_offs[i] as u32).to_be_bytes());
            r[4..8].copy_from_slice(&(p.len() as u32).to_be_bytes());
            r[8..16].copy_from_slice(&(data_offs[i] as u64).to_be_bytes());
            r[16..24].copy_from_slice(&(d.as_ref().map_or(0, |d| d.len()) as u64).to_be_bytes());
            let flags = hi | if d.is_none() { 0x04 } else { 0x03 };
            r[24..28].copy_from_slice(&flags.to_be_bytes());
            sec[name_offs[i]..name_offs[i] + p.len()].copy_from_slice(p.as_bytes());
            if let Some(d) = d {
                sec[data_offs[i]..data_offs[i] + d.len()].copy_from_slice(d);
            }
        }
        let mut riv = [0xA5u8; 16];
        riv[13..].copy_from_slice(&[0xFF, 0xFF, 0xF0]); // forces carries across bytes
        let h = Header {
            pkg_type,
            metadata_offset: 0xC0,
            metadata_count: 0,
            header_size: 0xC0,
            item_count: n as u32,
            total_size: 0,
            data_offset: 0x100,
            data_size: sec.len() as u64,
            content_id: "UP0001-NPUB12345_00-0000000000000001".into(),
            qa_digest: *b"0123456789ABCDEF",
            data_riv: riv,
            sha1: [0; 20],
        };
        let table_c = match pkg_type {
            TYPE_DEBUG_PS3 => Cipher::Debug,
            TYPE_RETAIL_PSP => Cipher::Psp,
            _ => Cipher::Ps3,
        };
        let item_c = |hi: u32| match (pkg_type, hi) {
            (TYPE_DEBUG_PS3, _) => Cipher::Debug,
            (_, 0x9000_0000) => Cipher::Psp,
            _ => Cipher::Ps3,
        };
        Keystream::new(&h, table_c).apply(0, &mut sec[..n * 32]);
        for (i, (p, d, hi)) in items.iter().enumerate() {
            let name_c = if pkg_type == TYPE_RETAIL_PSP {
                item_c(*hi)
            } else {
                table_c
            };
            let s = name_offs[i];
            Keystream::new(&h, name_c)
                .apply(s as u64 / 16, &mut sec[s..s + p.len().next_multiple_of(16)]);
            if let Some(d) = d {
                let s = data_offs[i];
                Keystream::new(&h, item_c(*hi))
                    .apply(s as u64 / 16, &mut sec[s..s + d.len().next_multiple_of(16)]);
            }
        }
        let mut out = vec![0u8; 0x100];
        out[0..4].copy_from_slice(b"\x7FPKG");
        out[0x04..0x08].copy_from_slice(&pkg_type.to_be_bytes());
        out[0x08..0x0C].copy_from_slice(&h.metadata_offset.to_be_bytes());
        out[0x10..0x14].copy_from_slice(&h.header_size.to_be_bytes());
        out[0x14..0x18].copy_from_slice(&h.item_count.to_be_bytes());
        let total = (0x100 + sec.len()) as u64 + FOOTER_LEN;
        out[0x18..0x20].copy_from_slice(&total.to_be_bytes());
        out[0x20..0x28].copy_from_slice(&h.data_offset.to_be_bytes());
        out[0x28..0x30].copy_from_slice(&h.data_size.to_be_bytes());
        out[0x30..0x30 + h.content_id.len()].copy_from_slice(h.content_id.as_bytes());
        out[0x60..0x70].copy_from_slice(&h.qa_digest);
        out[0x70..0x80].copy_from_slice(&h.data_riv);
        out.extend_from_slice(&sec);
        let digest = Sha1::digest(&out);
        out.extend_from_slice(&digest);
        out.extend_from_slice(&[0u8; 12]);
        out
    }

    fn sfo(pairs: &[(&str, &str)]) -> Vec<u8> {
        let mut keys = vec![];
        let mut data = vec![];
        let mut idx = vec![];
        for (k, v) in pairs {
            idx.extend_from_slice(&(keys.len() as u16).to_le_bytes());
            idx.extend_from_slice(&0x0204u16.to_le_bytes());
            idx.extend_from_slice(&(v.len() as u32 + 1).to_le_bytes());
            idx.extend_from_slice(&(v.len() as u32 + 1).to_le_bytes());
            idx.extend_from_slice(&(data.len() as u32).to_le_bytes());
            keys.extend_from_slice(k.as_bytes());
            keys.push(0);
            data.extend_from_slice(v.as_bytes());
            data.push(0);
        }
        let key_start = 20 + idx.len();
        let mut b = b"\0PSF".to_vec();
        b.extend_from_slice(&0x101u32.to_le_bytes());
        b.extend_from_slice(&(key_start as u32).to_le_bytes());
        b.extend_from_slice(&((key_start + keys.len()) as u32).to_le_bytes());
        b.extend_from_slice(&(pairs.len() as u32).to_le_bytes());
        b.extend(idx);
        b.extend(keys);
        b.extend(data);
        b
    }

    fn sample_items() -> Vec<Item> {
        vec![
            ("USRDIR", None, 0),
            ("USRDIR/EMPTY", None, 0),
            ("USRDIR/EBOOT.BIN", Some(noise(3 * CHUNK + 5, 1)), 0),
            ("USRDIR/zero.bin", Some(vec![]), 0),
            (
                "PARAM.SFO",
                Some(sfo(&[("TITLE", "Test Game"), ("TITLE_ID", "NPUB12345")])),
                0,
            ),
            ("ICON0.PNG", Some(noise(100, 2)), 0),
            ("odd.txt", Some(noise(37, 3)), 0),
            ("PSP/DATA.PSP", Some(noise(4097, 4)), 0x9000_0000),
            ("deep/a/b/c.bin", Some(noise(16, 5)), 0),
        ]
    }

    fn write_pkg(bytes: &[u8]) -> (PathBuf, PathBuf) {
        let d = tmp("pkg");
        let p = d.join("UP0001-NPUB12345_00-0000000000000001.pkg");
        fs::write(&p, bytes).unwrap();
        (d, p)
    }

    #[test]
    fn counter_add() {
        let mut a = [0u8, 0xFF, 0xFF];
        add_be(&mut a, 1);
        assert_eq!(a, [1, 0, 0]);
        let mut b = [0xFFu8; 16];
        add_be(&mut b, 1);
        assert_eq!(b, [0; 16]);
        let mut c = [0xFFu8; 64];
        add_be(&mut c, 2);
        let mut want = [0u8; 64];
        want[63] = 1;
        assert_eq!(c, want);
        let mut d = [0u8; 16];
        d[15] = 0xF0;
        add_be(&mut d, 0x1_0000_0010);
        assert_eq!(&d[10..], &[0, 1, 0, 0, 1, 0]);
        let mut e = [0u8; 16];
        add_be(&mut e, u64::MAX);
        assert_eq!(&e[..8], &[0; 8]);
        assert_eq!(&e[8..], &[0xFF; 8]);
    }

    #[test]
    fn round_trip_all_types() {
        let items = sample_items();
        for t in [TYPE_DEBUG_PS3, TYPE_RETAIL_PS3, TYPE_RETAIL_PSP] {
            let (d, p) = write_pkg(&build(t, &items));
            let pkg = Pkg::open(&p).unwrap();
            assert_eq!(pkg.entries.len(), items.len());
            assert_eq!(pkg.header.root_folder(), "NPUB12345");
            // Names really are packed back-to-back, 16-aligned (the original's assumption).
            let mut packed = items.len() as u32 * 32;
            for e in &pkg.entries {
                assert_eq!(e.name_offset, packed);
                packed += e.path.len().next_multiple_of(16) as u32;
            }
            assert!(pkg.verify_sha1().unwrap());

            let out = d.join("out");
            let all = pkg.select(&[""]);
            let mut bytes = 0;
            let done = pkg
                .extract(
                    "game",
                    &all,
                    &out,
                    &ExtractOptions::default(),
                    &AtomicBool::new(false),
                    |p| {
                        if let Progress::Bytes(n) = p {
                            bytes += n
                        }
                    },
                )
                .unwrap();
            assert!(done);
            let root = out.join("game/NPUB12345");
            for (path, data, _) in &items {
                match data {
                    Some(data) => {
                        assert_eq!(&fs::read(root.join(path)).unwrap(), data, "{t:#x} {path}")
                    }
                    None => assert!(root.join(path).is_dir()),
                }
            }
            assert_eq!(
                bytes,
                items
                    .iter()
                    .map(|i| i.1.as_ref().map_or(0, |d| d.len() as u64))
                    .sum::<u64>()
            );
            let log = fs::read_to_string(out.join("game/game.log")).unwrap();
            assert!(log.trim_end().ends_with("Extraction completed."), "{log}");
            assert!(
                log.contains("NPUB12345/USRDIR/EBOOT.BIN 3,145,733 bytes OK"),
                "{log}"
            );

            let sfo = parse_sfo(&pkg.read_entry(pkg.find("param.sfo").unwrap()).unwrap()).unwrap();
            assert_eq!(sfo[0], ("TITLE".into(), "Test Game".into()));
            assert_eq!(
                pkg.read_entry(pkg.find("ICON0.PNG").unwrap()).unwrap(),
                items[5].1.clone().unwrap()
            );
            fs::remove_dir_all(d).unwrap();
        }
    }

    #[test]
    fn wrong_key_garbles() {
        // Guards key selection: the 0x90 entry must not decrypt with the PS3 key.
        let items = sample_items();
        let (d, p) = write_pkg(&build(TYPE_RETAIL_PS3, &items));
        let pkg = Pkg::open(&p).unwrap();
        let e = pkg.find("DATA.PSP").unwrap().clone();
        let mut buf = vec![0u8; 16];
        pkg.read_decrypt(Cipher::Ps3, e.offset, &mut buf).unwrap();
        assert_ne!(&buf[..], &items[7].1.as_ref().unwrap()[..16]);
        fs::remove_dir_all(d).unwrap();
    }

    #[test]
    fn tree_and_selection() {
        let (d, p) = write_pkg(&build(TYPE_RETAIL_PS3, &sample_items()));
        let pkg = Pkg::open(&p).unwrap();
        let (dirs, files) = pkg.children("");
        assert_eq!(dirs, ["PSP", "USRDIR", "deep"]);
        assert_eq!(files.len(), 3);
        let (dirs, files) = pkg.children("USRDIR");
        assert_eq!(dirs, ["EMPTY"]);
        assert_eq!(files.len(), 2);
        assert_eq!(pkg.children("deep").0, ["a"]); // implied folder, no dir entry
        assert_eq!(pkg.select(&["USRDIR"]).len(), 4);
        assert_eq!(pkg.select(&["USR"]).len(), 0);
        fs::remove_dir_all(d).unwrap();
    }

    #[test]
    fn cancel_removes_partial_file() {
        let (d, p) = write_pkg(&build(TYPE_RETAIL_PS3, &sample_items()));
        let pkg = Pkg::open(&p).unwrap();
        let cancel = AtomicBool::new(false);
        let idx = pkg.select(&["USRDIR/EBOOT.BIN"]);
        let done = pkg
            .extract("g", &idx, &d, &ExtractOptions::default(), &cancel, |p| {
                if let Progress::Bytes(_) = p {
                    cancel.store(true, Ordering::Relaxed)
                }
            })
            .unwrap();
        assert!(!done);
        assert!(!d.join("g/NPUB12345/USRDIR/EBOOT.BIN").exists());
        assert!(fs::read_to_string(d.join("g/g.log"))
            .unwrap()
            .contains("Extraction cancelled."));
        fs::remove_dir_all(d).unwrap();
    }

    #[test]
    fn malicious_input_is_rejected() {
        for name in [
            "../evil",
            "/etc/passwd",
            "a\\b",
            "a/../../b",
            "./x",
            "a//b",
            "a\0b",
        ] {
            let items: Vec<Item> = vec![(name, Some(b"pwned".to_vec()), 0)];
            let (d, p) = write_pkg(&build(TYPE_RETAIL_PS3, &items));
            assert!(Pkg::open(&p).is_err(), "{name:?} accepted");
            fs::remove_dir_all(d).unwrap();
        }
        let good = build(TYPE_RETAIL_PS3, &sample_items());
        let cases: Vec<(&str, Vec<u8>)> = vec![
            ("huge item_count", {
                let mut b = good.clone();
                b[0x14..0x18].copy_from_slice(&[0xFF; 4]);
                b
            }),
            ("data_offset past EOF", {
                let mut b = good.clone();
                b[0x20..0x28].copy_from_slice(&[0x7F; 8]);
                b
            }),
            ("truncated", good[..good.len() / 2].to_vec()),
            ("tiny", good[..40].to_vec()),
            ("bad magic", {
                let mut b = good.clone();
                b[1] = b'X';
                b
            }),
            ("bad type", {
                let mut b = good.clone();
                b[0x07] = 9;
                b
            }),
        ];
        for (what, bytes) in cases {
            let (d, p) = write_pkg(&bytes);
            assert!(Pkg::open(&p).is_err(), "{what} accepted");
            fs::remove_dir_all(d).unwrap();
        }
    }

    #[test]
    fn fuzz_never_panics() {
        let items: Vec<Item> = vec![
            ("USRDIR", None, 0),
            ("USRDIR/a", Some(noise(40, 1)), 0),
            ("PARAM.SFO", Some(sfo(&[("TITLE", "x")])), 0),
            ("b", Some(noise(20, 2)), 0x9000_0000),
        ];
        let good = build(TYPE_RETAIL_PSP, &items);
        let (d, p) = write_pkg(&good);
        let mut rng = noise(4096, 99).into_iter().cycle();
        let mut next = || rng.next().unwrap() as usize;
        for _ in 0..1000 {
            let mut b = good.clone();
            for _ in 0..1 + next() % 8 {
                let i = (next() << 8 | next()) % b.len();
                b[i] = next() as u8;
            }
            fs::write(&p, &b).unwrap();
            if let Ok(pkg) = Pkg::open(&p) {
                for e in pkg.entries.clone() {
                    let _ = pkg.read_entry(&e);
                }
                let _ = pkg.children("");
            }
        }
        assert!(parse_sfo(&noise(300, 7)).is_none());
        for i in 0..200 {
            let mut s = sfo(&[("TITLE", "x"), ("APP_VER", "01.00")]);
            let j = i % s.len();
            s[j] = noise(1, i as u64)[0];
            let _ = parse_sfo(&s);
        }
        fs::remove_dir_all(d).unwrap();
    }

    #[test]
    fn overwrite_policies() {
        let (d, p) = write_pkg(&build(TYPE_RETAIL_PS3, &sample_items()));
        let pkg = Pkg::open(&p).unwrap();
        let idx = pkg.select(&["odd.txt", "USRDIR"]);
        let run = |o: Overwrite| {
            let opts = ExtractOptions {
                overwrite: o,
                ..Default::default()
            };
            assert!(pkg
                .extract("g", &idx, &d, &opts, &AtomicBool::new(false), |_| {})
                .unwrap());
        };
        let root = d.join("g/NPUB12345");
        let odd = fs::read(root.join("odd.txt")).ok();
        assert!(odd.is_none());
        run(Overwrite::Replace);
        let good = fs::read(root.join("odd.txt")).unwrap();
        fs::write(root.join("odd.txt"), b"mine").unwrap();
        run(Overwrite::Skip);
        assert_eq!(fs::read(root.join("odd.txt")).unwrap(), b"mine");
        let log = fs::read_to_string(d.join("g/g.log")).unwrap();
        assert!(log.contains("NPUB12345/odd.txt SKIPPED (exists)"), "{log}");
        run(Overwrite::Rename);
        assert_eq!(fs::read(root.join("odd.txt")).unwrap(), b"mine");
        assert_eq!(fs::read(root.join("odd (1).txt")).unwrap(), good);
        assert!(root.join("USRDIR/EBOOT (1).BIN").exists());
        run(Overwrite::Rename);
        assert!(root.join("odd (2).txt").exists());
        run(Overwrite::Ask); // replaces until the GUI answers it
        assert_eq!(fs::read(root.join("odd.txt")).unwrap(), good);
        fs::remove_dir_all(d).unwrap();
    }

    #[test]
    fn layout_options() {
        let items: Vec<Item> = vec![
            ("a", None, 0),
            ("a/x.bin", Some(noise(20, 1)), 0),
            ("b/x.bin", Some(noise(30, 2)), 0),
            ("noext", Some(noise(5, 3)), 0),
        ];
        let (d, p) = write_pkg(&build(TYPE_RETAIL_PS3, &items));
        let pkg = Pkg::open(&p).unwrap();
        let all = pkg.select(&[""]);
        let go = |dir: &Path, pkg_folder, keep_paths| {
            let opts = ExtractOptions {
                pkg_folder,
                keep_paths,
                ..Default::default()
            };
            assert!(pkg
                .extract("g", &all, dir, &opts, &AtomicBool::new(false), |_| {})
                .unwrap());
        };
        let out = d.join("nofolder");
        go(&out, false, true);
        assert_eq!(
            fs::read(out.join("NPUB12345/b/x.bin")).unwrap(),
            noise(30, 2)
        );
        assert!(out.join("g.log").exists());
        let out = d.join("flat");
        go(&out, true, false);
        assert_eq!(fs::read(out.join("g/x.bin")).unwrap(), noise(20, 1));
        assert_eq!(fs::read(out.join("g/x (1).bin")).unwrap(), noise(30, 2));
        assert!(out.join("g/noext").exists());
        assert!(!out.join("g/NPUB12345").exists() && !out.join("g/a").exists());
        // A second flat run with Rename collides with the first run's files too.
        let opts = ExtractOptions {
            overwrite: Overwrite::Rename,
            keep_paths: false,
            ..Default::default()
        };
        assert!(pkg
            .extract("g", &all, &out, &opts, &AtomicBool::new(false), |_| {})
            .unwrap());
        assert!(out.join("g/x (2).bin").exists() && out.join("g/x (3).bin").exists());
        assert!(out.join("g/noext (1)").exists());
        fs::remove_dir_all(d).unwrap();
    }

    #[test]
    fn search_and_glob() {
        let (d, p) = write_pkg(&build(TYPE_RETAIL_PS3, &sample_items()));
        let pkg = Pkg::open(&p).unwrap();
        let names = |q: &str| -> Vec<String> {
            pkg.search(q)
                .iter()
                .map(|&i| pkg.entries[i].path.clone())
                .collect()
        };
        assert_eq!(names("eboot"), ["USRDIR/EBOOT.BIN"]);
        assert_eq!(names("usrdir/E").len(), 2); // EMPTY, EBOOT.BIN
        assert_eq!(names("*.png"), ["ICON0.PNG"]);
        assert_eq!(names("icon?.*"), ["ICON0.PNG"]); // matches the name, not only the full path
        assert_eq!(names("deep/*/c.bin"), ["deep/a/b/c.bin"]); // `*` crosses `/`
        assert_eq!(names("*.bin").len(), 3);
        assert!(names("nothing").is_empty());
        assert_eq!(names("").len(), pkg.entries.len());
        let g = |p: &str, s: &str| {
            glob_match(
                &p.chars().collect::<Vec<_>>(),
                &s.chars().collect::<Vec<_>>(),
            )
        };
        assert!(g("a*b*c", "aXbYbZc") && g("*", "") && g("?é", "xé") && g("**a", "a"));
        assert!(!g("a*b", "aXc") && !g("?", "") && !g("abc", "ab"));
        fs::remove_dir_all(d).unwrap();
    }

    #[test]
    fn finds_pkgs_recursively() {
        let d = tmp("find");
        fs::create_dir_all(d.join("a/b")).unwrap();
        for f in ["x.pkg", "a/Y.PKG", "a/b/z.pkg", "a/notes.txt", "a/b/pkg"] {
            fs::write(d.join(f), b"").unwrap();
        }
        std::os::unix::fs::symlink(&d, d.join("a/loop")).unwrap();
        let found: Vec<_> = find_pkgs(&d)
            .iter()
            .map(|p| p.strip_prefix(&d).unwrap().display().to_string())
            .collect();
        assert_eq!(found, ["a/Y.PKG", "a/b/z.pkg", "x.pkg"]);
        fs::remove_dir_all(d).unwrap();
    }

    #[test]
    fn formatting() {
        assert_eq!(round_bytes(5), "5 bytes");
        assert_eq!(round_bytes(1536), "1.5 KB");
        assert_eq!(round_bytes(3 * 1024 * 1024), "3 MB");
        assert_eq!(thousands(1234567), "1,234,567");
        assert_eq!(thousands(12), "12");
    }

    /// Real-world check over one pkg or every `*.pkg` in a folder (`make realtest SAMPLES=dir`):
    /// SHA-1 matches, PARAM.SFO's TITLE_ID agrees with the content ID, every entry extracts at its size.
    #[test]
    #[ignore]
    fn real_sample() {
        let path = PathBuf::from(std::env::var("PKGVIEW_SAMPLE").expect("set PKGVIEW_SAMPLE"));
        let mut pkgs: Vec<PathBuf> = if path.is_dir() {
            fs::read_dir(&path)
                .unwrap()
                .map(|e| e.unwrap().path())
                .filter(|p| p.extension().is_some_and(|x| x.eq_ignore_ascii_case("pkg")))
                .collect()
        } else {
            vec![path]
        };
        pkgs.sort();
        assert!(!pkgs.is_empty(), "no .pkg files found");
        for (n, path) in pkgs.iter().enumerate() {
            let pkg = Pkg::open(path).unwrap();
            let h = &pkg.header;
            // Generic description only: no titles or file names in logs.
            println!(
                "sample {}: {} {:#010x}, {}, {} entries, metadata {}x @ {:#x}",
                n + 1,
                h.type_label(),
                h.pkg_type,
                round_bytes(pkg.len),
                pkg.entries.len(),
                h.metadata_count,
                h.metadata_offset
            );
            assert!(
                pkg.verify_sha1().unwrap(),
                "sample {}: SHA-1 mismatch",
                n + 1
            );
            let sfo = pkg.sfo_summary();
            let id = sfo.iter().find(|(k, _)| k == "TITLE_ID").map(|(_, v)| v);
            assert_eq!(id, Some(&h.root_folder()), "sample {}: TITLE_ID", n + 1);
            let d = tmp("real");
            let all = pkg.select(&[""]);
            let opts = ExtractOptions::default();
            assert!(pkg
                .extract("s", &all, &d, &opts, &AtomicBool::new(false), |_| {})
                .unwrap());
            let root = d.join("s").join(h.root_folder());
            for e in pkg.entries.iter().filter(|e| !e.is_dir()) {
                assert_eq!(fs::metadata(root.join(&e.path)).unwrap().len(), e.size);
            }
            fs::remove_dir_all(d).unwrap();
        }
        println!("{} real pkg(s) OK", pkgs.len());
    }

    /// Writes a synthetic pkg for manual/CLI/GUI testing:
    /// `PKGVIEW_WRITE_SAMPLE=/tmp/x.pkg PKGVIEW_SAMPLE_TYPE=0x80000002 cargo test write_sample -- --ignored`
    /// (`PKGVIEW_SAMPLE_MB=n` adds an n MB file, `PKGVIEW_SAMPLE_NOICON=1` leaves out ICON0.PNG,
    /// `PKGVIEW_SAMPLE_DEMO=1` writes the screenshot layout instead)
    #[test]
    #[ignore]
    fn write_sample() {
        let path = std::env::var("PKGVIEW_WRITE_SAMPLE").expect("set PKGVIEW_WRITE_SAMPLE");
        let t = std::env::var("PKGVIEW_SAMPLE_TYPE").unwrap_or("0x80000001".into());
        let t = u32::from_str_radix(t.trim_start_matches("0x"), 16).unwrap();
        let mut items = sample_items();
        items[5].1 = Some(include_bytes!("../data/pkgview.png").to_vec());
        items.push(("USRDIR/data/level1.dat", Some(noise(200_000, 8)), 0));
        items.push(("USRDIR/data/music.at3", Some(noise(70_000, 9)), 0));
        items.push((
            "USRDIR/readme.txt",
            Some(b"hello from a synthetic pkg\n".to_vec()),
            0,
        ));
        if std::env::var_os("PKGVIEW_SAMPLE_DEMO").is_some() {
            // Screenshot material: a plausible game layout, no real game content.
            let sfo_bytes = sfo(&[
                ("CATEGORY", "HG"),
                ("TITLE", "Demo Package"),
                ("TITLE_ID", "NPUB12345"),
                ("APP_VER", "01.00"),
            ]);
            items = vec![
                ("PARAM.SFO", Some(sfo_bytes), 0),
                (
                    "ICON0.PNG",
                    Some(include_bytes!("../data/pkgview.png").to_vec()),
                    0,
                ),
                ("PS3LOGO.DAT", Some(noise(5120, 20)), 0),
                ("USRDIR", None, 0),
                ("USRDIR/EBOOT.BIN", Some(noise(2_400_000, 21)), 0),
                ("USRDIR/data", None, 0),
                ("USRDIR/data/level1.dat", Some(noise(820_000, 22)), 0),
                ("USRDIR/data/level2.dat", Some(noise(1_150_000, 23)), 0),
                ("USRDIR/data/textures.pak", Some(noise(3_600_000, 24)), 0),
                ("USRDIR/music", None, 0),
                ("USRDIR/music/title.at3", Some(noise(410_000, 25)), 0),
                ("USRDIR/music/stage1.at3", Some(noise(530_000, 26)), 0),
                ("USRDIR/manual", None, 0),
                (
                    "USRDIR/manual/readme.txt",
                    Some(b"Demo package for PkgView screenshots.\n".to_vec()),
                    0,
                ),
                (
                    "USRDIR/manual/controls.png",
                    Some(include_bytes!("../data/pkgview.png").to_vec()),
                    0,
                ),
            ];
        }
        if std::env::var_os("PKGVIEW_SAMPLE_NOICON").is_some() {
            items.retain(|i| i.0 != "ICON0.PNG");
        }
        if let Ok(mb) = std::env::var("PKGVIEW_SAMPLE_MB") {
            let n = mb.parse::<usize>().unwrap() << 20;
            items.push(("USRDIR/big.bin", Some(vec![0; n]), 0));
        }
        fs::write(path, build(t, &items)).unwrap();
    }
}
