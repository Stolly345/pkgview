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
    /// Print the header / properties table
    Info { file: PathBuf },
    /// List entries: flags, size, path
    List { file: PathBuf },
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

/// `verify` for one package: SHA-1, then the known-header check. Prints one line per check.
fn verify(file: &Path, prefix: &str) -> anyhow::Result<()> {
    let p = Pkg::open(file)?;
    let sha_ok = p.verify_sha1()?;
    println!("{prefix}SHA-1 {}", if sha_ok { "OK" } else { "MISMATCH" });
    let checks = p.check_headers();
    let bad: Vec<&str> = checks
        .iter()
        .filter(|c| !c.1)
        .map(|&(i, _)| p.entries[i].path.as_str())
        .collect();
    match (checks.len(), bad.len()) {
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
        Cmd::Info { file } => {
            let p = open(&file)?;
            let h = &p.header;
            println!("{:<38} {:<20} Value", "Field", "Hex");
            for [f, x, v] in h.rows() {
                println!("{f:<38} {x:<20} {v}");
            }
            println!("Content ID  {}", h.content_id);
            println!("QA digest   {}", pkg::hex_spaced(&h.qa_digest));
            println!("SHA-1       {}", pkg::hex_spaced(&h.sha1));
            for (k, v) in p.sfo_summary() {
                println!("{k:<11} {v}");
            }
        }
        Cmd::List { file } => {
            let p = open(&file)?;
            let root = p.header.root_folder();
            for e in &p.entries {
                let size = if e.is_dir() {
                    "<DIR>".into()
                } else {
                    e.size.to_string()
                };
                println!("{:08X} {size:>14} {root}/{}", e.flags, e.path);
            }
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
        Cmd::Verify { files } => {
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
    fn cli_parses() {
        Cli::command().debug_assert();
        assert!(Cli::try_parse_from(["pkgview", "verify"]).is_err()); // usage error, exit 2
        assert!(
            Cli::try_parse_from(["pkgview", "extract", "a.pkg", "--overwrite", "ask"]).is_err()
        );
        assert!(Cli::try_parse_from(["pkgview", "extract", "a.pkg", "b.pkg", "--flat"]).is_ok());
    }
}
