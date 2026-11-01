"""GNOME Files integration for PS3/PSP .pkg files (PkgView):
- right-click menu: Extract Here, Extract To…, Open with PkgView
- list-view columns: Game Title, Title ID, Kind, Version (choose them under "Visible Columns")
- a PkgView section in a package's Properties

Column values come from `pkgview info --json`, run asynchronously and cached per file (path + mtime + size),
so a folder of packages never blocks Files.
"""

import json

import gi

try:
    gi.require_version("Nautilus", "4.1")  # Nautilus 47+ (Ubuntu 26.04)
except ValueError:
    gi.require_version("Nautilus", "4.0")  # Nautilus 43-46 (e.g. Ubuntu 24.04)
from gi.repository import Gio, GLib, GObject, Nautilus

PKGVIEW = "/usr/bin/pkgview"  # absolute: a ~/.local/bin/pkgview must not shadow it
MIME = "application/x-ps3-pkg"

# (column name, file attribute, label, description)
COLUMNS = [
    ("PkgView::title", "pkgview_title", "Game Title", "Title from the package's PARAM.SFO"),
    ("PkgView::title_id", "pkgview_title_id", "Title ID", "Title ID of the PS3/PSP package"),
    ("PkgView::kind", "pkgview_kind", "Package Kind", "Game, Patch, PS1 Classic, Add-on…"),
    ("PkgView::version", "pkgview_version", "Package Version", "App version from PARAM.SFO"),
]


def local_path(f):
    loc = f.get_location()
    return loc.get_path() if loc else None  # remote (sftp://, smb://) files are skipped


def local_pkgs(files):
    """Local paths of the selection, or [] unless every selected file is a .pkg."""
    if not files or not all(f.is_mime_type(MIME) for f in files):
        return []
    return [p for p in map(local_path, files) if p]


def run(argv):
    # Gio.Subprocess reaps the child itself, so Files collects no zombies.
    Gio.Subprocess.new(argv, Gio.SubprocessFlags.NONE)


def values(info):
    """Column values from one `pkgview info --json` object."""
    sfo = info.get("sfo", {})
    meta = info.get("metadata", {})
    return {
        "pkgview_title": sfo.get("TITLE", ""),
        "pkgview_title_id": sfo.get("TITLE_ID") or meta.get("title_id") or info.get("root", ""),
        "pkgview_kind": info.get("kind", ""),
        "pkgview_version": sfo.get("APP_VER") or meta.get("app_version", ""),
    }


def properties(info):
    """(label, value) rows for the Properties section."""
    sfo = info.get("sfo", {})
    meta = info.get("metadata", {})
    rows = [
        ("Title", sfo.get("TITLE", "")),
        ("Title ID", values(info)["pkgview_title_id"]),
        ("Kind", info.get("kind", "")),
        ("Version", values(info)["pkgview_version"]),
        ("Category", sfo.get("CATEGORY", "")),
        ("Content ID", info.get("content_id", "")),
        ("Package type", info.get("type", "")),
        ("Content type", meta.get("content_type_name", "")),
        ("Needs system software", meta.get("min_firmware", "")),
        ("Files and folders", str(info.get("item_count", ""))),
    ]
    return [(k, v) for k, v in rows if v]


def stat_key(path):
    try:
        st = Gio.File.new_for_path(path).query_info("time::modified,standard::size", Gio.FileQueryInfoFlags.NONE, None)
        return (st.get_modification_date_time().to_unix(), st.get_size())
    except GLib.Error:
        return None


class Cache:
    """path -> (stat key, info dict or None); an entry is used only while the file is unchanged."""

    def __init__(self):
        self.entries = {}

    def get(self, path):
        hit = self.entries.get(path)
        return hit[1] if hit and hit[0] == stat_key(path) else None

    def put(self, path, info):
        self.entries[path] = (stat_key(path), info)
        if len(self.entries) > 4096:  # a bound, not an LRU: big folders just re-read
            self.entries.clear()


CACHE = Cache()


def fetch(path, cancellable, done):
    """Run `pkgview info --json PATH` without blocking; done(info dict or None)."""
    try:
        proc = Gio.Subprocess.new(
            [PKGVIEW, "info", "--json", path],
            Gio.SubprocessFlags.STDOUT_PIPE | Gio.SubprocessFlags.STDERR_SILENCE,
        )
    except GLib.Error:
        return done(None)

    def finished(p, result):
        try:
            _ok, out, _err = p.communicate_utf8_finish(result)
            info = json.loads(out.splitlines()[0]) if out and out.strip() else None
            if info is not None and "error" in info:
                info = None
        except (GLib.Error, ValueError, IndexError):
            info = None
        done(info)

    proc.communicate_utf8_async(None, cancellable, finished)


def run_sync(path):
    """`pkgview info --json PATH`, waiting at most 10 s (for the Properties dialog)."""
    hit = CACHE.get(path)
    if hit is not None:
        return hit
    try:
        proc = Gio.Subprocess.new(
            [PKGVIEW, "info", "--json", path],
            Gio.SubprocessFlags.STDOUT_PIPE | Gio.SubprocessFlags.STDERR_SILENCE,
        )
        _ok, out, _err = proc.communicate_utf8(None, None)
        info = json.loads(out.splitlines()[0]) if out and out.strip() else None
    except (GLib.Error, ValueError, IndexError):
        return None
    if info is not None and "error" not in info:
        CACHE.put(path, info)
        return info
    return None


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


class PkgViewColumns(GObject.GObject, Nautilus.ColumnProvider, Nautilus.InfoProvider):
    def get_columns(self):
        return [Nautilus.Column(name=n, attribute=a, label=l, description=d) for n, a, l, d in COLUMNS]

    def update_file_info_full(self, provider, handle, closure, file):
        path = local_path(file) if file.is_mime_type(MIME) else None
        if not path:
            return Nautilus.OperationResult.COMPLETE
        hit = CACHE.get(path)
        if hit is not None:
            self.apply(file, hit)
            return Nautilus.OperationResult.COMPLETE

        # Each request keeps its own file and closure. Handles are not used as keys: nautilus-python's
        # handle wrappers compare equal across concurrent requests (seen with Nautilus 46), which lost
        # results. Completing a request Nautilus has meanwhile cancelled is harmless: it ignores it.
        def done(info):
            if info is not None:
                CACHE.put(path, info)
                self.apply(file, info)
            Nautilus.info_provider_update_complete_invoke(
                closure, provider, handle, Nautilus.OperationResult.COMPLETE
            )

        fetch(path, None, done)
        return Nautilus.OperationResult.IN_PROGRESS

    def cancel_update(self, provider, handle):
        pass  # `pkgview info` takes milliseconds; let it finish (see update_file_info_full)

    @staticmethod
    def apply(file, info):
        for attr, value in values(info).items():
            file.add_string_attribute(attr, value)


if hasattr(Nautilus, "PropertiesModelProvider"):  # Nautilus 43+

    class PkgViewProperties(GObject.GObject, Nautilus.PropertiesModelProvider):
        def get_models(self, files):
            paths = local_pkgs(files)
            if len(paths) != 1:
                return []
            info = run_sync(paths[0])
            if info is None:
                return []
            store = Gio.ListStore.new(Nautilus.PropertiesItem)
            for name, value in properties(info):
                store.append(Nautilus.PropertiesItem(name=name, value=value))
            return [Nautilus.PropertiesModel(title="PS3/PSP Package (PkgView)", model=store)]
