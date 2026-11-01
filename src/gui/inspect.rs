//! Inspector pane (F9): a preview of the one selected file (image, PARAM.SFO table, text or hex), or an
//! overview of the package (icon, PARAM.SFO, package metadata) when nothing or several things are selected.
//! Reads run on a worker thread; a generation counter drops results the selection has moved past.

use super::{content_type, Ui};
use crate::pkg::{self, Cipher, Pkg};
use gtk::prelude::*;
use gtk::{gdk, gio, glib};
use std::rc::Rc;
use std::sync::Arc;

/// Text shown at most; hex shown at most.
const TEXT_MAX: u64 = 64 << 10;
const HEX_MAX: u64 = 4 << 10;

/// `b` as text if it looks like text: valid UTF-8 (a character cut at the end is fine), no NULs, and
/// almost no control characters.
pub(super) fn as_text(b: &[u8]) -> Option<&str> {
    let s = match std::str::from_utf8(b) {
        Ok(s) => s,
        Err(e) if e.error_len().is_none() && e.valid_up_to() + 4 > b.len() => {
            std::str::from_utf8(&b[..e.valid_up_to()]).ok()?
        }
        Err(_) => return None,
    };
    let ctrl = s
        .chars()
        .filter(|c| c.is_control() && !matches!(c, '\n' | '\r' | '\t'))
        .count();
    (!s.contains('\0') && ctrl * 100 <= s.len().max(1)).then_some(s)
}

/// Hex dump sized for the narrow pane: 4-digit offset (only the first 4 KB is shown), 8 bytes, ASCII.
pub(super) fn hex_dump(b: &[u8]) -> String {
    let mut out = String::new();
    for (i, row) in b.chunks(8).enumerate() {
        let hex: Vec<String> = row.iter().map(|x| format!("{x:02x}")).collect();
        let ascii: String = row
            .iter()
            .map(|&c| {
                if c.is_ascii_graphic() || c == b' ' {
                    c as char
                } else {
                    '.'
                }
            })
            .collect();
        out.push_str(&format!(
            "{:04x}  {:<23}  |{ascii}|\n",
            i * 8,
            hex.join(" ")
        ));
    }
    out
}

/// What the worker found, rendered on the UI thread.
enum View {
    Overview {
        icon: Option<Vec<u8>>,
        sfo: Vec<(String, String)>,
        /// None: the package has no metadata block (PS4)
        meta: Option<Vec<(&'static str, String)>>,
        header: Vec<(String, String)>,
    },
    File {
        facts: Vec<(String, String)>,
        body: Body,
    },
    Error(String),
}

enum Body {
    Image(Vec<u8>),
    Sfo(Vec<(String, String)>),
    Text(String, bool),
    Hex(String, bool),
    Nothing,
}

fn overview(p: &Pkg) -> View {
    let icon = p.find("ICON0.PNG").and_then(|e| p.read_entry(e).ok());
    let h = &p.header;
    let header = vec![
        ("Content ID".into(), h.content_id.clone()),
        ("Package type".into(), h.type_label().into()),
        ("Files and folders".into(), p.entries.len().to_string()),
        ("Total size".into(), pkg::round_bytes(h.total_size)),
    ];
    View::Overview {
        icon,
        sfo: p.sfo(),
        meta: p.header.ps4.is_none().then(|| p.meta.rows()),
        header,
    }
}

fn file_view(p: &Pkg, path: &str) -> View {
    let Some(e) = p.entries.iter().find(|e| e.path == path) else {
        return View::Error(format!("{path} is not in the package"));
    };
    let key = match p.cipher_for(e) {
        Cipher::None => "none (stored unencrypted)",
        Cipher::Debug => "Debug (SHA-1)",
        Cipher::Ps3 => "PS3",
        Cipher::Psp => "PSP",
    };
    let facts = vec![
        (
            "Size".into(),
            format!(
                "{} ({} bytes)",
                pkg::round_bytes(e.size),
                pkg::thousands(e.size)
            ),
        ),
        (
            "Type".into(),
            gio::content_type_get_description(&content_type(e.name())).into(),
        ),
        ("Path".into(), format!("/{}", e.path)),
        ("Key".into(), key.into()),
        ("Offset".into(), format!("0x{:X}", e.offset)),
        ("Flags".into(), format!("{:08X}", e.flags)),
    ];
    let ext = e.name().rsplit_once('.').map(|x| x.1.to_ascii_lowercase());
    let is_image = gio::content_type_is_a(&content_type(e.name()), "image/*")
        || matches!(
            ext.as_deref(),
            Some("png" | "jpg" | "jpeg" | "gif" | "bmp" | "webp")
        );
    let read = |max: u64| p.read_head(e, max);
    let body = if e.size == 0 {
        Ok(Body::Nothing)
    } else if e.name().eq_ignore_ascii_case("PARAM.SFO") {
        p.read_entry(e).map(|b| match pkg::parse_sfo(&b) {
            Some(kv) => Body::Sfo(kv),
            None => Body::Hex(
                hex_dump(&b[..b.len().min(HEX_MAX as usize)]),
                b.len() as u64 > HEX_MAX,
            ),
        })
    } else if is_image && e.size <= pkg::MAX_IN_MEMORY {
        p.read_entry(e).map(Body::Image)
    } else {
        read(TEXT_MAX).map(|b| match as_text(&b) {
            Some(t) => Body::Text(t.to_string(), e.size > TEXT_MAX),
            None => Body::Hex(
                hex_dump(&b[..b.len().min(HEX_MAX as usize)]),
                e.size > HEX_MAX,
            ),
        })
    };
    match body {
        Ok(body) => View::File { facts, body },
        Err(err) => View::Error(format!("Could not read {}: {err}", e.name())),
    }
}

fn label(text: &str) -> gtk::Label {
    gtk::Label::builder()
        .label(text)
        .xalign(0.0)
        .selectable(true)
        .wrap(true)
        .wrap_mode(gtk::pango::WrapMode::WordChar)
        .build()
}

fn heading(text: &str) -> gtk::Label {
    let l = gtk::Label::builder().label(text).xalign(0.0).build();
    l.add_css_class("heading");
    l.set_margin_top(8);
    l
}

fn grid<K: AsRef<str>>(rows: &[(K, String)]) -> gtk::Grid {
    let g = gtk::Grid::builder()
        .row_spacing(2)
        .column_spacing(10)
        .build();
    for (r, (k, v)) in rows.iter().enumerate() {
        let k = gtk::Label::builder()
            .label(k.as_ref())
            .xalign(0.0)
            .yalign(0.0)
            .build();
        k.add_css_class("dim-label");
        g.attach(&k, 0, r as i32, 1, 1);
        let v = label(v);
        v.set_hexpand(true);
        g.attach(&v, 1, r as i32, 1, 1);
    }
    g
}

fn mono(text: &str, wrap: bool) -> gtk::TextView {
    let t = gtk::TextView::builder()
        .editable(false)
        .monospace(true)
        .wrap_mode(if wrap {
            gtk::WrapMode::WordChar
        } else {
            gtk::WrapMode::None
        })
        .build();
    t.buffer().set_text(text);
    t
}

fn picture(png: Vec<u8>, alt: &str) -> Option<gtk::Picture> {
    let tex = gdk::Texture::from_bytes(&glib::Bytes::from_owned(png)).ok()?;
    let pic = gtk::Picture::for_paintable(&tex);
    pic.set_can_shrink(true);
    pic.set_content_fit(gtk::ContentFit::Contain);
    pic.set_size_request(-1, 180);
    pic.set_alternative_text(Some(alt));
    Some(pic)
}

impl Ui {
    /// Refresh the inspector for the current selection (no-op while it is hidden). `get_visible`, not
    /// `is_visible`: the latter is false until the window is shown, which skipped the first refresh.
    pub(super) fn inspect(self: &Rc<Self>) {
        if !self.inspector_pane.get_visible() {
            return;
        }
        let gen = self.inspect_gen.get() + 1;
        self.inspect_gen.set(gen);
        let Some((pkg, _)) = self.pkg.borrow().clone() else {
            return self.show_inspector(None, None);
        };
        let sel = self.selected_paths();
        let file = match sel.as_slice() {
            [one] if pkg.entries.iter().any(|e| e.path == *one && !e.is_dir()) => Some(one.clone()),
            _ => None,
        };
        let ui = self.clone();
        glib::spawn_future_local(async move {
            let p: Arc<Pkg> = pkg;
            let name = file
                .as_ref()
                .map(|f| f.rsplit('/').next().unwrap_or(f).to_string());
            let view = gio::spawn_blocking(move || match file {
                Some(f) => file_view(&p, &f),
                None => overview(&p),
            })
            .await
            .unwrap_or_else(|_| View::Error("reading crashed".into()));
            if ui.inspect_gen.get() == gen {
                ui.show_inspector(Some(view), name);
            }
        });
    }

    fn show_inspector(&self, view: Option<View>, name: Option<String>) {
        let b = &self.inspector;
        while let Some(c) = b.first_child() {
            b.remove(&c);
        }
        let Some(view) = view else {
            b.append(&label("Open a package to see its details."));
            return;
        };
        match view {
            View::Error(msg) => b.append(&label(&msg)),
            View::Overview {
                icon,
                sfo,
                meta,
                header,
            } => {
                if let Some(pic) = icon.and_then(|i| picture(i, "Package icon (ICON0.PNG)")) {
                    b.append(&pic);
                }
                if let Some((_, t)) = sfo.iter().find(|(k, _)| k == "TITLE") {
                    let t = label(t);
                    t.add_css_class("title-3");
                    b.append(&t);
                }
                b.append(&grid(&header));
                if meta.is_some() {
                    b.append(&heading("Package metadata"));
                }
                if meta.as_ref().is_some_and(|m| m.is_empty()) {
                    b.append(&label("No metadata records."));
                } else if let Some(meta) = meta {
                    b.append(&grid(&meta));
                    let note =
                        label("Decoded as RPCS3 does; not yet checked against a real package.");
                    note.add_css_class("dim-label");
                    note.add_css_class("caption");
                    b.append(&note);
                }
                b.append(&heading("PARAM.SFO"));
                if sfo.is_empty() {
                    b.append(&label("This package has no PARAM.SFO."));
                } else {
                    b.append(&grid(&sfo));
                }
            }
            View::File { facts, body } => {
                let n = label(name.as_deref().unwrap_or_default());
                n.add_css_class("title-4");
                b.append(&n);
                b.append(&grid(&facts));
                let cut = |b: &gtk::Box, more: bool, what: &str| {
                    if more {
                        let l = label(what);
                        l.add_css_class("dim-label");
                        b.append(&l);
                    }
                };
                match body {
                    Body::Image(png) => {
                        b.append(&heading("Preview"));
                        match picture(png, name.as_deref().unwrap_or("image")) {
                            Some(p) => b.append(&p),
                            None => b.append(&label("Not an image GTK can show.")),
                        }
                    }
                    Body::Sfo(kv) => {
                        b.append(&heading("PARAM.SFO"));
                        b.append(&grid(&kv));
                    }
                    Body::Text(t, more) => {
                        b.append(&heading("Text"));
                        b.append(&mono(&t, true));
                        cut(b, more, "Showing the first 64 KB.");
                    }
                    Body::Hex(h, more) => {
                        b.append(&heading("Hex"));
                        b.append(&mono(&h, false));
                        cut(b, more, "Showing the first 4 KB.");
                    }
                    Body::Nothing => b.append(&label("Empty file.")),
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{as_text, hex_dump};

    #[test]
    fn text_and_hex() {
        assert_eq!(as_text(b"hello\nworld\t!"), Some("hello\nworld\t!"));
        assert_eq!(as_text("caf\u{e9}".as_bytes()), Some("caf\u{e9}"));
        // A character cut at the end of a preview is dropped, not a reason to call it binary.
        let cut = &"ab\u{e9}".as_bytes()[..3];
        assert_eq!(as_text(cut), Some("ab"));
        assert_eq!(as_text(b"a\0b"), None);
        assert_eq!(as_text(&[0xFF, 0xFE, 0x00]), None);
        assert_eq!(as_text(b"\x01\x02\x03\x04xyz"), None);
        assert_eq!(as_text(b""), Some(""));
        let d = hex_dump(b"PKG\x00\x01ABCDEFG");
        assert_eq!(
            d,
            "0000  50 4b 47 00 01 41 42 43  |PKG..ABC|\n\
             0008  44 45 46 47              |DEFG|\n"
        );
    }
}
