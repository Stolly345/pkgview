//! Collections of packages, no UI: scan files or folders (one `Record` per `.pkg`), audit them (integrity,
//! duplicates, patches/add-ons without their base game, licences) and rename them by template (with undo).

use crate::pkg::{self, Pkg};
use std::collections::HashMap;
use std::ffi::CString;
use std::io::{self, Write};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

/// What a package is, from its metadata flags, PARAM.SFO CATEGORY and content type (in that order).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Kind {
    Game,
    Ps1,
    Ps2,
    Psp,
    Minis,
    /// SFO category GD or the PATCH flag: updates (and the rare add-on with a PARAM.SFO)
    Patch,
    /// No PARAM.SFO and an install folder or content type "game data": DLC
    AddOn,
    Theme,
    Avatar,
    Licence,
    Other(String),
}

impl Kind {
    pub fn label(&self) -> String {
        match self {
            Kind::Game => "Game".into(),
            Kind::Ps1 => "PS1 Classic".into(),
            Kind::Ps2 => "PS2 Classic".into(),
            Kind::Psp => "PSP".into(),
            Kind::Minis => "Minis".into(),
            Kind::Patch => "Patch".into(),
            Kind::AddOn => "Add-on".into(),
            Kind::Theme => "Theme".into(),
            Kind::Avatar => "Avatar".into(),
            Kind::Licence => "Licence".into(),
            Kind::Other(c) if c.is_empty() => "Other".into(),
            Kind::Other(c) => format!("Other ({c})"),
        }
    }

    /// Something a patch or add-on can belong to.
    pub fn is_base(&self) -> bool {
        matches!(
            self,
            Kind::Game | Kind::Ps1 | Kind::Ps2 | Kind::Psp | Kind::Minis
        )
    }

    fn of(category: &str, has_sfo: bool, m: &pkg::Metadata) -> Kind {
        if m.package_flags.is_some_and(|f| f & 0x10 != 0) {
            return Kind::Patch;
        }
        match category {
            "HG" => return Kind::Game,
            "1P" => return Kind::Ps1,
            "2P" => return Kind::Ps2,
            "PP" | "PE" => return Kind::Psp,
            "MN" => return Kind::Minis,
            "GD" => return Kind::Patch,
            _ => {}
        }
        match m.content_type {
            Some(5) => Kind::Game,
            Some(6) => Kind::Ps1,
            Some(0x12) => Kind::Ps2,
            Some(7 | 0xE | 0x14) => Kind::Psp,
            Some(0xF) => Kind::Minis,
            Some(9) => Kind::Theme,
            Some(0xD) => Kind::Avatar,
            Some(0xB) => Kind::Licence,
            Some(4) if !has_sfo => Kind::AddOn,
            _ if !has_sfo && m.install_dir.is_some() => Kind::AddOn,
            _ => Kind::Other(category.into()),
        }
    }
}

/// One package as found on disk: what could be read from its header, metadata and PARAM.SFO.
#[derive(Clone, Debug)]
pub struct Info {
    pub content_id: String,
    /// PARAM.SFO TITLE_ID, else the metadata's, else the content ID's middle part
    pub title_id: String,
    /// PARAM.SFO TITLE, else empty
    pub title: String,
    pub category: String,
    /// PARAM.SFO APP_VER, else the metadata's app version
    pub app_ver: String,
    /// PARAM.SFO VERSION (patch level)
    pub version: String,
    pub kind: Kind,
    /// The file is shorter than the header's total size (download cut off)
    pub truncated: bool,
    /// The trailing digest: identical packages have identical digests
    pub digest: [u8; 20],
    pub needs_licence: Option<bool>,
}

#[derive(Clone, Debug)]
pub struct Record {
    pub path: PathBuf,
    pub size: u64,
    pub info: Result<Info, String>,
    /// Could not be opened because the file is shorter than its header says
    pub cut_off: bool,
}

/// `.pkg` files in `paths` (files as given, folders searched recursively), sorted, without repeats.
pub fn collect(paths: &[PathBuf]) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = paths
        .iter()
        .flat_map(|p| {
            if p.is_dir() {
                pkg::find_pkgs(p)
            } else {
                vec![p.clone()]
            }
        })
        .collect();
    out.sort();
    out.dedup();
    out
}

pub fn read(path: &Path) -> Record {
    let size = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
    let mut cut_off = false;
    let info = Pkg::open(path)
        .map_err(|e| {
            cut_off = e.kind() == io::ErrorKind::UnexpectedEof;
            e.to_string()
        })
        .map(|p| {
            let sfo: HashMap<String, String> = p.sfo().into_iter().collect();
            let get = |k: &str| sfo.get(k).cloned().unwrap_or_default();
            let category = get("CATEGORY");
            let pick = |a: String, b: &Option<String>| {
                if a.is_empty() {
                    b.clone().unwrap_or_default()
                } else {
                    a
                }
            };
            Info {
                content_id: p.header.content_id.clone(),
                title_id: Some(get("TITLE_ID"))
                    .filter(|t| !t.is_empty())
                    .or_else(|| p.meta.title_id.clone())
                    .unwrap_or_else(|| p.header.root_folder()),
                title: get("TITLE"),
                app_ver: pick(get("APP_VER"), &p.meta.app_version),
                version: get("VERSION"),
                kind: Kind::of(&category, !sfo.is_empty(), &p.meta),
                category,
                truncated: size < p.header.total_size,
                digest: p.header.sha1,
                needs_licence: p.meta.needs_licence(),
            }
        });
    Record {
        path: path.to_path_buf(),
        size,
        info,
        cut_off,
    }
}

/// Read every package, several at a time. `progress(done)` after each.
pub fn scan(files: &[PathBuf], progress: impl Fn(usize) + Sync) -> Vec<Record> {
    let next = AtomicUsize::new(0);
    let done = AtomicUsize::new(0);
    let mut out: Vec<Option<Record>> = vec![None; files.len()];
    let slots: Vec<std::sync::Mutex<&mut Option<Record>>> =
        out.iter_mut().map(std::sync::Mutex::new).collect();
    let workers = std::thread::available_parallelism()
        .map_or(1, |n| n.get())
        .min(8);
    std::thread::scope(|s| {
        for _ in 0..workers.min(files.len().max(1)) {
            s.spawn(|| loop {
                let i = next.fetch_add(1, Ordering::Relaxed);
                let Some(f) = files.get(i) else { break };
                **slots[i].lock().unwrap() = Some(read(f));
                progress(done.fetch_add(1, Ordering::Relaxed) + 1);
            });
        }
    });
    drop(slots);
    out.into_iter()
        .map(|r| r.expect("every file read"))
        .collect()
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Integrity {
    /// Not hashed (`--no-hash`), or not yet
    Unchecked,
    Ok,
    /// SHA-1 mismatch
    Corrupt,
    /// File shorter than the header says
    Truncated,
    /// Could not be opened or read
    Unreadable(String),
}

impl Integrity {
    pub fn label(&self) -> &str {
        match self {
            Integrity::Unchecked => "not checked",
            Integrity::Ok => "OK",
            Integrity::Corrupt => "CORRUPT",
            Integrity::Truncated => "TRUNCATED",
            Integrity::Unreadable(_) => "UNREADABLE",
        }
    }

    pub fn is_bad(&self) -> bool {
        !matches!(self, Integrity::Unchecked | Integrity::Ok)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Licence {
    /// The package doesn't need one (or has no metadata to say)
    NotNeeded,
    Unknown,
    Found(PathBuf),
    Missing,
}

#[derive(Clone, Debug, Default)]
pub struct Findings {
    pub integrity: Option<Integrity>,
    /// Index of an identical package (same trailing digest) earlier in the list
    pub duplicate_of: Option<usize>,
    /// For a damaged package: an intact one with the same digest (set by `hash_all`)
    pub intact_copy: Option<usize>,
    /// Index of a different build with the same content ID and versions
    pub same_version_as: Option<usize>,
    /// A PSN patch or add-on whose base game is not in the list
    pub orphan: bool,
    /// A patch or add-on for a disc game (Title ID B…): its base is a disc, not a package
    pub for_disc: bool,
    pub licence: Option<Licence>,
}

/// Where RPCS3 keeps `.rap` licences (native and Flatpak installs).
pub fn rpcs3_rap_dirs() -> Vec<PathBuf> {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_default();
    let config = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".config"));
    let exdata = "rpcs3/dev_hdd0/home/00000001/exdata";
    vec![
        config.join(exdata),
        home.join(".var/app/net.rpcs3.RPCS3/config").join(exdata),
    ]
}

/// `<content id>.rap` (any case) next to the package or in one of `dirs`.
pub fn find_rap(pkg_path: &Path, content_id: &str, dirs: &[PathBuf]) -> Option<PathBuf> {
    let want = format!("{content_id}.rap").to_ascii_lowercase();
    pkg_path
        .parent()
        .into_iter()
        .chain(dirs.iter().map(PathBuf::as_path))
        .filter_map(|d| std::fs::read_dir(d).ok())
        .flat_map(|rd| rd.flatten())
        .find(|e| e.file_name().to_string_lossy().to_ascii_lowercase() == want)
        .map(|e| e.path())
}

/// Everything except hashing: duplicates, orphans, licences, truncation. Cheap.
pub fn audit(records: &[Record], rap_dirs: &[PathBuf]) -> Vec<Findings> {
    let mut out = vec![Findings::default(); records.len()];
    let mut by_digest: HashMap<[u8; 20], usize> = HashMap::new();
    let mut by_version: HashMap<(String, String, String), (usize, [u8; 20])> = HashMap::new();
    let bases: std::collections::HashSet<&str> = records
        .iter()
        .filter_map(|r| r.info.as_ref().ok())
        .filter(|i| i.kind.is_base())
        .map(|i| i.title_id.as_str())
        .collect();
    for (n, r) in records.iter().enumerate() {
        let f = &mut out[n];
        let i = match &r.info {
            Ok(i) => i,
            Err(_) if r.cut_off => {
                f.integrity = Some(Integrity::Truncated);
                continue;
            }
            Err(e) => {
                f.integrity = Some(Integrity::Unreadable(e.clone()));
                continue;
            }
        };
        if i.truncated {
            f.integrity = Some(Integrity::Truncated);
        }
        if !i.truncated {
            match by_digest.get(&i.digest) {
                Some(&first) => f.duplicate_of = Some(first),
                None => {
                    by_digest.insert(i.digest, n);
                }
            }
            let key = (i.content_id.clone(), i.app_ver.clone(), i.version.clone());
            match by_version.get(&key) {
                Some(&(first, d)) if d != i.digest => f.same_version_as = Some(first),
                Some(_) => {}
                None => {
                    by_version.insert(key, (n, i.digest));
                }
            }
        }
        if matches!(i.kind, Kind::Patch | Kind::AddOn) {
            if i.title_id.starts_with('B') {
                f.for_disc = true;
            } else if !bases.contains(i.title_id.as_str()) {
                f.orphan = true;
            }
        }
        f.licence = Some(match i.needs_licence {
            None => Licence::Unknown,
            Some(false) => Licence::NotNeeded,
            Some(true) => match find_rap(&r.path, &i.content_id, rap_dirs) {
                Some(p) => Licence::Found(p),
                None => Licence::Missing,
            },
        });
    }
    out
}

/// SHA-1 check of every package that is readable and complete, several at a time. Fills `integrity`.
pub fn hash_all(records: &[Record], findings: &mut [Findings], progress: impl Fn(usize) + Sync) {
    let todo: Vec<usize> = (0..records.len())
        .filter(|&n| findings[n].integrity.is_none() && records[n].info.is_ok())
        .collect();
    let next = AtomicUsize::new(0);
    let done = AtomicUsize::new(0);
    let results: Vec<std::sync::Mutex<Option<Integrity>>> =
        todo.iter().map(|_| std::sync::Mutex::new(None)).collect();
    // ponytail: 4 readers; more only thrash a single disk.
    std::thread::scope(|s| {
        for _ in 0..4.min(todo.len()) {
            s.spawn(|| loop {
                let k = next.fetch_add(1, Ordering::Relaxed);
                let Some(&n) = todo.get(k) else { break };
                let r = match Pkg::open(&records[n].path).and_then(|p| p.verify_sha1()) {
                    Ok(true) => Integrity::Ok,
                    Ok(false) => Integrity::Corrupt,
                    Err(e) => Integrity::Unreadable(e.to_string()),
                };
                *results[k].lock().unwrap() = Some(r);
                progress(done.fetch_add(1, Ordering::Relaxed) + 1);
            });
        }
    });
    for (k, &n) in todo.iter().enumerate() {
        findings[n].integrity = results[k].lock().unwrap().take();
    }
    settle(records, findings);
}

/// After SHA-1 results are in (hashed now, or remembered from before): a damaged copy shares its digest with
/// an intact one, so it isn't a duplicate.
pub fn settle(records: &[Record], findings: &mut [Findings]) {
    // The digest is read from the file, so a damaged copy shares it with an intact one: that isn't a
    // duplicate. Point the damaged one at an intact copy instead, and re-link the rest to an intact first.
    let bad = |f: &Findings| f.integrity.as_ref().is_some_and(Integrity::is_bad);
    let mut intact: HashMap<[u8; 20], usize> = HashMap::new();
    for (n, r) in records.iter().enumerate() {
        if let (Ok(i), false) = (&r.info, bad(&findings[n])) {
            intact.entry(i.digest).or_insert(n);
        }
    }
    for (n, r) in records.iter().enumerate() {
        let Ok(i) = &r.info else { continue };
        let first = intact.get(&i.digest).copied();
        if bad(&findings[n]) {
            findings[n].duplicate_of = None;
            findings[n].intact_copy = first;
        } else if findings[n].duplicate_of.is_some() {
            findings[n].duplicate_of = first.filter(|&f| f != n);
        }
    }
}

/// The findings for package `n` as sentences; `name(k)` names package `k`.
pub fn notes(
    records: &[Record],
    found: &[Findings],
    n: usize,
    name: impl Fn(usize) -> String,
) -> Vec<String> {
    let (r, f) = (&records[n], &found[n]);
    let mut out = vec![];
    if let Some(d) = f.duplicate_of {
        if f.integrity.is_some() && found[d].integrity.is_some() {
            out.push(format!("identical to {}", name(d)));
        } else {
            out.push(format!(
                "same digest as {} (Verify to tell whether both are intact)",
                name(d)
            ));
        }
    }
    if let Some(d) = f.intact_copy {
        out.push(format!("an intact copy is {}", name(d)));
    }
    if let Some(d) = f.same_version_as {
        out.push(format!(
            "same content ID and version as {}, but different bytes",
            name(d)
        ));
    }
    if let (true, Ok(i)) = (f.orphan, &r.info) {
        out.push(format!(
            "no game {} here for this {}",
            i.title_id,
            i.kind.label().to_lowercase()
        ));
    }
    if f.for_disc {
        out.push("for a disc game (its base is a disc, not a package)".into());
    }
    if let (Some(Licence::Missing), Ok(i)) = (&f.licence, &r.info) {
        out.push(format!(
            "needs a licence: no {}.rap next to it or in RPCS3",
            i.content_id
        ));
    }
    out
}

/// Region from the content ID's first letter (`UP0001-…` = USA).
pub fn region(content_id: &str) -> &'static str {
    match content_id.chars().next() {
        Some('U') => "USA",
        Some('E') => "Europe",
        Some('J') => "Japan",
        Some('H') => "Asia",
        Some('K') => "Korea",
        Some('I') => "Internal",
        _ => "",
    }
}

pub const DEFAULT_TEMPLATE: &str = "{TITLE} [{TITLE_ID}] [v{APP_VER}] [{CATEGORY}]";
pub const TOKENS: [&str; 8] = [
    "TITLE",
    "TITLE_ID",
    "CONTENT_ID",
    "APP_VER",
    "VERSION",
    "CATEGORY",
    "KIND",
    "REGION",
];

fn token(i: &Info, name: &str) -> Option<String> {
    Some(match name {
        "TITLE" => i.title.clone(),
        "TITLE_ID" => i.title_id.clone(),
        "CONTENT_ID" => i.content_id.clone(),
        "APP_VER" => i.app_ver.clone(),
        "VERSION" => i.version.clone(),
        "CATEGORY" => i.category.clone(),
        "KIND" => i.kind.label(),
        "REGION" => region(&i.content_id).into(),
        _ => return None,
    })
}

/// Check a template: every `{TOKEN}` known, braces and brackets balanced.
pub fn check_template(t: &str) -> Result<(), String> {
    let mut depth = 0;
    let mut rest = t;
    while let Some(c) = rest.chars().next() {
        match c {
            '[' if depth == 0 => depth = 1,
            '[' => return Err("“[” inside “[…]”".into()),
            ']' if depth == 1 => depth = 0,
            ']' => return Err("“]” without “[”".into()),
            '{' => {
                let end = rest.find('}').ok_or("“{” without “}”")?;
                let name = &rest[1..end];
                if !TOKENS.contains(&name) {
                    return Err(format!(
                        "unknown {{{name}}}: use one of {}",
                        TOKENS.map(|t| format!("{{{t}}}")).join(" ")
                    ));
                }
                rest = &rest[end..];
            }
            '}' => return Err("“}” without “{”".into()),
            _ => {}
        }
        rest = &rest[c.len_utf8()..];
    }
    if depth != 0 {
        return Err("“[” without “]”".into());
    }
    if t.trim().is_empty() {
        return Err("the template is empty".into());
    }
    Ok(())
}

/// Fill a (checked) template. A `[…]` group in which any token is empty disappears, brackets and all.
pub fn render(t: &str, i: &Info) -> String {
    let mut out = String::new();
    let mut group: Option<(String, bool)> = None; // (text, all tokens non-empty)
    let mut rest = t;
    while let Some(c) = rest.chars().next() {
        let mut piece = c.to_string();
        let mut empty = false;
        if c == '{' {
            if let Some(end) = rest.find('}') {
                let v = token(i, &rest[1..end]).unwrap_or_default();
                empty = v.trim().is_empty();
                piece = v;
                rest = &rest[end..];
            }
        }
        match (c, &mut group) {
            ('[', None) => group = Some(("[".into(), true)),
            (']', Some(_)) => {
                let (text, keep) = group.take().unwrap();
                if keep {
                    out.push_str(&text);
                    out.push(']');
                }
            }
            (_, Some((text, keep))) => {
                text.push_str(&piece);
                *keep &= !empty;
            }
            (_, None) => out.push_str(&piece),
        }
        rest = &rest[c.len_utf8()..];
    }
    out
}

/// A safe file name on Linux and on FAT/exFAT/NTFS drives: no `/ \ : * ? " < > |` or control characters,
/// single spaces, no leading/trailing dots or spaces, at most 200 bytes, ending in `.pkg`.
pub fn file_name(stem: &str) -> String {
    let mut s = String::new();
    for c in stem.chars() {
        match c {
            ':' => s.push_str(" -"),
            '/' | '\\' | '|' => s.push('-'),
            '*' | '?' | '"' | '<' | '>' => {}
            c if c.is_control() => {}
            c => s.push(c),
        }
    }
    let mut s = s.split_whitespace().collect::<Vec<_>>().join(" ");
    s = s.trim_matches(|c: char| c == '.' || c == ' ').to_string();
    while s.len() > 196 {
        s.pop();
    }
    if s.is_empty() {
        s = "package".into();
    }
    format!("{s}.pkg")
}

/// Renames for `records` (readable ones only) under `template`, each staying in its folder. Names already
/// right are left out; a name taken by another file or another package of the batch gets " (1)", " (2)"…
/// (files of the batch don't swap names: that would need a temporary name).
pub fn plan(records: &[Record], template: &str) -> Result<Vec<(PathBuf, PathBuf)>, String> {
    check_template(template)?;
    let mut taken: std::collections::HashSet<PathBuf> = std::collections::HashSet::new();
    let mut out = vec![];
    for r in records {
        let Ok(i) = &r.info else { continue };
        let dir = r.path.parent().unwrap_or(Path::new("."));
        let name = file_name(&render(template, i));
        let stem = name.trim_end_matches(".pkg").to_string();
        let mut target = dir.join(&name);
        let mut k = 1;
        // The package's own current name is always fine (also a numbered one from an earlier run).
        while target != r.path && (taken.contains(&target) || target.symlink_metadata().is_ok()) {
            target = dir.join(format!("{stem} ({k}).pkg"));
            k += 1;
        }
        taken.insert(target.clone());
        if target != r.path {
            out.push((r.path.clone(), target));
        }
    }
    Ok(out)
}

/// rename(2) that never replaces an existing file (renameat2 RENAME_NOREPLACE, or a check where the
/// filesystem doesn't support it).
fn rename_noreplace(from: &Path, to: &Path) -> io::Result<()> {
    let c = |p: &Path| {
        CString::new(p.as_os_str().as_bytes()).map_err(|_| io::Error::other("NUL in path"))
    };
    let (a, b) = (c(from)?, c(to)?);
    // SAFETY: valid NUL-terminated paths.
    let r = unsafe {
        libc::renameat2(
            libc::AT_FDCWD,
            a.as_ptr(),
            libc::AT_FDCWD,
            b.as_ptr(),
            libc::RENAME_NOREPLACE,
        )
    };
    if r == 0 {
        return Ok(());
    }
    let e = io::Error::last_os_error();
    if e.raw_os_error() == Some(libc::EINVAL) || e.raw_os_error() == Some(libc::ENOSYS) {
        if to.symlink_metadata().is_ok() {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!("{} exists", to.display()),
            ));
        }
        return std::fs::rename(from, to);
    }
    Err(e)
}

/// `$XDG_STATE_HOME/pkgview/renames` (default `~/.local/state/…`).
pub fn log_dir() -> PathBuf {
    std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            std::env::var_os("HOME")
                .map(PathBuf::from)
                .unwrap_or_default()
                .join(".local/state")
        })
        .join("pkgview/renames")
}

fn stamp() -> String {
    // SAFETY: plain libc calls with valid pointers.
    let tm = unsafe {
        let t = libc::time(std::ptr::null_mut());
        let mut tm: libc::tm = std::mem::zeroed();
        libc::localtime_r(&t, &mut tm);
        tm
    };
    format!(
        "{}{:02}{:02}-{:02}{:02}{:02}",
        tm.tm_year + 1900,
        tm.tm_mon + 1,
        tm.tm_mday,
        tm.tm_hour,
        tm.tm_min,
        tm.tm_sec
    )
}

/// Carry out a plan. Each done rename is written to a log in `log_dir` first (`old<TAB>new` per line) so
/// `undo` can reverse it. Stops at the first failure; what was done stays logged. Returns the log and the
/// number done.
pub fn apply(plan: &[(PathBuf, PathBuf)], log_dir: &Path) -> io::Result<(PathBuf, usize)> {
    std::fs::create_dir_all(log_dir)?;
    let mut log_path = log_dir.join(format!("{}.tsv", stamp()));
    let mut k = 1;
    while log_path.exists() {
        log_path = log_dir.join(format!("{}-{k}.tsv", stamp()));
        k += 1;
    }
    let mut log = std::fs::File::create(&log_path)?;
    for (n, (from, to)) in plan.iter().enumerate() {
        if let Err(e) = rename_noreplace(from, to) {
            return Err(io::Error::new(
                e.kind(),
                format!(
                    "{} → {}: {e} ({n} renamed, logged in {})",
                    from.display(),
                    to.display(),
                    log_path.display()
                ),
            ));
        }
        writeln!(log, "{}\t{}", from.display(), to.display())?;
        log.sync_data()?;
    }
    Ok((log_path, plan.len()))
}

/// The newest rename log, if any.
pub fn last_log(log_dir: &Path) -> Option<PathBuf> {
    let mut logs: Vec<PathBuf> = std::fs::read_dir(log_dir)
        .ok()?
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "tsv"))
        .collect();
    logs.sort();
    logs.pop()
}

/// Reverse a rename log, newest rename first. A file that is gone, or a name that has been taken again, is
/// reported and skipped. The log is renamed to `*.undone`. Returns (reversed, problems).
pub fn undo(log: &Path) -> io::Result<(usize, Vec<String>)> {
    let text = std::fs::read_to_string(log)?;
    let mut done = 0;
    let mut problems = vec![];
    for line in text.lines().rev() {
        let Some((old, new)) = line.split_once('\t') else {
            continue;
        };
        match rename_noreplace(Path::new(new), Path::new(old)) {
            Ok(()) => done += 1,
            Err(e) => problems.push(format!("{new} → {old}: {e}")),
        }
    }
    std::fs::rename(log, log.with_extension("undone"))?;
    Ok((done, problems))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info(title: &str, id: &str, cat: &str, app: &str) -> Info {
        Info {
            content_id: format!("UP0001-{id}_00-0000000000000001"),
            title_id: id.into(),
            title: title.into(),
            category: cat.into(),
            app_ver: app.into(),
            version: String::new(),
            kind: Kind::of(cat, true, &pkg::Metadata::default()),
            truncated: false,
            digest: [0; 20],
            needs_licence: None,
        }
    }

    #[test]
    fn templates() {
        let i = info("Rayman: Origins / Deluxe?", "NPUB12345", "HG", "01.00");
        assert_eq!(
            render(DEFAULT_TEMPLATE, &i),
            "Rayman: Origins / Deluxe? [NPUB12345] [v01.00] [HG]"
        );
        assert_eq!(
            file_name(&render(DEFAULT_TEMPLATE, &i)),
            "Rayman - Origins - Deluxe [NPUB12345] [v01.00] [HG].pkg"
        );
        // A group with an empty token disappears; text outside groups stays.
        let no_ver = info("Game", "NPUB12345", "HG", "");
        assert_eq!(render(DEFAULT_TEMPLATE, &no_ver), "Game [NPUB12345]  [HG]");
        assert_eq!(
            file_name(&render(DEFAULT_TEMPLATE, &no_ver)),
            "Game [NPUB12345] [HG].pkg"
        );
        assert_eq!(
            render("{KIND} - {REGION} - {TITLE}", &i),
            "Game - USA - Rayman: Origins / Deluxe?"
        );
        assert!(check_template(DEFAULT_TEMPLATE).is_ok());
        for bad in ["{NOPE}", "[a", "a]", "[[x]]", "{TITLE", "TITLE}", "  "] {
            assert!(check_template(bad).is_err(), "{bad:?}");
        }
        assert_eq!(file_name(" ..  "), "package.pkg");
        assert_eq!(file_name(&"x".repeat(300)).len(), 200);
        assert_eq!(file_name("a\u{0}b\tc"), "abc.pkg");
    }

    #[test]
    fn kinds() {
        let m = pkg::Metadata::default();
        assert_eq!(Kind::of("HG", true, &m), Kind::Game);
        assert_eq!(Kind::of("GD", true, &m), Kind::Patch);
        assert_eq!(Kind::of("1P", true, &m), Kind::Ps1);
        let patch = pkg::Metadata {
            package_flags: Some(0x10),
            ..Default::default()
        };
        assert_eq!(Kind::of("HG", true, &patch), Kind::Patch);
        let dlc = pkg::Metadata {
            content_type: Some(4),
            ..Default::default()
        };
        assert_eq!(Kind::of("", false, &dlc), Kind::AddOn);
        let theme = pkg::Metadata {
            content_type: Some(9),
            ..Default::default()
        };
        assert_eq!(Kind::of("", false, &theme), Kind::Theme);
        assert_eq!(Kind::of("XX", true, &m), Kind::Other("XX".into()));
        assert_eq!(region("EP9000-NPEA00001_00-X"), "Europe");
    }

    fn rec(path: &str, i: Info) -> Record {
        Record {
            path: path.into(),
            size: 1,
            info: Ok(i),
            cut_off: false,
        }
    }

    #[test]
    fn audit_findings() {
        let d = std::env::temp_dir().join(format!("pkgview-lib-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(d.join("rpcs3")).unwrap();
        let base = info("Game", "NPUB00001", "HG", "01.00");
        let mut dup = base.clone();
        dup.digest = [0; 20]; // same digest as base: identical
        let mut rebuilt = base.clone();
        rebuilt.digest = [9; 20]; // same content ID and versions, other bytes
        let mut patch = info("Game", "NPUB00001", "GD", "01.01");
        patch.digest = [1; 20];
        let mut orphan = info("Other", "NPUB99999", "GD", "01.01");
        orphan.digest = [2; 20];
        let mut disc = info("Disc", "BLUS30001", "GD", "01.02");
        disc.digest = [3; 20];
        let mut cut = info("Cut", "NPUB00002", "HG", "01.00");
        cut.digest = [4; 20];
        cut.truncated = true;
        let mut lic = info("Lic", "NPUB00003", "HG", "01.00");
        lic.digest = [5; 20];
        lic.needs_licence = Some(true);
        let mut lic2 = lic.clone();
        lic2.digest = [6; 20];
        lic2.content_id = "UP0001-NPUB00004_00-0000000000000001".into();
        std::fs::write(
            d.join("rpcs3")
                .join("up0001-npub00003_00-0000000000000001.RAP"),
            b"k",
        )
        .unwrap();
        let recs = vec![
            rec("/a/base.pkg", base),
            rec("/a/dup.pkg", dup),
            rec("/a/rebuilt.pkg", rebuilt),
            rec("/a/patch.pkg", patch),
            rec("/a/orphan.pkg", orphan),
            rec("/a/disc.pkg", disc),
            rec("/a/cut.pkg", cut),
            rec("/a/lic.pkg", lic),
            rec("/a/lic2.pkg", lic2),
            Record {
                path: "/a/bad.pkg".into(),
                size: 3,
                info: Err("This file is not supported.".into()),
                cut_off: false,
            },
        ];
        let f = audit(&recs, &[d.join("rpcs3")]);
        assert_eq!(f[1].duplicate_of, Some(0));
        assert_eq!(f[2].same_version_as, Some(0));
        assert!(f[2].duplicate_of.is_none());
        assert!(!f[3].orphan && !f[3].for_disc);
        assert!(f[4].orphan);
        assert!(f[5].for_disc && !f[5].orphan);
        assert_eq!(f[6].integrity, Some(Integrity::Truncated));
        assert!(matches!(f[7].licence, Some(Licence::Found(_))));
        assert_eq!(f[8].licence, Some(Licence::Missing));
        assert_eq!(f[0].licence, Some(Licence::Unknown));
        assert!(matches!(f[9].integrity, Some(Integrity::Unreadable(_))));
        std::fs::remove_dir_all(d).unwrap();
    }

    #[test]
    fn rename_plan_apply_undo() {
        let d = std::env::temp_dir().join(format!("pkgview-ren-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        let p = |n: &str| d.join(n);
        for n in ["a.pkg", "b.pkg", "c.pkg", "Taken [NPUB3].pkg"] {
            std::fs::write(p(n), n).unwrap();
        }
        let recs = vec![
            rec(
                p("a.pkg").to_str().unwrap(),
                info("Same", "NPUB1", "HG", ""),
            ),
            rec(
                p("b.pkg").to_str().unwrap(),
                info("Same", "NPUB1", "HG", ""),
            ),
            rec(p("c.pkg").to_str().unwrap(), info("Taken", "NPUB3", "", "")),
            Record {
                path: p("x.pkg"),
                size: 0,
                info: Err("bad".into()),
                cut_off: false,
            },
        ];
        let t = "{TITLE} [{TITLE_ID}]";
        let pl = plan(&recs, t).unwrap();
        let names: Vec<String> = pl
            .iter()
            .map(|(_, n)| n.file_name().unwrap().to_string_lossy().into())
            .collect();
        assert_eq!(
            names,
            [
                "Same [NPUB1].pkg",
                "Same [NPUB1] (1).pkg",
                "Taken [NPUB3] (1).pkg"
            ]
        );
        let logs = d.join("logs");
        let (log, n) = apply(&pl, &logs).unwrap();
        assert_eq!(n, 3);
        assert_eq!(
            std::fs::read_to_string(p("Same [NPUB1] (1).pkg")).unwrap(),
            "b.pkg"
        );
        assert_eq!(
            std::fs::read_to_string(p("Taken [NPUB3].pkg")).unwrap(),
            "Taken [NPUB3].pkg"
        );
        assert_eq!(last_log(&logs), Some(log.clone()));
        // Planning again finds nothing to do.
        let again: Vec<Record> = pl
            .iter()
            .zip(&recs)
            .map(|((_, new), r)| Record {
                path: new.clone(),
                ..r.clone()
            })
            .collect();
        assert!(plan(&again[..2], t).unwrap().is_empty());
        // Undo, with one name taken meanwhile: reported, the rest reversed.
        std::fs::write(p("c.pkg"), "intruder").unwrap();
        let (done, problems) = undo(&log).unwrap();
        assert_eq!(done, 2);
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert_eq!(std::fs::read_to_string(p("a.pkg")).unwrap(), "a.pkg");
        assert_eq!(std::fs::read_to_string(p("c.pkg")).unwrap(), "intruder");
        assert!(last_log(&logs).is_none() && log.with_extension("undone").exists());
        // Never overwrites, even if the plan says so.
        std::fs::write(p("z.pkg"), "z").unwrap();
        assert!(apply(&[(p("a.pkg"), p("z.pkg"))], &logs).is_err());
        assert_eq!(std::fs::read_to_string(p("z.pkg")).unwrap(), "z");
        assert!(check_template("{BAD}").is_err() && plan(&recs, "{BAD}").is_err());
        std::fs::remove_dir_all(d).unwrap();
    }
}
