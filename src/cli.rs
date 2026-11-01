//! Headless commands: `info`, `list`, `extract`, `verify`.

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
    /// Check the trailing SHA-1 and that known files (PARAM.SFO, PNG, EBOOT…) decrypt to their headers
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
    let sfo = p.sfo().into_iter().map(|(k, v)| (k, J::S(v))).collect();
    J::o([
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
        ("sha1", J::s(pkg::hex(&h.sha1))),
        ("metadata", J::O(meta)),
        ("sfo", J::O(sfo)),
    ])
}

fn key_name(p: &Pkg, e: &pkg::Entry) -> &'static str {
    match p.cipher_for(e) {
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
    println!("{prefix}SHA-1 {}", if sha_ok { "OK" } else { "MISMATCH" });
    match (n_checks, bad.len()) {
        (0, _) => println!("{prefix}Headers: no known files to check"),
        (n, 0) => println!("{prefix}Headers OK ({n} known files decrypt correctly)"),
        (n, b) => println!(
            "{prefix}Headers WRONG: {b} of {n} known files don't decrypt to their header (wrong key?): {}",
            bad.join(", ")
        ),
    }
    if !sha_ok {
        bail!("SHA-1 MISMATCH");
    }
    if !bad.is_empty() {
        bail!("{} file header(s) wrong", bad.len());
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
                println!("Content ID  {}", h.content_id);
                println!("QA digest   {}", pkg::hex_spaced(&h.qa_digest));
                println!("SHA-1       {}", pkg::hex_spaced(&h.sha1));
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
