mod cli;
mod gui;
mod pkg;

use clap::Parser;
use std::process::ExitCode;

fn main() -> ExitCode {
    let mut args: Vec<String> = std::env::args().collect();
    // Original PkgView's "extract here" switch.
    if args.get(1).is_some_and(|a| a == "-ex") {
        args[1] = "extract".into();
    }
    let cli = cli::Cli::parse_from(args);
    let Some(cmd) = cli.cmd else {
        if cli.extract_here || cli.extract_to {
            return gui::run_extract(cli.files, cli.extract_to);
        }
        return gui::run(cli.files.into_iter().next());
    };
    // `pkgview list x.pkg | head` should exit quietly, not panic on EPIPE.
    // SAFETY: restoring the default disposition before any threads exist.
    unsafe { libc::signal(libc::SIGPIPE, libc::SIG_DFL) };
    match cli::run(cmd) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("pkgview: {e:#}");
            ExitCode::FAILURE
        }
    }
}
