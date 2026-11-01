//! PS3/PSP `.pkg` parsing, decryption and extraction; PS4 `.pkg` read-only (header, plain entries). No UI
//! dependencies.
//!
//! Format and crypto mirror the original PkgView (`pkgmgr.vb`); see README "PKG format".

use aes::cipher::{generic_array::GenericArray, BlockEncrypt, KeyInit};
use aes::Aes128;
use sha1::{Digest, Sha1};
use sha2::Sha256;
use std::collections::{BTreeSet, HashSet};
use std::ffi::CString;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufWriter, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{FileExt, OpenOptionsExt};
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
pub const MAX_IN_MEMORY: u64 = 16 << 20;

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
    /// The 20 bytes at EOF-32. PS4: the first 20 bytes of the header SHA-256 (tells packages apart).
    pub sha1: [u8; 20],
    pub ps4: Option<Ps4>,
}

/// PS4 (`\x7FCNT`) header fields. Layout checked on a retail PSN package: all three digests match.
#[derive(Debug, Clone)]
pub struct Ps4 {
    pub content_type: u32,
    pub drm_type: u32,
    pub table_offset: u64,
    /// (offset, size)
    pub body: (u64, u64),
    /// The encrypted game image (offset, size): the game's files, not readable without the console's keys
    pub pfs: (u64, u64),
    /// SHA-256 of header bytes 0..0xFE0, of the body, of the PFS image
    pub digests: [[u8; 32]; 3],
}

const PS4_HEADER_LEN: u64 = 0x1000;

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
            ps4: None,
        }
    }

    fn parse_ps4(h: &[u8]) -> Header {
        let id = &h[0x40..0x64];
        let id = &id[..id.iter().position(|&c| c == 0).unwrap_or(id.len())];
        let d = |o: usize| -> [u8; 32] { h[o..o + 32].try_into().unwrap() };
        let ps4 = Ps4 {
            content_type: be32(h, 0x74),
            drm_type: be32(h, 0x70),
            table_offset: be32(h, 0x18) as u64,
            body: (be64(h, 0x20), be64(h, 0x28)),
            pfs: (be64(h, 0x410), be64(h, 0x418)),
            digests: [d(0xFE0), d(0x160), d(0x440)],
        };
        Header {
            pkg_type: be32(h, 0x04),
            metadata_offset: 0,
            metadata_count: 0,
            header_size: PS4_HEADER_LEN as u32,
            item_count: be32(h, 0x10),
            total_size: be64(h, 0x430),
            data_offset: 0,
            data_size: ps4.body.1,
            content_id: String::from_utf8_lossy(id).into_owned(),
            qa_digest: [0; 16],
            data_riv: [0; 16],
            sha1: ps4.digests[0][..20].try_into().unwrap(),
            ps4: Some(ps4),
        }
    }

    /// What Verify checks, for messages: "SHA-1" or "SHA-256".
    pub fn digest_name(&self) -> &'static str {
        if self.ps4.is_some() {
            "SHA-256"
        } else {
            "SHA-1"
        }
    }

    /// The package's own digests, labelled, for the properties dialog and `pkgview info`.
    pub fn digest_lines(&self) -> Vec<(&'static str, String)> {
        match &self.ps4 {
            Some(p) => vec![
                ("Header SHA-256", hex(&p.digests[0])),
                ("Body SHA-256", hex(&p.digests[1])),
                ("Game image SHA-256", hex(&p.digests[2])),
            ],
            None => vec![
                ("QA digest", hex_spaced(&self.qa_digest)),
                ("SHA-1", hex_spaced(&self.sha1)),
            ],
        }
    }

    pub fn type_label(&self) -> &'static str {
        if self.ps4.is_some() {
            return "PS4";
        }
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
        if let Some(p) = &self.ps4 {
            return vec![
                ["PKG type".into(), h32(self.pkg_type), "PS4".into()],
                ["Content type".into(), h32(p.content_type), String::new()],
                ["DRM type".into(), h32(p.drm_type), String::new()],
                [
                    "Number of entries".into(),
                    h32(self.item_count),
                    self.item_count.to_string(),
                ],
                [
                    "Total size".into(),
                    h64(self.total_size),
                    round_bytes(self.total_size),
                ],
                ["Body offset".into(), h64(p.body.0), p.body.0.to_string()],
                ["Body size".into(), h64(p.body.1), round_bytes(p.body.1)],
                [
                    "Game image offset".into(),
                    h64(p.pfs.0),
                    p.pfs.0.to_string(),
                ],
                [
                    "Game image size (encrypted)".into(),
                    h64(p.pfs.1),
                    round_bytes(p.pfs.1),
                ],
            ];
        }
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

pub fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
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
    /// PS4 entries PkgView shows are stored unencrypted
    None,
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
            Cipher::None => unreachable!("plain entries are not decrypted"),
        }
    }

    /// XOR `buf` (multiple of 16) with the keystream from data-section block `first` on.
    /// The counter is computed directly, so any offset is O(1) (the original stepped it block by block).
    fn apply(&self, first: u64, buf: &mut [u8]) {
        debug_assert!(buf.len() % 16 == 0);
        match self {
            Keystream::Debug(k) => {
                // One SHA-1 per 16 bytes is CPU-bound (~140 MB/s on one core): spread big buffers over the
                // cores. Blocks are independent, so the split changes nothing in the output.
                let threads = if buf.len() >= 1 << 16 {
                    std::thread::available_parallelism().map_or(1, |n| n.get())
                } else {
                    1
                };
                let per = (buf.len() / 16).div_ceil(threads) * 16;
                if threads == 1 || per == 0 {
                    return debug_xor(k, first, buf);
                }
                std::thread::scope(|s| {
                    for (j, part) in buf.chunks_mut(per).enumerate() {
                        let first = first + (j * per / 16) as u64;
                        s.spawn(move || debug_xor(k, first, part));
                    }
                });
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

/// XOR `buf` with the debug keystream (SHA-1 of the 64-byte counter `k + n`) from block `first` on.
fn debug_xor(k: &[u8; 64], first: u64, buf: &mut [u8]) {
    for (i, blk) in buf.chunks_exact_mut(16).enumerate() {
        let mut ctr = *k;
        add_be(&mut ctr, first + i as u64);
        let h = Sha1::digest(ctr);
        blk.iter_mut().zip(h.iter()).for_each(|(b, k)| *b ^= k);
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
        .find(|c| !present(c) && !taken.contains(c))
        .unwrap()
}

/// Something (file, folder, or symlink, even a dangling one) is at `p`.
fn present(p: &Path) -> bool {
    fs::symlink_metadata(p).is_ok()
}

/// Create `top/sub` one level at a time without following symlinks below `top` (`top` itself may be one:
/// the user chose it). A symlink or file in the way is an error; nothing is ever written through a link.
fn mkdirs_nofollow(top: &Path, sub: &Path) -> io::Result<()> {
    let mut cur = top.to_path_buf();
    for c in sub.components() {
        cur.push(c);
        match fs::symlink_metadata(&cur) {
            Ok(m) if m.is_dir() => {}
            Ok(m) => {
                let what = if m.file_type().is_symlink() {
                    "a symlink"
                } else {
                    "a file"
                };
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    format!("{} is {what}, not a folder", cur.display()),
                ));
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => fs::create_dir(&cur)?,
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

/// Create or truncate `p`; a symlink at `p` is replaced by the new file, never followed.
fn create_nofollow(p: &Path) -> io::Result<File> {
    let open = || {
        OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(p)
    };
    match open() {
        Err(e) if e.raw_os_error() == Some(libc::ELOOP) => {
            fs::remove_file(p)?; // removes the link itself
            open()
        }
        r => r,
    }
}

/// Bytes an unprivileged user may still write on the filesystem holding `dir` (or its nearest existing
/// parent). None if it can't be determined.
pub fn free_space(dir: &Path) -> Option<u64> {
    let p = dir
        .ancestors()
        .map(|a| {
            if a.as_os_str().is_empty() {
                Path::new(".")
            } else {
                a
            }
        })
        .find(|a| a.exists())?;
    let c = CString::new(p.as_os_str().as_bytes()).ok()?;
    // SAFETY: valid NUL-terminated path, zeroed out-struct.
    let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
    (unsafe { libc::statvfs(c.as_ptr(), &mut st) } == 0)
        .then(|| st.f_bavail as u64 * st.f_frsize as u64)
}

/// File kinds whose first bytes are known, and those bytes. After decryption with the right key they match;
/// with the wrong key they are noise.
const MAGICS: &[(&str, &[&[u8]])] = &[
    (".SFO", &[b"\0PSF"]),
    (".PNG", &[b"\x89PNG"]),
    ("EBOOT.BIN", &[b"SCE\0", b"\x7FELF"]),
    (".SELF", &[b"SCE\0"]),
    (".SPRX", &[b"SCE\0", b"\x7FELF"]),
    (".PBP", &[b"\0PBP"]),
    (".EDAT", &[b"NPD\0"]),
    (".SDAT", &[b"NPD\0"]),
];

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

/// Package metadata: records after the header, not encrypted: id u32, size u32, data (big-endian).
/// Layout from RPCS3's `package_reader::read_metadata` (rpcs3/Crypto/unpkg.cpp). Not yet checked against a
/// real package by this project; a malformed block only leaves fields empty, it never fails `open`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Metadata {
    /// 0x1
    pub drm_type: Option<u32>,
    /// 0x2
    pub content_type: Option<u32>,
    /// 0x3, bit flags (`PKG_FLAGS`)
    pub package_flags: Option<u32>,
    /// 0x4
    pub package_size: Option<u64>,
    /// 0x5: package version, "01.00"
    pub package_version: Option<String>,
    /// 0x6
    pub title_id: Option<String>,
    /// 0x8: minimum system software, "03.41"
    pub min_firmware: Option<String>,
    /// 0x8: application version, "01.00"
    pub app_version: Option<String>,
    /// 0xA: install folder (DLC)
    pub install_dir: Option<String>,
}

/// Content types, from RPCS3's `pkg_content_type`.
const CONTENT_TYPES: &[(u32, &str)] = &[
    (0x04, "Game data / patch"),
    (0x05, "Game"),
    (0x06, "PS1 Classic"),
    (0x07, "PSP / PC Engine"),
    (0x09, "Theme"),
    (0x0A, "Widget"),
    (0x0B, "License"),
    (0x0C, "VSH module"),
    (0x0D, "PSN avatar"),
    (0x0E, "PSP go"),
    (0x0F, "Minis"),
    (0x10, "NEOGEO"),
    (0x11, "Virtual memory card"),
    (0x12, "PS2 Classic"),
    (0x14, "PSP Remastered"),
    (0x15, "PS Vita game data"),
    (0x16, "PS Vita add-on"),
    (0x17, "PS Vita LiveArea"),
    (0x18, "PS Vita PSM"),
    (0x19, "Web TV"),
    (0x1F, "PS Vita theme"),
];

/// Package flag bits, from RPCS3's `pkg_flag`.
const PKG_FLAGS: &[(u32, &str)] = &[
    (0x02, "EBOOT"),
    (0x04, "requires licence"),
    (0x08, "HDD/MC"),
    (0x10, "patch"),
    (0x40, "rename directory"),
    (0x80, "EDAT"),
    (0x200, "emulator"),
    (0x400, "VSH module"),
    (0x800, "disc bound"),
];

impl Metadata {
    fn parse(b: &[u8], count: u32) -> Metadata {
        let mut m = Metadata::default();
        let mut o = 0usize;
        // BCD bytes → "01.00"
        let ver = |x: &[u8]| format!("{:02X}.{:02X}", x[0], x[1]);
        let text = |x: &[u8]| {
            let x = &x[..x.iter().position(|&c| c == 0).unwrap_or(x.len())];
            Some(String::from_utf8_lossy(x).trim().to_string()).filter(|s| !s.is_empty())
        };
        for _ in 0..count.min(64) {
            let (Some(id), Some(size)) = (b.get(o..o + 4), b.get(o + 4..o + 8)) else {
                break;
            };
            let (id, size) = (be32(id, 0), be32(size, 0) as usize);
            let Some(d) = o.checked_add(8 + size).and_then(|end| b.get(o + 8..end)) else {
                break;
            };
            match (id, d.len()) {
                (0x1, 4) => m.drm_type = Some(be32(d, 0)),
                (0x2, 4) => m.content_type = Some(be32(d, 0)),
                (0x3, 4) => m.package_flags = Some(be32(d, 0)),
                (0x4, 8) => m.package_size = Some(be64(d, 0)),
                (0x5, 4) => m.package_version = Some(ver(&d[2..4])),
                (0x6, 12) => m.title_id = text(d),
                (0x8, 8) => {
                    m.min_firmware = Some(ver(&d[1..3]));
                    m.app_version = Some(ver(&d[6..8]));
                }
                (0xA, n) if n > 8 => m.install_dir = text(&d[8..]),
                _ => {}
            }
            o += 8 + size;
        }
        m
    }

    pub fn content_type_name(&self) -> Option<&'static str> {
        let c = self.content_type?;
        Some(
            CONTENT_TYPES
                .iter()
                .find(|t| t.0 == c)
                .map_or("unknown", |t| t.1),
        )
    }

    pub fn flag_names(&self) -> Vec<&'static str> {
        let f = self.package_flags.unwrap_or(0);
        PKG_FLAGS
            .iter()
            .filter(|p| f & p.0 != 0)
            .map(|p| p.1)
            .collect()
    }

    /// (label, value) for display. DRM type names are not confirmed by a code source (marked so).
    pub fn rows(&self) -> Vec<(&'static str, String)> {
        let mut r = vec![];
        if let (Some(c), Some(name)) = (self.content_type, self.content_type_name()) {
            r.push(("Content type", format!("{name} (0x{c:X})")));
        }
        if let Some(d) = self.drm_type {
            let name = match d {
                1 => "Network (unverified)",
                2 => "Local (unverified)",
                3 => "Free (unverified)",
                _ => "unknown",
            };
            r.push(("DRM type", format!("{name} (0x{d:X})")));
        }
        if let Some(f) = self.package_flags {
            r.push((
                "Package flags",
                format!("{} (0x{f:X})", self.flag_names().join(", ")),
            ));
        }
        for (label, v) in [
            ("Package version", &self.package_version),
            ("App version", &self.app_version),
            ("Needs system software", &self.min_firmware),
            ("Title ID", &self.title_id),
            ("Install folder", &self.install_dir),
        ] {
            if let Some(v) = v {
                r.push((label, v.clone()));
            }
        }
        if let Some(n) = self.package_size {
            r.push(("Package size", round_bytes(n)));
        }
        r
    }

    /// Requires a licence (`.rap`) to run, per package flag 0x04.
    #[cfg_attr(not(test), allow(dead_code))] // the .rap check (F27) uses it
    pub fn needs_licence(&self) -> Option<bool> {
        self.package_flags.map(|f| f & 0x04 != 0)
    }
}

pub enum Progress<'a> {
    File(&'a Entry),
    Bytes(u64),
}

pub struct Pkg {
    file: File,
    len: u64,
    pub header: Header,
    pub meta: Metadata,
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
        if raw[0..4] == *b"\x7FCNT" {
            return Self::open_ps4(file, len);
        }
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
        // Metadata sits between its offset and the data section; read at most 64 KiB of it.
        let (start, end) = (header.metadata_offset as u64, header.data_offset.min(len));
        let meta = if start >= HEADER_LEN as u64 && start < end {
            let mut b = vec![0u8; (end - start).min(1 << 16) as usize];
            file.read_exact_at(&mut b, start)?;
            Metadata::parse(&b, header.metadata_count)
        } else {
            Metadata::default()
        };
        let mut pkg = Pkg {
            file,
            len,
            header,
            meta,
            entries: vec![],
        };
        pkg.entries = pkg.read_table()?;
        Ok(pkg)
    }

    /// PS4: the header and the entries stored unencrypted that have a name in the package's own name table
    /// (param.sfo, icon0.png, pic0/pic1.png, playgo-…), listed under `sce_sys/` as on an installed game. The
    /// rest (licence, keys, the game image) is encrypted with keys PkgView doesn't have, and isn't listed.
    fn open_ps4(file: File, len: u64) -> io::Result<Pkg> {
        if len < PS4_HEADER_LEN {
            return Err(bad("This file is not supported."));
        }
        let mut h = vec![0u8; PS4_HEADER_LEN as usize];
        file.read_exact_at(&mut h, 0)?;
        let mut pkg = Pkg {
            file,
            len,
            header: Header::parse_ps4(&h),
            meta: Metadata::default(),
            entries: vec![],
        };
        pkg.entries = pkg.read_table_ps4()?;
        Ok(pkg)
    }

    /// Bytes `off..off+n` of the file, or a cut-off / corrupt error.
    fn read_at(&self, off: u64, n: u64, what: &str) -> io::Result<Vec<u8>> {
        if off.checked_add(n).is_none_or(|end| end > self.len) {
            return Err(self.past_end(what));
        }
        let mut b = vec![0u8; n as usize];
        self.file.read_exact_at(&mut b, off)?;
        Ok(b)
    }

    fn read_table_ps4(&self) -> io::Result<Vec<Entry>> {
        let n = self.header.item_count as u64;
        let table = self.header.ps4.as_ref().map_or(0, |p| p.table_offset);
        // ponytail: real packages have about 20 entries; 64k is a corrupt-header guard, not a format limit.
        if n > 1 << 16 {
            return Err(bad("corrupt entry table"));
        }
        let t = self.read_at(table, n * 32, "entry table lies outside the file")?;
        // (id, name offset, flags, offset, size)
        let recs: Vec<(u32, u32, u32, u64, u64)> = t
            .chunks_exact(32)
            .map(|r| {
                (
                    be32(r, 0),
                    be32(r, 4),
                    be32(r, 8),
                    be32(r, 16) as u64,
                    be32(r, 20) as u64,
                )
            })
            .collect();
        let names = match recs.iter().find(|r| r.0 == 0x200) {
            Some(r) if r.4 <= 1 << 20 => {
                self.read_at(r.3, r.4, "name table lies outside the file")?
            }
            _ => vec![],
        };
        let mut entries = vec![];
        for &(_, name_off, flags, offset, size) in &recs {
            if name_off == 0 || flags & 0x8000_0000 != 0 {
                continue;
            }
            let raw = names
                .get(name_off as usize..)
                .map(|r| &r[..r.iter().position(|&c| c == 0).unwrap_or(r.len())])
                .ok_or_else(|| bad("file name outside name table"))?;
            let path = format!("sce_sys/{}", sanitize(raw)?);
            // read_decrypt reads whole 16-byte blocks
            if offset
                .checked_add(size.next_multiple_of(16))
                .is_none_or(|end| end > self.len)
            {
                return Err(self.past_end(&format!("{path}: data lies outside the file")));
            }
            entries.push(Entry {
                path,
                name_offset: name_off,
                offset,
                size,
                flags: 0x03,
            });
        }
        Ok(entries)
    }

    /// An offset beyond the end: a cut-off download (UnexpectedEof) when the file is shorter than the
    /// header's total size, otherwise a corrupt package.
    fn past_end(&self, what: &str) -> io::Error {
        if self.len < self.header.total_size {
            io::Error::new(
                io::ErrorKind::UnexpectedEof,
                format!(
                    "the file is cut off ({} of {} bytes): download it again",
                    thousands(self.len),
                    thousands(self.header.total_size)
                ),
            )
        } else {
            bad(format!("{what} (corrupt package)"))
        }
    }

    fn table_cipher(&self) -> Cipher {
        match self.header.pkg_type {
            TYPE_DEBUG_PS3 => Cipher::Debug,
            TYPE_RETAIL_PSP => Cipher::Psp,
            _ => Cipher::Ps3,
        }
    }

    pub fn cipher_for(&self, e: &Entry) -> Cipher {
        if self.header.ps4.is_some() {
            return Cipher::None;
        }
        // Debug pkgs: SHA-1 keystream for every entry, as RPCS3's unpkg.cpp does (it ignores the entry key
        // there). The original used the PSP AES key for 0x90 entries even in debug pkgs.
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
            .ok_or_else(|| self.past_end("pkg points past the end of the file"))?;
        self.file.read_exact_at(buf, abs)?;
        if c != Cipher::None {
            Keystream::new(&self.header, c).apply(rel / 16, buf);
        }
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
        let mut recs = vec![0u8; n as usize * 32];
        self.read_decrypt(tc, 0, &mut recs)?;
        // Records, then names. Sized from where the names really end (the original took the first
        // entry's data offset, which a crafted pkg can point gigabytes away).
        let table_len = recs
            .chunks_exact(32)
            .map(|r| be32(r, 0) as u64 + be32(r, 4) as u64)
            .max()
            .unwrap_or(0)
            .max(n * 32);
        // ponytail: names are packed after the records in every known pkg; 1 KiB per name is generous.
        if table_len > avail || table_len > n * (32 + 1024) {
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
                    return Err(self.past_end(&format!("{}: data lies outside the file", e.path)));
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

    /// Every key of the package's PARAM.SFO (the shallowest one), in file order; empty if there is none.
    pub fn sfo(&self) -> Vec<(String, String)> {
        self.find("PARAM.SFO")
            .and_then(|e| self.read_entry(e).ok())
            .and_then(|b| parse_sfo(&b))
            .unwrap_or_default()
    }

    /// TITLE, TITLE_ID, APP_VER and CATEGORY from PARAM.SFO, if there is one.
    pub fn sfo_summary(&self) -> Vec<(String, String)> {
        self.sfo()
            .into_iter()
            .filter(|(k, _)| ["TITLE", "TITLE_ID", "APP_VER", "CATEGORY"].contains(&k.as_str()))
            .collect()
    }

    pub fn read_entry(&self, e: &Entry) -> io::Result<Vec<u8>> {
        if e.size > MAX_IN_MEMORY {
            return Err(bad("entry too large to load into memory"));
        }
        self.read_head(e, e.size)
    }

    /// The first `max` bytes of an entry (all of it if smaller), decrypted. For previews.
    pub fn read_head(&self, e: &Entry, max: u64) -> io::Result<Vec<u8>> {
        let n = e.size.min(max);
        let mut buf = vec![0u8; n.next_multiple_of(16) as usize];
        self.read_decrypt(self.cipher_for(e), e.offset, &mut buf)?;
        buf.truncate(n as usize);
        Ok(buf)
    }

    /// Extract `indices` into `<dir>/<pkg_name>/<root>/…` and write `<pkg_name>.log` next to it, like
    /// the original (`opts` can drop the `<pkg_name>` level or flatten). Returns Ok(false) if cancelled;
    /// a cancelled or failed file is deleted. `Overwrite::Ask` replaces (see `extract_asking`).
    pub fn extract(
        &self,
        pkg_name: &str,
        indices: &[usize],
        dir: &Path,
        opts: &ExtractOptions,
        cancel: &AtomicBool,
        progress: impl FnMut(Progress),
    ) -> io::Result<bool> {
        let mut replace = |_: &Path| Overwrite::Replace;
        self.extract_asking(pkg_name, indices, dir, opts, cancel, progress, &mut replace)
    }

    /// `extract`, with `ask(existing file)` deciding Skip / Replace / Rename when the policy is Ask.
    /// Nothing below `dir` is written through a symlink.
    #[allow(clippy::too_many_arguments)]
    pub fn extract_asking(
        &self,
        pkg_name: &str,
        indices: &[usize],
        dir: &Path,
        opts: &ExtractOptions,
        cancel: &AtomicBool,
        mut progress: impl FnMut(Progress),
        ask: &mut dyn FnMut(&Path) -> Overwrite,
    ) -> io::Result<bool> {
        // ponytail: counts every file, also ones Skip will leave alone; good enough to fail early.
        let need: u64 = indices
            .iter()
            .map(|&i| &self.entries[i])
            .filter(|e| !e.is_dir())
            .map(|e| e.size)
            .sum();
        if let Some(free) = free_space(dir).filter(|&f| need > f) {
            return Err(io::Error::other(format!(
                "not enough space in {}: needs {}, {} free",
                dir.display(),
                round_bytes(need),
                round_bytes(free)
            )));
        }
        let base_rel = if opts.pkg_folder {
            PathBuf::from(pkg_name)
        } else {
            PathBuf::new()
        };
        let base = dir.join(&base_rel);
        let root = base.join(self.header.root_folder());
        fs::create_dir_all(dir)?;
        mkdirs_nofollow(
            dir,
            &if opts.keep_paths {
                base_rel.join(self.header.root_folder())
            } else {
                base_rel
            },
        )?;
        let mut log = BufWriter::new(create_nofollow(&base.join(format!("{pkg_name}.log")))?);
        // Files written by this run: a second entry landing on one of them (flattening) is renamed.
        let mut taken = HashSet::new();
        for &i in indices {
            if cancel.load(Ordering::Relaxed) {
                writeln!(log, "[{}] Extraction cancelled.", now_hms())?;
                log.flush()?;
                return Ok(false);
            }
            let e = &self.entries[i];
            progress(Progress::File(e));
            let mut target = match (e.is_dir(), opts.keep_paths) {
                (true, false) => continue,
                (_, true) => root.join(&e.path),
                (false, false) => base.join(e.name()),
            };
            let rel = |t: &Path| t.strip_prefix(&base).unwrap_or(t).display().to_string();
            let below = |t: &Path| t.strip_prefix(dir).unwrap_or(t).to_path_buf();
            let r = if e.is_dir() {
                mkdirs_nofollow(dir, &below(&target)).map(|_| true)
            } else {
                let policy = if taken.contains(&target) {
                    Overwrite::Rename
                } else if !present(&target) {
                    Overwrite::Replace
                } else if opts.overwrite == Overwrite::Ask {
                    ask(&target)
                } else {
                    opts.overwrite
                };
                match policy {
                    Overwrite::Skip => {
                        writeln!(log, "[{}] {} SKIPPED (exists)", now_hms(), rel(&target))?;
                        progress(Progress::Bytes(e.size));
                        continue;
                    }
                    Overwrite::Rename => target = free_name(&target, &taken),
                    // `ask` answering Ask itself has no better meaning than the original's Replace.
                    Overwrite::Ask | Overwrite::Replace => {}
                }
                taken.insert(target.clone());
                match target.parent() {
                    Some(p) => mkdirs_nofollow(dir, &below(p)),
                    None => Ok(()),
                }
                .and_then(|_| self.extract_file(e, &target, cancel, &mut progress))
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
        let mut out = BufWriter::new(create_nofollow(target)?);
        let res = self.write_entry(e, &mut out, cancel, progress);
        if !matches!(res, Ok(true)) {
            drop(out);
            let _ = fs::remove_file(target);
        }
        res
    }

    /// Decrypt entry `e` into `out` in 1 MiB chunks (flushed at the end). Ok(false) if cancelled.
    pub fn write_entry(
        &self,
        e: &Entry,
        out: &mut dyn Write,
        cancel: &AtomicBool,
        progress: &mut dyn FnMut(Progress),
    ) -> io::Result<bool> {
        let c = self.cipher_for(e);
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
    }

    /// Decrypt the first block of every file of a known kind (PARAM.SFO, PNGs, EBOOT.BIN, SELF/SPRX, PBP,
    /// EDAT/SDAT) and check its magic: a wrong key choice shows up as a mismatch. (entry index, ok) each.
    pub fn check_headers(&self) -> Vec<(usize, bool)> {
        let mut out = vec![];
        for (i, e) in self.entries.iter().enumerate() {
            let up = e.name().to_ascii_uppercase();
            let Some((_, magics)) = MAGICS.iter().find(|(suffix, _)| up.ends_with(suffix)) else {
                continue;
            };
            if e.is_dir() || e.size < 4 {
                continue;
            }
            let mut b = [0u8; 16];
            let ok = self
                .read_decrypt(self.cipher_for(e), e.offset, &mut b)
                .is_ok()
                && magics.iter().any(|m| b.starts_with(m));
            out.push((i, ok));
        }
        out
    }

    /// Compare the trailing digest with SHA-1 of everything before the last 32 bytes (checked on real pkgs).
    pub fn verify_sha1(&self) -> io::Result<bool> {
        Ok(self.hashes()?.0)
    }

    /// One read of the whole file: (the package's own digests match, SHA-1 of the entire file as DAT files
    /// list it). PS3/PSP: SHA-1 of everything before the 32-byte footer. PS4: SHA-256 of the header, the
    /// body and the game image (together they cover the whole file).
    pub fn hashes(&self) -> io::Result<(bool, [u8; 20])> {
        let ps4 = self.header.ps4.as_ref();
        let span = |(o, n): (u64, u64)| (o, o.saturating_add(n));
        let ranges = match ps4 {
            Some(p) => vec![(0, 0xFE0), span(p.body), span(p.pfs)],
            None => vec![(0, self.len - FOOTER_LEN)],
        };
        let (mut whole, mut s1) = (Sha1::new(), Sha1::new());
        let mut s256 = vec![Sha256::new(); ranges.len()];
        let mut buf = vec![0u8; CHUNK];
        let mut pos = 0;
        while pos < self.len {
            let n = (self.len - pos).min(CHUNK as u64) as usize;
            self.file.read_exact_at(&mut buf[..n], pos)?;
            whole.update(&buf[..n]);
            for (i, &(start, end)) in ranges.iter().enumerate() {
                let (a, b) = (start.max(pos), end.min(pos + n as u64));
                if a < b {
                    let part = &buf[(a - pos) as usize..(b - pos) as usize];
                    match ps4 {
                        Some(_) => s256[i].update(part),
                        None => s1.update(part),
                    }
                }
            }
            pos += n as u64;
        }
        let ok = match ps4 {
            Some(p) => {
                ranges.iter().all(|&(_, end)| end <= self.len)
                    && s256
                        .into_iter()
                        .zip(&p.digests)
                        .all(|(h, d)| h.finalize()[..] == d[..])
            }
            None => s1.finalize()[..] == self.header.sha1,
        };
        Ok((ok, whole.finalize().into()))
    }

    /// RPCS3's install folder name: the metadata's install folder (DLC), else the 9 characters at header
    /// offset 55 (the content ID's Title ID part). None if unsafe as a single folder name.
    pub fn install_dir(&self) -> Option<String> {
        let d = match &self.meta.install_dir {
            Some(d) => d.clone(),
            None => self.header.content_id.get(7..16)?.to_string(),
        };
        let d = d.trim_end_matches('\0').to_string();
        (!d.is_empty() && d != "." && d != ".." && !d.contains(['/', '\\', '\0'])).then_some(d)
    }

    /// Where RPCS3 puts this package below `dev_hdd0`, by content type (rpcs3/Crypto/unpkg.cpp,
    /// `set_install_path`). `user` is the RPCS3 user ("00000001").
    pub fn rpcs3_subdir(&self, user: &str) -> io::Result<PathBuf> {
        let dir = || {
            self.install_dir()
                .ok_or_else(|| bad("no valid install folder name in the package"))
        };
        Ok(match self.meta.content_type {
            Some(0x09) => PathBuf::from("theme"),
            Some(0x0A) => Path::new("widget").join(dir()?),
            Some(0x0B) => Path::new("home").join(user).join("exdata"),
            Some(0x0C) => Path::new("vsh/modules").join(dir()?),
            Some(0x0D) => Path::new("home").join(user).join("psn_avatar").join(dir()?),
            Some(0x11) => Path::new("tmp/vmc").join(dir()?),
            _ => Path::new("game").join(dir()?),
        })
    }

    /// Install into RPCS3's `dev_hdd0` the way RPCS3's own installer does: folder by content type, folders
    /// for entry types 0x04/0x12, files only for the entry types RPCS3 writes, an existing file replaced
    /// only if its entry has the overwrite flag (0x80000000), a new install folder removed again if the
    /// install fails or is cancelled. Nothing is written through a symlink below `hdd0`.
    pub fn install_rpcs3(
        &self,
        hdd0: &Path,
        user: &str,
        cancel: &AtomicBool,
        mut progress: impl FnMut(Progress),
    ) -> io::Result<Installed> {
        const FILE_TYPES: [u32; 15] = [
            0x01, 0x02, 0x03, 0x05, 0x06, 0x09, 0x0E, 0x10, 0x11, 0x13, 0x14, 0x15, 0x16, 0x18,
            0x19,
        ];
        if self.header.ps4.is_some() {
            return Err(bad("this is a PS4 package: RPCS3 is a PS3 emulator"));
        }
        if !hdd0.is_dir() {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!(
                    "{} doesn't exist: is that RPCS3's dev_hdd0 folder?",
                    hdd0.display()
                ),
            ));
        }
        let sub = self.rpcs3_subdir(user)?;
        let dest = hdd0.join(&sub);
        let fresh = !present(&dest);
        let mut done = Installed {
            folder: dest.clone(),
            ..Default::default()
        };
        let res = (|| {
            mkdirs_nofollow(hdd0, &sub)?;
            for e in &self.entries {
                if cancel.load(Ordering::Relaxed) {
                    return Ok(false);
                }
                progress(Progress::File(e));
                let target = dest.join(&e.path);
                let kind = e.flags & 0xFF;
                if kind == 0x04 || kind == 0x12 {
                    mkdirs_nofollow(hdd0, &sub.join(&e.path))?;
                    continue;
                }
                if !FILE_TYPES.contains(&kind) {
                    done.skipped_types += 1;
                    progress(Progress::Bytes(e.size));
                    continue;
                }
                if fs::symlink_metadata(&target).is_ok_and(|m| m.is_file())
                    && e.flags & 0x8000_0000 == 0
                {
                    done.kept += 1;
                    progress(Progress::Bytes(e.size));
                    continue;
                }
                if let Some(p) = Path::new(&e.path).parent() {
                    mkdirs_nofollow(hdd0, &sub.join(p))?;
                }
                if !self.extract_file(e, &target, cancel, &mut progress)? {
                    return Ok(false);
                }
                done.written += 1;
            }
            Ok(true)
        })();
        if fresh && !matches!(res, Ok(true)) {
            let _ = fs::remove_dir_all(&dest);
        }
        match res {
            Ok(true) => Ok(done),
            Ok(false) => Err(io::Error::new(io::ErrorKind::Interrupted, "cancelled")),
            Err(e) => Err(e),
        }
    }
}

/// What `install_rpcs3` did.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Installed {
    pub folder: PathBuf,
    pub written: usize,
    /// Existing files left alone (their entries lack the overwrite flag)
    pub kept: usize,
    /// Entries of a type RPCS3 doesn't write
    pub skipped_types: usize,
}

/// One difference between two packages (`diff`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Change {
    Added(String),
    Removed(String),
    /// path, what differs
    Changed(String, String),
}

/// Entries of `b` compared with `a` by path: added, removed, changed size / flags / folder-ness, and with
/// `content` also the decrypted bytes of same-size files. Sorted by path.
pub fn diff(a: &Pkg, b: &Pkg, content: bool) -> io::Result<Vec<Change>> {
    use std::collections::BTreeMap;
    let index = |p: &Pkg| -> BTreeMap<String, usize> {
        p.entries
            .iter()
            .enumerate()
            .map(|(i, e)| (e.path.clone(), i))
            .collect()
    };
    let (ia, ib) = (index(a), index(b));
    let mut out = vec![];
    for (path, &i) in &ia {
        let Some(&j) = ib.get(path) else {
            out.push(Change::Removed(path.clone()));
            continue;
        };
        let (x, y) = (&a.entries[i], &b.entries[j]);
        let mut what = vec![];
        if x.is_dir() != y.is_dir() {
            what.push(if y.is_dir() {
                "now a folder".into()
            } else {
                "now a file".into()
            });
        } else if !x.is_dir() && x.size != y.size {
            what.push(format!(
                "size {} → {}",
                thousands(x.size),
                thousands(y.size)
            ));
        } else if content && !x.is_dir() && entry_sha1(a, x)? != entry_sha1(b, y)? {
            what.push("content".into());
        }
        if x.flags != y.flags {
            what.push(format!("flags {:08X} → {:08X}", x.flags, y.flags));
        }
        if !what.is_empty() {
            out.push(Change::Changed(path.clone(), what.join(", ")));
        }
    }
    for path in ib.keys().filter(|p| !ia.contains_key(*p)) {
        out.push(Change::Added(path.clone()));
    }
    out.sort_by(|x, y| {
        let key = |c: &Change| match c {
            Change::Added(p) | Change::Removed(p) | Change::Changed(p, _) => p.clone(),
        };
        key(x).cmp(&key(y))
    });
    Ok(out)
}

/// SHA-1 of one entry's decrypted bytes.
fn entry_sha1(p: &Pkg, e: &Entry) -> io::Result<[u8; 20]> {
    struct H(Sha1);
    impl Write for H {
        fn write(&mut self, b: &[u8]) -> io::Result<usize> {
            self.0.update(b);
            Ok(b.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    let mut h = H(Sha1::new());
    p.write_entry(e, &mut h, &AtomicBool::new(false), &mut |_| {})?;
    Ok(h.0.finalize().into())
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
        build_id(pkg_type, items, "UP0001-NPUB12345_00-0000000000000001")
    }

    fn build_id(pkg_type: u32, items: &[Item], content_id: &str) -> Vec<u8> {
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
            content_id: content_id.into(),
            qa_digest: *b"0123456789ABCDEF",
            data_riv: riv,
            sha1: [0; 20],
            ps4: None,
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

    /// Test-only PS4 package: 0x1000 header, body (entry table, name table, data), a noise "game image",
    /// and the three SHA-256 digests where real packages have them. Items: (id, name or "", data, encrypted).
    fn build_ps4(items: &[(u32, &str, Vec<u8>, bool)]) -> Vec<u8> {
        fn put(out: &mut [u8], o: usize, b: &[u8]) {
            out[o..o + b.len()].copy_from_slice(b);
        }
        let (body, n) = (0x2000usize, items.len() + 1);
        let mut names = vec![0u8];
        let name_offs: Vec<usize> = items
            .iter()
            .map(|(_, nm, _, _)| {
                if nm.is_empty() {
                    return 0;
                }
                let o = names.len();
                names.extend_from_slice(nm.as_bytes());
                names.push(0);
                o
            })
            .collect();
        let names_at = body + n * 32;
        let mut cur = (names_at + names.len()).next_multiple_of(16);
        let mut offs = vec![];
        for (_, _, d, _) in items {
            offs.push(cur);
            cur = (cur + d.len()).next_multiple_of(16);
        }
        let body_end = cur.next_multiple_of(0x100);
        let image = noise(0x3000, 9);
        let mut out = vec![0u8; body_end + image.len()];
        let total = out.len();
        put(&mut out, 0, b"\x7FCNT");
        put(&mut out, 4, &0x8100_0001u32.to_be_bytes());
        put(&mut out, 0x10, &(n as u32).to_be_bytes());
        put(&mut out, 0x18, &(body as u32).to_be_bytes());
        put(&mut out, 0x20, &(body as u64).to_be_bytes());
        put(&mut out, 0x28, &((body_end - body) as u64).to_be_bytes());
        put(&mut out, 0x40, b"EP0001-CUSA00001_00-0000000000000001");
        put(&mut out, 0x70, &0xFu32.to_be_bytes());
        put(&mut out, 0x74, &0x1Au32.to_be_bytes());
        put(&mut out, 0x410, &(body_end as u64).to_be_bytes());
        put(&mut out, 0x418, &(image.len() as u64).to_be_bytes());
        put(&mut out, 0x430, &(total as u64).to_be_bytes());
        let rec = |id: u32, name: usize, flags: u32, off: usize, size: usize| {
            [id, name as u32, flags, 0, off as u32, size as u32, 0, 0]
                .iter()
                .flat_map(|v| v.to_be_bytes())
                .collect::<Vec<u8>>()
        };
        put(
            &mut out,
            body,
            &rec(0x200, 0, 0x4000_0000, names_at, names.len()),
        );
        for (i, (id, _, d, enc)) in items.iter().enumerate() {
            let flags = if *enc { 0x8000_0000 } else { 0 };
            put(
                &mut out,
                body + (i + 1) * 32,
                &rec(*id, name_offs[i], flags, offs[i], d.len()),
            );
            put(&mut out, offs[i], d);
        }
        put(&mut out, names_at, &names);
        put(&mut out, body_end, &image);
        let d = Sha256::digest(&out[body..body_end]);
        put(&mut out, 0x160, &d);
        put(&mut out, 0x440, &Sha256::digest(&image));
        let d = Sha256::digest(&out[..0xFE0]);
        put(&mut out, 0xFE0, &d);
        out
    }

    #[test]
    fn ps4_read_only() {
        let d = tmp("ps4");
        let f = d.join("ps4.pkg");
        let mut icon = b"\x89PNG\r\n\x1a\n".to_vec();
        icon.extend(noise(300, 3));
        let sfo_b = sfo(&[
            ("CATEGORY", "gd"),
            ("TITLE", "Test"),
            ("TITLE_ID", "CUSA00001"),
        ]);
        let bytes = build_ps4(&[
            (0x400, "", noise(1024, 1), true),
            (0x1000, "param.sfo", sfo_b, false),
            (0x1200, "icon0.png", icon.clone(), false),
            (0x1300, "trophy/secret.trp", noise(40, 2), true),
        ]);
        fs::write(&f, &bytes).unwrap();
        let p = Pkg::open(&f).unwrap();
        assert_eq!(p.header.type_label(), "PS4");
        assert_eq!(p.header.content_id, "EP0001-CUSA00001_00-0000000000000001");
        assert_eq!(p.header.root_folder(), "CUSA00001");
        let paths: Vec<&str> = p.entries.iter().map(|e| e.path.as_str()).collect();
        assert_eq!(paths, ["sce_sys/param.sfo", "sce_sys/icon0.png"]);
        assert!(p.sfo().contains(&("TITLE".into(), "Test".into())));
        assert_eq!(p.read_entry(p.find("ICON0.PNG").unwrap()).unwrap(), icon);
        assert_eq!(p.check_headers(), [(0, true), (1, true)]);
        assert_eq!(p.hashes().unwrap(), (true, Sha1::digest(&bytes).into()));
        let out = d.join("out");
        assert!(p
            .extract(
                "ps4",
                &[0, 1],
                &out,
                &Default::default(),
                &AtomicBool::new(false),
                |_| {}
            )
            .unwrap());
        assert_eq!(
            fs::read(out.join("ps4/CUSA00001/sce_sys/icon0.png")).unwrap(),
            icon
        );
        let e = p
            .install_rpcs3(&d, "00000001", &AtomicBool::new(false), |_| {})
            .unwrap_err();
        assert!(e.to_string().contains("PS4"), "{e}");
        assert_eq!(
            crate::library::Kind::of("gd", true, &p.meta),
            crate::library::Kind::Ps4
        );

        // A changed byte anywhere (here: in the game image) fails Verify; the package still opens.
        let mut bad = bytes.clone();
        let l = bad.len();
        bad[l - 5] ^= 1;
        fs::write(&f, &bad).unwrap();
        assert!(!Pkg::open(&f).unwrap().hashes().unwrap().0);
        // Cut off inside the game image: opens, shorter than its total size, fails Verify.
        fs::write(&f, &bytes[..l - 0x1000]).unwrap();
        let p = Pkg::open(&f).unwrap();
        assert!(p.len < p.header.total_size && !p.hashes().unwrap().0);
        // Cut off inside the entry data: the cut-off error.
        fs::write(&f, &bytes[..0x2100]).unwrap();
        let e = Pkg::open(&f).err().unwrap();
        assert_eq!(e.kind(), io::ErrorKind::UnexpectedEof, "{e}");
        fs::remove_dir_all(d).unwrap();
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
        // A name pointing far past the records (the table would be huge) is refused, cheaply.
        let mut b = build(TYPE_RETAIL_PS3, &[("a", Some(noise(16, 1)), 0)]);
        let h = Pkg::open(&write_pkg(&b).1).unwrap().header;
        let rec = &mut b[0x100..0x120];
        Keystream::new(&h, Cipher::Ps3).apply(0, rec);
        rec[0..4].copy_from_slice(&0x10_0000u32.to_be_bytes());
        Keystream::new(&h, Cipher::Ps3).apply(0, rec);
        b.resize(0x20_0000, 0);
        let (d, p) = write_pkg(&b);
        assert!(Pkg::open(&p).is_err(), "far name accepted");
        fs::remove_dir_all(d).unwrap();
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
        run(Overwrite::Ask); // plain extract() has nobody to ask: replaces
        assert_eq!(fs::read(root.join("odd.txt")).unwrap(), good);
        // extract_asking: the answer is used per file, and only for files that exist.
        fs::write(root.join("odd.txt"), b"mine").unwrap();
        let mut asked = vec![];
        let opts = ExtractOptions {
            overwrite: Overwrite::Ask,
            ..Default::default()
        };
        let mut ask = |p: &Path| {
            asked.push(p.file_name().unwrap().to_string_lossy().into_owned());
            if p.ends_with("odd.txt") {
                Overwrite::Skip
            } else {
                Overwrite::Rename
            }
        };
        assert!(pkg
            .extract_asking(
                "g",
                &idx,
                &d,
                &opts,
                &AtomicBool::new(false),
                |_| {},
                &mut ask
            )
            .unwrap());
        asked.sort();
        assert_eq!(asked, ["EBOOT.BIN", "odd.txt", "zero.bin"]);
        assert_eq!(fs::read(root.join("odd.txt")).unwrap(), b"mine");
        assert!(root.join("USRDIR/EBOOT (2).BIN").exists());
        fs::remove_dir_all(d).unwrap();
    }

    #[test]
    fn never_writes_through_symlinks() {
        use std::os::unix::fs::symlink;
        let (d, p) = write_pkg(&build(TYPE_RETAIL_PS3, &sample_items()));
        let pkg = Pkg::open(&p).unwrap();
        let outside = d.join("outside");
        fs::create_dir_all(&outside).unwrap();
        fs::write(outside.join("odd.txt"), b"precious").unwrap();
        let all = pkg.select(&[""]);
        let go = |dir: &Path| {
            pkg.extract(
                "g",
                &all,
                dir,
                &ExtractOptions::default(),
                &AtomicBool::new(false),
                |_| {},
            )
        };

        // A symlinked folder inside the destination is refused, nothing lands outside.
        let out = d.join("out1");
        fs::create_dir_all(out.join("g/NPUB12345")).unwrap();
        symlink(&outside, out.join("g/NPUB12345/USRDIR")).unwrap();
        let err = go(&out).unwrap_err().to_string();
        assert!(err.contains("is a symlink"), "{err}");
        assert_eq!(fs::read_dir(&outside).unwrap().count(), 1);

        // The same for the pkg folder or root folder level.
        let out = d.join("out2");
        fs::create_dir_all(&out).unwrap();
        symlink(&outside, out.join("g")).unwrap();
        assert!(go(&out).is_err());
        assert_eq!(fs::read_dir(&outside).unwrap().count(), 1);

        // A symlinked file (also the log) is replaced by a real file; its target is untouched.
        let out = d.join("out3");
        fs::create_dir_all(out.join("g/NPUB12345")).unwrap();
        symlink(outside.join("odd.txt"), out.join("g/NPUB12345/odd.txt")).unwrap();
        symlink(outside.join("odd.txt"), out.join("g/g.log")).unwrap();
        symlink(outside.join("nowhere"), out.join("g/NPUB12345/ICON0.PNG")).unwrap(); // dangling
        assert!(go(&out).unwrap());
        assert_eq!(fs::read(outside.join("odd.txt")).unwrap(), b"precious");
        assert!(!outside.join("nowhere").exists());
        for f in ["g/NPUB12345/odd.txt", "g/g.log", "g/NPUB12345/ICON0.PNG"] {
            assert!(
                !fs::symlink_metadata(out.join(f))
                    .unwrap()
                    .file_type()
                    .is_symlink(),
                "{f}"
            );
        }
        assert_eq!(
            fs::read(out.join("g/NPUB12345/ICON0.PNG")).unwrap(),
            sample_items()[5].1.clone().unwrap()
        );

        // A symlink counts as an existing file: Rename writes next to it.
        let opts = ExtractOptions {
            overwrite: Overwrite::Rename,
            ..Default::default()
        };
        let out = d.join("out4");
        fs::create_dir_all(out.join("g/NPUB12345")).unwrap();
        symlink(outside.join("odd.txt"), out.join("g/NPUB12345/odd.txt")).unwrap();
        assert!(pkg
            .extract("g", &all, &out, &opts, &AtomicBool::new(false), |_| {})
            .unwrap());
        assert!(out.join("g/NPUB12345/odd (1).txt").is_file());
        assert_eq!(fs::read(outside.join("odd.txt")).unwrap(), b"precious");

        // The destination itself may be a symlink: the user chose it.
        let link = d.join("dest-link");
        symlink(&outside, &link).unwrap();
        assert!(go(&link).unwrap());
        assert!(outside.join("g/NPUB12345/odd.txt").is_file());
        fs::remove_dir_all(d).unwrap();
    }

    /// Metadata records: id u32, size u32, data.
    fn meta(records: &[(u32, &[u8])]) -> Vec<u8> {
        let mut b = vec![];
        for (id, d) in records {
            b.extend_from_slice(&id.to_be_bytes());
            b.extend_from_slice(&(d.len() as u32).to_be_bytes());
            b.extend_from_slice(d);
        }
        b
    }

    #[test]
    fn metadata_records() {
        let md = meta(&[
            (0x1, &2u32.to_be_bytes()),
            (0x2, &6u32.to_be_bytes()),
            (0x3, &0x0000_0084u32.to_be_bytes()), // requires licence + EDAT
            (0x4, &0x1234_5678u64.to_be_bytes()),
            (0x5, &[0x17, 0x61, 0x01, 0x00]),
            (0x6, b"NPUB12345\0\0\0"),
            (0x7, &[0xAB; 24]), // QA digest: known, not shown
            (0x8, &[0x00, 0x03, 0x41, 0x50, 0x00, 0x00, 0x01, 0x02]),
            (0xA, b"\0\0\0\0\0\0\0\0NPUB12345-DLC\0\0\0"),
            (0x99, b"ignored"),
        ]);
        let m = Metadata::parse(&md, 10);
        assert_eq!(m.drm_type, Some(2));
        assert_eq!(m.content_type, Some(6));
        assert_eq!(m.needs_licence(), Some(true));
        assert_eq!(m.package_size, Some(0x1234_5678));
        assert_eq!(m.package_version.as_deref(), Some("01.00"));
        assert_eq!(m.title_id.as_deref(), Some("NPUB12345"));
        assert_eq!(m.min_firmware.as_deref(), Some("03.41"));
        assert_eq!(m.app_version.as_deref(), Some("01.02"));
        assert_eq!(m.install_dir.as_deref(), Some("NPUB12345-DLC"));
        let rows = m.rows();
        assert_eq!(rows[0], ("Content type", "PS1 Classic (0x6)".into()));
        assert_eq!(rows[1], ("DRM type", "Local (unverified) (0x2)".into()));
        assert_eq!(
            rows[2],
            ("Package flags", "requires licence, EDAT (0x84)".into())
        );
        // count limits what is read; a record running past the end stops the parse, nothing panics.
        assert_eq!(Metadata::parse(&md, 1).content_type, None);
        let cut = Metadata::parse(&md[..md.len() - 30], 10);
        assert_eq!(cut.title_id.as_deref(), Some("NPUB12345"));
        assert_eq!(cut.install_dir, None);
        assert_eq!(Metadata::parse(&[0xFF; 7], 5), Metadata::default());
        let huge = meta(&[(0x1, &[0; 4])]);
        let mut huge = huge.clone();
        huge[4..8].copy_from_slice(&u32::MAX.to_be_bytes());
        assert_eq!(Metadata::parse(&huge, 1), Metadata::default());
        for i in 0..300 {
            let mut b = md.clone();
            let j = i % b.len();
            b[j] = noise(1, i as u64)[0];
            let _ = Metadata::parse(&b, 10).rows();
        }
    }

    /// build() leaves 0xC0..0x100 free: put metadata records there (64 bytes at most) and point the header
    /// at them. The SHA-1 footer is not updated.
    fn add_meta(b: &mut [u8], records: &[(u32, &[u8])]) {
        let md = meta(records);
        assert!(md.len() <= 0x40, "metadata too big for build()'s gap");
        b[0xC0..0xC0 + md.len()].copy_from_slice(&md);
        b[0x0C..0x10].copy_from_slice(&(records.len() as u32).to_be_bytes());
    }

    #[test]
    fn metadata_from_a_pkg() {
        let mut b = build(TYPE_RETAIL_PS3, &sample_items());
        add_meta(
            &mut b,
            &[
                (0x2, &5u32.to_be_bytes()),
                (0x6, b"NPUB12345\0\0\0"),
                (0x8, &[0, 0x04, 0x20, 0, 0, 0, 0x01, 0x05]),
            ],
        );
        let (d, p) = write_pkg(&b);
        let pkg = Pkg::open(&p).unwrap();
        assert_eq!(pkg.meta.content_type, Some(5));
        assert_eq!(pkg.meta.min_firmware.as_deref(), Some("04.20"));
        assert_eq!(pkg.meta.app_version.as_deref(), Some("01.05"));
        // The usual test pkg has no records.
        let (d2, p2) = write_pkg(&build(TYPE_RETAIL_PS3, &sample_items()));
        assert_eq!(Pkg::open(&p2).unwrap().meta, Metadata::default());
        fs::remove_dir_all(d).unwrap();
        fs::remove_dir_all(d2).unwrap();
    }

    #[test]
    fn parallel_debug_keystream_matches_serial() {
        let (d, p) = write_pkg(&build(TYPE_DEBUG_PS3, &sample_items()));
        let h = Pkg::open(&p).unwrap().header;
        let Keystream::Debug(k) = Keystream::new(&h, Cipher::Debug) else {
            unreachable!()
        };
        for len in [16, 1 << 16, (1 << 20) + 16 * 7, 3 << 20] {
            let data = noise(len, len as u64);
            let mut a = data.clone();
            debug_xor(&k, 12345, &mut a);
            let mut b = data.clone();
            Keystream::new(&h, Cipher::Debug).apply(12345, &mut b);
            assert!(a == b, "len {len}");
        }
        fs::remove_dir_all(d).unwrap();
    }

    #[test]
    fn head_sfo_and_stream() {
        let items = sample_items();
        let (d, p) = write_pkg(&build(TYPE_RETAIL_PSP, &items));
        let pkg = Pkg::open(&p).unwrap();
        let eboot = pkg.find("EBOOT.BIN").unwrap();
        let want = items[2].1.as_ref().unwrap();
        assert_eq!(pkg.read_head(eboot, 37).unwrap(), want[..37]);
        assert_eq!(
            pkg.read_head(pkg.find("odd.txt").unwrap(), 1 << 20)
                .unwrap()
                .len(),
            37
        );
        assert_eq!(pkg.sfo().len(), 2);
        assert_eq!(pkg.sfo()[1], ("TITLE_ID".into(), "NPUB12345".into()));
        let mut out = vec![];
        let mut n = 0;
        let done = pkg
            .write_entry(eboot, &mut out, &AtomicBool::new(false), &mut |p| {
                if let Progress::Bytes(b) = p {
                    n += b
                }
            })
            .unwrap();
        assert!(done && &out == want && n == want.len() as u64);
        fs::remove_dir_all(d).unwrap();
    }

    #[test]
    fn rpcs3_install_follows_rpcs3() {
        let items: Vec<Item> = vec![
            ("USRDIR", None, 0),
            ("USRDIR/EBOOT.BIN", Some(noise(3000, 1)), 0x8000_0000), // overwrite flag
            ("PARAM.SFO", Some(sfo(&[("TITLE", "x")])), 0),
            ("odd.unk", Some(noise(10, 2)), 0x08), // type 0x0B: RPCS3 doesn't write it
            ("deep/a/b.bin", Some(noise(40, 3)), 0),
        ];
        let d = tmp("rpcs3");
        let hdd0 = d.join("dev_hdd0");
        let missing = Pkg::open(&write_pkg(&build(TYPE_RETAIL_PS3, &items)).1).unwrap();
        let err = missing.install_rpcs3(&hdd0, "00000001", &AtomicBool::new(false), |_| {});
        assert!(err.unwrap_err().to_string().contains("dev_hdd0"));
        fs::create_dir_all(&hdd0).unwrap();
        let pkg_at = |bytes: &[u8], name: &str| {
            let p = d.join(name);
            fs::write(&p, bytes).unwrap();
            Pkg::open(&p).unwrap()
        };
        let go = |p: &Pkg| p.install_rpcs3(&hdd0, "00000001", &AtomicBool::new(false), |_| {});
        let p = pkg_at(&build(TYPE_RETAIL_PS3, &items), "a.pkg");
        assert_eq!(p.install_dir().as_deref(), Some("NPUB12345"));
        let r = go(&p).unwrap();
        let game = hdd0.join("game/NPUB12345");
        assert_eq!(r.folder, game);
        assert_eq!((r.written, r.kept, r.skipped_types), (3, 0, 1));
        assert_eq!(
            fs::read(game.join("USRDIR/EBOOT.BIN")).unwrap(),
            noise(3000, 1)
        );
        assert!(game.join("deep/a/b.bin").is_file() && !game.join("odd.unk").exists());
        // Again over changed files: only the entry with the overwrite flag replaces its file.
        fs::write(game.join("USRDIR/EBOOT.BIN"), b"old").unwrap();
        fs::write(game.join("PARAM.SFO"), b"mine").unwrap();
        let r = go(&p).unwrap();
        assert_eq!((r.written, r.kept), (1, 2)); // EBOOT.BIN replaced; PARAM.SFO and deep/a/b.bin kept
        assert_eq!(
            fs::read(game.join("USRDIR/EBOOT.BIN")).unwrap(),
            noise(3000, 1)
        );
        assert_eq!(fs::read(game.join("PARAM.SFO")).unwrap(), b"mine");
        // Folder by content type, and the metadata's install folder (DLC).
        let with = |records: &[(u32, &[u8])], name: &str| {
            let mut b = build(TYPE_RETAIL_PS3, &items[..3]);
            add_meta(&mut b, records);
            pkg_at(&b, name)
        };
        let theme = with(&[(0x2, &9u32.to_be_bytes())], "t.pkg");
        assert_eq!(go(&theme).unwrap().folder, hdd0.join("theme"));
        let lic = with(&[(0x2, &0xBu32.to_be_bytes())], "l.pkg");
        assert_eq!(go(&lic).unwrap().folder, hdd0.join("home/00000001/exdata"));
        let dlc = with(&[(0xA, b"\0\0\0\0\0\0\0\0NPUB12345-DLC1\0\0")], "d.pkg");
        assert_eq!(go(&dlc).unwrap().folder, hdd0.join("game/NPUB12345-DLC1"));
        let evil = with(&[(0xA, b"\0\0\0\0\0\0\0\0../../x\0")], "e.pkg");
        assert!(evil.install_dir().is_none() && go(&evil).is_err());
        // Cancelled into a new folder: the folder is removed again.
        let cancel = AtomicBool::new(false);
        let mut b = build_id(
            TYPE_RETAIL_PS3,
            &items,
            "UP0001-NPUB77777_00-0000000000000001",
        );
        b[0x0C..0x10].copy_from_slice(&0u32.to_be_bytes());
        let p2 = pkg_at(&b, "c.pkg");
        let e = p2.install_rpcs3(&hdd0, "00000001", &cancel, |pr| {
            if let Progress::Bytes(_) = pr {
                cancel.store(true, Ordering::Relaxed)
            }
        });
        assert_eq!(e.unwrap_err().kind(), io::ErrorKind::Interrupted);
        assert!(!hdd0.join("game/NPUB77777").exists());
        // Never through a symlink below dev_hdd0.
        let hdd1 = d.join("hdd1");
        fs::create_dir_all(&hdd1).unwrap();
        std::os::unix::fs::symlink(d.join("elsewhere"), hdd1.join("game")).unwrap();
        assert!(p
            .install_rpcs3(&hdd1, "00000001", &AtomicBool::new(false), |_| {})
            .is_err());
        assert!(!d.join("elsewhere").exists());
        fs::remove_dir_all(d).unwrap();
    }

    #[test]
    fn diff_and_whole_file_sha1() {
        let a_items: Vec<Item> = vec![
            ("same.bin", Some(noise(50, 1)), 0),
            ("gone.bin", Some(noise(20, 2)), 0),
            ("bigger.bin", Some(noise(30, 3)), 0),
            ("flags.bin", Some(noise(16, 4)), 0),
            ("content.bin", Some(noise(64, 5)), 0),
        ];
        let mut b_items = a_items.clone();
        b_items.remove(1);
        b_items[1].1 = Some(noise(31, 3));
        b_items[2].2 = 0x8000_0000;
        b_items[3].1 = Some(noise(64, 6));
        b_items.push(("new/added.bin", Some(noise(8, 7)), 0));
        let (da, pa) = write_pkg(&build(TYPE_RETAIL_PS3, &a_items));
        let (db, pb) = write_pkg(&build(TYPE_DEBUG_PS3, &b_items));
        let (a, b) = (Pkg::open(&pa).unwrap(), Pkg::open(&pb).unwrap());
        let quick = diff(&a, &b, false).unwrap();
        assert_eq!(
            quick,
            [
                Change::Changed("bigger.bin".into(), "size 30 → 31".into()),
                Change::Changed("flags.bin".into(), "flags 00000003 → 80000003".into()),
                Change::Removed("gone.bin".into()),
                Change::Added("new/added.bin".into()),
            ]
        );
        let deep = diff(&a, &b, true).unwrap();
        assert!(
            deep.contains(&Change::Changed("content.bin".into(), "content".into())),
            "{deep:?}"
        );
        assert!(diff(&a, &a, true).unwrap().is_empty());
        let (ok, whole) = a.hashes().unwrap();
        assert!(ok);
        assert_eq!(whole[..], Sha1::digest(fs::read(&pa).unwrap())[..]);
        fs::remove_dir_all(da).unwrap();
        fs::remove_dir_all(db).unwrap();
    }

    #[test]
    fn free_space_check() {
        let d = tmp("free");
        let free = free_space(&d).unwrap();
        assert!(free > 0);
        // Same filesystem via the nearest existing parent (other tests write meanwhile: compare loosely).
        let below = free_space(&d.join("not/yet/there")).unwrap();
        assert!(below.abs_diff(free) < 1 << 30, "{below} vs {free}");
        assert!(free_space(Path::new("relative/dir")).is_some());
        fs::remove_dir_all(d).unwrap();
    }

    #[test]
    fn header_check_finds_wrong_keys() {
        let png = [b"\x89PNG\r\n\x1a\n".as_slice(), &noise(40, 1)].concat();
        let items: Vec<Item> = vec![
            ("PARAM.SFO", Some(sfo(&[("TITLE", "x")])), 0),
            ("ICON0.PNG", Some(png.clone()), 0),
            (
                "USRDIR/EBOOT.BIN",
                Some([b"SCE\0".as_slice(), &noise(60, 2)].concat()),
                0,
            ),
            (
                "USRDIR/CONTENT/EBOOT.PBP",
                Some([b"\0PBP".as_slice(), &noise(30, 3)].concat()),
                0x9000_0000,
            ),
            (
                "USRDIR/ISO.BIN.EDAT",
                Some([b"NPD\0".as_slice(), &noise(30, 4)].concat()),
                0,
            ),
            ("USRDIR/fake.png", Some(noise(30, 5)), 0),
            ("USRDIR/other.bin", Some(noise(30, 6)), 0),
        ];
        for t in [TYPE_DEBUG_PS3, TYPE_RETAIL_PS3, TYPE_RETAIL_PSP] {
            let (d, p) = write_pkg(&build(t, &items));
            let pkg = Pkg::open(&p).unwrap();
            let got: Vec<(&str, bool)> = pkg
                .check_headers()
                .iter()
                .map(|&(i, ok)| (pkg.entries[i].path.as_str(), ok))
                .collect();
            assert_eq!(
                got,
                [
                    ("PARAM.SFO", true),
                    ("ICON0.PNG", true),
                    ("USRDIR/EBOOT.BIN", true),
                    ("USRDIR/CONTENT/EBOOT.PBP", true),
                    ("USRDIR/ISO.BIN.EDAT", true),
                    ("USRDIR/fake.png", false),
                ],
                "{t:#x}"
            );
            fs::remove_dir_all(d).unwrap();
        }
    }

    /// Keystream block `n`, computed independently of `Keystream` (u128 / RPCS3-style counter).
    fn oracle_block(h: &Header, c: Cipher, n: u64) -> [u8; 16] {
        match c {
            Cipher::Debug => {
                let mut k = [0u8; 64];
                for (j, half) in [0, 0, 1, 1].iter().enumerate() {
                    k[j * 8..j * 8 + 8].copy_from_slice(&h.qa_digest[half * 8..half * 8 + 8]);
                }
                k[56..].copy_from_slice(&n.to_be_bytes());
                Sha1::digest(k)[..16].try_into().unwrap()
            }
            Cipher::None => [0; 16],
            Cipher::Ps3 | Cipher::Psp => {
                let key = if c == Cipher::Ps3 { PS3_KEY } else { PSP_KEY };
                let ctr = u128::from_be_bytes(h.data_riv).wrapping_add(n as u128);
                let mut b = GenericArray::from(ctr.to_be_bytes());
                Aes128::new(&key.into()).encrypt_block(&mut b);
                b.as_slice().try_into().unwrap()
            }
        }
    }

    fn oracle_xor(h: &Header, c: Cipher, first: u64, buf: &mut [u8]) {
        for (i, blk) in buf.chunks_mut(16).enumerate() {
            let k = oracle_block(h, c, first + i as u64);
            blk.iter_mut().zip(k).for_each(|(b, k)| *b ^= k);
        }
    }

    /// An entry stored 4 GiB+ into the data section (sparse file, no real disk use) decrypts correctly:
    /// guards against 32-bit offsets or block counters anywhere on the read path.
    #[test]
    fn entry_beyond_4gib() {
        const FAR: u64 = 0x1_0000_0040;
        let plain = noise(40, 77);
        for (t, hi) in [
            (TYPE_DEBUG_PS3, 0),
            (TYPE_RETAIL_PS3, 0),
            (TYPE_RETAIL_PS3, 0x9000_0000),
            (TYPE_RETAIL_PSP, 0x9000_0000),
        ] {
            let items: Vec<Item> = vec![
                ("first.bin", Some(noise(16, 1)), 0),
                ("big.bin", Some(plain.clone()), hi),
            ];
            let mut bytes = build(t, &items);
            let (d, p) = write_pkg(&bytes);
            let h = Pkg::open(&p).unwrap().header;
            let table_c = match t {
                TYPE_DEBUG_PS3 => Cipher::Debug,
                TYPE_RETAIL_PSP => Cipher::Psp,
                _ => Cipher::Ps3,
            };
            let item_c = match (t, hi) {
                (TYPE_DEBUG_PS3, _) => Cipher::Debug,
                (_, 0x9000_0000) => Cipher::Psp,
                _ => Cipher::Ps3,
            };
            // Move the entry: rewrite its record's data offset, then put the data at FAR.
            let rec = &mut bytes[0x120..0x140]; // record 1 = blocks 2 and 3
            oracle_xor(&h, table_c, 2, rec);
            rec[8..16].copy_from_slice(&FAR.to_be_bytes());
            oracle_xor(&h, table_c, 2, rec);
            fs::write(&p, &bytes).unwrap();
            let mut data = plain.clone();
            data.resize(48, 0);
            oracle_xor(&h, item_c, FAR / 16, &mut data);
            let f = OpenOptions::new().write(true).open(&p).unwrap();
            f.write_all_at(&data, 0x100 + FAR).unwrap();
            f.set_len(0x100 + FAR + 48 + FOOTER_LEN).unwrap();
            drop(f);

            let pkg = Pkg::open(&p).unwrap();
            assert_eq!(pkg.entries[1].offset, FAR);
            assert_eq!(
                pkg.read_entry(&pkg.entries[1]).unwrap(),
                plain,
                "{t:#x} {hi:#x}"
            );
            assert!(pkg
                .extract(
                    "g",
                    &[1],
                    &d,
                    &ExtractOptions::default(),
                    &AtomicBool::new(false),
                    |_| {}
                )
                .unwrap());
            assert_eq!(fs::read(d.join("g/NPUB12345/big.bin")).unwrap(), plain);
            fs::remove_dir_all(d).unwrap();
        }
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

    /// A made-up collection for `audit`, `rename` and the library window:
    /// `PKGVIEW_WRITE_COLLECTION=/tmp/c cargo test write_collection -- --ignored`. Game + identical copy +
    /// its patch, a patch without its game, a patch for a disc game, a PS1 Classic (debug), the same one
    /// corrupted, a cut-off download, a non-package, and a .rap for the game.
    #[test]
    #[ignore]
    fn write_collection() {
        let dir = PathBuf::from(
            std::env::var("PKGVIEW_WRITE_COLLECTION").expect("set PKGVIEW_WRITE_COLLECTION"),
        );
        fs::create_dir_all(&dir).unwrap();
        let icon = || Some(include_bytes!("../data/pkgview.png").to_vec());
        let one = |t: u32, cid: &str, sfo_kv: &[(&str, &str)], extra: Vec<Item>| {
            let mut items: Vec<Item> = vec![
                ("PARAM.SFO", Some(sfo(sfo_kv)), 0),
                ("ICON0.PNG", icon(), 0),
            ];
            items.extend(extra);
            build_id(t, &items, cid)
        };
        let eboot = || {
            (
                "USRDIR/EBOOT.BIN",
                Some([b"SCE\0".as_slice(), &noise(5000, 1)].concat()),
                0,
            )
        };
        let mut game = one(
            TYPE_RETAIL_PS3,
            "UP0001-NPUB12345_00-0000000000000001",
            &[
                ("CATEGORY", "HG"),
                ("TITLE", "Demo Game: Deluxe"),
                ("TITLE_ID", "NPUB12345"),
                ("APP_VER", "01.00"),
            ],
            vec![("USRDIR", None, 0), eboot()],
        );
        add_meta(
            &mut game,
            &[(0x2, &5u32.to_be_bytes()), (0x3, &0x06u32.to_be_bytes())],
        );
        let end = game.len() - FOOTER_LEN as usize;
        let digest = Sha1::digest(&game[..end]);
        game[end..end + 20].copy_from_slice(&digest);
        let patch = |cid: &str, id: &str, title: &str, ver: &str| {
            one(
                TYPE_RETAIL_PS3,
                cid,
                &[
                    ("CATEGORY", "GD"),
                    ("TITLE", title),
                    ("TITLE_ID", id),
                    ("APP_VER", ver),
                ],
                vec![("USRDIR", None, 0), eboot()],
            )
        };
        let classic = one(
            TYPE_DEBUG_PS3,
            "JP0001-NPUJ00001_00-0000000000000001",
            &[
                ("CATEGORY", "1P"),
                ("TITLE", "Demo Classic"),
                ("TITLE_ID", "NPUJ00001"),
                ("APP_VER", "01.00"),
            ],
            vec![
                ("USRDIR", None, 0),
                (
                    "USRDIR/CONTENT/EBOOT.PBP",
                    Some([b"\0PBP".as_slice(), &noise(3000, 2)].concat()),
                    0,
                ),
            ],
        );
        let mut corrupt = classic.clone();
        let at = corrupt.len() - 200;
        corrupt[at] ^= 0xFF;
        let p2 = patch(
            "UP0001-NPUB12345_00-0000000000000002",
            "NPUB12345",
            "Demo Game: Deluxe",
            "01.01",
        );
        let files: Vec<(&str, Vec<u8>)> = vec![
            ("UP0001-NPUB12345_00-0000000000000001.pkg", game.clone()),
            ("game copy.pkg", game),
            ("patch.pkg", p2.clone()),
            (
                "lonely patch.pkg",
                patch(
                    "UP0001-NPUB99999_00-0000000000000001",
                    "NPUB99999",
                    "Lost Game",
                    "01.02",
                ),
            ),
            (
                "disc patch.pkg",
                patch(
                    "UP0001-BLUS30001_00-0000000000000001",
                    "BLUS30001",
                    "Disc Game",
                    "01.03",
                ),
            ),
            ("classic.pkg", classic),
            ("classic damaged.pkg", corrupt),
            ("cut download.pkg", p2[..p2.len() / 2].to_vec()),
            ("notes.pkg", b"not a package".to_vec()),
        ];
        for (name, bytes) in files {
            fs::write(dir.join(name), bytes).unwrap();
        }
        fs::write(
            dir.join("UP0001-NPUB12345_00-0000000000000001.rap"),
            [7u8; 16],
        )
        .unwrap();
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
        // A real EBOOT.BIN is a SELF: give the synthetic one its header so `verify` passes.
        for it in items.iter_mut().filter(|i| i.0.ends_with("EBOOT.BIN")) {
            if let Some(d) = it.1.as_mut() {
                d[..4].copy_from_slice(b"SCE\0");
            }
        }
        if std::env::var_os("PKGVIEW_SAMPLE_NOICON").is_some() {
            items.retain(|i| i.0 != "ICON0.PNG");
        }
        if let Ok(mb) = std::env::var("PKGVIEW_SAMPLE_MB") {
            let n = mb.parse::<usize>().unwrap() << 20;
            items.push(("USRDIR/big.bin", Some(vec![0; n]), 0));
        }
        let mut b = build(t, &items);
        if std::env::var_os("PKGVIEW_SAMPLE_DEMO").is_some() {
            // Made-up but plausible metadata: a game, local licence, EBOOT + licence flags, FW 3.40.
            add_meta(
                &mut b,
                &[
                    (0x1, &2u32.to_be_bytes()),
                    (0x2, &5u32.to_be_bytes()),
                    (0x3, &0x06u32.to_be_bytes()),
                    (0x8, &[0, 0x03, 0x40, 0, 0, 0, 0x01, 0x00]),
                ],
            );
            // Keep `verify` happy: the footer covers the header too.
            let end = b.len() - FOOTER_LEN as usize;
            let digest = Sha1::digest(&b[..end]);
            b[end..end + 20].copy_from_slice(&digest);
        }
        fs::write(path, b).unwrap();
    }
}
