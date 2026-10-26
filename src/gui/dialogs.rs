//! Properties and About windows.

use super::{close_on_escape, Ui};
use crate::pkg;
use gtk::prelude::*;
use gtk::{gdk, gio, glib};
use std::rc::Rc;

impl Ui {
    pub(super) fn properties(self: &Rc<Self>) {
        let Some((pkg, _)) = self.pkg.borrow().clone() else {
            return;
        };
        let h = &pkg.header;
        let grid = gtk::Grid::builder()
            .row_spacing(4)
            .column_spacing(16)
            .build();
        let mut r = 0;
        let mut add = |cells: [&str; 3], bold: bool| {
            for (c, text) in cells.iter().enumerate() {
                let l = gtk::Label::builder()
                    .label(*text)
                    .xalign(0.0)
                    .selectable(!bold)
                    .build();
                if bold {
                    l.add_css_class("heading");
                }
                grid.attach(&l, c as i32, r, 1, 1);
            }
            r += 1;
        };
        add(["Field", "Hex", "Value"], true);
        for [f, x, v] in h.rows() {
            add([&f, &x, &v], false);
        }
        let b = gtk::Box::new(gtk::Orientation::Vertical, 10);
        b.set_margin_start(14);
        b.set_margin_end(14);
        b.set_margin_top(14);
        b.set_margin_bottom(14);

        let top = gtk::Box::new(gtk::Orientation::Horizontal, 12);
        if let Some(png) = pkg.find("ICON0.PNG").and_then(|e| pkg.read_entry(e).ok()) {
            if let Ok(tex) = gdk::Texture::from_bytes(&glib::Bytes::from_owned(png)) {
                let pic = gtk::Picture::for_paintable(&tex);
                pic.set_size_request(160, 88);
                top.append(&pic);
            }
        }
        let info = gtk::Box::new(gtk::Orientation::Vertical, 2);
        let line = |t: &str| {
            gtk::Label::builder()
                .label(t)
                .xalign(0.0)
                .selectable(true)
                .wrap(true)
                .build()
        };
        let id = line(&h.content_id);
        id.set_wrap(false);
        id.add_css_class("heading");
        info.append(&id);
        for (k, v) in pkg.sfo_summary() {
            info.append(&line(&format!("{k}: {v}")));
        }
        top.append(&info);
        b.append(&top);
        b.append(&grid);
        b.append(&line(&format!(
            "QA digest: {}",
            pkg::hex_spaced(&h.qa_digest)
        )));
        b.append(&line(&format!("SHA-1: {}", pkg::hex_spaced(&h.sha1))));

        let verify = gtk::Button::with_label("Verify SHA-1");
        let result = gtk::Label::new(None);
        let row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        row.append(&verify);
        row.append(&result);
        b.append(&row);
        verify.connect_clicked(move |btn| {
            btn.set_sensitive(false);
            result.set_text("Hashing…");
            let p = pkg.clone();
            let result = result.clone();
            let btn = btn.clone();
            glib::spawn_future_local(async move {
                let r = gio::spawn_blocking(move || p.verify_sha1()).await;
                result.set_text(match r {
                    Ok(Ok(true)) => "OK (matches)",
                    Ok(Ok(false)) => "MISMATCH",
                    _ => "Error reading file",
                });
                btn.set_sensitive(true);
            });
        });

        let win = gtk::Window::builder()
            .title("Pkg properties")
            .transient_for(&self.window)
            .modal(true)
            .resizable(false)
            .child(&b)
            .build();
        close_on_escape(&win);
        GtkWindowExt::set_focus(&win, Some(&verify)); // otherwise the first selectable label grabs focus and selects itself
        win.present();
    }

    pub(super) fn shortcuts(&self) {
        let grid = gtk::Grid::builder()
            .row_spacing(6)
            .column_spacing(24)
            .margin_start(18)
            .margin_end(18)
            .margin_top(18)
            .margin_bottom(18)
            .build();
        for (r, (keys, what)) in [
            ("Ctrl+O", "Open a package"),
            ("Ctrl+E", "Extract the selection to…"),
            ("Ctrl+Shift+E", "Bulk extract packages"),
            ("Ctrl+I", "Package properties"),
            ("Ctrl+F", "Search this package"),
            ("Esc", "Leave search / close a dialog"),
            ("Ctrl+A", "Select all"),
            ("Enter, double-click", "Open a folder or file"),
            ("Backspace, Alt+Up", "Up one folder"),
            ("Alt+Left / Alt+Right", "Back / forward"),
            ("F5", "Reload the package"),
            ("Ctrl+?", "This list"),
            ("Ctrl+Q", "Quit"),
        ]
        .into_iter()
        .enumerate()
        {
            let k = gtk::Label::builder().label(keys).xalign(0.0).build();
            k.add_css_class("heading");
            grid.attach(&k, 0, r as i32, 1, 1);
            grid.attach(
                &gtk::Label::builder().label(what).xalign(0.0).build(),
                1,
                r as i32,
                1,
                1,
            );
        }
        let win = gtk::Window::builder()
            .title("Keyboard Shortcuts")
            .transient_for(&self.window)
            .modal(true)
            .resizable(false)
            .child(&grid)
            .build();
        close_on_escape(&win);
        win.present();
    }

    pub(super) fn about(&self) {
        gtk::AboutDialog::builder()
            .transient_for(&self.window)
            .modal(true)
            .program_name("PkgView")
            .version(env!("CARGO_PKG_VERSION"))
            .logo_icon_name("pkgview")
            .comments(
                "Browse and extract PS3/PSP .pkg files.\nRust/GTK4 port of PkgView 1.3 by Ifcaro.",
            )
            .website("https://github.com/Stolly345/pkgview")
            .authors([
                "Ifcaro (original PkgView, 2010-2012)",
                "Alex Stoll (Rust port, 2026)",
            ])
            .license_type(gtk::License::Custom)
            .license(include_str!("../../LICENSE"))
            .build()
            .present();
    }
}
