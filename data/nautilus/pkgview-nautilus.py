"""GNOME Files menu for PS3/PSP .pkg files: Extract Here, Extract To…, Open with PkgView."""

import gi

try:
    gi.require_version("Nautilus", "4.1")  # Nautilus 47+ (Ubuntu 26.04)
except ValueError:
    gi.require_version("Nautilus", "4.0")  # Nautilus 43-46 (Debian 12)
from gi.repository import Gio, GObject, Nautilus

PKGVIEW = "/usr/bin/pkgview"  # absolute: a ~/.local/bin/pkgview must not shadow it
MIME = "application/x-ps3-pkg"


def local_pkgs(files):
    """Local paths of the selection, or [] unless every selected file is a .pkg."""
    if not files or not all(f.is_mime_type(MIME) for f in files):
        return []
    paths = []
    for f in files:
        loc = f.get_location()
        path = loc.get_path() if loc else None
        if path:  # remote (sftp://, smb://) files are skipped
            paths.append(path)
    return paths


def run(argv):
    # Gio.Subprocess reaps the child itself, so Files collects no zombies.
    Gio.Subprocess.new(argv, Gio.SubprocessFlags.NONE)


class PkgViewMenu(GObject.GObject, Nautilus.MenuProvider):
    def get_file_items(self, *args):
        paths = local_pkgs(args[-1])  # 4.x passes (files); older APIs passed (window, files)
        if not paths:
            return []
        entries = [
            ("PkgView::extract_here", "Extract Here", [PKGVIEW, "--extract-here", *paths]),
            ("PkgView::extract_to", "Extract To…", [PKGVIEW, "--extract-to", *paths]),
        ]
        if len(paths) == 1:
            entries.append(("PkgView::open", "Open with PkgView", [PKGVIEW, paths[0]]))
        items = []
        for name, label, argv in entries:
            item = Nautilus.MenuItem(name=name, label=label)
            item.connect("activate", lambda _item, argv=argv: run(argv))
            items.append(item)
        return items
