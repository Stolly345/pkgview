#!/bin/sh
# `make compare-rpcs3`: does `pkgview install-rpcs3` install exactly what RPCS3's own installer installs?
# Downloads the latest RPCS3 Linux AppImage (github.com/RPCS3/rpcs3-binaries-linux), then in the build
# container under Xvfb installs each synthetic test package twice into empty dev_hdd0 folders, once with
# `rpcs3 --installpkg` and once with PkgView, and compares the two game/ trees byte for byte.
# Exit 0 = identical for every package.
#
# STATUS: written for 2.7.0 but NOT RUN YET: running a downloaded emulator needs the owner's go-ahead.
# The first run may need adjusting (RPCS3 dialogs, timing); the output says which package differs and how.
set -eu
cd "$(dirname "$0")/.."
W=target/rpcs3cmp
rm -rf "$W" && mkdir -p "$W"
gh release download -R RPCS3/rpcs3-binaries-linux --pattern '*linux64.AppImage' -D "$W"
./run-in-docker.sh sh -c '
set -eu
W=target/rpcs3cmp
cargo build -q
(cd $W && chmod +x ./*.AppImage && ./*.AppImage --appimage-extract >/dev/null)
for t in 0x00000001 0x80000001 0x80000002; do
    env PKGVIEW_WRITE_SAMPLE=/w/$W/sample-$t.pkg PKGVIEW_SAMPLE_TYPE=$t PKGVIEW_SAMPLE_DEMO=1 \
        cargo test -q write_sample -- --ignored >/dev/null
done
export DISPLAY=:99 QT_QPA_PLATFORM=xcb
Xvfb :99 -screen 0 1280x800x24 >/dev/null 2>&1 &
sleep 1
rc=0
for p in $W/sample-*.pkg; do
    a=/tmp/by-rpcs3; b=/tmp/by-pkgview
    rm -rf $a $b
    mkdir -p $a/.config/rpcs3/dev_hdd0 $b/.config/rpcs3/dev_hdd0
    HOME=$a XDG_CONFIG_HOME=$a/.config $W/squashfs-root/AppRun --installpkg "$p" >$W/rpcs3.log 2>&1 &
    R=$!
    # Wait for the game folder, then until nothing changes for 4 s; press Return for RPCS3 dialogs.
    last=; same=0
    for _ in $(seq 90); do
        sleep 1
        xdotool key Return 2>/dev/null || true
        now=$(cd $a/.config/rpcs3/dev_hdd0 && find game -type f -exec stat -c "%n %s" {} + 2>/dev/null | sort | md5sum)
        if [ -d $a/.config/rpcs3/dev_hdd0/game ] && [ "$now" = "$last" ]; then same=$((same + 1)); else same=0; fi
        last=$now
        [ $same -ge 4 ] && break
    done
    kill $R 2>/dev/null || true
    HOME=$b XDG_CONFIG_HOME=$b/.config target/debug/pkgview install-rpcs3 "$p" >/dev/null
    if diff -r $a/.config/rpcs3/dev_hdd0/game $b/.config/rpcs3/dev_hdd0/game; then
        echo "$(basename $p): identical ($(find $b/.config/rpcs3/dev_hdd0/game -type f | wc -l) files)"
    else
        echo "$(basename $p): DIFFERENT (RPCS3 log: $W/rpcs3.log)"
        rc=1
    fi
done
exit $rc
'
