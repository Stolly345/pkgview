#!/bin/sh
# GUI smoke run under Xvfb (inside the build image: `make gui`). Drives PkgView with xdotool and writes one
# screenshot per step to target/gui/NN-step.png; inspect them by eye. Synthetic packages only.
# Usage: tools/gui-harness.sh [step…]   (default: all steps)
set -eu
B=${BIN:-target/debug/pkgview}
OUT=target/gui
W=$(mktemp -d)
export HOME=$W GDK_BACKEND=x11 DISPLAY=:99 NO_AT_BRIDGE=1 GSETTINGS_BACKEND=memory
rm -rf "$OUT" && mkdir -p "$OUT"
Xvfb :99 -screen 0 1280x800x24 >/dev/null 2>&1 &
XV=$!
trap 'rc=$?; kill $XV 2>/dev/null; pkill -f "$B" 2>/dev/null; rm -rf "$W" 2>/dev/null; exit $rc' EXIT
sleep 1

PKGVIEW_WRITE_SAMPLE=$W/demo.pkg PKGVIEW_SAMPLE_DEMO=1 cargo test -q write_sample -- --ignored >/dev/null 2>&1
n=0
shot() { n=$((n + 1)); sleep "${2:-1}"; import -window root "$OUT/$(printf %02d $n)-$1.png"; echo "shot $n: $1"; }
key() { xdotool key --clearmodifiers "$@"; sleep 0.6; }
win() { xdotool search --sync --onlyvisible --name "$1" | tail -1; }
focus() { xdotool windowactivate --sync "$(win "$1")" 2>/dev/null || xdotool windowfocus --sync "$(win "$1")"; sleep 0.3; }

"$B" "$W/demo.pkg" 2>>"$OUT/stderr.log" &
win "PkgView" >/dev/null
shot main 2

# Tree: right-click a folder that is NOT selected; it must become selected (row under the pointer).
focus "PkgView"
eval "$(xdotool getwindowgeometry --shell "$(win PkgView)")"
xdotool mousemove $((X + 60)) $((Y + 140)) click 3
shot tree-right-click
key Escape

# Extract dialog (Ctrl+E): defaults, then extract to $W/out.
key ctrl+e
shot extract-dialog
focus "Extract from"
key ctrl+a
xdotool type --delay 20 "$W/out"
key Return
shot extracted 2
ls -R "$W/out" >"$OUT/extract-1.txt" 2>&1

# Same again: files exist, policy Ask → the "already exists" question.
focus "PkgView"
key ctrl+e
focus "Extract from"
key Return
shot ask-dialog 2
key Escape # Cancel = stop the extraction
shot after-cancel
cat "$W/out/demo/demo.log" >"$OUT/extract-2-log.txt" 2>&1 || true

# Search, Enter on a result: jumps to its folder with the file selected and scrolled into view.
focus "PkgView"
key ctrl+f
xdotool type --delay 20 "level2"
shot search-results
eval "$(xdotool getwindowgeometry --shell "$(win PkgView)")"
xdotool mousemove $((X + 300)) $((Y + 156)) click --repeat 2 --delay 120 1
shot search-jump

# Properties + Verify.
key ctrl+i
focus "Pkg properties"
key Return
shot verify 2
key Escape

# Bulk extract dialog (drop or File menu) is the same dialog; standalone --extract-to as well.
# Files-menu modes. --extract-to: the dialog; Escape must end the process.
"$B" --extract-to "$W/demo.pkg" 2>>"$OUT/stderr.log" &
P=$!
focus "Extract demo"
shot standalone-dialog
key Escape
sleep 1.5
kill -0 $P 2>/dev/null && echo "extract-to after Escape: STILL RUNNING" || echo "extract-to after Escape: exited"
# --extract-to again, Extract into a new folder: progress, then the process ends by itself.
"$B" --extract-to "$W/demo.pkg" 2>>"$OUT/stderr.log" &
P=$!
focus "Extract demo"
key ctrl+a
xdotool type --delay 20 "$W/standalone"
key Return
sleep 3
kill -0 $P 2>/dev/null && echo "extract-to after Extract: STILL RUNNING" || echo "extract-to after Extract: exited"
ls "$W/standalone/demo/NPUB12345" 2>&1 | tr '\n' ' '; echo
# --extract-here twice: the second run finds every file present and (policy Ask) asks; Replace All.
"$B" --extract-here "$W/demo.pkg" 2>>"$OUT/stderr.log"; echo "extract-here #1 exit $?"
"$B" --extract-here "$W/demo.pkg" 2>>"$OUT/stderr.log" &
P=$!
sleep 2
shot extract-here-ask
# The alert has no title; it is the visible window with an empty name. Its 6th button is Replace All.
for A in $(xdotool search --onlyvisible --name '^$' | sort -rn); do
    eval "$(xdotool getwindowgeometry --shell "$A")"
    [ "$WIDTH" -gt 200 ] && break # skip GTK's 1x1 helper windows
done
xdotool mousemove $((X + WIDTH - 40)) $((Y + HEIGHT - 16)) click 1 # last button, bottom right
shot extract-here-replaced 2
sleep 2
kill -0 $P 2>/dev/null && echo "extract-here #2: STILL RUNNING" || echo "extract-here #2: exited"
grep -c " OK$" "$W/demo/demo.log" | sed 's/^/extract-here #2 files written: /' 
cp "$W/.config/pkgview/settings.ini" "$OUT/settings.ini" 2>/dev/null || true
echo "done: $OUT"
exit 0
