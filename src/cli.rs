//! Headless commands: `info`, `list`, `extract`, `verify`.

use crate::pkg::{self, ExtractOptions, Pkg, Progress};
use anyhow::{bail, Context};
use clap::{CommandFactory, Parser, Subcommand};
use gtk::{gdk_pixbuf, gio, glib};
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;

#[derive(Parser)]
#[command(
    name = "pkgview",
    version,
    about = "View and extract PS3/PSP .pkg files"
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
    /// Extract everything (or only PATHs) into DIR/<pkg name>/ (default: next to the pkg)
    Extract {
        file: PathBuf,
        #[arg(short, long, value_name = "DIR")]
        output: Option<PathBuf>,
        paths: Vec<String>,
    },
    /// Check the trailing SHA-1 digest
    Verify { file: PathBuf },
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
            file,
            output,
            paths,
        } => {
            let p = open(&file)?;
            let dir = match output {
                Some(d) => d,
                None => file
                    .canonicalize()?
                    .parent()
                    .context("pkg has no parent dir")?
                    .to_path_buf(),
            };
            let paths: Vec<&str> = paths.iter().map(|s| s.trim_matches('/')).collect();
            let idx = p.select(if paths.is_empty() { &[""] } else { &paths });
            if idx.is_empty() {
                bail!("no entries match {paths:?}");
            }
            let n = idx.len();
            let mut i = 0;
            let name = pkg::pkg_name(&file);
            let opts = ExtractOptions::default();
            p.extract(&name, &idx, &dir, &opts, &AtomicBool::new(false), |pr| {
                if let Progress::File(e) = pr {
                    i += 1;
                    eprintln!("[{i}/{n}] {}", e.path);
                }
            })?;
            eprintln!("Extracted {n} entries to {}", dir.join(name).display());
        }
        Cmd::Verify { file } => {
            if !open(&file)?.verify_sha1()? {
                bail!("SHA-1 MISMATCH");
            }
            println!("SHA-1 OK");
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
