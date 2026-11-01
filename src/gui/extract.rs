//! Extraction jobs on a worker thread, with a progress window; opening and dragging out single entries.

use super::settings::Settings;
use super::{alert, close_on_escape, Job, Ui};
use crate::library;
use crate::pkg::{pkg_name, round_bytes, ExtractOptions, Overwrite, Pkg, Progress};
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

/// Recent destinations kept in the Extract dialog.
const MAX_DESTINATIONS: usize = 8;

/// `$XDG_CACHE_HOME/pkgview/open/<pid>`: files extracted to be opened or dragged out; removed at the
/// next start once this process is gone.
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
        gtk::AlertDialog::builder()
            .modal(true)
            .message(format!(
                "{} is {}. Extract it to a temporary folder and open it?",
                pkg.entries[i].name(),
                round_bytes(pkg.entries[i].size)
            ))
            .buttons(["Cancel", "Open"])
            .cancel_button(0)
            .default_button(1)
            .build()
            .choose(Some(&self.window), gio::Cancellable::NONE, move |r| {
                if r == Ok(1) {
                    go();
                }
            });
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

    /// Extract the selection (or current folder) of the open package into `dir`.
    pub(super) fn extract(self: &Rc<Self>, dir: PathBuf, opts: ExtractOptions, open_after: bool) {
        let Some((pkg, path)) = self.pkg.borrow().clone() else {
            return;
        };
        let paths = self.selection();
        let show = open_after.then(|| output_folder(&dir, &path, &opts));
        self.run_jobs(
            vec![Job {
                path,
                pkg: Some(pkg),
                paths,
                dest: dir,
                opts,
                rpcs3: None,
            }],
            show,
        );
    }

    /// `start_jobs` from the main window: result in the status bar, failures in a dialog; `show` is opened
    /// in the file manager if everything succeeded.
    pub(super) fn run_jobs(self: &Rc<Self>, jobs: Vec<Job>, show: Option<PathBuf>) {
        self.run_jobs_again(jobs, show, false)
    }

    /// `run_jobs`; `retry` = these jobs failed before (the failure message says "again").
    fn run_jobs_again(self: &Rc<Self>, jobs: Vec<Job>, show: Option<PathBuf>, retry: bool) {
        let ui = self.clone();
        let app = self
            .window
            .application()
            .expect("main window has an application");
        let again = jobs.clone();
        start_jobs(&app, Some(self.window.upcast_ref()), jobs, move |o| {
            ui.status.set_text(&o.summary);
            if o.failed.is_empty() {
                if let (Some(dir), false) = (&show, o.cancelled) {
                    show_folder(dir);
                }
                return;
            }
            // The failed packages again, with the same destination and options.
            let failed_jobs: Vec<Job> = o.failed.iter().map(|(k, _)| again[*k].clone()).collect();
            let (ui, show) = (ui.clone(), show.clone());
            gtk::AlertDialog::builder()
                .modal(true)
                .message(format!(
                    "{} of {} package(s) failed{}",
                    o.failed.len(),
                    again.len(),
                    if retry { " again" } else { "" }
                ))
                .detail(o.messages())
                .buttons(["Close", "Retry Failed"])
                .cancel_button(0)
                .default_button(1)
                .build()
                .choose(Some(&ui.window.clone()), gio::Cancellable::NONE, move |r| {
                    if r == Ok(1) {
                        ui.run_jobs_again(failed_jobs, show, true);
                    }
                });
        });
    }
}

/// Where a package's files land: `dir/<pkg name>` or, without the package folder, `dir`.
fn output_folder(dir: &Path, pkg_path: &Path, opts: &ExtractOptions) -> PathBuf {
    if opts.pkg_folder {
        dir.join(pkg_name(pkg_path))
    } else {
        dir.to_path_buf()
    }
}

/// Open `dir` in the default file manager.
fn show_folder(dir: &Path) {
    let ctx = gdk::Display::default().map(|d| d.app_launch_context());
    let _ = gio::AppInfo::launch_default_for_uri(&gio::File::for_path(dir).uri(), ctx.as_ref());
}

/// What a finished `start_jobs` reports.
pub(super) struct Outcome {
    pub summary: String,
    /// (job index, "<pkg>: <reason>") per package that failed to open or extract.
    pub failed: Vec<(usize, String)>,
    pub cancelled: bool,
}

impl Outcome {
    pub fn messages(&self) -> String {
        self.failed
            .iter()
            .map(|f| f.1.as_str())
            .collect::<Vec<_>>()
            .join("\n")
    }
}

/// What the Extract dialog returns.
pub(super) struct Choice {
    pub dest: PathBuf,
    pub opts: ExtractOptions,
    pub open_after: bool,
}

/// `~/x` → `$HOME/x`; other paths unchanged.
fn expand_home(s: &str) -> PathBuf {
    match s.strip_prefix("~/").or((s == "~").then_some("")) {
        Some(rest) => glib::home_dir().join(rest),
        None => PathBuf::from(s),
    }
}

/// The Extract dialog: destination (typed, browsed or recent), package folder, folder structure, overwrite
/// policy, open when done. The choices are remembered in settings.ini for next time. Returns the dialog.
pub(super) fn extract_dialog(
    app: &gtk::Application,
    parent: Option<&gtk::Window>,
    title: &str,
    default_dest: PathBuf,
    on_ok: impl Fn(Choice) + 'static,
) -> gtk::Window {
    let settings = Settings::load();
    let opts = settings.extract_options();
    let recent = settings.list("destinations");

    let dest = gtk::Entry::builder()
        .text(default_dest.to_string_lossy())
        .hexpand(true)
        .activates_default(true)
        .build();
    dest.update_property(&[gtk::accessible::Property::Label("Destination folder")]);
    let browse = gtk::Button::with_mnemonic("_Browse…");
    let recent_btn = gtk::MenuButton::builder()
        .icon_name("document-open-recent-symbolic")
        .tooltip_text("Recent destinations")
        .sensitive(!recent.is_empty())
        .build();
    let recent_box = gtk::Box::new(gtk::Orientation::Vertical, 0);
    let pop = gtk::Popover::builder().child(&recent_box).build();
    recent_btn.set_popover(Some(&pop));
    for r in &recent {
        let b = gtk::Button::builder()
            .label(r.as_str())
            .has_frame(false)
            .build();
        if let Some(l) = b.child().and_downcast::<gtk::Label>() {
            l.set_xalign(0.0);
        }
        let (dest, pop, r) = (dest.clone(), pop.clone(), r.clone());
        b.connect_clicked(move |_| {
            dest.set_text(&r);
            pop.popdown();
        });
        recent_box.append(&b);
    }
    let row = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    row.append(&dest);
    row.append(&recent_btn);
    row.append(&browse);

    let pkg_folder = gtk::CheckButton::with_mnemonic("Create a _folder named after the package");
    pkg_folder.set_active(opts.pkg_folder);
    let keep_paths = gtk::CheckButton::with_mnemonic("_Keep the package's folder structure");
    keep_paths.set_active(opts.keep_paths);
    let open_after = gtk::CheckButton::with_mnemonic("_Open the destination when done");
    open_after.set_active(settings.open_after());
    const POLICIES: [(Overwrite, &str); 4] = [
        (Overwrite::Ask, "Ask"),
        (Overwrite::Skip, "Skip it"),
        (Overwrite::Replace, "Replace it"),
        (Overwrite::Rename, "Keep both (rename the new file)"),
    ];
    let policy = gtk::DropDown::from_strings(&POLICIES.map(|p| p.1));
    policy.set_selected(
        POLICIES
            .iter()
            .position(|p| p.0 == opts.overwrite)
            .unwrap_or(0) as u32,
    );
    let policy_lbl = gtk::Label::with_mnemonic("If a file already _exists:");
    policy_lbl.set_mnemonic_widget(Some(&policy));
    let policy_row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    policy_row.append(&policy_lbl);
    policy_row.append(&policy);
    let problem = gtk::Label::builder().xalign(0.0).visible(false).build();
    problem.add_css_class("error");

    let cancel = gtk::Button::with_mnemonic("_Cancel");
    let ok = gtk::Button::with_mnemonic("_Extract");
    ok.add_css_class("suggested-action");
    let buttons = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    buttons.set_halign(gtk::Align::End);
    buttons.set_margin_top(6);
    buttons.append(&cancel);
    buttons.append(&ok);

    let dest_lbl = gtk::Label::with_mnemonic("_Destination:");
    dest_lbl.set_xalign(0.0);
    dest_lbl.set_mnemonic_widget(Some(&dest));
    let b = gtk::Box::new(gtk::Orientation::Vertical, 8);
    for m in [
        gtk::Widget::set_margin_start,
        gtk::Widget::set_margin_end,
        gtk::Widget::set_margin_top,
        gtk::Widget::set_margin_bottom,
    ] {
        m(b.upcast_ref(), 14);
    }
    b.append(&dest_lbl);
    b.append(&row);
    b.append(&pkg_folder);
    b.append(&keep_paths);
    b.append(&policy_row);
    b.append(&open_after);
    b.append(&problem);
    b.append(&buttons);
    let win = gtk::Window::builder()
        .title(title)
        .modal(true)
        .default_width(520)
        .resizable(false)
        .child(&b)
        .default_widget(&ok)
        .build();
    win.set_transient_for(parent);
    win.set_application(Some(app));
    close_on_escape(&win);

    // Closures reach the window through `root()`: holding it would be a reference cycle (never freed).
    let root = |b: &gtk::Button| b.root().and_downcast::<gtk::Window>();
    let d = dest.clone();
    browse.connect_clicked(move |b| {
        let Some(w) = root(b) else { return };
        let fd = gtk::FileDialog::builder()
            .title("Extract to")
            .modal(true)
            .initial_folder(&gio::File::for_path(expand_home(&d.text())))
            .build();
        let d = d.clone();
        fd.select_folder(Some(&w), gio::Cancellable::NONE, move |r| {
            if let Some(p) = r.ok().and_then(|f| f.path()) {
                d.set_text(&p.to_string_lossy());
            }
        });
    });
    cancel.connect_clicked(move |b| {
        if let Some(w) = root(b) {
            w.close();
        }
    });
    ok.connect_clicked(move |b| {
        let text = dest.text();
        let dir = expand_home(text.trim());
        if text.trim().is_empty() || dir.is_relative() {
            problem.set_text("Choose a destination folder (a full path).");
            problem.set_visible(true);
            return;
        }
        if dir.exists() && !dir.is_dir() {
            problem.set_text("That path is a file, not a folder.");
            problem.set_visible(true);
            return;
        }
        let opts = ExtractOptions {
            overwrite: POLICIES[policy.selected() as usize % POLICIES.len()].0,
            pkg_folder: pkg_folder.is_active(),
            keep_paths: keep_paths.is_active(),
        };
        settings.set_extract_options(&opts, open_after.is_active());
        let d = dir.to_string_lossy().into_owned();
        let mut list = settings.list("destinations");
        list.retain(|r| *r != d);
        list.insert(0, d);
        list.truncate(MAX_DESTINATIONS);
        let _ = settings.set_list("destinations", &list);
        // on_ok first: it may open the progress window, which keeps a standalone app alive.
        on_ok(Choice {
            dest: dir,
            opts,
            open_after: open_after.is_active(),
        });
        if let Some(w) = root(b) {
            w.close();
        }
    });
    win.present();
    win
}

/// Default destination for the Extract dialog: the last one used, else `fallback`.
pub(super) fn last_destination(fallback: PathBuf) -> PathBuf {
    Settings::load()
        .list("destinations")
        .into_iter()
        .next()
        .map(PathBuf::from)
        .unwrap_or(fallback)
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
    let next_to = |p: &Path| {
        p.canonicalize()
            .ok()
            .and_then(|c| c.parent().map(Path::to_path_buf))
            .unwrap_or_else(|| ".".into())
    };
    let go = {
        let (app, hold, files) = (app.clone(), hold.clone(), files.clone());
        move |dest: Option<PathBuf>, opts: ExtractOptions, show: bool| {
            let jobs: Vec<Job> = files
                .iter()
                .map(|p| Job {
                    path: p.clone(),
                    pkg: None,
                    paths: vec![],
                    dest: dest.clone().unwrap_or_else(|| next_to(p)),
                    opts,
                    rpcs3: None,
                })
                .collect();
            let show = match (show, dest.as_ref(), jobs.as_slice()) {
                (false, ..) => None,
                (true, _, [j]) => Some(output_folder(&j.dest, &j.path, &opts)),
                (true, d, _) => d.cloned(),
            };
            let (hold, failed) = (hold.clone(), failed.clone());
            start_jobs(&app, None, jobs, move |o| {
                if o.failed.is_empty() {
                    if let (Some(dir), false) = (&show, o.cancelled) {
                        show_folder(dir);
                    }
                    hold.take();
                    return;
                }
                failed.set(true);
                let hold = hold.clone();
                alert(
                    None,
                    &format!("Extraction failed\n{}", o.messages()),
                    move || {
                        hold.take();
                    },
                );
            });
        }
    };
    let settings = Settings::load();
    if !ask_dest {
        return go(None, settings.quick_options(), false);
    }
    let fallback = files.first().map(|p| next_to(p)).unwrap_or_default();
    let title = match files.len() {
        1 => format!("Extract {}", pkg_name(&files[0])),
        n => format!("Extract {n} packages"),
    };
    let chosen = Rc::new(Cell::new(false));
    let c = chosen.clone();
    let d = extract_dialog(app, None, &title, last_destination(fallback), move |ch| {
        c.set(true);
        go(Some(ch.dest), ch.opts, ch.open_after);
    });
    // Closing the dialog without choosing ends the app.
    d.connect_close_request(move |_| {
        if !chosen.get() {
            hold.take();
        }
        glib::Propagation::Proceed
    });
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
    let installing = jobs.first().is_some_and(|j| j.rpcs3.is_some());
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
        .title(match (installing, n_jobs > 1) {
            (true, true) => format!("Installing {n_jobs} packages into RPCS3"),
            (true, false) => format!("Installing {first} into RPCS3"),
            (false, true) => format!("Bulk extract ({n_jobs} packages)"),
            (false, false) => format!("Extracting {first}"),
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
        /// An existing file under the Ask policy: answer (what to do, for all later files too).
        Ask(PathBuf, mpsc::Sender<(Overwrite, bool)>),
        Done {
            ok: usize,
            entries: usize,
            failed: Vec<(usize, String)>,
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
        for (k, j) in jobs.into_iter().enumerate() {
            let name = pkg_name(&j.path);
            let pkg = match j.pkg {
                Some(p) => p,
                None => match Pkg::open(&j.path) {
                    Ok(p) => Arc::new(p),
                    Err(e) => {
                        failed.push((k, format!("{name}: {e}")));
                        continue;
                    }
                },
            };
            let paths: Vec<&str> = j.paths.iter().map(String::as_str).collect();
            let idx = pkg.select(if paths.is_empty() { &[""] } else { &paths });
            ready.push((name, pkg, idx, j.dest, j.opts, k, j.rpcs3, j.path));
        }
        let bytes = ready
            .iter()
            .flat_map(|(_, p, idx, ..)| idx.iter().map(|&i| p.entries[i].size))
            .sum();
        let entries = ready.iter().map(|r| r.2.len()).sum();
        let _ = tx.send(Msg::Total(bytes, entries, ready.len()));
        let (mut ok, mut cancelled) = (0, false);
        // "Skip all" / "Replace all" answers hold for the rest of the run, across packages.
        let mut sticky: Option<Overwrite> = None;
        let mut ask = |p: &Path| {
            if let Some(s) = sticky {
                return s;
            }
            let (rtx, rrx) = mpsc::channel();
            if tx.send(Msg::Ask(p.to_path_buf(), rtx)).is_err() {
                return Overwrite::Skip;
            }
            match rrx.recv() {
                Ok((o, all)) => {
                    if all {
                        sticky = Some(o);
                    }
                    o
                }
                Err(_) => Overwrite::Skip, // window gone: cancelled
            }
        };
        for (i, (name, pkg, idx, dest, opts, k, rpcs3, path)) in ready.iter().enumerate() {
            let _ = tx.send(Msg::Pkg(i, name.clone()));
            let progress = |p: Progress| {
                let _ = tx.send(match p {
                    Progress::File(e) => Msg::File(e.path.clone(), e.size),
                    Progress::Bytes(n) => Msg::Bytes(n),
                });
            };
            let r = match rpcs3 {
                Some((hdd0, user)) => library::install_to_rpcs3(path, hdd0, user, &c, progress)
                    .map(|_| true)
                    .or_else(|e| match e.kind() {
                        std::io::ErrorKind::Interrupted => Ok(false),
                        _ => Err(e),
                    }),
                None => pkg.extract_asking(name, idx, dest, opts, &c, progress, &mut ask),
            };
            match r {
                Ok(true) => ok += 1,
                Ok(false) => {
                    cancelled = true;
                    break;
                }
                Err(e) => failed.push((*k, format!("{name}: {e}"))),
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
    let c = cancel.clone();
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
                Msg::Ask(path, reply) => {
                    let name = path.file_name().unwrap_or_default().to_string_lossy();
                    let c = c.clone();
                    gtk::AlertDialog::builder()
                        .modal(true)
                        .message(format!("“{name}” already exists"))
                        .detail(path.display().to_string())
                        .buttons([
                            "Cancel",
                            "Skip",
                            "Skip All",
                            "Keep Both",
                            "Replace",
                            "Replace All",
                        ])
                        .cancel_button(0)
                        .default_button(4)
                        .build()
                        .choose(Some(&win), gio::Cancellable::NONE, move |r| {
                            let a = match r {
                                Ok(1) => (Overwrite::Skip, false),
                                Ok(2) => (Overwrite::Skip, true),
                                Ok(3) => (Overwrite::Rename, false),
                                Ok(4) => (Overwrite::Replace, false),
                                Ok(5) => (Overwrite::Replace, true),
                                _ => {
                                    c.store(true, Ordering::Relaxed);
                                    (Overwrite::Skip, false)
                                }
                            };
                            let _ = reply.send(a);
                        });
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
                        "Cancelled".to_string()
                    } else if installing {
                        format!(
                            "Installed {ok} of {} into RPCS3 ({})",
                            plural(n_jobs, "package", "packages"),
                            out_dir.display()
                        )
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
                    on_done(Outcome {
                        summary,
                        failed,
                        cancelled,
                    });
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
