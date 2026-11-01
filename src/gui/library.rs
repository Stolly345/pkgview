//! Library window: a folder of packages as a table (icon, title, Title ID, kind, version, size, status,
//! licence, notes) with a filter, Verify (SHA-1 of everything), Rename… (template, preview, undo), Extract…
//! and Open. The data and the audit come from `crate::library`; this file is only the window.

use super::extract::{extract_dialog, last_destination};
use super::{alert, close_on_escape, Job, Ui};
use crate::library::{self, Findings, Integrity, Licence, Record};
use crate::pkg;
use gtk::prelude::*;
use gtk::{gdk, gio, glib};
use std::cell::{Cell, RefCell};
use std::cmp::Ordering;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};
use std::sync::Arc;
use std::time::Duration;

struct Lib {
    dir: PathBuf,
    records: RefCell<Vec<Record>>,
    found: RefCell<Vec<Findings>>,
    icons: RefCell<Vec<Option<gdk::Texture>>>,
    /// SHA-1 results by file identity (device, inode, size, mtime): a rename keeps them, a changed file not.
    checked: RefCell<std::collections::HashMap<(u64, u64, u64, i64), Integrity>>,
    /// Row indices (into `records`) as BoxedAnyObject(usize)
    store: gio::ListStore,
    sel: gtk::MultiSelection,
    status: gtk::Label,
    busy: Cell<bool>,
    win: gtk::ApplicationWindow,
    ui: Rc<Ui>,
}

fn idx(o: &glib::Object) -> usize {
    *o.downcast_ref::<glib::BoxedAnyObject>()
        .unwrap()
        .borrow::<usize>()
}

fn file_name(p: &Path) -> String {
    p.file_name().map_or_else(
        || p.display().to_string(),
        |n| n.to_string_lossy().into_owned(),
    )
}

/// Table cell text for row `n`, column `col`.
fn cell(l: &Lib, n: usize, col: &str) -> String {
    let (records, found) = (l.records.borrow(), l.found.borrow());
    let (r, f) = (&records[n], &found[n]);
    let i = r.info.as_ref().ok();
    match col {
        "title" => i.map_or_else(
            || file_name(&r.path),
            |i| {
                if i.title.is_empty() {
                    i.content_id.clone()
                } else {
                    i.title.clone()
                }
            },
        ),
        "id" => i.map(|i| i.title_id.clone()).unwrap_or_default(),
        "kind" => i.map(|i| i.kind.label()).unwrap_or_default(),
        "version" => i.map(|i| i.app_ver.clone()).unwrap_or_default(),
        "size" => pkg::round_bytes(r.size),
        "status" => match (&f.integrity, &r.info) {
            (Some(Integrity::Unreadable(e)), _) => format!("Unreadable: {e}"),
            (Some(Integrity::Truncated), Err(e)) => format!("Truncated: {e}"),
            (Some(x), _) => x.label().into(),
            (None, _) => "not checked".into(),
        },
        "licence" => match &f.licence {
            Some(Licence::Found(p)) => format!("found ({})", file_name(p)),
            Some(Licence::Missing) => "MISSING".into(),
            Some(Licence::NotNeeded) => "not needed".into(),
            _ => String::new(),
        },
        "notes" => library::notes(&records, &found, n, |k| file_name(&records[k].path)).join("; "),
        "file" => file_name(&r.path),
        _ => String::new(),
    }
}

/// (id, title, expands)
const COLUMNS: [(&str, &str, bool); 9] = [
    ("title", "Title", true),
    ("id", "Title ID", false),
    ("kind", "Kind", false),
    ("version", "Version", false),
    ("size", "Size", false),
    ("status", "Status", false),
    ("licence", "Licence", false),
    ("notes", "Notes", true),
    ("file", "File", true),
];

impl Lib {
    fn selected(&self) -> Vec<usize> {
        let s = self.sel.selection();
        (0..s.size() as u32)
            .filter_map(|k| self.sel.item(s.nth(k)))
            .map(|o| idx(&o))
            .collect()
    }

    /// Selected rows, or every row shown when nothing is selected.
    fn targets(&self) -> Vec<usize> {
        let s = self.selected();
        if !s.is_empty() {
            return s;
        }
        (0..self.sel.n_items())
            .filter_map(|k| self.sel.item(k))
            .map(|o| idx(&o))
            .collect()
    }

    fn refill(&self) {
        let n = self.records.borrow().len();
        let rows: Vec<glib::BoxedAnyObject> = (0..n).map(glib::BoxedAnyObject::new).collect();
        self.store.remove_all();
        self.store.extend_from_slice(&rows);
        self.summary();
    }

    fn summary(&self) {
        let (records, found) = (self.records.borrow(), self.found.borrow());
        let bad = found
            .iter()
            .filter(|f| f.integrity.as_ref().is_some_and(Integrity::is_bad))
            .count();
        let unchecked = found.iter().filter(|f| f.integrity.is_none()).count();
        let dups = found.iter().filter(|f| f.duplicate_of.is_some()).count();
        let orphans = found.iter().filter(|f| f.orphan).count();
        let missing = found
            .iter()
            .filter(|f| f.licence == Some(Licence::Missing))
            .count();
        let size: u64 = records.iter().map(|r| r.size).sum();
        let mut s = format!("{} packages, {}", records.len(), pkg::round_bytes(size));
        for (n, what) in [
            (bad, "with problems"),
            (dups, "duplicates"),
            (orphans, "without their game"),
            (missing, "missing a licence"),
        ] {
            if n > 0 {
                s.push_str(&format!(" · {n} {what}"));
            }
        }
        if unchecked > 0 {
            s.push_str(&format!(" · SHA-1 not checked for {unchecked} (Verify)"));
        }
        self.status.set_text(&s);
    }

    /// Read the folder again (after renames, or Rescan).
    fn scan(self: &Rc<Self>) {
        if self.busy.replace(true) {
            return;
        }
        self.status
            .set_text(&format!("Reading {}…", self.dir.display()));
        let dir = self.dir.clone();
        let l = self.clone();
        glib::spawn_future_local(async move {
            let res = gio::spawn_blocking(move || {
                let files = library::collect(&[dir]);
                let records = library::scan(&files, |_| {});
                let found = library::audit(&records, &library::rpcs3_rap_dirs());
                let icons: Vec<Option<Vec<u8>>> = records
                    .iter()
                    .map(|r| {
                        let p = r.info.as_ref().ok().and(pkg::Pkg::open(&r.path).ok())?;
                        let e = p.find("ICON0.PNG")?;
                        p.read_entry(e).ok()
                    })
                    .collect();
                (records, found, icons)
            })
            .await;
            l.busy.set(false);
            let Ok((records, found, icons)) = res else {
                return l.status.set_text("Reading the folder failed");
            };
            *l.icons.borrow_mut() = icons
                .into_iter()
                .map(|b| b.and_then(|b| gdk::Texture::from_bytes(&glib::Bytes::from_owned(b)).ok()))
                .collect();
            let mut found = found;
            l.remember(&records, &mut found);
            *l.records.borrow_mut() = records;
            *l.found.borrow_mut() = found;
            l.refill();
        });
    }

    /// Fill unknown SHA-1 results from earlier checks of the same files, remember new ones, and settle
    /// duplicates against them.
    fn remember(&self, records: &[Record], found: &mut [Findings]) {
        use std::os::unix::fs::MetadataExt;
        let key = |p: &Path| {
            std::fs::metadata(p)
                .ok()
                .map(|m| (m.dev(), m.ino(), m.size(), m.mtime()))
        };
        let mut checked = self.checked.borrow_mut();
        for (r, f) in records.iter().zip(found.iter_mut()) {
            let Some(k) = key(&r.path) else { continue };
            match &f.integrity {
                None => f.integrity = checked.get(&k).cloned(),
                Some(i @ (Integrity::Ok | Integrity::Corrupt)) => {
                    checked.insert(k, i.clone());
                }
                Some(_) => {}
            }
        }
        library::settle(records, found);
    }

    /// SHA-1 of every package not yet checked, 4 at a time, with a running count.
    fn verify(self: &Rc<Self>) {
        if self.busy.replace(true) {
            return;
        }
        let records = self.records.borrow().clone();
        let mut found = self.found.borrow().clone();
        let total = found.iter().filter(|f| f.integrity.is_none()).count();
        let done = Arc::new(AtomicUsize::new(0));
        let d = done.clone();
        let status = self.status.clone();
        let tick = glib::timeout_add_local(Duration::from_millis(200), move || {
            status.set_text(&format!(
                "Verifying {}/{total}…",
                d.load(AtomicOrdering::Relaxed)
            ));
            glib::ControlFlow::Continue
        });
        let l = self.clone();
        glib::spawn_future_local(async move {
            let res = gio::spawn_blocking(move || {
                library::hash_all(&records, &mut found, |n| {
                    done.store(n, AtomicOrdering::Relaxed)
                });
                found
            })
            .await;
            tick.remove();
            l.busy.set(false);
            if let Ok(mut found) = res {
                l.remember(&l.records.borrow(), &mut found);
                *l.found.borrow_mut() = found;
            }
            l.refill();
        });
    }

    fn open_selected(&self, n: usize) {
        let p = self.records.borrow()[n].path.clone();
        self.ui.load(&p);
        self.ui.window.present();
    }

    fn extract(self: &Rc<Self>) {
        let paths: Vec<PathBuf> = {
            let records = self.records.borrow();
            self.targets()
                .into_iter()
                .filter(|&n| records[n].info.is_ok())
                .map(|n| records[n].path.clone())
                .collect()
        };
        if paths.is_empty() {
            return;
        }
        let app = self
            .win
            .application()
            .expect("library window has an application");
        let title = format!("Extract {} package(s)", paths.len());
        let ui = self.ui.clone();
        extract_dialog(
            &app,
            Some(self.win.upcast_ref()),
            &title,
            last_destination(self.dir.clone()),
            move |c| {
                let jobs = paths
                    .iter()
                    .map(|p| Job {
                        path: p.clone(),
                        pkg: None,
                        paths: vec![],
                        dest: c.dest.clone(),
                        opts: c.opts,
                    })
                    .collect();
                ui.run_jobs(jobs, c.open_after.then(|| c.dest.clone()));
            },
        );
    }

    fn undo_rename(self: &Rc<Self>) {
        let Some(log) = library::last_log(&library::log_dir()) else {
            return alert(
                Some(self.win.upcast_ref()),
                "Nothing to undo\nNo rename has been done yet.",
                || {},
            );
        };
        let msg = match library::undo(&log) {
            Ok((n, problems)) if problems.is_empty() => format!("Reversed {n} rename(s)"),
            Ok((n, problems)) => format!(
                "Reversed {n} rename(s); {} could not be:\n{}",
                problems.len(),
                problems.join("\n")
            ),
            Err(e) => format!("Undo failed\n{e}"),
        };
        alert(Some(self.win.upcast_ref()), &msg, || {});
        self.scan();
    }

    /// Rename… dialog: template, live preview of old → new for the targets, Rename.
    fn rename(self: &Rc<Self>) {
        let targets = self.targets();
        let recs: Vec<Record> = {
            let records = self.records.borrow();
            targets.iter().map(|&n| records[n].clone()).collect()
        };
        let template = gtk::Entry::builder()
            .text(
                self.ui
                    .settings
                    .get("rename_template")
                    .unwrap_or_else(|| library::DEFAULT_TEMPLATE.into()),
            )
            .hexpand(true)
            .build();
        template.update_property(&[gtk::accessible::Property::Label("Name template")]);
        let help = gtk::Label::builder()
            .label(format!(
                "Fields: {}. A […] group is left out when a field in it is empty.",
                library::TOKENS.map(|t| format!("{{{t}}}")).join(" ")
            ))
            .wrap(true)
            .xalign(0.0)
            .build();
        help.add_css_class("dim-label");
        help.add_css_class("caption");
        let preview = gtk::TextView::builder()
            .editable(false)
            .monospace(true)
            .wrap_mode(gtk::WrapMode::WordChar)
            .build();
        let scroll = gtk::ScrolledWindow::builder()
            .child(&preview)
            .min_content_height(240)
            .vexpand(true)
            .build();
        let ok = gtk::Button::with_mnemonic("_Rename");
        ok.add_css_class("suggested-action");
        let cancel = gtk::Button::with_mnemonic("_Cancel");
        let buttons = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        buttons.set_halign(gtk::Align::End);
        buttons.append(&cancel);
        buttons.append(&ok);
        let b = gtk::Box::new(gtk::Orientation::Vertical, 8);
        for m in [
            gtk::Widget::set_margin_start,
            gtk::Widget::set_margin_end,
            gtk::Widget::set_margin_top,
            gtk::Widget::set_margin_bottom,
        ] {
            m(b.upcast_ref(), 14);
        }
        let tl = gtk::Label::with_mnemonic("_Name template:");
        tl.set_xalign(0.0);
        tl.set_mnemonic_widget(Some(&template));
        b.append(&tl);
        b.append(&template);
        b.append(&help);
        b.append(&scroll);
        b.append(&buttons);
        let win = gtk::Window::builder()
            .title(format!("Rename {} package(s)", recs.len()))
            .modal(true)
            .default_width(760)
            .default_height(460)
            .child(&b)
            .transient_for(&self.win)
            .build();
        close_on_escape(&win);
        let plan: Rc<RefCell<Vec<(PathBuf, PathBuf)>>> = Rc::default();
        let update = {
            let (plan, ok, preview, recs) =
                (plan.clone(), ok.clone(), preview.clone(), recs.clone());
            move |t: &str| {
                let text = match library::plan(&recs, t) {
                    Err(e) => {
                        plan.borrow_mut().clear();
                        format!("Template problem: {e}")
                    }
                    Ok(p) if p.is_empty() => {
                        plan.borrow_mut().clear();
                        "Every name already matches the template.".into()
                    }
                    Ok(p) => {
                        let lines: Vec<String> = p
                            .iter()
                            .map(|(a, b)| format!("{}\n  → {}", file_name(a), file_name(b)))
                            .collect();
                        *plan.borrow_mut() = p;
                        lines.join("\n")
                    }
                };
                ok.set_sensitive(!plan.borrow().is_empty());
                preview.buffer().set_text(&text);
            }
        };
        update(&template.text());
        let u = update.clone();
        template.connect_changed(move |e| u(&e.text()));
        let w = win.clone();
        cancel.connect_clicked(move |_| w.close());
        let (l, w, t) = (self.clone(), win.clone(), template.clone());
        ok.connect_clicked(move |_| {
            let _ = l.ui.settings.set("rename_template", &t.text());
            let res = library::apply(&plan.borrow(), &library::log_dir());
            w.close();
            let msg = match res {
                Ok((_, n)) => format!(
                    "Renamed {n} package(s)\nUndo with “Undo Rename” in the library window."
                ),
                Err(e) => format!("Renaming stopped\n{e}"),
            };
            alert(Some(l.win.upcast_ref()), &msg, || {});
            l.scan();
        });
        win.present();
    }
}

fn column(l: &Rc<Lib>, id: &'static str, title: &str, expand: bool) -> gtk::ColumnViewColumn {
    let f = gtk::SignalListItemFactory::new();
    f.connect_setup(|_, li| {
        let li = li.downcast_ref::<gtk::ListItem>().unwrap();
        li.set_child(Some(
            &gtk::Label::builder()
                .xalign(0.0)
                .ellipsize(gtk::pango::EllipsizeMode::End)
                .build(),
        ));
    });
    let lw = Rc::downgrade(l);
    f.connect_bind(move |_, li| {
        let li = li.downcast_ref::<gtk::ListItem>().unwrap();
        let (Some(l), Some(o)) = (lw.upgrade(), li.item()) else {
            return;
        };
        let label = li.child().and_downcast::<gtk::Label>().unwrap();
        let text = cell(&l, idx(&o), id);
        label.set_tooltip_text(Some(&text).filter(|t| !t.is_empty()).map(String::as_str));
        label.set_text(&text);
        let warn = matches!(id, "status" | "licence")
            && text.chars().next().is_some_and(char::is_uppercase)
            && text != "OK";
        if warn {
            label.add_css_class("error")
        } else {
            label.remove_css_class("error")
        }
    });
    let c = gtk::ColumnViewColumn::new(Some(title), Some(f));
    c.set_resizable(true);
    c.set_expand(expand);
    let lw = Rc::downgrade(l);
    c.set_sorter(Some(&gtk::CustomSorter::new(move |a, b| {
        let Some(l) = lw.upgrade() else {
            return Ordering::Equal.into();
        };
        let (x, y) = (idx(a), idx(b));
        if id == "size" {
            let r = l.records.borrow();
            return r[x].size.cmp(&r[y].size).into();
        }
        cell(&l, x, id)
            .to_lowercase()
            .cmp(&cell(&l, y, id).to_lowercase())
            .into()
    })));
    c
}

fn icon_column(l: &Rc<Lib>) -> gtk::ColumnViewColumn {
    let f = gtk::SignalListItemFactory::new();
    f.connect_setup(|_, li| {
        let li = li.downcast_ref::<gtk::ListItem>().unwrap();
        let p = gtk::Picture::new();
        p.set_size_request(56, 32);
        p.set_content_fit(gtk::ContentFit::Contain);
        li.set_child(Some(&p));
    });
    let lw = Rc::downgrade(l);
    f.connect_bind(move |_, li| {
        let li = li.downcast_ref::<gtk::ListItem>().unwrap();
        let (Some(l), Some(o)) = (lw.upgrade(), li.item()) else {
            return;
        };
        let p = li.child().and_downcast::<gtk::Picture>().unwrap();
        p.set_paintable(l.icons.borrow().get(idx(&o)).cloned().flatten().as_ref());
    });
    gtk::ColumnViewColumn::new(None, Some(f))
}

impl Ui {
    /// File → Open Library Folder…: choose a folder, then show it as a library.
    pub(super) fn choose_library(self: &Rc<Self>) {
        let start = self
            .settings
            .get("library")
            .map(PathBuf::from)
            .filter(|p| p.is_dir())
            .unwrap_or_else(glib::home_dir);
        let d = gtk::FileDialog::builder()
            .title("Open a folder of packages")
            .modal(true)
            .initial_folder(&gio::File::for_path(start))
            .build();
        let ui = self.clone();
        d.select_folder(Some(&self.window), gio::Cancellable::NONE, move |r| {
            if let Some(p) = r.ok().and_then(|f| f.path()) {
                ui.open_library(p);
            }
        });
    }

    pub(super) fn open_library(self: &Rc<Self>, dir: PathBuf) {
        let _ = self.settings.set("library", &dir.to_string_lossy());
        let app = self
            .window
            .application()
            .expect("main window has an application");
        let store = gio::ListStore::new::<glib::BoxedAnyObject>();
        let view = gtk::ColumnView::new(None::<gtk::SelectionModel>);
        let filter = gtk::CustomFilter::new(|_| true);
        let sorted = gtk::SortListModel::new(
            Some(gtk::FilterListModel::new(
                Some(store.clone()),
                Some(filter.clone()),
            )),
            view.sorter(),
        );
        let sel = gtk::MultiSelection::new(Some(sorted));
        view.set_model(Some(&sel));
        let status = gtk::Label::builder()
            .xalign(0.0)
            .hexpand(true)
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .build();
        let win = gtk::ApplicationWindow::builder()
            .application(&app)
            .title(format!("Library — {}", dir.display()))
            .icon_name("pkgview")
            .default_width(1200)
            .default_height(640)
            .build();
        let l = Rc::new(Lib {
            dir,
            records: RefCell::default(),
            found: RefCell::default(),
            icons: RefCell::default(),
            checked: RefCell::default(),
            store,
            sel,
            status: status.clone(),
            busy: Cell::new(false),
            win: win.clone(),
            ui: self.clone(),
        });
        view.append_column(&icon_column(&l));
        for (id, title, expand) in COLUMNS {
            view.append_column(&column(&l, id, title, expand));
        }
        let lw = Rc::downgrade(&l);
        view.connect_activate(move |v, pos| {
            if let (Some(l), Some(o)) = (lw.upgrade(), v.model().and_then(|m| m.item(pos))) {
                l.open_selected(idx(&o));
            }
        });

        let search = gtk::SearchEntry::builder()
            .placeholder_text("Filter: title, Title ID, kind, file name")
            .width_chars(32)
            .build();
        let lw = Rc::downgrade(&l);
        let f = filter.clone();
        search.connect_search_changed(move |e| {
            let q = e.text().to_lowercase();
            let lw = lw.clone();
            f.set_filter_func(move |o| {
                let Some(l) = lw.upgrade() else { return true };
                let n = idx(o);
                q.is_empty()
                    || ["title", "id", "kind", "file", "status"]
                        .iter()
                        .any(|c| cell(&l, n, c).to_lowercase().contains(&q))
            });
        });
        let bar = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        for m in [
            gtk::Widget::set_margin_start,
            gtk::Widget::set_margin_end,
            gtk::Widget::set_margin_top,
            gtk::Widget::set_margin_bottom,
        ] {
            m(bar.upcast_ref(), 6);
        }
        bar.append(&search);
        let spacer = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        spacer.set_hexpand(true);
        bar.append(&spacer);
        for (label, tip, act) in [
            (
                "_Verify",
                "Check the SHA-1 of every package not checked yet",
                "verify",
            ),
            (
                "Re_name…",
                "Rename the selected packages (all shown, if none is selected) from their titles",
                "rename",
            ),
            ("_Undo Rename", "Reverse the last rename", "undo"),
            (
                "E_xtract…",
                "Extract the selected packages (all shown, if none is selected)",
                "extract",
            ),
            ("Re_scan", "Read the folder again", "rescan"),
        ] {
            let b = gtk::Button::with_mnemonic(label);
            b.set_tooltip_text(Some(tip));
            let lw = Rc::downgrade(&l);
            b.connect_clicked(move |_| {
                let Some(l) = lw.upgrade() else { return };
                match act {
                    "verify" => l.verify(),
                    "rename" => l.rename(),
                    "undo" => l.undo_rename(),
                    "extract" => l.extract(),
                    _ => l.scan(),
                }
            });
            bar.append(&b);
        }
        let status_bar = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        status_bar.set_margin_start(8);
        status_bar.set_margin_top(3);
        status_bar.set_margin_bottom(3);
        status_bar.append(&status);
        let vbox = gtk::Box::new(gtk::Orientation::Vertical, 0);
        vbox.append(&bar);
        vbox.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
        vbox.append(
            &gtk::ScrolledWindow::builder()
                .child(&view)
                .vexpand(true)
                .build(),
        );
        vbox.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
        vbox.append(&status_bar);
        win.set_child(Some(&vbox));
        close_on_escape(win.upcast_ref());
        // The window owns the library state; closures only hold weak references.
        // SAFETY: stored and read only as Rc<Lib> under this key.
        unsafe { win.set_data("lib", l.clone()) };
        win.connect_close_request(|w| {
            // SAFETY: see set_data above. Dropping it breaks the window <-> state cycle.
            drop(unsafe { w.steal_data::<Rc<Lib>>("lib") });
            glib::Propagation::Proceed
        });
        win.present();
        l.scan();
    }
}
