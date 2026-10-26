//! `$XDG_CONFIG_HOME/pkgview/settings.ini`: one key file, written through on every `set`.

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

    pub(super) fn save(&self) -> Result<(), glib::Error> {
        let _ = std::fs::create_dir_all(Self::dir());
        self.0.save_to_file(Self::dir().join("settings.ini"))
    }
}
