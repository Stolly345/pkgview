//! `$XDG_CONFIG_HOME/pkgview/settings.ini`: one key file, written through on every `set`.

use crate::pkg::{ExtractOptions, Overwrite};
use gtk::glib;
use std::path::PathBuf;

const GROUP: &str = "pkgview";

#[derive(Clone)]
pub(super) struct Settings(glib::KeyFile);

impl Settings {
    fn dir() -> PathBuf {
        glib::user_config_dir().join("pkgview")
    }

    pub(super) fn load() -> Settings {
        let kf = glib::KeyFile::new();
        let _ = kf.load_from_file(Self::dir().join("settings.ini"), glib::KeyFileFlags::NONE);
        let s = Settings(kf);
        // 2.1.0 kept only the view mode, in a file of its own.
        let old = Self::dir().join("view");
        if let Ok(v) = std::fs::read_to_string(&old) {
            if s.set("view", v.trim()).is_ok() {
                let _ = std::fs::remove_file(old);
            }
        }
        s
    }

    pub(super) fn get(&self, key: &str) -> Option<String> {
        self.0.string(GROUP, key).ok().map(Into::into)
    }

    pub(super) fn set(&self, key: &str, value: &str) -> Result<(), glib::Error> {
        self.0.set_string(GROUP, key, value);
        self.save()
    }

    /// A list stored one item per line (glib's own list setter isn't bound in glib-rs 0.20).
    pub(super) fn list(&self, key: &str) -> Vec<String> {
        self.get(key)
            .map(|v| {
                v.lines()
                    .filter(|l| !l.is_empty())
                    .map(String::from)
                    .collect()
            })
            .unwrap_or_default()
    }

    pub(super) fn set_list(&self, key: &str, values: &[String]) -> Result<(), glib::Error> {
        self.set(key, &values.join("\n"))
    }

    fn flag(&self, key: &str, default: bool) -> bool {
        self.get(key).map_or(default, |v| v == "true")
    }

    /// Options last used in the Extract dialog. The overwrite policy defaults to Ask.
    pub(super) fn extract_options(&self) -> ExtractOptions {
        ExtractOptions {
            overwrite: match self.get("overwrite").as_deref() {
                Some("skip") => Overwrite::Skip,
                Some("replace") => Overwrite::Replace,
                Some("rename") => Overwrite::Rename,
                _ => Overwrite::Ask,
            },
            pkg_folder: self.flag("pkg_folder", true),
            keep_paths: self.flag("keep_paths", true),
        }
    }

    /// Options for one-click actions (Extract Here, the Files menu): the original layout, with the
    /// remembered overwrite policy.
    pub(super) fn quick_options(&self) -> ExtractOptions {
        ExtractOptions {
            overwrite: self.extract_options().overwrite,
            ..ExtractOptions::default()
        }
    }

    pub(super) fn set_extract_options(&self, o: &ExtractOptions, open_after: bool) {
        let ow = match o.overwrite {
            Overwrite::Ask => "ask",
            Overwrite::Skip => "skip",
            Overwrite::Replace => "replace",
            Overwrite::Rename => "rename",
        };
        self.0.set_string(GROUP, "overwrite", ow);
        self.0
            .set_string(GROUP, "pkg_folder", &o.pkg_folder.to_string());
        self.0
            .set_string(GROUP, "keep_paths", &o.keep_paths.to_string());
        let _ = self.set("open_after", &open_after.to_string());
    }

    pub(super) fn open_after(&self) -> bool {
        self.flag("open_after", false)
    }

    pub(super) fn save(&self) -> Result<(), glib::Error> {
        let _ = std::fs::create_dir_all(Self::dir());
        self.0.save_to_file(Self::dir().join("settings.ini"))
    }
}
