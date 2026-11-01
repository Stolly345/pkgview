//! Headless commands: `info`, `list`, `extract`, `verify`.

use crate::library::{self, Integrity, Licence};
use crate::pkg::{self, ExtractOptions, Overwrite, Pkg, Progress};
use anyhow::{bail, Context};
use clap::{CommandFactory, Parser, Subcommand, ValueEnum};
use gtk::{gdk_pixbuf, gio, glib};
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;

#[derive(Parser)]
#[command(
    name = "pkgview",
    version,
    about = "View and extract PS3/PSP .pkg files",
    after_help = "Exit status: 0 = success, 1 = a package or entry failed, 2 = usage error."
)]
#[command(args_conflicts_with_subcommands = true)]
pub struct Cli {
    #[command(subcommand)]
    pub cmd: Option<Cmd>,
    /// Open this .pkg in the GUI (with --extract-here / --extract-to: the packages to extract)
    pub files: Vec<PathBuf>,
    /// Extract FILEs next to themselves, showing only a progress window (file-manager action)
    #[arg(long, conflicts_with = "extract_to", requires = "files")]
    pub extract_here: bool,
    /// Ask for a folder, then extract FILEs into it, showing only a progress window
    #[arg(long, requires = "files")]
    pub extract_to: bool,
    /// Open DIR (a folder of packages) in the library window
    #[arg(long, value_name = "DIR", conflicts_with_all = ["extract_here", "extract_to"])]
    pub library: Option<PathBuf>,
    /// With --library: check the folder against this No-Intro / Redump DAT (runs Verify)
    #[arg(long, value_name = "FILE", requires = "library")]
    pub dat: Option<PathBuf>,
}

#[derive(Subcommand)]
pub enum Cmd {
    /// Print the header, package metadata and PARAM.SFO
    Info {
        #[arg(required = true)]
        files: Vec<PathBuf>,
        /// One JSON object per package, one per line
        #[arg(long)]
        json: bool,
    },
    /// List entries: flags, size, path
    List {
        #[arg(required = true)]
        files: Vec<PathBuf>,
        /// One JSON object per package, one per line
        #[arg(long)]
        json: bool,
    },
    /// Check a collection: SHA-1, cut-off downloads, duplicates, patches/add-ons without their game, licences
    Audit {
        /// .pkg files and/or folders (searched recursively)
        #[arg(required = true)]
        paths: Vec<PathBuf>,
        /// Skip the digest check (reads only headers: instant)
        #[arg(long, conflicts_with = "dat")]
        no_hash: bool,
        /// Also look every package up in a No-Intro / Redump DAT (Logiqx XML), by SHA-1 of the whole file
        #[arg(long, value_name = "FILE")]
        dat: Option<PathBuf>,
        /// One JSON object per package, one per line
        #[arg(long)]
        json: bool,
    },
    /// Rename packages from their PARAM.SFO and header fields; preview with --dry-run, reverse with --undo
    Rename {
        /// .pkg files and/or folders (searched recursively)
        #[arg(required_unless_present = "undo")]
        paths: Vec<PathBuf>,
        /// {TITLE} {TITLE_ID} {CONTENT_ID} {APP_VER} {VERSION} {CATEGORY} {KIND} {REGION}; a […] group with an
        /// empty field is left out
        #[arg(short, long, default_value = library::DEFAULT_TEMPLATE)]
        template: String,
        /// Show what would be renamed, change nothing
        #[arg(short = 'n', long)]
        dry_run: bool,
        /// Reverse the last rename run
        #[arg(long, conflicts_with_all = ["paths", "dry_run"])]
        undo: bool,
    },
    /// Install packages into RPCS3's dev_hdd0 the way RPCS3's own installer does (folder by content type,
    /// existing files kept unless the package marks them overwritable); copies a .rap found next to a package
    #[command(name = "install-rpcs3")]
    InstallRpcs3 {
        #[arg(required = true)]
        files: Vec<PathBuf>,
        /// RPCS3's dev_hdd0 folder (default: found from RPCS3's config, native or Flatpak)
        #[arg(long, value_name = "DIR")]
        hdd0: Option<PathBuf>,
        /// RPCS3 user for licences and avatars
        #[arg(long, default_value = "00000001")]
        user: String,
    },
    /// Compare two packages: entries added, removed or changed (size, flags; with --content also bytes).
    /// Exit status 0 = same, 1 = different
    Diff {
        a: PathBuf,
        b: PathBuf,
        /// Also compare the decrypted bytes of same-size files (reads both packages)
        #[arg(long)]
        content: bool,
        /// One JSON object
        #[arg(long)]
        json: bool,
    },
    /// Write one file from the package to stdout (PATH as shown by `list`, without the root)
    Cat { file: PathBuf, path: String },
    /// Extract packages into DIR/<pkg name>/ (default: next to each pkg); with one package, optionally
    /// only the given inner PATHs. Arguments after the first that end in .pkg are packages, others PATHs.
    Extract {
        #[arg(required = true, value_name = "FILE.pkg… [PATH…]")]
        args: Vec<String>,
        #[arg(short, long, value_name = "DIR")]
        output: Option<PathBuf>,
        /// What to do with files that already exist
        #[arg(long, value_enum, default_value_t = OverwriteArg::Replace)]
        overwrite: OverwriteArg,
        /// Don't create the DIR/<pkg name>/ folder (the log goes into DIR)
        #[arg(long)]
        no_pkg_folder: bool,
        /// Put every file directly into the output folder, without the package's folders
        #[arg(long)]
        flat: bool,
    },
    /// Check the package digests (PS3/PSP SHA-1, PS4 SHA-256) and that known files decrypt to their headers
    Verify {
        #[arg(required = true)]
        files: Vec<PathBuf>,
        /// One JSON object per package, one per line
        #[arg(long)]
        json: bool,
    },
    /// Write ICON0.PNG (or PIC1.PNG) scaled to fit SIZE as a PNG (freedesktop thumbnailer)
    #[command(hide = true)]
    Thumbnail {
        input: PathBuf,
        output: PathBuf,
        size: u32,
    },
    /// Print the man page (roff) to stdout
    #[command(hide = true)]
    Man,
    /// Print shell completions to stdout
    #[command(hide = true)]
    Completions { shell: clap_complete::Shell },
}

#[derive(Clone, Copy, ValueEnum)]
pub enum OverwriteArg {
    Skip,
    Replace,
    Rename,
}

/// Run `f` per package; failures are printed and counted, never stop the rest. Err = at least one failed.
fn each<T: AsRef<Path>>(
    files: &[T],
    mut f: impl FnMut(&Path) -> anyhow::Result<()>,
) -> anyhow::Result<()> {
    let mut failed = 0;
    for p in files {
        if let Err(e) = f(p.as_ref()) {
            if files.len() > 1 {
                eprintln!("pkgview: {}: {e:#}", p.as_ref().display());
            } else {
                return Err(e.context(p.as_ref().display().to_string()));
            }
            failed += 1;
        }
    }
    match failed {
        0 => Ok(()),
        n => bail!("{n} of {} packages failed", files.len()),
    }
}

/// A JSON value, written compactly. Enough for `--json`; no parsing.
enum J {
    Null,
    S(String),
    N(u64),
    B(bool),
    A(Vec<J>),
    O(Vec<(String, J)>),
}

impl J {
    fn s(v: impl Into<String>) -> J {
        J::S(v.into())
    }
    fn o<const N: usize>(fields: [(&str, J); N]) -> J {
        J::O(
            fields
                .into_iter()
                .map(|(k, v)| (k.to_string(), v))
                .collect(),
        )
    }
}

fn json_str(s: &str, f: &mut std::fmt::Formatter) -> std::fmt::Result {
    f.write_str("\"")?;
    for c in s.chars() {
        match c {
            '"' => f.write_str("\\\"")?,
            '\\' => f.write_str("\\\\")?,
            '\n' => f.write_str("\\n")?,
            '\r' => f.write_str("\\r")?,
            '\t' => f.write_str("\\t")?,
            c if (c as u32) < 0x20 => write!(f, "\\u{:04x}", c as u32)?,
            c => write!(f, "{c}")?,
        }
    }
    f.write_str("\"")
}

impl std::fmt::Display for J {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match self {
            J::Null => f.write_str("null"),
            J::S(s) => json_str(s, f),
            J::N(n) => write!(f, "{n}"),
            J::B(b) => write!(f, "{b}"),
            J::A(v) => {
                f.write_str("[")?;
                for (i, x) in v.iter().enumerate() {
                    if i > 0 {
                        f.write_str(",")?;
                    }
                    write!(f, "{x}")?;
                }
                f.write_str("]")
            }
            J::O(v) => {
                f.write_str("{")?;
                for (i, (k, x)) in v.iter().enumerate() {
                    if i > 0 {
                        f.write_str(",")?;
                    }
                    json_str(k, f)?;
                    write!(f, ":{x}")?;
                }
                f.write_str("}")
            }
        }
    }
}

/// `each` for `--json`: prints the package's object, or `{"file":…,"error":…}`. The closure says whether the
/// package passed (false = counted as failed after printing).
fn json_each(
    files: &[PathBuf],
    f: impl Fn(&Path) -> anyhow::Result<(J, bool)>,
) -> anyhow::Result<()> {
    each(files, |p| {
        let file = J::s(p.to_string_lossy());
        match f(p) {
            Ok((J::O(mut fields), ok)) => {
                fields.insert(0, ("file".into(), file));
                println!("{}", J::O(fields));
                if ok {
                    Ok(())
                } else {
                    bail!("failed")
                }
            }
            Ok((j, _)) => unreachable!("json_each wants an object, got {j}"),
            Err(e) => {
                println!(
                    "{}",
                    J::o([("file", file), ("error", J::s(format!("{e:#}")))])
                );
                Err(e)
            }
        }
    })
}

fn info_json(p: &Pkg) -> J {
    let h = &p.header;
    let m = &p.meta;
    let mut meta = vec![];
    let opt_s = |k: &str, v: &Option<String>, meta: &mut Vec<(String, J)>| {
        if let Some(v) = v {
            meta.push((k.into(), J::s(v.as_str())));
        }
    };
    if let Some(v) = m.content_type {
        meta.push(("content_type".into(), J::N(v as u64)));
        meta.push((
            "content_type_name".into(),
            J::s(m.content_type_name().unwrap_or_default()),
        ));
    }
    if let Some(v) = m.drm_type {
        meta.push(("drm_type".into(), J::N(v as u64)));
    }
    if let Some(v) = m.package_flags {
        meta.push(("package_flags".into(), J::N(v as u64)));
        let names = m.flag_names().into_iter().map(J::s).collect();
        meta.push(("package_flag_names".into(), J::A(names)));
    }
    if let Some(v) = m.package_size {
        meta.push(("package_size".into(), J::N(v)));
    }
    opt_s("package_version", &m.package_version, &mut meta);
    opt_s("app_version", &m.app_version, &mut meta);
    opt_s("min_firmware", &m.min_firmware, &mut meta);
    opt_s("title_id", &m.title_id, &mut meta);
    opt_s("install_dir", &m.install_dir, &mut meta);
    let sfo_kv = p.sfo();
    let category = sfo_kv
        .iter()
        .find(|(k, _)| k == "CATEGORY")
        .map(|(_, v)| v.clone())
        .unwrap_or_default();
    let kind = library::Kind::of(&category, !sfo_kv.is_empty(), m).label();
    let sfo = sfo_kv.into_iter().map(|(k, v)| (k, J::S(v))).collect();
    J::o([
        ("kind", J::s(kind)),
        ("content_id", J::s(h.content_id.as_str())),
        ("type", J::s(h.type_label())),
        ("pkg_type", J::N(h.pkg_type as u64)),
        ("root", J::s(h.root_folder())),
        ("item_count", J::N(h.item_count as u64)),
        ("total_size", J::N(h.total_size)),
        ("data_offset", J::N(h.data_offset)),
        ("data_size", J::N(h.data_size)),
        ("metadata_offset", J::N(h.metadata_offset as u64)),
        ("metadata_count", J::N(h.metadata_count as u64)),
        ("header_size", J::N(h.header_size as u64)),
        ("qa_digest", J::s(pkg::hex(&h.qa_digest))),
        (
            "sha1",
            if h.ps4.is_some() {
                J::Null
            } else {
                J::s(pkg::hex(&h.sha1))
            },
        ),
        (
            "ps4",
            match &h.ps4 {
                Some(p) => J::o([
                    ("content_type", J::N(p.content_type as u64)),
                    ("drm_type", J::N(p.drm_type as u64)),
                    ("header_sha256", J::s(pkg::hex(&p.digests[0]))),
                    ("body_sha256", J::s(pkg::hex(&p.digests[1]))),
                    ("image_sha256", J::s(pkg::hex(&p.digests[2]))),
                ]),
                None => J::Null,
            },
        ),
        ("metadata", J::O(meta)),
        ("sfo", J::O(sfo)),
    ])
}

fn key_name(p: &Pkg, e: &pkg::Entry) -> &'static str {
    match p.cipher_for(e) {
        pkg::Cipher::None => "none",
        pkg::Cipher::Debug => "Debug",
        pkg::Cipher::Ps3 => "PS3",
        pkg::Cipher::Psp => "PSP",
    }
}

fn list_json(p: &Pkg) -> J {
    let entries = p
        .entries
        .iter()
        .map(|e| {
            J::o([
                ("path", J::s(e.path.as_str())),
                ("dir", J::B(e.is_dir())),
                ("size", J::N(if e.is_dir() { 0 } else { e.size })),
                ("offset", J::N(e.offset)),
                ("flags", J::N(e.flags as u64)),
                ("key", J::s(key_name(p, e))),
            ])
        })
        .collect();
    J::o([
        ("root", J::s(p.header.root_folder())),
        ("entries", J::A(entries)),
    ])
}

/// SHA-1 result and the paths of known files that don't decrypt to their header.
fn verify_checks(p: &Pkg) -> anyhow::Result<(bool, usize, Vec<String>)> {
    let sha_ok = p.verify_sha1()?;
    let checks = p.check_headers();
    let bad = checks
        .iter()
        .filter(|c| !c.1)
        .map(|&(i, _)| p.entries[i].path.clone())
        .collect();
    Ok((sha_ok, checks.len(), bad))
}

/// `verify` for one package: SHA-1, then the known-header check. Prints one line per check.
fn verify(file: &Path, prefix: &str) -> anyhow::Result<()> {
    let p = Pkg::open(file)?;
    let (sha_ok, n_checks, bad) = verify_checks(&p)?;
    let digest = p.header.digest_name();
    println!(
        "{prefix}{digest} {}",
        if sha_ok { "OK" } else { "MISMATCH" }
    );
    match (n_checks, bad.len()) {
        (0, _) => println!("{prefix}Headers: no known files to check"),
        (n, 0) => println!("{prefix}Headers OK ({n} known files decrypt correctly)"),
        (n, b) => println!(
            "{prefix}Headers WRONG: {b} of {n} known files don't decrypt to their header (wrong key?): {}",
            bad.join(", ")
        ),
    }
    if !sha_ok {
        bail!("{digest} MISMATCH");
    }
    if !bad.is_empty() {
        bail!("{} file header(s) wrong", bad.len());
    }
    Ok(())
}

/// "Hashing 3/12" on stderr, redrawn in place when stderr is a terminal.
fn progress_line(what: &'static str, total: usize) -> impl Fn(usize) + Sync {
    // SAFETY: isatty only inspects the descriptor.
    let tty = unsafe { libc::isatty(2) } == 1;
    move |n| {
        if tty {
            eprint!("\r{what} {n}/{total}…");
            if n == total {
                eprint!("\r\x1b[K");
            }
        }
    }
}

fn audit(paths: &[PathBuf], no_hash: bool, dat: Option<&Path>, json: bool) -> anyhow::Result<()> {
    let dat = match dat {
        Some(p) => {
            let text = std::fs::read_to_string(p).with_context(|| p.display().to_string())?;
            Some(library::Dat::parse(&text).map_err(|e| anyhow::anyhow!("{}: {e}", p.display()))?)
        }
        None => None,
    };
    let files = library::collect(paths);
    if files.is_empty() {
        bail!("no .pkg files in {paths:?}");
    }
    let records = library::scan(&files, progress_line("Reading", files.len()));
    let mut found = library::audit(&records, &library::rpcs3_rap_dirs());
    if !no_hash {
        let n = found.iter().filter(|f| f.integrity.is_none()).count();
        library::hash_all(&records, &mut found, progress_line("Hashing", n));
    }
    if let Some(d) = &dat {
        library::match_dat(&records, &mut found, d);
        if !json {
            eprintln!("DAT: {} ({} files)", d.name, d.len());
        }
    }
    let name = |n: usize| records[n].path.display().to_string();
    let mut bad = 0;
    let (mut dups, mut orphans, mut missing) = (0, 0, 0);
    for (n, (r, f)) in records.iter().zip(&found).enumerate() {
        let integrity = f.integrity.clone().unwrap_or(Integrity::Unchecked);
        bad += integrity.is_bad() as usize;
        dups += (f.duplicate_of.is_some() || f.same_version_as.is_some()) as usize;
        orphans += f.orphan as usize;
        missing += (f.licence == Some(Licence::Missing)) as usize;
        let licence = match &f.licence {
            Some(Licence::Found(_)) => "found",
            Some(Licence::Missing) => "missing",
            Some(Licence::NotNeeded) => "not needed",
            _ => "unknown",
        };
        if json {
            let mut o = vec![
                ("file".to_string(), J::s(name(n))),
                ("size".into(), J::N(r.size)),
                ("integrity".into(), J::s(integrity.label())),
            ];
            match &r.info {
                Ok(i) => o.extend([
                    ("content_id".into(), J::s(i.content_id.as_str())),
                    ("title_id".into(), J::s(i.title_id.as_str())),
                    ("title".into(), J::s(i.title.as_str())),
                    ("kind".into(), J::s(i.kind.label())),
                    ("category".into(), J::s(i.category.as_str())),
                    ("app_ver".into(), J::s(i.app_ver.as_str())),
                    ("version".into(), J::s(i.version.as_str())),
                ]),
                Err(e) => o.push(("error".into(), J::s(e.as_str()))),
            }
            let opt = |x: Option<usize>| x.map_or(J::Null, |n| J::s(name(n)));
            o.extend([
                ("duplicate_of".into(), opt(f.duplicate_of)),
                ("same_version_as".into(), opt(f.same_version_as)),
                ("intact_copy".into(), opt(f.intact_copy)),
                ("orphan".into(), J::B(f.orphan)),
                ("for_disc_game".into(), J::B(f.for_disc)),
                ("licence".into(), J::s(licence)),
            ]);
            if let Some(m) = &f.dat {
                let (k, g) = match m {
                    library::DatMatch::Match(g) => ("match", g.as_str()),
                    library::DatMatch::NameOnly(g) => ("name only", g.as_str()),
                    library::DatMatch::NotInDat => ("not in DAT", ""),
                };
                o.push(("dat".into(), J::s(k)));
                o.push(("dat_game".into(), J::s(g)));
            }
            if let Some(h) = f.whole_sha1 {
                o.push(("sha1".into(), J::s(pkg::hex(&h))));
            }
            if let Some(Licence::Found(p)) = &f.licence {
                o.push(("licence_file".into(), J::s(p.to_string_lossy())));
            }
            println!("{}", J::O(o));
            continue;
        }
        match &r.info {
            Ok(i) => println!(
                "{:<11} {:<12} {:<10} {:<6} {:<28} {}",
                integrity.label(),
                i.kind.label(),
                i.title_id,
                i.app_ver,
                i.title.chars().take(28).collect::<String>(),
                name(n)
            ),
            Err(e) => println!("{:<11} {}: {e}", integrity.label(), name(n)),
        }
        for t in library::notes(&records, &found, n, name) {
            println!("            ↳ {t}");
        }
    }
    if !json {
        let checked = if no_hash {
            " (digests not checked)"
        } else {
            ""
        };
        println!(
            "\n{} package(s){checked}: {} with problems, {dups} duplicate(s), {orphans} without their game, \
             {missing} missing a licence",
            records.len(),
            bad
        );
    }
    if bad > 0 {
        bail!(
            "{bad} of {} packages are damaged or unreadable",
            records.len()
        );
    }
    Ok(())
}

fn rename(paths: &[PathBuf], template: &str, dry_run: bool, undo: bool) -> anyhow::Result<()> {
    let logs = library::log_dir();
    if undo {
        let log = library::last_log(&logs).context("nothing to undo: no rename log")?;
        let (done, problems) = library::undo(&log)?;
        for p in &problems {
            eprintln!("pkgview: {p}");
        }
        println!("Reversed {done} rename(s) from {}", log.display());
        if !problems.is_empty() {
            bail!("{} could not be reversed", problems.len());
        }
        return Ok(());
    }
    library::check_template(template).map_err(|e| anyhow::anyhow!("template: {e}"))?;
    let files = library::collect(paths);
    let records = library::scan(&files, |_| {});
    for r in &records {
        if let Err(e) = &r.info {
            eprintln!("pkgview: {}: {e} (left alone)", r.path.display());
        }
    }
    let plan = library::plan(&records, template).map_err(|e| anyhow::anyhow!(e))?;
    for (from, to) in &plan {
        let to = to.file_name().unwrap_or_default().to_string_lossy();
        println!("{} → {to}", from.display());
    }
    if plan.is_empty() {
        println!("Nothing to rename.");
    } else if dry_run {
        println!(
            "\n{} file(s) would be renamed (dry run, nothing changed).",
            plan.len()
        );
    } else {
        let (log, n) = library::apply(&plan, &logs)?;
        println!(
            "\nRenamed {n} file(s). Undo: pkgview rename --undo (log: {})",
            log.display()
        );
    }
    Ok(())
}

fn open(path: &Path) -> anyhow::Result<Pkg> {
    Pkg::open(path).with_context(|| path.display().to_string())
}

pub fn run(cmd: Cmd) -> anyhow::Result<()> {
    match cmd {
        Cmd::Info { files, json: true } => {
            json_each(&files, |f| Ok((info_json(&Pkg::open(f)?), true)))?
        }
        Cmd::Info { files, json: false } => {
            let many = files.len() > 1;
            each(&files, |f| {
                let p = Pkg::open(f)?;
                if many {
                    println!("== {}", f.display());
                }
                let h = &p.header;
                println!("{:<38} {:<20} Value", "Field", "Hex");
                for [f, x, v] in h.rows() {
                    println!("{f:<38} {x:<20} {v}");
                }
                let digests = h.digest_lines();
                let w = digests.iter().map(|d| d.0.len()).max().unwrap_or(0).max(10);
                println!("{:<w$}  {}", "Content ID", h.content_id);
                for (k, v) in digests {
                    println!("{k:<w$}  {v}");
                }
                let meta = p.meta.rows();
                if !meta.is_empty() {
                    println!("\nPackage metadata");
                    for (k, v) in meta {
                        println!("  {k:<22} {v}");
                    }
                }
                let sfo = p.sfo();
                if !sfo.is_empty() {
                    println!("\nPARAM.SFO");
                    for (k, v) in sfo {
                        println!("  {k:<22} {v}");
                    }
                }
                if many {
                    println!();
                }
                Ok(())
            })?
        }
        Cmd::List { files, json: true } => {
            json_each(&files, |f| Ok((list_json(&Pkg::open(f)?), true)))?
        }
        Cmd::List { files, json: false } => {
            let many = files.len() > 1;
            each(&files, |f| {
                let p = Pkg::open(f)?;
                let root = p.header.root_folder();
                for e in &p.entries {
                    let size = if e.is_dir() {
                        "<DIR>".into()
                    } else {
                        e.size.to_string()
                    };
                    let file = if many {
                        format!("{}: ", f.display())
                    } else {
                        String::new()
                    };
                    println!("{file}{:08X} {size:>14} {root}/{}", e.flags, e.path);
                }
                Ok(())
            })?
        }
        Cmd::Audit {
            paths,
            no_hash,
            dat,
            json,
        } => audit(&paths, no_hash, dat.as_deref(), json)?,
        Cmd::InstallRpcs3 { files, hdd0, user } => {
            let hdd0 = library::resolve_hdd0(hdd0.as_deref()).map_err(|e| anyhow::anyhow!(e))?;
            eprintln!("RPCS3 dev_hdd0: {}", hdd0.display());
            each(&files, |f| {
                let done =
                    library::install_to_rpcs3(f, &hdd0, &user, &AtomicBool::new(false), |_| {})?;
                let i = &done.installed;
                let mut line = format!(
                    "{}: {} file(s) into {}",
                    f.display(),
                    i.written,
                    i.folder.display()
                );
                if i.kept > 0 {
                    line.push_str(&format!(
                        "; kept {} existing (not marked overwritable)",
                        i.kept
                    ));
                }
                if i.skipped_types > 0 {
                    line.push_str(&format!(
                        "; skipped {} of a type RPCS3 doesn't install",
                        i.skipped_types
                    ));
                }
                if let Some(r) = &done.rap_copied {
                    line.push_str(&format!("; licence copied to {}", r.display()));
                }
                println!("{line}");
                Ok(())
            })?
        }
        Cmd::Diff {
            a,
            b,
            content,
            json,
        } => {
            let (pa, pb) = (open(&a)?, open(&b)?);
            let changes = pkg::diff(&pa, &pb, content)?;
            if json {
                let list = changes
                    .iter()
                    .map(|c| match c {
                        pkg::Change::Added(p) => {
                            J::o([("change", J::s("added")), ("path", J::s(p.as_str()))])
                        }
                        pkg::Change::Removed(p) => {
                            J::o([("change", J::s("removed")), ("path", J::s(p.as_str()))])
                        }
                        pkg::Change::Changed(p, w) => J::o([
                            ("change", J::s("changed")),
                            ("path", J::s(p.as_str())),
                            ("what", J::s(w.as_str())),
                        ]),
                    })
                    .collect();
                println!(
                    "{}",
                    J::o([
                        ("a", J::s(a.to_string_lossy())),
                        ("b", J::s(b.to_string_lossy())),
                        ("same", J::B(changes.is_empty())),
                        ("changes", J::A(list)),
                    ])
                );
            } else {
                for c in &changes {
                    match c {
                        pkg::Change::Added(p) => println!("+ {p}"),
                        pkg::Change::Removed(p) => println!("- {p}"),
                        pkg::Change::Changed(p, w) => println!("~ {p} ({w})"),
                    }
                }
                if changes.is_empty() {
                    let how = if content {
                        "same entries and bytes"
                    } else {
                        "same entries, sizes and flags (--content compares bytes)"
                    };
                    println!("No differences: {how}");
                }
            }
            if !changes.is_empty() {
                std::process::exit(1);
            }
        }
        Cmd::Rename {
            paths,
            template,
            dry_run,
            undo,
        } => rename(&paths, &template, dry_run, undo)?,
        Cmd::Cat { file, path } => {
            let p = open(&file)?;
            let want = path.trim_matches('/');
            let e = p
                .entries
                .iter()
                .find(|e| e.path == want)
                .with_context(|| format!("no entry {want:?} (see `pkgview list`)"))?;
            if e.is_dir() {
                bail!("{want} is a folder");
            }
            let mut out = std::io::BufWriter::new(std::io::stdout().lock());
            p.write_entry(e, &mut out, &AtomicBool::new(false), &mut |_| {})?;
        }
        Cmd::Extract {
            args,
            output,
            overwrite,
            no_pkg_folder,
            flat,
        } => {
            let (files, paths) = split_extract_args(&args);
            if files.len() > 1 && !paths.is_empty() {
                bail!("inner PATHs can only be given with one package (got {paths:?})");
            }
            let opts = ExtractOptions {
                overwrite: match overwrite {
                    OverwriteArg::Skip => Overwrite::Skip,
                    OverwriteArg::Replace => Overwrite::Replace,
                    OverwriteArg::Rename => Overwrite::Rename,
                },
                pkg_folder: !no_pkg_folder,
                keep_paths: !flat,
            };
            each(&files, |file| {
                let p = Pkg::open(file)?;
                let dir = match &output {
                    Some(d) => d.clone(),
                    None => file
                        .canonicalize()?
                        .parent()
                        .context("pkg has no parent dir")?
                        .to_path_buf(),
                };
                let idx = p.select(if paths.is_empty() { &[""] } else { &paths });
                if idx.is_empty() {
                    bail!("no entries match {paths:?}");
                }
                let n = idx.len();
                let mut i = 0;
                let name = pkg::pkg_name(file);
                p.extract(&name, &idx, &dir, &opts, &AtomicBool::new(false), |pr| {
                    if let Progress::File(e) = pr {
                        i += 1;
                        eprintln!("[{i}/{n}] {}", e.path);
                    }
                })?;
                let to = if opts.pkg_folder { dir.join(name) } else { dir };
                eprintln!("Extracted {n} entries to {}", to.display());
                Ok(())
            })?;
        }
        Cmd::Verify { files, json: true } => json_each(&files, |f| {
            let p = Pkg::open(f)?;
            let (sha_ok, n, bad) = verify_checks(&p)?;
            let ok = sha_ok && bad.is_empty();
            let bad = bad.into_iter().map(J::S).collect();
            Ok((
                J::o([
                    ("ok", J::B(ok)),
                    ("sha1_ok", J::B(sha_ok)),
                    ("headers_checked", J::N(n as u64)),
                    ("headers_wrong", J::A(bad)),
                ]),
                ok,
            ))
        })?,
        Cmd::Verify { files, json: false } => {
            let many = files.len() > 1;
            each(&files, |f| {
                let prefix = if many {
                    format!("{}: ", f.display())
                } else {
                    String::new()
                };
                verify(f, &prefix)
            })?;
        }
        Cmd::Thumbnail {
            input,
            output,
            size,
        } => {
            let p = open(&input)?;
            let e = p
                .find("ICON0.PNG")
                .or_else(|| p.find("PIC1.PNG"))
                .context("no ICON0.PNG or PIC1.PNG")?;
            let png = glib::Bytes::from_owned(p.read_entry(e)?);
            let stream = gio::MemoryInputStream::from_bytes(&png);
            let pix = gdk_pixbuf::Pixbuf::from_stream(&stream, None::<&gio::Cancellable>)?;
            let (w, h) = (pix.width(), pix.height());
            let scale = size.max(1) as f64 / w.max(h) as f64;
            let pix = if scale < 1.0 {
                let fit = |v: i32| ((v as f64 * scale).round() as i32).max(1);
                pix.scale_simple(fit(w), fit(h), gdk_pixbuf::InterpType::Bilinear)
                    .context("scaling failed")?
            } else {
                pix
            };
            pix.savev(&output, "png", &[])?;
        }
        Cmd::Man => clap_mangen::Man::new(Cli::command()).render(&mut std::io::stdout())?,
        Cmd::Completions { shell } => clap_complete::generate(
            shell,
            &mut Cli::command(),
            "pkgview",
            &mut std::io::stdout(),
        ),
    }
    Ok(())
}

/// `extract` arguments: the first is a package; later ones are packages if they end in `.pkg` (any case),
/// otherwise inner paths (leading/trailing `/` dropped).
fn split_extract_args(args: &[String]) -> (Vec<PathBuf>, Vec<&str>) {
    let is_pkg = |a: &str| {
        Path::new(a)
            .extension()
            .is_some_and(|x| x.eq_ignore_ascii_case("pkg"))
    };
    let mut files = vec![];
    let mut paths = vec![];
    for (i, a) in args.iter().enumerate() {
        if i == 0 || is_pkg(a) {
            files.push(PathBuf::from(a));
        } else {
            paths.push(a.trim_matches('/'));
        }
    }
    (files, paths)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_args() {
        let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        let a = s(&["game", "/USRDIR/", "PARAM.SFO"]);
        let (f, p) = split_extract_args(&a);
        assert_eq!(f, [PathBuf::from("game")]);
        assert_eq!(p, ["USRDIR", "PARAM.SFO"]);
        let a = s(&["a.pkg", "b.PKG", "dir/c.pkg"]);
        let (f, p) = split_extract_args(&a);
        assert_eq!(f.len(), 3);
        assert!(p.is_empty());
    }

    #[test]
    fn json_writer() {
        let j = J::o([
            ("s", J::s("a\"b\\c\nd\u{1}é")),
            ("n", J::N(42)),
            ("b", J::B(false)),
            ("a", J::A(vec![J::N(1), J::s("x")])),
            ("o", J::o([])),
        ]);
        assert_eq!(
            j.to_string(),
            r#"{"s":"a\"b\\c\nd\u0001é","n":42,"b":false,"a":[1,"x"],"o":{}}"#
        );
    }

    #[test]
    fn cli_parses() {
        Cli::command().debug_assert();
        assert!(Cli::try_parse_from(["pkgview", "verify"]).is_err()); // usage error, exit 2
        assert!(
            Cli::try_parse_from(["pkgview", "extract", "a.pkg", "--overwrite", "ask"]).is_err()
        );
        assert!(Cli::try_parse_from(["pkgview", "extract", "a.pkg", "b.pkg", "--flat"]).is_ok());
    }
}
