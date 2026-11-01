# PkgView — Product Requirements Document

2026-10-25 · updated 2026-10-31 after an outside review · 2.4.0 shipped and published 2026-11-01 · 2.5.0–2.7.0 built 2026-11-01 (roadmap complete) · Alex Stoll

## Overview

PkgView is a native Linux desktop app and CLI that browses, verifies and extracts Sony PS3/PSP `.pkg` installation packages. Versions 2.5.0 to 2.7.0 are built, which completes this roadmap (not yet published); 2.4.0 is the first published release: public at [github.com/Stolly345/pkgview](https://github.com/Stolly345/pkgview) since 2026-11-01, as a `.deb`, an `.rpm` and an Arch PKGBUILD. (2.3.1 was prepared for release but only ever installed locally.) It is a Rust/GTK 4 rewrite of PkgView 1.3 (VB.NET/WinForms, 2010–2012, by Ifcaro), shipped as a `.deb`. Through 2.3.x it supports Debian 12+ and Ubuntu 24.04+; from 2.4.0 it supports Debian 13+ and Ubuntu 24.04+ (see Decisions).

This PRD fixes what the product must do through 2.7.0: the features already shipped, the ones planned, and the bar each release must clear. Implementation detail lives in `README.md` and the code's tests.

## Problem and goals

On Linux the original Windows tool only runs under Mono: it's slow, works one file at a time and has no desktop integration. PkgView should feel like a normal archive manager for one format. Beyond that, it should be the tool for **inspecting and managing a collection** of PS3/PSP packages, which no generic archive manager can do.

**Goals**

- Extract any supported `.pkg` byte for byte the same as the original tool, only faster.
- Make everyday work quick: browse, search, open a file, drag it out, extract many packages at once.
- Show what a package *is* (title, Title ID, category, versions, DRM) without extracting it.
- Manage a folder of hundreds of packages: verify them all, name them readably, and spot duplicates and orphans.
- Fit GNOME: open from Files, icon thumbnails, right-click Extract Here / Extract To, package details in Files.
- Never damage input or anything outside the destination. Packages are opened read-only and treated as untrusted.

**Non-goals** (decided 2026-10-25; extended 2026-10-31)

- Creating or editing packages, debug or retail.
- PS Vita, PS4 or PS5 packages; EDAT/RAP decryption. Reporting whether a `.rap` file is *present* is in scope (F27).
- Packaging beyond `.deb`, `.rpm` and the Arch PKGBUILD (translations, Flatpak/AppImage, CI) until there is demand. (`.rpm` and Arch added 2026-11-01 at the owner's request.)
- Tabs, skins, cloud or URL opening.
- Split or multi-part packages, unless a real sample turns up.
- Generic archive-manager machinery, which buys nothing when an extract takes under a second: pause/resume, a live multi-worker queue, a Preferences window (2026-10-31).

## Users and use cases

The primary user is someone on a GNOME desktop (Ubuntu 24.04+ or Debian 13+) with a collection of PS3/PSP packages, for example PS1 Classics of 60–365 MB. They're comfortable in a terminal but use the GUI day to day.

| # | Use case | Status at 2.7.0 → target |
| --- | --- | --- |
| 1 | Double-click a `.pkg` in Files and see what is inside | Supported |
| 2 | Find one file (e.g. `ICON0.PNG`, `PARAM.SFO`) and open or drag it out | Supported |
| 3 | Extract a whole package, or a few folders, to a chosen place | Supported, with options (2.4.0) |
| 4 | Extract a folder of packages in one go and see which failed | Supported, with Retry failed (2.6.0) |
| 5 | Check a download is intact (SHA-1) before using it | Supported, also a whole folder (2.6.0) |
| 6 | Right-click packages in Files → Extract Here, with game-icon thumbnails | Supported |
| 7 | Read a package's title, Title ID, category, versions and DRM without extracting it | Supported (2.5.0: inspector, `info`) |
| 8 | Script it: `info`, `list`, `extract`, `verify`, `cat` from the shell | Supported, with `--json` (2.5.0) |
| 9 | Turn `UP9000-NPUJ00944_00-….pkg` into a readable name | Supported (2.6.0) |
| 10 | Audit a folder: corrupt files, duplicates, patches or DLC with no base game, missing `.rap` | Supported (2.6.0) |
| 11 | See title and Title ID as columns in Files without opening PkgView | Supported (2.6.0) |
| 12 | Put a PS3 package straight into RPCS3's `dev_hdd0` | Supported (2.7.0); not yet compared with RPCS3 itself |

## Functional requirements

Thirteen requirement groups shipped in 2.0–2.3; the rest land one release at a time through 2.7.0. Must = needed for daily use; Should = clear value; Could = only if earlier phases leave the code clean. The 2026-10-31 review renumbered nothing that had shipped; planned items F14–F19 were rewritten and F20–F30 added.

| ID | Area | Requirement | Priority | Release | Status |
| --- | --- | --- | --- | --- | --- |
| F1 | Core | Open debug PS3, retail PS3 and retail PSP pkgs; list entries; decrypt any entry with O(1) seeking | Must | 2.0.0 | Shipped |
| F2 | Core | Extract all, a folder or a selection to `DIR/<pkg>/<root>/…` with a per-pkg log, matching the original's layout | Must | 2.0.0 | Shipped |
| F3 | Core | Verify the trailing SHA-1 | Must | 2.0.0 | Shipped |
| F4 | Browse | Folder tree + four views (big icons, small icons, list, details); properties with header, PARAM.SFO and ICON0 | Must | 2.0.0 | Shipped |
| F5 | CLI | `info`, `list`, `extract`, `verify`, original `-ex` switch | Must | 2.0.0 | Shipped |
| F6 | Desktop | `.deb`, MIME type with magic match, desktop entry, default-handler action | Must | 2.0.0 | Shipped |
| F6b | Packaging | Same binary as `.rpm` (Fedora 40+, openSUSE Tumbleweed; `cargo generate-rpm`, requirements = the binary's own sonames) and as an Arch `pkgview-bin` PKGBUILD repackaging the `.deb` (`.SRCINFO` kept current; AUR upload when registration reopens) | Should | 2.4.0 | Shipped |
| F7 | Bulk | Bulk extract several pkgs to one folder; failures reported at the end, the rest continue | Must | 2.1.0 | Shipped |
| F8 | Browse | Sortable columns; optional Offset, Key, Flags columns, remembered | Should | 2.2.0 | Shipped |
| F9 | Browse | Search by substring or `*`/`?` wildcard across all folders; jump to result | Must | 2.2.0 | Shipped |
| F10 | Browse | Path bar, back/forward/up, keyboard shortcuts, recent files, welcome page, selection size | Should | 2.2.0 | Shipped |
| F11 | Browse | Open a file with its default app; drag files out (512 MB cap); drop pkgs or a folder to bulk-extract | Must | 2.2.0 | Shipped |
| F12 | Files | Thumbnailer showing ICON0.PNG; Nautilus menu Extract Here / Extract To… / Open; GUI-only progress mode for those | Must | 2.3.0 | Shipped |
| F13 | Docs | Man page and bash/zsh/fish completions | Could | 2.3.0 | Shipped |
| F14 | Extract | Extract dialog: destination + recent destinations, create pkg folder, keep structure, open when done, overwrite policy (Ask / Skip / Replace / Rename). Last choices are remembered in `settings.ini`; this replaces a Preferences window | Must | 2.4.0 | Shipped |
| F15 | Extract | Free-space check before writing (one `statvfs`); "Test" = decrypt `PARAM.SFO`, `ICON0.PNG` and `EBOOT.BIN` and check their magic (`\0PSF`, `\x89PNG`, `SCE\0`), which catches a wrong key. No general per-entry magic validator: most game assets have no fixed header | Should | 2.4.0 | Shipped |
| F16 | Safety | Writes never follow symlinks: an existing symlink at a target file or any directory below the destination is replaced or refused, never written through (`O_NOFOLLOW` / `symlink_metadata`) | Must | 2.4.0 | Shipped |
| F17 | CLI | Fixed exit codes: 0 = all OK, 1 = at least one package or entry failed, 2 = usage error. The failure count goes in the output, never in the exit status (8-bit, wraps at 256). `extract` and `verify` accept several packages | Must | 2.4.0 | Shipped |
| F18 | Platform | Raise the GTK floor to 4.14 (`v4_14`): `FileDialog` instead of `FileChooserNative`; the tree's right-click acts on the row under the pointer; search results and history scroll to the item. Delete the 4.8 workarounds | Should | 2.4.0 | Shipped |
| F19 | Open | Files opened or dragged out stay in `~/.cache/pkgview/open/<pid>/` until the next start (the existing stale-PID sweep removes them) instead of being deleted at exit while a viewer may still have them open | Should | 2.4.0 | Shipped |
| F20 | Info | Inspector pane (toggle, right side): image preview, text, hex, PARAM.SFO as a full key table (TITLE, TITLE_ID, CATEGORY, APP_VER, VERSION, PS3_SYSTEM_VER, resolution/sound flags), and the package metadata block (content type, DRM type, package version). Fields not confirmed on a real package are labelled UNVERIFIED | Must | 2.5.0 | Shipped |
| F21 | CLI | `--json` on `info`, `list`, `verify`: one compact object per package per line, so one package is JSON and several are JSON Lines (no separate `--jsonl`); errors as `{"file","error"}`. No CSV: `jq` covers it | Must | 2.5.0 | Shipped |
| F22 | CLI | `pkgview cat FILE.pkg PATH` streams one entry to stdout, no temp files | Should | 2.5.0 | Shipped |
| F23 | Core | Debug keystream generated on several threads (`std::thread::scope`, no new crate). Every real sample is a debug pkg, so this is the common case | Could | 2.5.0 | Shipped |
| F24 | Collection | `pkgview rename [--dry-run] FILE…` and a GUI bulk action: template from SFO/header fields (default `{TITLE} [{TITLE_ID}] [v{APP_VER}] [{CATEGORY}].pkg`), shown as a preview before anything changes, undoable from a rename log | Must | 2.6.0 | Shipped |
| F25 | Collection | `pkgview audit DIR` (+ `--json`): SHA-1 status (OK / CORRUPT / TRUNCATED) for every pkg; duplicates (same SHA-1, or same content ID + version); patches and DLC with no base game for their Title ID. Packages are hashed on several threads; one bad pkg never stops the run | Must | 2.6.0 | Shipped |
| F26 | Collection | Library view in the GUI: open a folder → icon grid / table with title, Title ID, category, version, size and audit status; sort, filter, open, extract, rename. Uses the same scan as F25 | Should | 2.6.0 | Shipped |
| F27 | Collection | `.rap` status: for packages whose DRM type needs a `.rap`, show *RAP found* / *RAP missing*, looking next to the pkg and in RPCS3's `dev_hdd0/home/00000001/exdata/`. Presence only, nothing is decrypted | Should | 2.6.0 | Shipped |
| F28 | Files | Nautilus extension adds list-view columns (Title, Title ID, Category, Version) and a Properties tab with the SFO summary, reading `pkgview info --json`; keeps the API 4.0 / 4.1 fallback | Should | 2.6.0 | Shipped |
| F29 | Bulk | The bulk-extract summary lists successes and failures with reasons and a **Retry failed** button. This replaces the planned queue window | Should | 2.6.0 | Shipped |
| F30 | Hand-off | Install to RPCS3: extract a PS3 pkg into `<rpcs3>/dev_hdd0/game/<TITLE_ID>/` (path detected from RPCS3's config, overridable), GUI action + `pkgview install-rpcs3 FILE…`. Must be checked against RPCS3's own installer on a real package before release | Should | 2.7.0 | Shipped (see Checks: RPCS3 comparison not yet run) |
| F31 | CLI | `pkgview diff A.pkg B.pkg`: added / removed / changed entries by path, size, flags, optionally content hash | Could | 2.7.0 | Shipped |
| F32 | Collection | Check packages against an offline No-Intro / Redump DAT file (`--dat FILE`) in `audit` and the library view | Could | 2.7.0 | Shipped |

## Format and correctness

Output must match the original tool byte for byte on every retail package and every real sample; the only allowed difference is the documented debug `0x90` key choice.

- **Format:** big-endian header (`\x7FPKG`, type, metadata offset/count, item count, sizes, data offset, content ID, QA digest, data RIV), a 32-byte-per-entry item table, SHA-1 over `[0, EOF−32)`.
- **Crypto:** retail = AES-128 counter mode from the data RIV (PS3 or PSP key; entries with flags `0x90xxxxxx` use the PSP key); debug = SHA-1 of a 64-byte counter built from the QA digest. The block index is a `u64` added into the full 128-bit / 64-byte counter, so there is no 32-bit truncation (checked 2026-10-31).
- **Untrusted input:** reject `..`, absolute paths, NUL and `\` in names; bounds-check every offset and size; cap allocations; never panic on corrupt data (fuzz-tested). Two entries landing on the same path are renamed deterministically (already implemented). From 2.4.0, never write through a symlink (F16).
- **Read-only:** packages are never opened for writing; a cancelled or failed file is deleted, never left half-written.

| Check | How | Result at 2.5.0 |
| --- | --- | --- |
| Unit tests | 32 tests (2.7.0 adds: RPCS3 install rules incl. overwrite flag, content-type folders, DLC install folder, unsafe names, cancel cleanup, symlinks; diff + whole-file SHA-1; RPCS3 dev_hdd0 from vfs.yml; DAT parsing and matching). 2.6.0's 29 (2.6.0 adds: kinds, templates and safe names, audit findings incl. disc patches and licences, rename plan/apply/undo incl. never overwriting). 2.5.0's 25 (2.5.0 adds: metadata records incl. truncated/garbled blocks, metadata read from a pkg, parallel = serial debug keystream, `read_head`/`sfo`/`write_entry`, JSON writer escaping, text/hex detection). 2.4.0's 19: crypto round-trip for all 3 types, wrong key, malicious input (incl. far-away names), fuzz, options, Ask hook, symlinks, free space, header check, >4 GiB sparse entry (independent counter), search, history, CLI args | Pass |
| GUI | `make gui`: Xvfb + xdotool, 27 screenshots (2.7.0 adds Install to RPCS3 from the library and a library opened with `--dat`); 2.6.0's 25 (2.6.0 adds the library: verify, rename + undo on real files, filter, extract failure + Retry Failed); 2.5.0's 18 (2.5.0 adds the inspector: package overview on open, PARAM.SFO table, image, hex, text, F9 hide); 2.4.0's 12 (tree right-click, Extract dialog, Ask round trip + cancel, search jump, Verify, `--extract-to` cancel/extract, `--extract-here` + Replace All) | Pass, inspected |
| Original code | `make compare`: the original VB crypto compiled under Mono vs the port, synthetic pkgs of each type | Retail identical; debug differs only in the `0x90` entry |
| Real packages | `make realtest` + `make compare SAMPLES=…` on 3 debug PS3 pkgs | SHA-1 OK, TITLE\_ID matches, identical to the original's output |

**Checks added in 2.7.0:**
- **Install to RPCS3 mirrors RPCS3's source** (`set_install_path`, `extract_worker` in `rpcs3/Crypto/unpkg.cpp`): folder by content type, install folder from header offset 55 or metadata `0xA`, folders for entry types 0x04/0x12, only the file types RPCS3 writes, existing files kept unless flagged `0x80000000`, a failed new install removed. Unit-tested rule by rule; CLI and GUI installs checked into a fake RPCS3 config (`vfs.yml` with `$(EmulatorDir)`), including the `.rap` copy.
- **NOT DONE: comparison with RPCS3 itself.** `make compare-rpcs3` installs the test packages with `rpcs3 --installpkg` and with PkgView and diffs the trees; running the downloaded RPCS3 needs the owner's go-ahead, so it has not run. Until it has, treat F30 as following RPCS3's code, not proven against it.
- **DAT check** on a DAT built from real hashes of the test collection plus a wrong one: match, not in DAT and same name / other SHA-1 all reported (CLI and GUI). Real No-Intro / Redump DATs not tried (none on hand).
- **Found while testing:** a `.rap` beside a package given by bare file name (`x.pkg`) was never found: the parent of a bare name is the empty path, not `.`.

**Checks added in 2.6.0** (all done):
- **Collection end to end:** `write_collection` makes a game, an identical copy, its patch, a patch without its game, a patch for a disc game, a PS1 Classic, a corrupted copy, a cut-off download, a non-package and a `.rap`; `audit`, `rename` (+ dry run, undo, bad template) and the library window all report and act on it correctly.
- **Two findings fixed during testing:** a damaged copy shares its digest with an intact one (the digest is read from the file), so after hashing it is shown as "an intact copy is …", not as a duplicate; a cut-off download failed `open` with "data lies outside the file" and is now classed TRUNCATED with "the file is cut off (x of y bytes)".
- **GNOME Files extension in real Nautilus:** Nautilus 46 (Ubuntu 24.04, API 4.0) under Xvfb: columns filled, Properties section complete (screenshots). Nautilus 49 (Fedora 43, API 4.1): the extension loads, its providers are defined and every Nautilus call it uses exists; the window could not be drawn in the container. Found: nautilus-python's operation handles compare equal across concurrent requests, which lost column values; requests are no longer keyed by handle.

**Checks added in 2.5.0** (all done):
- **Metadata layout from source:** RPCS3's `read_metadata` and its `pkg_content_type` / `pkg_flag` enums; psdevwiki was unreachable (Cloudflare) and windsurfer1122's PSN_get_pkg_info names licence types (1 network, 2 local, 3 free) only for EDAT files, so DRM type names stay marked *unverified*. Not checked on a real package: none with metadata is on hand.
- **Debug keystream speed:** 1 GiB debug entry extracts in 1.5–1.9 s on 16 threads (was 7.2 s), output identical (unit test against the single-threaded version).
- **`cat` = `extract`** byte for byte; `cat … | head` exits cleanly; JSON output parsed by Python for `info`, `list`, `verify` incl. an unreadable file.
- **Found while testing:** gtk-rs `is_visible()` is `gtk_widget_is_visible` (checks every parent up to the window), so the inspector skipped its first refresh before the window was shown; it uses `get_visible()`.

**Checks added in 2.4.0** (all done):
- **>4 GiB test** on a sparse file: an entry at data offset `0x1_0000_0040` decrypts correctly for all three key types, encrypted with an independent u128 / RPCS3-style counter. It also found a real bug: the item table was sized from the first entry's data offset (the original's assumption), so a pkg whose first entry lies far away made PkgView allocate and decrypt gigabytes. The table is now sized from where the names end, capped at 1 KiB per name.
- **Debug `0x90` settled from source:** RPCS3's `unpkg.cpp` uses the SHA-1 keystream for every entry of a debug pkg, as the port does. RPCS3 differs from the original (and the port) elsewhere: it picks the PSP key by bit `0x10000000` rather than top byte `0x90`, and decrypts PSP entries' names with the PSP key in retail PS3 pkgs. Kept as the original for now (see Risks).
- **Symlink test:** symlinked folders in the destination are refused, symlinked files (also a dangling one, and the log) are replaced, targets untouched; the destination itself may be a symlink.
- **Header magic** checked against the files of a real PS3 disc game folder (not a pkg): `\0PSF`, `\x89PNG`, `SCE\0` match.

## Non-functional requirements

| Area | Requirement |
| --- | --- |
| Platform | Linux x86-64. Until 2.3.x: Debian 12+ / Ubuntu 24.04+, GTK ≥ 4.8. **From 2.4.0: Debian 13+ / Ubuntu 24.04+, GTK ≥ 4.14**, built in an `ubuntu:24.04` container (the oldest target; the host has no Rust toolchain) |
| Performance | Retail extract ≥ 800 MB/s from page cache (measured 1 GiB in 1.2 s); debug 1 GiB in ≤ 2.5 s on a 4+ core machine (measured 1.5–1.9 s on 16 threads in 2.5.0; was 7.2 s); SHA-1 verify 1 GiB in < 1 s; thumbnail of the largest pkg < 1 s; `audit` of 100 pkgs bounded by disk read speed |
| Responsiveness | No GTK main-thread I/O over ~50 ms; extraction, open-file, verify and library scans run on worker threads with progress and cancel |
| Security | Read-only input, path sanitising, bounds checks, no writes through symlinks, no shell in subprocess calls, temp files only under `~/.cache/pkgview/open/<pid>/` |
| Reliability | One broken package never stops a bulk run, audit or rename; failures name the package and reason; partial files removed; renames are logged and reversible |
| Accessibility | Built in as each widget is added, not as a separate release: every action reachable by keyboard, labels and tooltips on icon buttons, usable at 200% text scale |
| Dependencies | Rust std, gtk4-rs (with glib/gio/gdk-pixbuf), aes, sha1, clap, anyhow, libc; new crates need a stated reason (allowed: `clap_complete`, `clap_mangen`). JSON output may add `serde_json` if hand-written output proves error-prone |
| Attribution | PkgView is a port of Ifcaro's PkgView (github.com/ifcaro/PkgView, ISC) and must say so wherever it is presented: README intro + Credits section, `LICENSE` (his notice first), About dialog, `.deb`/`.rpm` descriptions, metainfo, Arch `pkgdesc`, GitHub repo description and every release's notes. Never removed or shortened |
| Settings | One file, `~/.config/pkgview/settings.ini` (view, columns, recent files, last extract options, rename template, RPCS3 path) |
| CLI | Exit codes 0 / 1 / 2 only (F17); machine output on stdout, messages on stderr |

## Release plan and roadmap

| Release | Phase | Scope | Status |
| --- | --- | --- | --- |
| 2.0.0 | — | Rust/GTK4 port, CLI, .deb, MIME | Shipped |
| 2.1.0 | — | Bulk extract to one destination | Shipped |
| 2.1.1 | A | Code split, extract options, real-pkg tests | Shipped |
| 2.2.0 | B | Search, sorting, open, drag out | Shipped |
| 2.3.0 | C | Thumbnails, Files menu, man page | Shipped |
| 2.3.1 | — | Release prep: app ID, homepage, screenshots (installed locally, never published as a download) | Shipped |
| 2.4.0 | D | **Safety + extract:** symlink-safe writes, exit codes, >4 GiB + `0x90` checks, GTK 4.14 floor, extract dialog, key test, keep opened files until next start (F14–F19) | Shipped; published 2026-11-01 (GitHub release v2.4.0: .deb + .rpm; Arch PKGBUILD in the repo) |
| 2.5.0 | E | **Inspect:** inspector pane + full metadata, `--json`, `cat`, parallel debug keystream (F20–F23) | Shipped (built 2026-11-01, not yet published) |
| 2.6.0 | F | **Collection:** rename, audit, library view, `.rap` status, Files columns + Properties tab, Retry failed (F24–F29) | Shipped (built 2026-11-01, not yet published) |
| 2.7.0 | G | **Hand-off:** install to RPCS3; diff and DAT check (F30–F32) | Shipped (built 2026-11-01, not yet published); RPCS3 comparison pending |

Each phase ships as its own installable `.deb` and must pass the acceptance list below before the next starts.

## Decisions from the 2026-10-31 review

An outside review (another AI, given `notes/PkgView-review-brief.md`) was compared with Claude's own suggestions. Its findings were checked against the code before being adopted.

**Cut**
- **Pause/resume.** Extracting a 4 GB package takes about 5 s, so cancel plus clean deletion is enough.
- **Queue window with 1–4 workers and add-while-running.** Parallel writers to one disk don't help. The bulk summary gets Retry failed instead (F29).
- **Per-entry magic "Test integrity".** It's reduced to checking three known system files (F15).
- **Preferences dialog.** Settings live in the dialog that uses them (F14).
- **Accessibility as its own release.** It's now a standing requirement.
- **Desktop notification and CSV export.**

**Moved up:** metadata and preview move from 2.6 to 2.5, and JSON moves from 2.6 to 2.5.

**Added:** rename, audit, library view, `.rap` status, Files columns and Properties tab, `cat`, parallel debug keystream, install to RPCS3, DAT check.

**Platform:** the GTK floor moves from 4.8 to 4.14, which drops Debian 12 (GTK 4.8). Both reviews recommended it, since daily use is on GTK 4.22. Reverse this if Debian 12 users turn up.

**Review points checked and rejected or corrected:**
- *"Stale `<pid>` temp folders accumulate after a crash."* They don't: `clean_stale_open_dirs()` already removes them at startup. Only the delete-at-exit half was kept (F19).
- *"The block counter may be truncated to 32 bits above 4 GiB."* It isn't; the code uses `u64`. The sparse-file test is still added as a guard.
- *"Duplicate entry paths could corrupt output."* They can't: they're already renamed through the `taken` set.
- *"Dropping Debian 12 lets the Nautilus API 4.0 shim go."* It doesn't: Ubuntu 24.04's Nautilus 46 still uses API 4.0, so the fallback stays.
- *"Index folders at load to fix O(entries) listing."* Not needed: even 30k entries scan in well under a millisecond. The `ponytail:` ceiling note stays.
- *"Superseded patch detection."* Dropped: whether PS3 patches are cumulative varies by title, and there's no reliable signal for it.
- *Claims about RPCS3's installer speed and `0x90` entries in PS1 Classics.* These weren't verified. F30 and the `0x90` check both require confirming against RPCS3's source or a real package first.

**Confirmed and adopted as Must:**
- The symlink write-through. `extract_file` uses `File::create` and `create_dir_all`, both of which follow symlinks.
- The exit-code wrap trap in the old 2.5.0 plan ("exit code = number of failures").

## Success metrics and acceptance

Success means every package task can be done from Files or PkgView, a folder of packages can be audited and named in one step, and extraction never differs from the original.

A release ships only when all of these hold:

- [ ] `make test` and `make lint` green; new parsing logic has unit tests and goes through the fuzz test
- [ ] `make realtest` passes on every real sample (extract, verify, output deleted after)
- [ ] `make compare`: retail identical to the original; debug differs only in `0x90` (or matches once settled from RPCS3)
- [ ] Every new visible feature exercised in the Xvfb harness and its screenshots inspected; untested paths reported as “built, not exercised”
- [ ] `.deb` installs cleanly in fresh containers of every supported base (`debian:12` + `ubuntu:24.04` through 2.3.x; `debian:13` + `ubuntu:24.04` from 2.4.0) and runs `--version` + `info`
- [ ] `.rpm` installs in `fedora:43` and `opensuse/tumbleweed`; `make arch` builds, passes namcap and installs in `archlinux` (from 2.4.0)
- [ ] Rename and audit tested on a scratch copy of a folder, never on the real collection
- [ ] Version bumped, metainfo release added and validated, README and this PRD updated
- [ ] One git commit per phase
- [ ] Installed on a real GNOME desktop; thumbnails, the Files menu and (from 2.6.0) the Files columns checked by hand
- [ ] Ifcaro still credited everywhere listed under Attribution
- [ ] Commits use the no-reply address (`git log --format=%ae`); only `main` and tags are pushed, never the local `history` branch (it has the private email)
- [ ] GitHub release in the same format as Kestrel and FinGraph: title "PkgView X.Y.Z", credit line, **What's new**, **Install** table (System / File / Command) with the Arch block, apt-not-dpkg note, **Not supported**, "Tested by installing on …"; marked Latest
- [ ] The uploaded `.deb` and `.rpm` are exactly the tested files (the PKGBUILD's sha256 matches the uploaded `.deb`), and the README's Arch commands work from GitHub in a fresh `archlinux` container

## Risks and open questions

The biggest risk is still narrow sample coverage: every real package so far is a debug PS3 PS1 Classic, so retail and PSP handling is proven only against the original's code, not against real files. The metadata, `.rap` and RPCS3 features add more format guesses on top of that.

| Risk / question | Impact | Mitigation or next step |
| --- | --- | --- |
| No retail PS3, retail PSP or >4 GB real sample | A format misreading could go unnoticed for those types | Matches the original's code byte for byte; sparse >4 GiB test (2.4.0); add samples to `make realtest` when available |
| Debug pkgs with `0x90` entries: the port keeps the SHA-1 scheme, the original used the PSP AES key | One entry type may extract differently from the original | Settled 2026-10-31: RPCS3 does the same as the port |
| PSP-key choice differs from RPCS3: bit `0x10000000` vs top byte `0x90`; RPCS3 also decrypts PSP entries' names with the PSP key in retail PS3 pkgs | A retail pkg with `0x10…`/`0x30…` PSP entries would extract garbled, or show garbled names | `verify`'s header check flags garbled known files; switch to RPCS3's rule if a real pkg shows it (would break byte-identity with the original, so only with evidence) |
| Real samples are gone: the 3 debug PS3 pkgs used for 2.1–2.3 are no longer in `~/Downloads`, and the TV box share was offline on 2026-11-01 | `make realtest` / `make compare SAMPLES=` could not run for 2.4.0 | Find or re-download them before 2.5.0's metadata work; synthetic + original-code compare still pass |
| Metadata block, DRM type and SFO category codes come from RPCS3's source (psdevwiki unreachable) | Wrong labels in the inspector, `info` and (2.6.0) rename/audit | DRM names marked *unverified* in the UI; README lists metadata decoding as UNVERIFIED on a real pkg; check with the first real sample that has metadata |
| Rename touches the user's real files | A bad template or bug renames a collection wrongly | Preview before anything changes, each rename logged before the next, undo, never overwrites (renameat2 NOREPLACE); tested on synthetic collections only |
| Kind detection relies on PARAM.SFO CATEGORY and metadata flags; DLC without PARAM.SFO needs metadata | An add-on may show as "Other", a patch as "Game" | Classifier unit-tested; check against real patches/DLC when samples exist |
| Install to RPCS3 may not match what RPCS3's own installer does (DLC placement, PS1 Classics) | Games that don't boot in RPCS3 | Rules taken from RPCS3's source and unit-tested; `make compare-rpcs3` (RPCS3 vs PkgView on the test packages) written but not run; run it, and once with a real package, before publishing 2.7.0 |
| RPCS3 changes its installer (new content types, folders) | Installs drift from RPCS3's | Re-read `unpkg.cpp` and re-run `make compare-rpcs3` when updating |
| Raising the floor to GTK 4.14 drops Debian 12 | Debian 12 users have no package (2.3.1 was never published as a download) | Accepted 2026-10-31; they can build commit `4bfd30c` (2.3.1) from source. Publish a 2.3.1 `.deb` only if someone asks |
| The AUR doesn't have `pkgview-bin`: registration closed and no account (2026-11-01) | Arch users must clone the repo and run `makepkg -si` | README and release notes say so; upload `packaging/aur` (PKGBUILD + `.SRCINFO`) when registration reopens |
| Drag-out extracts at drag start (GTK 4 has no XDS) | UI pauses on large drags | 512 MB cap with “Use Extract” message |
| Nautilus extension API differs between Nautilus 47+ (API 4.1) and 43–46 (API 4.0, incl. Ubuntu 24.04) | Menu items or columns missing on one of them | The extension asks for 4.1 and falls back to 4.0; test columns under both in 2.6.0 |
| Files columns call `pkgview info --json` per file | Slow listing in a folder of hundreds of pkgs | Runs in the background (async subprocess), cached by path + mtime + size; checked in Nautilus 46 |
| Dropping files from Files onto PkgView not yet tested (harness cannot fake an external drag) | Bulk-by-drop could fail silently | Manual test in Files |
