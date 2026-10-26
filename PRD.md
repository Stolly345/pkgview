# PkgView — Product Requirements Document

2026-10-25 · Alex Stoll

## Overview

PkgView is a native Linux desktop app and CLI that browses, verifies and extracts Sony PS3/PSP `.pkg` installation packages; version 2.3.1 is the first public release. It is a Rust/GTK 4 rewrite of PkgView 1.3 (VB.NET/WinForms, 2010–2012, by Ifcaro), shipped as a `.deb` for Debian 12+ and Ubuntu 24.04+.

This PRD fixes what the product must do through 2.7.0: the features already shipped, the ones planned, and the bar each release must clear. Implementation detail lives in `README.md` and the code's tests.

## Problem and goals

On Linux the original Windows tool only runs under Mono: slow, one file at a time, no desktop integration. PkgView should feel like a normal archive manager (File Roller, Ark, 7-Zip) for one format.

**Goals**

- Extract any supported `.pkg` byte-for-byte the same as the original tool, faster.
- Make everyday archive work quick: browse, search, open a file, drag it out, extract many packages at once.
- Fit GNOME: open from Files, icon thumbnails, right-click Extract Here / Extract To.
- Never damage input: packages are opened read-only and treated as untrusted.

**Non-goals** (decided 2026-10-25)

- Creating or editing packages, debug or retail.
- PS Vita, PS4 or PS5 packages; EDAT/RAP decryption.
- Packaging beyond the `.deb` (translations, Flatpak/AppImage, CI) until there is demand.
- Tabs, skins, cloud or URL opening.
- Split or multi-part packages, unless a real sample turns up.

## Users and use cases

Primary user: someone on a GNOME desktop (Debian 12+ or Ubuntu 24.04+) with PS3/PSP packages, for example PS1 Classics of 60–365 MB. Comfortable with a terminal, but wants the GUI for daily use.

| # | Use case | Today (2.2.0) |
| --- | --- | --- |
| 1 | Double-click a `.pkg` in Files and see what is inside | Supported |
| 2 | Find one file (e.g. `ICON0.PNG`, `PARAM.SFO`) and open or drag it out | Supported |
| 3 | Extract a whole package, or a few folders, to a chosen place | Supported (options in 2.4.0) |
| 4 | Extract a folder of packages in one go and see which failed | Supported (queue window in 2.5.0) |
| 5 | Check a download is intact (SHA-1) before using it | Supported |
| 6 | Right-click packages in Files → Extract Here, with thumbnails of game icons | Supported (2.3.0) |
| 7 | Read a package's title, version and content type without extracting | Partly (PARAM.SFO); full in 2.6.0 |
| 8 | Script it: `info`, `list`, `extract`, `verify` from the shell | Supported; JSON output in 2.6.0 |

## Functional requirements

Thirteen requirement groups shipped in 2.0–2.3; the rest land one release at a time through 2.7.0. Must = needed for daily use; Should = clear value; Could = only if earlier phases leave the code clean.

| ID | Area | Requirement | Priority | Release | Status |
| --- | --- | --- | --- | --- | --- |
| F1 | Core | Open debug PS3, retail PS3 and retail PSP pkgs; list entries; decrypt any entry with O(1) seeking | Must | 2.0.0 | Shipped |
| F2 | Core | Extract all, a folder or a selection to `DIR/<pkg>/<root>/…` with a per-pkg log, matching the original's layout | Must | 2.0.0 | Shipped |
| F3 | Core | Verify the trailing SHA-1 | Must | 2.0.0 | Shipped |
| F4 | Browse | Folder tree + four views (big icons, small icons, list, details); properties with header, PARAM.SFO and ICON0 | Must | 2.0.0 | Shipped |
| F5 | CLI | `info`, `list`, `extract`, `verify`, original `-ex` switch | Must | 2.0.0 | Shipped |
| F6 | Desktop | `.deb`, MIME type with magic match, desktop entry, default-handler action | Must | 2.0.0 | Shipped |
| F7 | Bulk | Bulk extract several pkgs to one folder; failures reported at the end, the rest continue | Must | 2.1.0 | Shipped |
| F8 | Browse | Sortable columns; optional Offset, Key, Flags columns, remembered | Should | 2.2.0 | Shipped |
| F9 | Browse | Search by substring or `*`/`?` wildcard across all folders; jump to result | Must | 2.2.0 | Shipped |
| F10 | Browse | Path bar, back/forward/up, keyboard shortcuts, recent files, welcome page, selection size | Should | 2.2.0 | Shipped |
| F11 | Browse | Open a file with its default app; drag files out (512 MB cap); drop pkgs or a folder to bulk-extract | Must | 2.2.0 | Shipped |
| F12 | Files | Thumbnailer showing ICON0.PNG; Nautilus menu Extract Here / Extract To… / Open; GUI-only progress mode for those | Must | 2.3.0 | Shipped |
| F13 | Docs | Man page and bash/zsh/fish completions | Could | 2.3.0 | Shipped |
| F14 | Extract | Extract dialog: destination + recent, create pkg folder, keep structure, open when done, overwrite policy (Ask/Skip/Replace/Rename) | Must | 2.4.0 | Planned |
| F15 | Extract | Free-space check, pause/resume, desktop notification, Test integrity (magic checks per entry) | Should | 2.4.0 | Planned |
| F16 | Bulk | Queue window: per-package status, add while running, retry failed, 1–4 workers; CLI multi-pkg extract/verify with exit code = failures | Must | 2.5.0 | Planned |
| F17 | Info | Preview pane (image, text, hex, PARAM.SFO table); metadata section (content type, DRM, version); all SFO keys; export list as CSV/JSON | Should | 2.6.0 | Planned |
| F18 | Polish | Preferences dialog; accessibility pass (labels, focus order, 200% text) | Should | 2.7.0 | Planned |
| F19 | CLI | `pkgview diff A.pkg B.pkg` (added / removed / changed entries) | Could | 2.7.0 | Optional |

## Format and correctness

Output must match the original tool byte for byte on every retail package and every real sample; the only allowed difference is the documented debug `0x90` key choice.

- **Format:** big-endian header (`\x7FPKG`, type, metadata offset/count, item count, sizes, data offset, content ID, QA digest, data RIV), a 32-byte-per-entry item table, SHA-1 over `[0, EOF−32)`.
- **Crypto:** retail = AES-128 counter mode from the data RIV (PS3 or PSP key; entries with flags `0x90xxxxxx` use the PSP key); debug = SHA-1 of a 64-byte counter built from the QA digest.
- **Untrusted input:** reject `..`, absolute paths, NUL and `\` in names; bounds-check every offset and size; cap allocations; never panic on corrupt data (fuzz-tested).
- **Read-only:** packages are never opened for writing; a cancelled or failed file is deleted, never left half-written.

| Check | How | Result at 2.2.0 |
| --- | --- | --- |
| Unit tests | 13 tests: crypto round-trip for all 3 types, wrong key, malicious input, fuzz, options, search, history | Pass |
| Original code | `make compare`: the original VB crypto compiled under Mono vs the port, synthetic pkgs of each type | Retail identical; debug differs only in the `0x90` entry |
| Real packages | `make realtest` + `make compare SAMPLES=…` on 3 debug PS3 pkgs | SHA-1 OK, TITLE\_ID matches, identical to the original's output |

## Non-functional requirements

| Area | Requirement |
| --- | --- |
| Platform | Linux x86-64; `.deb` installs on Debian 12+ and Ubuntu 24.04+; GTK ≥ 4.8 API floor; built in a Debian 12 container (host has no Rust toolchain) |
| Performance | Retail extract ≥ 800 MB/s from page cache (measured 1 GiB in 1.2 s); debug 1 GiB in ≤ 8 s (measured 7.2 s); SHA-1 verify 1 GiB in < 1 s; thumbnail of the largest pkg < 1 s |
| Responsiveness | No GTK main-thread I/O over \~50 ms; extraction, open-file and verify run on worker threads with progress and cancel |
| Security | Read-only input, path sanitising, bounds checks, no shell in subprocess calls, temp files only under `~/.cache/pkgview/open/<pid>/` and removed on exit |
| Reliability | One broken package never stops a bulk run; failures name the package and reason; partial files removed |
| Accessibility | Every action reachable by keyboard; tooltips on icon buttons; readable at 200% text scale (2.7.0) |
| Dependencies | Rust std, gtk4-rs (with glib/gio/gdk-pixbuf), aes, sha1, clap, anyhow, libc; new crates need a stated reason (allowed: `clap_complete`, `clap_mangen`) |
| Settings | One file, `~/.config/pkgview/settings.ini` (view, columns, recent files, later extract options) |

## Release plan and roadmap

| Release | Phase | Scope | Status |
| --- | --- | --- | --- |
| 2.0.0 | — | Rust/GTK4 port, CLI, .deb, MIME | Shipped |
| 2.1.0 | — | Bulk extract to one destination | Shipped |
| 2.1.1 | A | Code split, extract options, real-pkg tests | Shipped |
| 2.2.0 | B | Search, sorting, open, drag out | Shipped |
| 2.3.0 | C | Thumbnails, Files menu, man page | Shipped |
| 2.3.1 | — | First public release: app ID, homepage, screenshots | Shipped |
| 2.4.0 | D | Extract dialog, overwrite rules | Next |
| 2.5.0 | E | Queue window, 1–4 workers | Planned |
| 2.6.0 | F | Preview pane, metadata, CSV/JSON | Planned |
| 2.7.0 | G | Preferences, accessibility, diff | Planned |

Each phase ships as its own installable `.deb` and must pass the acceptance list below before the next starts.

## Success metrics and acceptance

Success = every pkg task can be done from Files or PkgView, with zero extraction differences from the original.

A release ships only when all of these hold:

- [ ] `make test` and `make lint` green; new parsing logic has unit tests and goes through the fuzz test
- [ ] `make realtest` passes on every real sample (extract, verify, output deleted after)
- [ ] `make compare`: retail identical to the original; debug differs only in `0x90`
- [ ] Every new visible feature exercised in the Xvfb harness and its screenshots inspected; untested paths reported as “built, not exercised”
- [ ] `.deb` installs cleanly in fresh `debian:12` and `ubuntu:24.04` containers and runs `--version` + `info`
- [ ] Version bumped, metainfo release added and validated, README and this PRD updated
- [ ] One git commit per phase
- [ ] Installed on a real GNOME desktop; thumbnails and the Files menu checked by hand

## Risks and open questions

The biggest risk is narrow sample coverage: every real package so far is a debug PS3 PS1-Classic, so retail and PSP handling is proven only against the original's code, not against real files.

| Risk / question | Impact | Mitigation or next step |
| --- | --- | --- |
| No retail PS3, retail PSP or >4 GB real sample | A format misreading could go unnoticed for those types | Matches the original's code byte for byte; add samples to `make realtest` when available |
| Debug pkgs with `0x90` entries: the port keeps the SHA-1 scheme, the original used the PSP AES key | One entry type may extract differently from the original | Documented deviation; settle with a real sample if one appears |
| Metadata section layout (2.6.0) comes from psdevwiki | Wrong content type / DRM labels | Validate against real pkgs; mark unconfirmed fields UNVERIFIED |
| GTK 4.8 floor (no `FileDialog`, no row hit-test, no scroll-to) | Some UI is clumsier than on GTK 4.12+ | Workarounds noted with `ponytail:` comments; raise the floor if Debian 12 support stops mattering |
| Drag-out extracts at drag start (GTK 4 has no XDS) | UI pauses on large drags | 512 MB cap with “Use Extract” message |
| Nautilus extension API differs between Nautilus 47+ (API 4.1) and Debian 12's Nautilus 43 (API 4.0) | Right-click items missing on one of them | The extension asks for 4.1 and falls back to 4.0; tested under both in 2.3.0 |
| Dropping files from Files onto PkgView not yet tested (harness cannot fake an external drag) | Bulk-by-drop could fail silently | Manual test in Files |
