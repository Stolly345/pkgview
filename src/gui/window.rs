//! Main window: menus, folder tree, file views, search, navigation, context menu, actions.

use super::extract::{extract_dialog, last_destination};
use super::settings::Settings;
use super::{content_type, History, Job, Pick, Row, Ui, APP_ID};
use crate::pkg::{self, Pkg};
use gtk::prelude::*;
use gtk::{gdk, gio, glib};
use std::cell::{Cell, RefCell};
use std::cmp::Ordering;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;

const MAX_RECENT: usize = 10;

fn join(folder: &str, name: &str) -> String {
    if folder.is_empty() {
        name.into()
    } else {
        format!("{folder}/{name}")
    }
}

fn parent(path: &str) -> &str {
    path.rsplit_once('/').map_or("", |(p, _)| p)
}

fn row_path(obj: &glib::Object) -> String {
    let row = obj.downcast_ref::<gtk::TreeListRow>().unwrap();
    row.item()
        .and_downcast::<glib::BoxedAnyObject>()
        .unwrap()
        .borrow::<String>()
        .clone()
}

fn with_row<T>(obj: &glib::Object, f: impl FnOnce(&Row) -> T) -> T {
    f(&obj
        .downcast_ref::<glib::BoxedAnyObject>()
        .unwrap()
        .borrow::<Row>())
}

fn sorter(cmp: impl Fn(&Row, &Row) -> Ordering + 'static) -> gtk::CustomSorter {
    gtk::CustomSorter::new(move |a, b| with_row(a, |a| with_row(b, |b| cmp(a, b))).into())
}

/// An optional Details column: settings id, title, shown by default, x-alignment, cell text, sort order.
type Column = (
    &'static str,
    &'static str,
    bool,
    f32,
    fn(&Row) -> String,
    fn(&Row, &Row) -> Ordering,
);

const COLUMNS: [Column; 5] = [
    (
        "size",
        "Size",
        true,
        1.0,
        |r| r.size.map(pkg::round_bytes).unwrap_or_default(),
        |a, b| a.size.cmp(&b.size),
    ),
    (
        "type",
        "Type",
        true,
        0.0,
        |r| r.kind.clone(),
        |a, b| a.kind.cmp(&b.kind),
    ),
    (
        "offset",
        "Offset",
        false,
        1.0,
        |r| r.entry.map(|e| format!("0x{:X}", e.0)).unwrap_or_default(),
        |a, b| a.entry.map(|e| e.0).cmp(&b.entry.map(|e| e.0)),
    ),
    (
        "key",
        "Key",
        false,
        0.0,
        |r| r.entry.map(|e| e.2.to_string()).unwrap_or_default(),
        |a, b| a.entry.map(|e| e.2).cmp(&b.entry.map(|e| e.2)),
    ),
    (
        "flags",
        "Flags",
        false,
        0.0,
        |r| r.entry.map(|e| format!("{:08X}", e.1)).unwrap_or_default(),
        |a, b| a.entry.map(|e| e.1).cmp(&b.entry.map(|e| e.1)),
    ),
];

pub(super) fn build_ui(app: &gtk::Application, file: Option<PathBuf>) {
    for (action, keys) in [
        ("win.open", &["<Control>o"][..]),
        ("win.extract-to", &["<Control>e"]),
        ("win.bulk-extract", &["<Control><Shift>e"]),
        ("win.properties", &["<Control>i"]),
        ("win.search", &["<Control>f"]),
        ("win.reload", &["F5"]),
        ("win.quit", &["<Control>q"]),
        ("win.back", &["<Alt>Left"]),
        ("win.forward", &["<Alt>Right"]),
        ("win.up", &["<Alt>Up"]),
        ("win.shortcuts", &["<Control>question"]),
    ] {
        app.set_accels_for_action(action, keys);
    }
    let settings = Settings::load();
    let window = gtk::ApplicationWindow::builder()
        .application(app)
        .title("PkgView")
        .icon_name("pkgview")
        .default_width(860)
        .default_height(580)
        .build();

    // Menus
    let menu = gio::Menu::new();
    let m_file = gio::Menu::new();
    m_file.append(Some("_Open…"), Some("win.open"));
    let recent_menu = gio::Menu::new();
    m_file.append_submenu(Some("Open _Recent"), &recent_menu);
    m_file.append(Some("_Bulk extract to…"), Some("win.bulk-extract"));
    m_file.append(Some("_Reload"), Some("win.reload"));
    m_file.append(Some("E_xit"), Some("win.quit"));
    let m_tools = gio::Menu::new();
    m_tools.append(Some("_Search…"), Some("win.search"));
    m_tools.append(
        Some("_Associate .pkg files with this application"),
        Some("win.associate"),
    );
    let m_help = gio::Menu::new();
    m_help.append(Some("_Keyboard Shortcuts"), Some("win.shortcuts"));
    m_help.append(Some("_About PkgView"), Some("win.about"));
    menu.append_submenu(Some("_File"), &m_file);
    menu.append_submenu(Some("_Tools"), &m_tools);
    menu.append_submenu(Some("_Help"), &m_help);
    let menubar = gtk::PopoverMenuBar::from_model(Some(&menu));

    let context = gio::Menu::new();
    let extract_section = gio::Menu::new();
    extract_section.append(Some("Extract to source folder"), Some("win.extract-here"));
    extract_section.append(Some("Extract…"), Some("win.extract-to"));
    context.append_section(None, &extract_section);
    context.append(Some("Pkg properties"), Some("win.properties"));

    // Folder tree
    let tree_sel = gtk::SingleSelection::new(None::<gio::ListModel>);
    let tree_factory = gtk::SignalListItemFactory::new();
    tree_factory.connect_setup(|_, li| {
        let li = li.downcast_ref::<gtk::ListItem>().unwrap();
        let b = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        b.append(&gtk::Image::from_icon_name("folder"));
        b.append(&gtk::Label::new(None));
        let ex = gtk::TreeExpander::new();
        ex.set_child(Some(&b));
        li.set_child(Some(&ex));
    });
    let root_label = Rc::new(RefCell::new(String::new()));
    let tr = root_label.clone();
    tree_factory.connect_bind(move |_, li| {
        let li = li.downcast_ref::<gtk::ListItem>().unwrap();
        let row = li.item().and_downcast::<gtk::TreeListRow>().unwrap();
        let ex = li.child().and_downcast::<gtk::TreeExpander>().unwrap();
        ex.set_list_row(Some(&row));
        let b = ex.child().and_downcast::<gtk::Box>().unwrap();
        let img = b.first_child().and_downcast::<gtk::Image>().unwrap();
        let lbl = b.last_child().and_downcast::<gtk::Label>().unwrap();
        let path = row_path(row.upcast_ref());
        lbl.set_text(
            if path.is_empty() {
                tr.borrow().clone()
            } else {
                path.rsplit('/').next().unwrap().to_string()
            }
            .as_str(),
        );
        let binding = row
            .bind_property("expanded", &img, "icon-name")
            .transform_to(|_, e: bool| Some(if e { "folder-open" } else { "folder" }))
            .sync_create()
            .build();
        // SAFETY: only ever stored/stolen as glib::Binding under this key.
        unsafe { li.set_data("binding", binding) };
    });
    tree_factory.connect_unbind(|_, li| {
        // SAFETY: see bind.
        if let Some(b) = unsafe { li.steal_data::<glib::Binding>("binding") } {
            b.unbind();
        }
    });
    let tree = gtk::ListView::new(Some(tree_sel.clone()), Some(tree_factory));

    // File list: one sorted model, four views (large icons, small icons, list, details)
    let list_store = gio::ListStore::new::<glib::BoxedAnyObject>();
    let sort_model = gtk::SortListModel::new(Some(list_store.clone()), None::<gtk::Sorter>);
    let list_sel = gtk::MultiSelection::new(Some(sort_model.clone()));
    let large = gtk::GridView::new(Some(list_sel.clone()), Some(item_factory(48, true)));
    large.set_max_columns(64);
    let small = gtk::GridView::new(Some(list_sel.clone()), Some(item_factory(16, false)));
    small.set_max_columns(64);
    let list = gtk::ListView::new(Some(list_sel.clone()), Some(item_factory(16, false)));
    let details = gtk::ColumnView::new(Some(list_sel.clone()));

    let header_menu = gio::Menu::new();
    let name_col = gtk::ColumnViewColumn::new(Some("Name"), Some(item_factory(16, false)));
    name_col.set_expand(true);
    name_col.set_resizable(true);
    name_col.set_sorter(Some(&sorter(|a, b| {
        a.name.to_lowercase().cmp(&b.name.to_lowercase())
    })));
    name_col.set_header_menu(Some(&header_menu));
    details.append_column(&name_col);
    let path_col = gtk::ColumnViewColumn::new(
        Some("Folder"),
        Some(text_factory(0.0, |r| format!("/{}", parent(&r.path)))),
    );
    path_col.set_resizable(true);
    path_col.set_sorter(Some(&sorter(|a, b| parent(&a.path).cmp(parent(&b.path)))));
    path_col.set_visible(false);
    details.append_column(&path_col);
    let shown = settings
        .get("columns")
        .map(|s| s.split(',').map(String::from).collect::<Vec<_>>());
    let mut optional = vec![];
    for (id, title, default, xalign, text, cmp) in COLUMNS {
        let col = gtk::ColumnViewColumn::new(Some(title), Some(text_factory(xalign, text)));
        col.set_resizable(true);
        col.set_sorter(Some(&sorter(cmp)));
        col.set_header_menu(Some(&header_menu));
        col.set_visible(
            shown
                .as_ref()
                .map_or(default, |s| s.iter().any(|c| c == id)),
        );
        header_menu.append(Some(title), Some(&format!("win.col-{id}")));
        window.add_action(&gio::PropertyAction::new(
            &format!("col-{id}"),
            &col,
            "visible",
        ));
        details.append_column(&col);
        optional.push((id, col));
    }
    let optional = Rc::new(optional);
    for (_, col) in optional.iter() {
        let (s, all) = (settings.clone(), optional.clone());
        col.connect_visible_notify(move |_| {
            let ids: Vec<&str> = all
                .iter()
                .filter(|c| c.1.is_visible())
                .map(|c| c.0)
                .collect();
            let _ = s.set("columns", &ids.join(","));
        });
    }
    // Folders first, then whatever column header was clicked (if any).
    let folders_first = sorter(|a, b| b.size.is_none().cmp(&a.size.is_none()));
    let sorters = gtk::MultiSorter::new();
    sorters.append(folders_first);
    if let Some(s) = details.sorter() {
        sorters.append(s);
    }
    sort_model.set_sorter(Some(&sorters));

    let views = gtk::Stack::new();
    for (name, w) in [
        ("large", large.clone().upcast::<gtk::Widget>()),
        ("small", small.clone().upcast()),
        ("list", list.clone().upcast()),
        ("details", details.clone().upcast()),
    ] {
        views.add_named(
            &gtk::ScrolledWindow::builder().child(&w).build(),
            Some(name),
        );
    }
    let saved = settings.get("view").unwrap_or_default();
    views.set_visible_child_name(if views.child_by_name(&saved).is_some() {
        &saved
    } else {
        "details"
    });

    // Toolbar: open | back forward up | path bar … view modes
    let toolbar = gtk::Box::new(gtk::Orientation::Horizontal, 4);
    toolbar.set_margin_start(4);
    toolbar.set_margin_end(4);
    toolbar.set_margin_top(4);
    toolbar.set_margin_bottom(4);
    for (icon, tip, action) in [
        ("document-open-symbolic", "Open (Ctrl+O)", "win.open"),
        ("go-previous-symbolic", "Back (Alt+Left)", "win.back"),
        ("go-next-symbolic", "Forward (Alt+Right)", "win.forward"),
        ("go-up-symbolic", "Up (Alt+Up)", "win.up"),
    ] {
        let b = gtk::Button::from_icon_name(icon);
        b.set_tooltip_text(Some(tip));
        b.set_action_name(Some(action));
        toolbar.append(&b);
        if icon == "document-open-symbolic" {
            toolbar.append(&gtk::Separator::new(gtk::Orientation::Vertical));
        }
    }
    let crumbs = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    let crumb_scroll = gtk::ScrolledWindow::builder()
        .child(&crumbs)
        .vscrollbar_policy(gtk::PolicyType::Never)
        .hscrollbar_policy(gtk::PolicyType::External)
        .hexpand(true)
        .build();
    toolbar.append(&crumb_scroll);
    let mut group: Option<gtk::ToggleButton> = None;
    for (name, label) in [
        ("large", "Big icons"),
        ("small", "Small icons"),
        ("list", "List"),
        ("details", "Details"),
    ] {
        let b = gtk::ToggleButton::with_label(label);
        b.set_group(group.as_ref());
        b.set_active(views.visible_child_name().as_deref() == Some(name));
        let st = views.clone();
        let s = settings.clone();
        b.connect_toggled(move |b| {
            if b.is_active() {
                st.set_visible_child_name(name);
                let _ = s.set("view", name);
            }
        });
        toolbar.append(&b);
        group = Some(b);
    }

    let search_entry = gtk::SearchEntry::builder()
        .placeholder_text("Search this package: name or path, * and ? wildcards")
        .hexpand(true)
        .build();
    let search_bar = gtk::SearchBar::builder()
        .child(&search_entry)
        .show_close_button(true)
        .build();
    search_bar.connect_entry(&search_entry);

    let status = gtk::Label::new(None);
    status.set_xalign(0.0);
    status.set_hexpand(true);
    status.set_ellipsize(gtk::pango::EllipsizeMode::End);
    let spinner = gtk::Spinner::new();
    spinner.set_visible(false);
    let status_bar = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    status_bar.set_margin_start(6);
    status_bar.set_margin_top(2);
    status_bar.set_margin_bottom(2);
    status_bar.append(&spinner);
    status_bar.append(&status);

    let paned = gtk::Paned::new(gtk::Orientation::Horizontal);
    paned.set_start_child(Some(&gtk::ScrolledWindow::builder().child(&tree).build()));
    paned.set_end_child(Some(&views));
    paned.set_position(240);

    // Welcome page (nothing open): big hint + recent files.
    let welcome = gtk::Box::new(gtk::Orientation::Vertical, 12);
    welcome.set_valign(gtk::Align::Center);
    welcome.set_halign(gtk::Align::Center);
    let icon = gtk::Image::from_icon_name("pkgview");
    icon.set_pixel_size(96);
    welcome.append(&icon);
    let hint = gtk::Label::new(Some("Open a .pkg or drop it here"));
    hint.add_css_class("title-2");
    welcome.append(&hint);
    let open_btn = gtk::Button::with_mnemonic("_Open…");
    open_btn.set_action_name(Some("win.open"));
    open_btn.set_halign(gtk::Align::Center);
    open_btn.add_css_class("suggested-action");
    welcome.append(&open_btn);
    let welcome_recent = gtk::Box::new(gtk::Orientation::Vertical, 2);
    welcome.append(&welcome_recent);

    let main_stack = gtk::Stack::new();
    main_stack.set_vexpand(true);
    main_stack.add_named(&welcome, Some("welcome"));
    main_stack.add_named(&paned, Some("browser"));
    main_stack.set_visible_child_name("welcome");

    let vbox = gtk::Box::new(gtk::Orientation::Vertical, 0);
    vbox.append(&menubar);
    vbox.append(&toolbar);
    vbox.append(&search_bar);
    vbox.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
    vbox.append(&main_stack);
    vbox.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
    vbox.append(&status_bar);
    window.set_child(Some(&vbox));

    let ui = Rc::new(Ui {
        window: window.clone(),
        views: views.clone(),
        main_stack,
        tree: tree.clone(),
        tree_sel: tree_sel.clone(),
        list_store,
        list_sel: list_sel.clone(),
        path_col,
        crumbs,
        search_bar,
        search_entry: search_entry.clone(),
        searching: Cell::new(false),
        status,
        spinner,
        summary: RefCell::new(String::new()),
        recent_menu,
        welcome_recent,
        pkg: RefCell::new(None),
        folder: RefCell::new(String::new()),
        history: RefCell::new(History::default()),
        from_tree: Cell::new(false),
        root_label,
        settings,
    });
    ui.refresh_recent();

    // Tree selection drives the file list.
    let u = ui.clone();
    tree_sel.connect_selection_changed(move |sel, _, _| {
        if let Some(item) = sel.selected_item() {
            u.end_search();
            u.show_folder(&row_path(&item));
        }
    });
    let u = ui.clone();
    list_sel.connect_selection_changed(move |_, _, _| u.update_status());

    // Search
    let u = ui.clone();
    search_entry.connect_search_changed(move |e| {
        let q = e.text();
        if q.is_empty() {
            u.end_search();
        } else {
            u.show_results(&q);
        }
    });
    let u = ui.clone();
    search_entry.connect_stop_search(move |_| u.end_search());
    let u = ui.clone();
    ui.search_bar.connect_search_mode_enabled_notify(move |b| {
        if !b.is_search_mode() {
            u.end_search();
        }
    });

    // Enter / double-click: open a folder or file.
    for v in [
        large.clone().upcast::<gtk::Widget>(),
        small.clone().upcast(),
        list.clone().upcast(),
        details.clone().upcast(),
    ] {
        attach_drag(&ui, &v);
    }
    let activate = {
        let u = ui.clone();
        move |pos: u32| u.activate(pos)
    };
    let a = activate.clone();
    large.connect_activate(move |_, p| a(p));
    let a = activate.clone();
    small.connect_activate(move |_, p| a(p));
    let a = activate.clone();
    list.connect_activate(move |_, p| a(p));
    details.connect_activate(move |_, p| activate(p));

    // Enter acts on the selected row when there is exactly one. GTK's own Enter acts on the keyboard
    // cursor, which stays on row 0 when the selection is set from code (jump from a search result).
    let enter = gtk::EventControllerKey::new();
    enter.set_propagation_phase(gtk::PropagationPhase::Capture);
    let u = ui.clone();
    enter.connect_key_pressed(move |_, key, _, mods| {
        let sel = u.list_sel.selection();
        if !matches!(key, gdk::Key::Return | gdk::Key::KP_Enter)
            || !mods.is_empty()
            || sel.size() != 1
        {
            return glib::Propagation::Proceed;
        }
        u.activate(sel.nth(0));
        glib::Propagation::Stop
    });
    ui.views.add_controller(enter);

    // Backspace = up, only while the file area has focus (it must keep working in the search entry).
    let keys = gtk::ShortcutController::new();
    keys.add_shortcut(gtk::Shortcut::new(
        Some(gtk::KeyvalTrigger::new(
            gdk::Key::BackSpace,
            gdk::ModifierType::empty(),
        )),
        Some(gtk::NamedAction::new("win.up")),
    ));
    paned.add_controller(keys);

    // Context menus
    attach_context(&ui, tree.upcast_ref(), &context, true);
    attach_context(&ui, views.upcast_ref(), &context, false);

    // Drop one .pkg to open it; several, or a folder, to bulk-extract them.
    let drop = gtk::DropTarget::new(gdk::FileList::static_type(), gdk::DragAction::COPY);
    let u = ui.clone();
    drop.connect_drop(move |t, value, _, _| {
        if t.current_drop().and_then(|d| d.drag()).is_some() {
            return false; // our own drag-out
        }
        let Ok(list) = value.get::<gdk::FileList>() else {
            return false;
        };
        let paths: Vec<PathBuf> = list.files().iter().filter_map(|f| f.path()).collect();
        match paths.as_slice() {
            [] => false,
            [p] if !p.is_dir() => {
                u.load(p);
                true
            }
            _ => {
                let pkgs: Vec<PathBuf> = paths
                    .iter()
                    .flat_map(|p| {
                        if p.is_dir() {
                            pkg::find_pkgs(p)
                        } else {
                            vec![p.clone()]
                        }
                    })
                    .collect();
                if pkgs.is_empty() {
                    u.status.set_text("No .pkg files in what was dropped");
                } else {
                    u.choose(Pick::BulkDest(pkgs));
                }
                true
            }
        }
    });
    window.add_controller(drop);

    add_actions(&ui);
    ui.update_nav();
    if let Some(f) = file {
        ui.load(&f);
    }
    window.present();
}

impl Ui {
    fn load(self: &Rc<Self>, path: &Path) {
        let pkg = match Pkg::open(path) {
            Ok(p) => Arc::new(p),
            Err(e) => return self.error(&format!("{}:\n{e}", path.display())),
        };
        self.end_search();
        *self.root_label.borrow_mut() = pkg.header.root_folder();
        let roots = gio::ListStore::new::<glib::BoxedAnyObject>();
        roots.append(&glib::BoxedAnyObject::new(String::new()));
        let p = pkg.clone();
        let model = gtk::TreeListModel::new(roots, false, true, move |item| {
            let path = item
                .downcast_ref::<glib::BoxedAnyObject>()?
                .borrow::<String>()
                .clone();
            let (dirs, _) = p.children(&path);
            if dirs.is_empty() {
                return None;
            }
            let store = gio::ListStore::new::<glib::BoxedAnyObject>();
            for d in dirs {
                store.append(&glib::BoxedAnyObject::new(join(&path, &d)));
            }
            Some(store.upcast())
        });
        let title = pkg.header.content_id.clone();
        *self.pkg.borrow_mut() = Some((pkg, path.to_path_buf()));
        *self.history.borrow_mut() = History::default();
        self.folder.borrow_mut().clear();
        self.tree_sel.set_model(Some(&model));
        self.tree_sel.set_selected(0);
        self.show_folder("");
        self.main_stack.set_visible_child_name("browser");
        self.window.set_title(Some(&format!("PkgView — {title}")));
        for a in [
            "extract-here",
            "extract-to",
            "properties",
            "search",
            "reload",
        ] {
            self.action(a).set_enabled(true);
        }
        self.add_recent(path);
    }

    fn action(&self, name: &str) -> gio::SimpleAction {
        self.window
            .lookup_action(name)
            .and_downcast::<gio::SimpleAction>()
            .unwrap()
    }

    fn show_folder(self: &Rc<Self>, folder: &str) {
        *self.folder.borrow_mut() = folder.to_string();
        self.history.borrow_mut().visit(folder);
        self.update_nav();
        self.list_store.remove_all();
        let Some((pkg, _)) = self.pkg.borrow().clone() else {
            return;
        };
        let (dirs, files) = pkg.children(folder);
        let rows: Vec<glib::BoxedAnyObject> = dirs
            .iter()
            .map(|d| {
                let path = join(folder, d);
                let row = match pkg.entries.iter().position(|e| e.path == path) {
                    Some(i) => Row::entry(&pkg, i),
                    None => Row::folder(path), // implied by a file path, no entry of its own
                };
                glib::BoxedAnyObject::new(row)
            })
            .chain(
                files
                    .iter()
                    .map(|&i| glib::BoxedAnyObject::new(Row::entry(&pkg, i))),
            )
            .collect();
        self.list_store.extend_from_slice(&rows);
        let plural = |n: usize, w: &str| format!("{n} {w}{}", if n == 1 { "" } else { "s" });
        let mut s = vec![];
        if !files.is_empty() {
            s.push(plural(files.len(), "file"));
        }
        if !dirs.is_empty() {
            s.push(plural(dirs.len(), "folder"));
        }
        *self.summary.borrow_mut() = s.join(" and ");
        self.update_status();
        self.update_crumbs(folder);
    }

    /// Path bar: one flat button per folder level, the current one in bold.
    fn update_crumbs(self: &Rc<Self>, folder: &str) {
        while let Some(c) = self.crumbs.first_child() {
            self.crumbs.remove(&c);
        }
        let mut levels = vec![(String::new(), self.root_label.borrow().clone())];
        let mut acc = String::new();
        for part in folder.split('/').filter(|p| !p.is_empty()) {
            acc = join(&acc, part);
            levels.push((acc.clone(), part.to_string()));
        }
        let last = levels.len() - 1;
        for (i, (path, label)) in levels.into_iter().enumerate() {
            if i > 0 {
                self.crumbs.append(&gtk::Label::new(Some("›")));
            }
            let b = gtk::Button::with_label(&label);
            b.add_css_class("flat");
            if i == last {
                b.child()
                    .and_downcast::<gtk::Label>()
                    .unwrap()
                    .add_css_class("heading");
            }
            let u = self.clone();
            b.connect_clicked(move |_| u.select_folder(&path));
            self.crumbs.append(&b);
        }
    }

    fn update_nav(&self) {
        let h = self.history.borrow();
        let open = self.pkg.borrow().is_some();
        self.action("back").set_enabled(open && h.can_back());
        self.action("forward").set_enabled(open && h.can_forward());
        self.action("up")
            .set_enabled(open && !self.folder.borrow().is_empty());
    }

    /// "3 items selected (12.4 MB)" while something is selected, else the folder / search summary.
    pub(super) fn update_status(&self) {
        let paths = self.selected_paths();
        let Some((pkg, _)) = self.pkg.borrow().clone() else {
            return;
        };
        if paths.is_empty() {
            return self.status.set_text(&self.summary.borrow());
        }
        let refs: Vec<&str> = paths.iter().map(String::as_str).collect();
        let bytes: u64 = pkg.select(&refs).iter().map(|&i| pkg.entries[i].size).sum();
        let n = paths.len();
        self.status.set_text(&format!(
            "{n} item{} selected ({})",
            if n == 1 { "" } else { "s" },
            pkg::round_bytes(bytes)
        ));
    }

    /// Flat list of search results; the Folder column shows where each one lives.
    fn show_results(&self, query: &str) {
        let Some((pkg, _)) = self.pkg.borrow().clone() else {
            return;
        };
        self.searching.set(true);
        self.path_col.set_visible(true);
        let hits = pkg.search(query);
        let rows: Vec<glib::BoxedAnyObject> = hits
            .iter()
            .map(|&i| glib::BoxedAnyObject::new(Row::entry(&pkg, i)))
            .collect();
        self.list_store.remove_all();
        self.list_store.extend_from_slice(&rows);
        *self.summary.borrow_mut() = format!(
            "{} result{} for “{query}” (Esc to go back)",
            hits.len(),
            if hits.len() == 1 { "" } else { "s" }
        );
        self.update_status();
    }

    /// Leave search mode and show the current folder again.
    fn end_search(self: &Rc<Self>) {
        if !self.searching.replace(false) {
            return;
        }
        self.path_col.set_visible(false);
        self.search_entry.set_text("");
        self.search_bar.set_search_mode(false);
        let f = self.folder.borrow().clone();
        self.show_folder(&f);
    }

    fn activate(self: &Rc<Self>, pos: u32) {
        let Some(obj) = self.list_sel.item(pos) else {
            return;
        };
        let (path, is_dir) = with_row(&obj, |r| (r.path.clone(), r.size.is_none()));
        if !self.searching.get() {
            return if is_dir {
                self.select_folder(&path)
            } else {
                self.open_file(&path)
            };
        }
        // Search result: go to it. A folder opens; a file is shown selected in its folder.
        self.end_search();
        if is_dir {
            return self.select_folder(&path);
        }
        self.select_folder(parent(&path));
        let n = self.list_sel.n_items();
        if let Some(i) =
            (0..n).find(|&i| with_row(&self.list_sel.item(i).unwrap(), |r| r.path == path))
        {
            self.list_sel.select_item(i, true);
            self.scroll_to(i);
        }
    }

    /// Scroll the visible file view to row `pos`, focus it and select only it.
    fn scroll_to(&self, pos: u32) {
        let Some(v) = self
            .views
            .visible_child()
            .and_downcast::<gtk::ScrolledWindow>()
            .and_then(|s| s.child())
        else {
            return;
        };
        let f = gtk::ListScrollFlags::FOCUS | gtk::ListScrollFlags::SELECT;
        if let Some(l) = v.downcast_ref::<gtk::ListView>() {
            l.scroll_to(pos, f, None);
        } else if let Some(g) = v.downcast_ref::<gtk::GridView>() {
            g.scroll_to(pos, f, None);
        } else if let Some(c) = v.downcast_ref::<gtk::ColumnView>() {
            c.scroll_to(pos, None::<&gtk::ColumnViewColumn>, f, None);
        }
    }

    /// Select `path` in the tree, expanding collapsed ancestors on the way.
    fn select_folder(&self, path: &str) {
        let Some(model) = self.tree_sel.model() else {
            return;
        };
        let mut i = 0;
        while i < model.n_items() {
            let item = model.item(i).unwrap();
            let p = row_path(&item);
            if p == path {
                self.tree_sel.set_selected(i);
                self.tree.scroll_to(i, gtk::ListScrollFlags::NONE, None);
                return;
            }
            if p.is_empty() || path.starts_with(&format!("{p}/")) {
                item.downcast_ref::<gtk::TreeListRow>()
                    .unwrap()
                    .set_expanded(true);
            }
            i += 1;
        }
    }

    /// Paths of the rows selected in the file list.
    pub(super) fn selected_paths(&self) -> Vec<String> {
        let sel = self.list_sel.selection();
        (0..sel.size() as u32)
            .filter_map(|i| self.list_sel.item(sel.nth(i)))
            .map(|o| with_row(&o, |r| r.path.clone()))
            .collect()
    }

    /// Paths to extract: list selection, or the whole current folder (tree menu / nothing selected).
    pub(super) fn selection(&self) -> Vec<String> {
        let sel = self.selected_paths();
        if self.from_tree.get() || sel.is_empty() {
            return vec![self.folder.borrow().clone()];
        }
        sel
    }

    fn add_recent(self: &Rc<Self>, path: &Path) {
        let p = path
            .canonicalize()
            .unwrap_or_else(|_| path.to_path_buf())
            .to_string_lossy()
            .into_owned();
        let mut list = self.settings.list("recent");
        list.retain(|r| *r != p);
        list.insert(0, p);
        list.truncate(MAX_RECENT);
        let _ = self.settings.set_list("recent", &list);
        self.refresh_recent();
    }

    /// Rebuild File → Open Recent and the welcome page list; files that are gone are dropped.
    fn refresh_recent(self: &Rc<Self>) {
        let mut list = self.settings.list("recent");
        let before = list.len();
        list.retain(|p| Path::new(p).is_file());
        if list.len() != before {
            let _ = self.settings.set_list("recent", &list);
        }
        self.recent_menu.remove_all();
        while let Some(c) = self.welcome_recent.first_child() {
            self.welcome_recent.remove(&c);
        }
        if !list.is_empty() {
            let l = gtk::Label::new(Some("Recent"));
            l.add_css_class("dim-label");
            l.set_margin_top(12);
            self.welcome_recent.append(&l);
        }
        for p in list {
            let name = Path::new(&p)
                .file_name()
                .map_or(p.clone(), |n| n.to_string_lossy().into_owned());
            let item = gio::MenuItem::new(Some(&name.replace('_', "__")), None);
            item.set_action_and_target_value(Some("win.open-recent"), Some(&p.to_variant()));
            self.recent_menu.append_item(&item);
            let b = gtk::Button::with_label(&name);
            b.add_css_class("flat");
            b.set_tooltip_text(Some(&p));
            let u = self.clone();
            b.connect_clicked(move |_| u.load(Path::new(&p)));
            self.welcome_recent.append(&b);
        }
    }

    fn choose(self: &Rc<Self>, pick: Pick) {
        let app = self
            .window
            .application()
            .expect("main window has an application");
        let pkgs = match pick {
            Pick::ExtractTo => {
                let Some((_, path)) = self.pkg.borrow().clone() else {
                    return;
                };
                let next_to = path
                    .canonicalize()
                    .ok()
                    .and_then(|c| c.parent().map(Path::to_path_buf))
                    .unwrap_or_else(glib::home_dir);
                let ui = self.clone();
                let title = format!("Extract from {}", pkg::pkg_name(&path));
                extract_dialog(
                    &app,
                    Some(self.window.upcast_ref()),
                    &title,
                    last_destination(next_to),
                    move |c| ui.extract(c.dest, c.opts, c.open_after),
                );
                return;
            }
            Pick::BulkDest(pkgs) => pkgs,
            Pick::Open | Pick::BulkFiles => {
                let many = matches!(pick, Pick::BulkFiles);
                let f = gtk::FileFilter::new();
                f.set_name(Some("PS3/PSP packages (*.pkg)"));
                f.add_suffix("pkg");
                let filters = gio::ListStore::new::<gtk::FileFilter>();
                filters.append(&f);
                let d = gtk::FileDialog::builder()
                    .title(if many {
                        "Bulk extract: choose .pkg files"
                    } else {
                        "Open PKG"
                    })
                    .modal(true)
                    .filters(&filters)
                    .default_filter(&f)
                    .build();
                let ui = self.clone();
                if many {
                    d.open_multiple(Some(&self.window), gio::Cancellable::NONE, move |r| {
                        let Ok(model) = r else { return };
                        let picked: Vec<PathBuf> = (0..model.n_items())
                            .filter_map(|i| model.item(i).and_downcast::<gio::File>()?.path())
                            .collect();
                        if !picked.is_empty() {
                            ui.choose(Pick::BulkDest(picked));
                        }
                    });
                } else {
                    d.open(Some(&self.window), gio::Cancellable::NONE, move |r| {
                        if let Some(p) = r.ok().and_then(|f| f.path()) {
                            ui.load(&p);
                        }
                    });
                }
                return;
            }
        };
        let ui = self.clone();
        let fallback = pkgs
            .first()
            .and_then(|p| p.canonicalize().ok()?.parent().map(Path::to_path_buf))
            .unwrap_or_else(glib::home_dir);
        let title = match pkgs.len() {
            1 => format!("Extract {}", pkg::pkg_name(&pkgs[0])),
            n => format!("Bulk extract {n} packages"),
        };
        extract_dialog(
            &app,
            Some(self.window.upcast_ref()),
            &title,
            last_destination(fallback),
            move |c| {
                let jobs = pkgs
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
}

fn item_factory(icon_px: i32, vertical: bool) -> gtk::SignalListItemFactory {
    let f = gtk::SignalListItemFactory::new();
    f.connect_setup(move |_, li| {
        let li = li.downcast_ref::<gtk::ListItem>().unwrap();
        let orient = if vertical {
            gtk::Orientation::Vertical
        } else {
            gtk::Orientation::Horizontal
        };
        let b = gtk::Box::new(orient, 6);
        let img = gtk::Image::new();
        img.set_pixel_size(icon_px);
        let lbl = gtk::Label::new(None);
        if vertical {
            lbl.set_wrap(true);
            lbl.set_wrap_mode(gtk::pango::WrapMode::WordChar);
            lbl.set_justify(gtk::Justification::Center);
            lbl.set_max_width_chars(14);
            b.set_size_request(96, -1);
        } else {
            lbl.set_xalign(0.0);
            lbl.set_ellipsize(gtk::pango::EllipsizeMode::End);
        }
        b.append(&img);
        b.append(&lbl);
        li.set_child(Some(&b));
    });
    f.connect_bind(|_, li| {
        let li = li.downcast_ref::<gtk::ListItem>().unwrap();
        let obj = li.item().and_downcast::<glib::BoxedAnyObject>().unwrap();
        let row = obj.borrow::<Row>();
        let b = li.child().and_downcast::<gtk::Box>().unwrap();
        let img = b.first_child().and_downcast::<gtk::Image>().unwrap();
        b.last_child()
            .and_downcast::<gtk::Label>()
            .unwrap()
            .set_text(&row.name);
        match row.size {
            None => img.set_icon_name(Some("folder")),
            Some(_) => {
                img.set_from_gicon(&gio::content_type_get_icon(&content_type(&row.name)));
            }
        }
    });
    f
}

/// A Details column that shows `text(row)` in a label.
fn text_factory(xalign: f32, text: fn(&Row) -> String) -> gtk::SignalListItemFactory {
    let f = gtk::SignalListItemFactory::new();
    f.connect_setup(move |_, li| {
        let li = li.downcast_ref::<gtk::ListItem>().unwrap();
        li.set_child(Some(
            &gtk::Label::builder()
                .xalign(xalign)
                .ellipsize(gtk::pango::EllipsizeMode::End)
                .build(),
        ));
    });
    f.connect_bind(move |_, li| {
        let li = li.downcast_ref::<gtk::ListItem>().unwrap();
        let obj = li.item().unwrap();
        li.child()
            .and_downcast::<gtk::Label>()
            .unwrap()
            .set_text(&with_row(&obj, text));
    });
    f
}

fn attach_drag(ui: &Rc<Ui>, view: &gtk::Widget) {
    let src = gtk::DragSource::new();
    src.set_actions(gdk::DragAction::COPY);
    let u = ui.clone();
    src.connect_prepare(move |_, _, _| u.drag_files());
    view.add_controller(src);
}

/// Right-click menu. In the tree, the folder under the pointer is selected first, so the menu acts on it.
fn attach_context(ui: &Rc<Ui>, w: &gtk::Widget, menu: &gio::Menu, from_tree: bool) {
    let pop = gtk::PopoverMenu::from_model(Some(menu));
    pop.set_parent(w);
    pop.set_has_arrow(false);
    let click = gtk::GestureClick::builder().button(3).build();
    let u = ui.clone();
    let target = w.clone();
    click.connect_pressed(move |_, _, x, y| {
        if u.pkg.borrow().is_none() {
            return;
        }
        // The Details header has its own (column toggle) menu.
        let mut hit = target.pick(x, y, gtk::PickFlags::DEFAULT);
        while let Some(h) = hit {
            if h.css_name() == "header" {
                return;
            }
            if let Some(row) = h
                .downcast_ref::<gtk::TreeExpander>()
                .and_then(|e| e.list_row())
            {
                u.tree_sel.set_selected(row.position());
            }
            hit = h.parent();
        }
        u.from_tree.set(from_tree);
        pop.set_pointing_to(Some(&gdk::Rectangle::new(x as i32, y as i32, 1, 1)));
        pop.popup();
    });
    w.add_controller(click);
}

fn add_actions(ui: &Rc<Ui>) {
    let add = |name: &str, enabled: bool, f: Box<dyn Fn()>| {
        let a = gio::SimpleAction::new(name, None);
        a.set_enabled(enabled);
        a.connect_activate(move |_, _| f());
        ui.window.add_action(&a);
    };
    let u = ui.clone();
    add("open", true, Box::new(move || u.choose(Pick::Open)));
    let u = ui.clone();
    add(
        "bulk-extract",
        true,
        Box::new(move || u.choose(Pick::BulkFiles)),
    );
    let u = ui.clone();
    add("quit", true, Box::new(move || u.window.close()));
    let u = ui.clone();
    add(
        "extract-here",
        false,
        Box::new(move || {
            let dir = u
                .pkg
                .borrow()
                .as_ref()
                .and_then(|(_, p)| p.canonicalize().ok()?.parent().map(Path::to_path_buf));
            if let Some(d) = dir {
                u.extract(d, u.settings.quick_options(), false);
            }
        }),
    );
    let u = ui.clone();
    add(
        "extract-to",
        false,
        Box::new(move || u.choose(Pick::ExtractTo)),
    );
    let u = ui.clone();
    add("properties", false, Box::new(move || u.properties()));
    let u = ui.clone();
    add(
        "search",
        false,
        Box::new(move || {
            u.search_bar.set_search_mode(true);
            u.search_entry.grab_focus();
        }),
    );
    let u = ui.clone();
    add(
        "reload",
        false,
        Box::new(move || {
            let Some((_, path)) = u.pkg.borrow().clone() else {
                return;
            };
            let folder = u.folder.borrow().clone();
            u.load(&path);
            u.select_folder(&folder);
        }),
    );
    let u = ui.clone();
    add(
        "back",
        false,
        Box::new(move || {
            let p = u.history.borrow_mut().back();
            if let Some(p) = p {
                u.end_search();
                u.select_folder(&p);
            }
        }),
    );
    let u = ui.clone();
    add(
        "forward",
        false,
        Box::new(move || {
            let p = u.history.borrow_mut().forward();
            if let Some(p) = p {
                u.end_search();
                u.select_folder(&p);
            }
        }),
    );
    let u = ui.clone();
    add(
        "up",
        false,
        Box::new(move || {
            u.end_search();
            let f = u.folder.borrow().clone();
            if !f.is_empty() {
                u.select_folder(parent(&f));
            }
        }),
    );
    let u = ui.clone();
    add(
        "associate",
        true,
        Box::new(move || {
            let r = std::process::Command::new("xdg-mime")
                .args([
                    "default",
                    &format!("{APP_ID}.desktop"),
                    "application/x-ps3-pkg",
                ])
                .status();
            u.status.set_text(match r {
                Ok(s) if s.success() => ".pkg files now open with PkgView",
                _ => "xdg-mime failed (is xdg-utils installed?)",
            });
        }),
    );
    let u = ui.clone();
    add("shortcuts", true, Box::new(move || u.shortcuts()));
    let u = ui.clone();
    add("about", true, Box::new(move || u.about()));

    let recent = gio::SimpleAction::new("open-recent", Some(glib::VariantTy::STRING));
    let u = ui.clone();
    recent.connect_activate(move |_, v| {
        if let Some(p) = v.and_then(|v| v.get::<String>()) {
            u.load(Path::new(&p));
        }
    });
    ui.window.add_action(&recent);
}
