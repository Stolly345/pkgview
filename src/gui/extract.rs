//! Extraction jobs on a worker thread, with a progress window; opening and dragging out single entries.

use super::{close_on_escape, Job, Ui};
use crate::pkg::{pkg_name, round_bytes, ExtractOptions, Pkg, Progress};
use gtk::prelude::*;
use gtk::{gdk, gio, glib};
use std::cell::{Cell, RefCell};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant};

/// Largest file opened, or selection dragged out, without asking / at all.
const OPEN_LIMIT: u64 = 512 << 20;

/// `$XDG_CACHE_HOME/pkgview/open/<pid>`: files extracted to be opened or dragged out; removed on exit.
pub(super) fn open_dir() -> PathBuf {
    glib::user_cache_dir()
        .join("pkgview/open")
        .join(std::process::id().to_string())
}

/// Remove `open/<pid>` folders left by PkgView processes that no longer run (crash, kill).
pub(super) fn clean_stale_open_dirs() {
    let Ok(rd) = std::fs::read_dir(glib::user_cache_dir().join("pkgview/open")) else {
        return;
    };
    for e in rd.flatten() {
        let Some(pid) = e.file_name().to_str().and_then(|s| s.parse::<i32>().ok()) else {
            continue;
        };
        // SAFETY: signal 0 only checks whether the process exists.
        let dead = unsafe { libc::kill(pid, 0) } == -1
            && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH);
        if dead {
            let _ = std::fs::remove_dir_all(e.path());
        }
    }
}

/// Extract `paths` (and everything below them) into the open cache; returns where each path landed.
fn extract_to_cache(pkg: &Pkg, pkg_path: &Path, paths: &[String]) -> std::io::Result<Vec<PathBuf>> {
    let dir = open_dir();
    let name = pkg_name(pkg_path);
    let refs: Vec<&str> = paths.iter().map(String::as_str).collect();
    let idx = pkg.select(&refs);
    let opts = ExtractOptions::default();
    pkg.extract(&name, &idx, &dir, &opts, &AtomicBool::new(false), |_| {})?;
    let root = dir.join(name).join(pkg.header.root_folder());
    Ok(paths.iter().map(|p| root.join(p)).collect())
}

impl Ui {
    /// Extract one file to the cache on a worker thread, then open it with the default application.
    pub(super) fn open_file(self: &Rc<Self>, path: &str) {
        let Some((pkg, pkg_path)) = self.pkg.borrow().clone() else {
            return;
        };
        let Some(&i) = pkg.select(&[path]).first() else {
            return;
        };
        let e = pkg.entries[i].clone();
        let go = {
            let (ui, pkg, path) = (self.clone(), pkg.clone(), path.to_string());
            move || {
                ui.busy(Some(&format!("Opening {}…", e.name())));
                let (ui, pkg, pkg_path, path, e) = (
                    ui.clone(),
                    pkg.clone(),
                    pkg_path.clone(),
                    path.clone(),
                    e.clone(),
                );
                glib::spawn_future_local(async move {
                    let r = gio::spawn_blocking(move || extract_to_cache(&pkg, &pkg_path, &[path]))
                        .await;
                    ui.busy(None);
                    let launched = match r {
                        Ok(Ok(files)) => gio::AppInfo::launch_default_for_uri(
                            &gio::File::for_path(&files[0]).uri(),
                            Some(&WidgetExt::display(&ui.window).app_launch_context()),
                        )
                        .map_err(|e| e.to_string()),
                        Ok(Err(e)) => Err(e.to_string()),
                        Err(_) => Err("extraction crashed".into()),
                    };
                    if let Err(msg) = launched {
                        ui.error(&format!("Could not open {}:\n{msg}", e.name()));
                    }
                });
            }
        };
        if pkg.entries[i].size <= OPEN_LIMIT {
            return go();
        }
        let d = gtk::MessageDialog::builder()
            .transient_for(&self.window)
            .modal(true)
            .message_type(gtk::MessageType::Question)
            .buttons(gtk::ButtonsType::OkCancel)
            .text(format!(
                "{} is {}. Extract it to a temporary folder and open it?",
                pkg.entries[i].name(),
                round_bytes(pkg.entries[i].size)
            ))
            .build();
        d.connect_response(move |d, r| {
            d.close();
            if r == gtk::ResponseType::Ok {
                go();
            }
        });
        d.present();
    }

    /// Drag-out payload: the selected files/folders, extracted to the cache.
    // ponytail: GTK4 has no XDS (drop-site-chosen destination), so extraction happens at drag start,
    // on the UI thread; capped at OPEN_LIMIT, larger selections go through Extract instead.
    pub(super) fn drag_files(&self) -> Option<gdk::ContentProvider> {
        let (pkg, pkg_path) = self.pkg.borrow().clone()?;
        let paths = self.selected_paths();
        if paths.is_empty() {
            return None;
        }
        let refs: Vec<&str> = paths.iter().map(String::as_str).collect();
        let bytes: u64 = pkg.select(&refs).iter().map(|&i| pkg.entries[i].size).sum();
        if bytes > OPEN_LIMIT {
            self.status.set_text(&format!(
                "Use Extract for large selections ({} > {})",
                round_bytes(bytes),
                round_bytes(OPEN_LIMIT)
            ));
            return None;
        }
        match extract_to_cache(&pkg, &pkg_path, &paths) {
            Ok(files) => {
                let files: Vec<gio::File> = files.iter().map(gio::File::for_path).collect();
                let list = gdk::FileList::from_array(&files);
                Some(gdk::ContentProvider::for_value(&list.to_value()))
            }
            Err(e) => {
                self.status.set_text(&format!("Drag failed: {e}"));
                None
            }
        }
    }

    pub(super) fn extract(self: &Rc<Self>, dir: PathBuf) {
        let Some((pkg, path)) = self.pkg.borrow().clone() else {
            return;
        };
        let paths = self.selection();
        self.run_jobs(vec![Job {
            path,
            pkg: Some(pkg),
            paths,
            dest: dir,
        }]);
    }

    /// `start_jobs` from the main window: result in the status bar, failures in a dialog.
    pub(super) fn run_jobs(self: &Rc<Self>, jobs: Vec<Job>) {
        let ui = self.clone();
        let app = self
            .window
            .application()
            .expect("main window has an application");
        start_jobs(&app, Some(self.window.upcast_ref()), jobs, move |o| {
            ui.status.set_text(&o.summary);
            if !o.failed.is_empty() {
                ui.error(&format!("Extraction failed:\n{}", o.failed.join("\n")));
            }
        });
    }
}

/// What a finished `start_jobs` reports.
pub(super) struct Outcome {
    pub summary: String,
    /// "<pkg>: <reason>" per package that failed to open or extract.
    pub failed: Vec<String>,
}

/// `pkgview --extract-here/--extract-to FILE…`: no main window, only the folder chooser (`ask_dest`) and the
/// progress window; a failure ends in an error dialog and sets `failed`.
pub(super) fn standalone(
    app: &gtk::Application,
    files: Vec<PathBuf>,
    ask_dest: bool,
    failed: Rc<Cell<bool>>,
) {
    let hold = Rc::new(RefCell::new(Some(app.hold()))); // the app quits once this is dropped
    let go = {
        let (app, hold) = (app.clone(), hold.clone());
        move |dest: Option<PathBuf>| {
            let jobs = files
                .iter()
                .map(|p| Job {
                    path: p.clone(),
                    pkg: None,
                    paths: vec![],
                    dest: dest.clone().unwrap_or_else(|| {
                        p.canonicalize()
                            .ok()
                            .and_then(|c| c.parent().map(Path::to_path_buf))
                            .unwrap_or_else(|| ".".into())
                    }),
                })
                .collect();
            let (app2, hold, failed) = (app.clone(), hold.clone(), failed.clone());
            start_jobs(&app, None, jobs, move |o| {
                if o.failed.is_empty() {
                    hold.take();
                    return;
                }
                failed.set(true);
                let d = gtk::MessageDialog::builder()
                    .application(&app2)
                    .message_type(gtk::MessageType::Warning)
                    .buttons(gtk::ButtonsType::Ok)
                    .text("Extraction failed")
                    .secondary_text(o.failed.join("\n"))
                    .build();
                let hold = hold.clone();
                d.connect_response(move |d, _| {
                    d.close();
                    hold.take();
                });
                d.present();
            });
        }
    };
    if !ask_dest {
        return go(None);
    }
    let d = gtk::FileChooserNative::new(
        Some("Extract to"),
        None::<&gtk::Window>,
        gtk::FileChooserAction::SelectFolder,
        Some("_Select"),
        Some("_Cancel"),
    );
    let keep = d.clone(); // ponytail: the closure keeps the dialog alive until it answers (cycle broken on exit)
    d.connect_response(move |d, r| {
        let _ = &keep;
        match d.file().and_then(|f| f.path()) {
            Some(dir) if r == gtk::ResponseType::Accept => go(Some(dir)),
            _ => {
                hold.take();
            }
        }
    });
    d.show();
}

/// Extract each job into `<its dest>/<pkg name>/` on a worker thread, with one progress window for all of
/// them. A package that fails is reported at the end; the others still run.
pub(super) fn start_jobs(
    app: &gtk::Application,
    parent: Option<&gtk::Window>,
    jobs: Vec<Job>,
    on_done: impl Fn(Outcome) + 'static,
) {
    let n_jobs = jobs.len();
    let first = jobs.first().map(|j| pkg_name(&j.path)).unwrap_or_default();

    // Progress window
    let pkg_lbl = gtk::Label::builder()
        .xalign(0.0)
        .visible(n_jobs > 1)
        .build();
    let file_lbl = gtk::Label::builder()
        .xalign(0.0)
        .ellipsize(gtk::pango::EllipsizeMode::Middle)
        .build();
    let action_lbl = gtk::Label::builder()
        .label(if n_jobs > 1 {
            "Opening packages…"
        } else {
            "Decrypting…"
        })
        .xalign(0.0)
        .build();
    let file_bar = gtk::ProgressBar::builder().show_text(true).build();
    let total_bar = gtk::ProgressBar::builder().show_text(true).build();
    let time_lbl = gtk::Label::builder().xalign(0.0).build();
    let cancel_btn = gtk::Button::with_label("Cancel");
    cancel_btn.set_halign(gtk::Align::End);
    let b = gtk::Box::new(gtk::Orientation::Vertical, 8);
    b.set_margin_start(12);
    b.set_margin_end(12);
    b.set_margin_top(12);
    b.set_margin_bottom(12);
    b.append(&pkg_lbl);
    b.append(&file_lbl);
    b.append(&action_lbl);
    b.append(&file_bar);
    b.append(&gtk::Label::builder().label("Total").xalign(0.0).build());
    b.append(&total_bar);
    b.append(&time_lbl);
    b.append(&cancel_btn);
    let win = gtk::Window::builder()
        .title(if n_jobs > 1 {
            format!("Bulk extract ({n_jobs} packages)")
        } else {
            format!("Extracting {first}")
        })
        .default_width(460)
        .child(&b)
        .build();
    win.set_transient_for(parent);
    win.set_application(Some(app));
    let cancel = Arc::new(AtomicBool::new(false));
    let c = cancel.clone();
    let w = win.clone();
    cancel_btn.connect_clicked(move |_| {
        c.store(true, Ordering::Relaxed);
        w.close();
    });
    close_on_escape(&win);
    let c = cancel.clone();
    win.connect_close_request(move |_| {
        c.store(true, Ordering::Relaxed);
        glib::Propagation::Proceed
    });
    win.present();

    enum Msg {
        /// bytes, entries, packages that opened
        Total(u64, usize, usize),
        Pkg(usize, String),
        File(String, u64),
        Bytes(u64),
        Done {
            ok: usize,
            entries: usize,
            failed: Vec<String>,
            cancelled: bool,
        },
    }
    let (tx, rx) = mpsc::channel();
    let c = cancel.clone();
    let out_dir = jobs.first().map(|j| j.dest.clone()).unwrap_or_default();
    let same_dest = jobs.iter().all(|j| j.dest == out_dir);
    std::thread::spawn(move || {
        // Open everything first so the total bar covers every package.
        let mut ready = vec![];
        let mut failed = vec![];
        for j in jobs {
            let name = pkg_name(&j.path);
            let pkg = match j.pkg {
                Some(p) => p,
                None => match Pkg::open(&j.path) {
                    Ok(p) => Arc::new(p),
                    Err(e) => {
                        failed.push(format!("{name}: {e}"));
                        continue;
                    }
                },
            };
            let paths: Vec<&str> = j.paths.iter().map(String::as_str).collect();
            let idx = pkg.select(if paths.is_empty() { &[""] } else { &paths });
            ready.push((name, pkg, idx, j.dest));
        }
        let bytes = ready
            .iter()
            .flat_map(|(_, p, idx, _)| idx.iter().map(|&i| p.entries[i].size))
            .sum();
        let entries = ready.iter().map(|r| r.2.len()).sum();
        let _ = tx.send(Msg::Total(bytes, entries, ready.len()));
        let (mut ok, mut cancelled) = (0, false);
        for (i, (name, pkg, idx, dest)) in ready.iter().enumerate() {
            let _ = tx.send(Msg::Pkg(i, name.clone()));
            let r = pkg.extract(name, idx, dest, &ExtractOptions::default(), &c, |p| {
                let _ = tx.send(match p {
                    Progress::File(e) => Msg::File(e.path.clone(), e.size),
                    Progress::Bytes(n) => Msg::Bytes(n),
                });
            });
            match r {
                Ok(true) => ok += 1,
                Ok(false) => {
                    cancelled = true;
                    break;
                }
                Err(e) => failed.push(format!("{name}: {e}")),
            }
        }
        let _ = tx.send(Msg::Done {
            ok,
            entries,
            failed,
            cancelled,
        });
    });

    let start = Instant::now();
    let (mut done, mut total, mut file_done, mut file_size) = (0u64, 1u64, 0u64, 0u64);
    let (mut files, mut count, mut n_ready) = (0usize, 0usize, 0usize);
    glib::timeout_add_local(Duration::from_millis(100), move || {
        for m in rx.try_iter() {
            match m {
                Msg::Total(b, e, n) => (total, count, n_ready) = (b.max(1), e, n),
                Msg::Pkg(i, name) => {
                    pkg_lbl.set_text(&format!("Package {}/{n_ready}: {name}", i + 1));
                    action_lbl.set_text("Decrypting…");
                }
                Msg::File(p, size) => {
                    file_lbl.set_text(&p);
                    (file_done, file_size) = (0, size);
                    files += 1;
                }
                Msg::Bytes(n) => {
                    done += n;
                    file_done += n;
                }
                Msg::Done {
                    ok,
                    entries,
                    failed,
                    cancelled,
                } => {
                    win.close();
                    let plural = |n: usize, one: &str, many: &str| {
                        format!("{n} {}", if n == 1 { one } else { many })
                    };
                    let summary = if cancelled {
                        "Extraction cancelled".to_string()
                    } else if n_jobs == 1 && ok == 1 {
                        format!(
                            "Extracted {} to {}",
                            plural(entries, "entry", "entries"),
                            out_dir.join(&first).display()
                        )
                    } else if same_dest {
                        format!(
                            "Extracted {ok} of {} to {}",
                            plural(n_jobs, "package", "packages"),
                            out_dir.display()
                        )
                    } else {
                        format!(
                            "Extracted {ok} of {}, each next to its .pkg",
                            plural(n_jobs, "package", "packages")
                        )
                    };
                    on_done(Outcome { summary, failed });
                    return glib::ControlFlow::Break;
                }
            }
        }
        let frac = |a: u64, b: u64| if b == 0 { 1.0 } else { a as f64 / b as f64 };
        file_bar.set_fraction(frac(file_done, file_size));
        file_bar.set_text(Some(&format!("{:.0}%", frac(file_done, file_size) * 100.0)));
        total_bar.set_fraction(frac(done, total));
        total_bar.set_text(Some(&format!(
            "{:.0}%  ({files}/{count})",
            frac(done, total) * 100.0
        )));
        let el = start.elapsed().as_secs_f64();
        let left = if done > 0 {
            el * total as f64 / done as f64 - el
        } else {
            0.0
        };
        time_lbl.set_text(&format!("Elapsed {}   Time left {}", hms(el), hms(left)));
        glib::ControlFlow::Continue
    });
}

fn hms(secs: f64) -> String {
    let s = secs.max(0.0) as u64;
    format!("{:02}:{:02}:{:02}", s / 3600, s / 60 % 60, s % 60)
}
