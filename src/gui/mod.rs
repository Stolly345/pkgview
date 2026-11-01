//! GTK4 front end: main window (`window`), extraction jobs + progress + open/drag-out (`extract`), dialogs,
//! settings.

mod dialogs;
mod extract;
mod inspect;
mod settings;
mod window;

use crate::pkg::{Cipher, ExtractOptions, Pkg};
use gtk::prelude::*;
use gtk::{gdk, gio, glib};
use settings::Settings;
use std::cell::{Cell, RefCell};
use std::path::PathBuf;
use std::process::ExitCode;
use std::rc::Rc;
use std::sync::Arc;

const APP_ID: &str = "io.github.stolly345.pkgview";

/// One row in the right-hand file list. `size: None` = folder.
struct Row {
    name: String,
    /// Full path inside the pkg.
    path: String,
    size: Option<u64>,
    /// Content-type description ("PNG image", "Folder").
    kind: String,
    /// (offset, flags, key) when the row is a real entry.
    entry: Option<(u64, u32, &'static str)>,
}

impl Row {
    fn folder(path: String) -> Row {
        Row {
            name: path.rsplit('/').next().unwrap_or_default().into(),
            path,
            size: None,
            kind: "Folder".into(),
            entry: None,
        }
    }

    fn entry(pkg: &Pkg, i: usize) -> Row {
        let e = &pkg.entries[i];
        let key = match pkg.cipher_for(e) {
            Cipher::Debug => "Debug",
            Cipher::Ps3 => "PS3",
            Cipher::Psp => "PSP",
        };
        if e.is_dir() {
            return Row {
                entry: Some((e.offset, e.flags, "")),
                ..Row::folder(e.path.clone())
            };
        }
        let ct = content_type(e.name());
        Row {
            name: e.name().into(),
            path: e.path.clone(),
            size: Some(e.size),
            kind: gio::content_type_get_description(&ct).into(),
            entry: Some((e.offset, e.flags, key)),
        }
    }
}

/// Content type from the name alone. gio reads an empty data slice as an empty file ("empty document"),
/// so unknown extensions fall back to plain binary.
fn content_type(name: &str) -> glib::GString {
    let (ct, _) = gio::content_type_guess(Some(name), &[]);
    if ct == "application/x-zerosize" {
        "application/octet-stream".into()
    } else {
        ct
    }
}

/// What a file chooser is picking.
enum Pick {
    Open,
    ExtractTo,
    BulkFiles,
    BulkDest(Vec<PathBuf>),
}

/// One package to extract into `dest/<pkg name>/`: `pkg` already open (current window) or opened by the
/// worker; empty `paths` = all.
struct Job {
    path: PathBuf,
    pkg: Option<Arc<Pkg>>,
    paths: Vec<String>,
    dest: PathBuf,
    opts: ExtractOptions,
}

/// Back/forward list of visited folders.
#[derive(Default)]
struct History {
    stack: Vec<String>,
    pos: usize,
}

impl History {
    /// Record `folder` as current; visiting the current folder again changes nothing.
    fn visit(&mut self, folder: &str) {
        if self.stack.get(self.pos).is_some_and(|c| c == folder) {
            return;
        }
        self.stack.truncate(self.pos + 1);
        self.stack.push(folder.into());
        self.pos = self.stack.len() - 1;
    }

    fn back(&mut self) -> Option<String> {
        if self.pos == 0 {
            return None;
        }
        self.pos -= 1;
        Some(self.stack[self.pos].clone())
    }

    fn forward(&mut self) -> Option<String> {
        if self.pos + 1 >= self.stack.len() {
            return None;
        }
        self.pos += 1;
        Some(self.stack[self.pos].clone())
    }

    fn can_back(&self) -> bool {
        self.pos > 0
    }

    fn can_forward(&self) -> bool {
        self.pos + 1 < self.stack.len()
    }
}

struct Ui {
    window: gtk::ApplicationWindow,
    /// "welcome" (nothing open) or "browser".
    main_stack: gtk::Stack,
    tree: gtk::ListView,
    tree_sel: gtk::SingleSelection,
    /// The four file views (big/small icons, list, details), one visible.
    views: gtk::Stack,
    list_store: gio::ListStore,
    /// Inspector pane (F9): its scroller (shown/hidden) and the box its content goes in.
    inspector_pane: gtk::ScrolledWindow,
    inspector: gtk::Box,
    /// Bumped per refresh; a worker result for an older one is dropped.
    inspect_gen: Cell<u64>,
    /// Selection over the sorted view of `list_store`: index rows through this, not the store.
    list_sel: gtk::MultiSelection,
    path_col: gtk::ColumnViewColumn,
    crumbs: gtk::Box,
    search_bar: gtk::SearchBar,
    search_entry: gtk::SearchEntry,
    searching: Cell<bool>,
    status: gtk::Label,
    spinner: gtk::Spinner,
    /// Folder summary, shown again when the selection is cleared.
    summary: RefCell<String>,
    recent_menu: gio::Menu,
    welcome_recent: gtk::Box,
    pkg: RefCell<Option<(Arc<Pkg>, PathBuf)>>,
    folder: RefCell<String>,
    history: RefCell<History>,
    /// Context menu opened from the tree: act on the whole folder.
    from_tree: Cell<bool>,
    /// Label of the tree's root node (shared with the tree factory).
    root_label: Rc<RefCell<String>>,
    settings: Settings,
}

pub fn run(file: Option<PathBuf>) -> ExitCode {
    extract::clean_stale_open_dirs();
    let app = gtk::Application::builder()
        .application_id(APP_ID)
        .flags(gio::ApplicationFlags::NON_UNIQUE)
        .build();
    app.connect_activate(move |app| window::build_ui(app, file.clone()));
    let code = app.run_with_args::<&str>(&[]);
    // Files opened from the package stay until the next start (clean_stale_open_dirs): a viewer may still
    // be reading them when PkgView quits.
    ExitCode::from(code.value() as u8)
}

/// `--extract-here` / `--extract-to`: extraction with only a progress window, for file-manager actions.
pub fn run_extract(files: Vec<PathBuf>, ask_dest: bool) -> ExitCode {
    let failed = Rc::new(Cell::new(false));
    let app = gtk::Application::builder()
        .application_id(APP_ID)
        .flags(gio::ApplicationFlags::NON_UNIQUE)
        .build();
    let f = failed.clone();
    app.connect_activate(move |app| extract::standalone(app, files.clone(), ask_dest, f.clone()));
    app.run_with_args::<&str>(&[]);
    if failed.get() {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    }
}

/// Escape closes `w` (plain `gtk::Window`s have no such binding).
fn close_on_escape(w: &gtk::Window) {
    let c = gtk::ShortcutController::new();
    c.add_shortcut(gtk::Shortcut::new(
        Some(gtk::KeyvalTrigger::new(
            gdk::Key::Escape,
            gdk::ModifierType::empty(),
        )),
        Some(gtk::NamedAction::new("window.close")),
    ));
    w.add_controller(c);
}

/// A modal message: the first line is the heading, the rest the detail. `done` runs once it is closed.
fn alert(parent: Option<&gtk::Window>, msg: &str, done: impl FnOnce() + 'static) {
    let (head, detail) = msg.split_once('\n').unwrap_or((msg, ""));
    gtk::AlertDialog::builder()
        .modal(true)
        .message(head)
        .detail(detail)
        .build()
        .choose(parent, gio::Cancellable::NONE, move |_| done());
}

impl Ui {
    fn error(&self, msg: &str) {
        alert(Some(self.window.upcast_ref()), msg, || {});
    }

    /// Spinner + message in the status bar while a background task runs.
    fn busy(&self, msg: Option<&str>) {
        self.spinner.set_visible(msg.is_some());
        self.spinner.set_spinning(msg.is_some());
        match msg {
            Some(m) => self.status.set_text(m),
            None => self.update_status(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::History;

    #[test]
    fn history() {
        let mut h = History::default();
        assert!(!h.can_back() && !h.can_forward());
        h.visit("");
        h.visit("");
        assert!(!h.can_back());
        h.visit("A");
        h.visit("A/B");
        assert_eq!(h.back().as_deref(), Some("A"));
        h.visit("A"); // the back step's own navigation is a no-op
        assert_eq!(h.back().as_deref(), Some(""));
        assert_eq!(h.back(), None);
        assert_eq!(h.forward().as_deref(), Some("A"));
        h.visit("C"); // drops the forward entry "A/B"
        assert!(!h.can_forward());
        assert_eq!(h.back().as_deref(), Some("A"));
        assert_eq!(h.forward().as_deref(), Some("C"));
        assert_eq!(h.forward(), None);
    }
}
