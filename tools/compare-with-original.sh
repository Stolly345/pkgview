#!/bin/sh
# Extract synthetic pkgs with the ORIGINAL PkgView crypto code (../PkgView/{pkgmgr,AESEngine,Archivo}.vb,
# compiled with Mono's vbnc) and with this port, then diff the outputs.
# Two mechanical edits are needed for Mono, neither changes behaviour vs. Microsoft's VB runtime:
#   Array.Resize(buf2, …) → Array.Resize(Of Byte)(buf2, …)   vbnc cannot infer T
#   "&H…" string literals → &H…UI numeric literals          Mono's VB runtime cannot parse hex strings
# Expected: retail types identical; debug differs only in PSP/DATA.PSP (0x90 entry, see README "Differences").
# With SAMPLES=/folder, every real *.pkg in it (mounted read-only) is also extracted by both and diffed.
# The original steps its counter block by block, so this takes minutes per few hundred MB.
set -e
cd "$(dirname "$0")/.."
rm -rf .cmp && mkdir .cmp
cp ../PkgView/pkgmgr.vb ../PkgView/AESEngine.vb ../PkgView/Archivo.vb tools/Harness.vb .cmp/
sed -i -E -e 's/Array\.Resize\(buf2, /Array.Resize(Of Byte)(buf2, /' -e 's/"&[hH]([0-9A-Fa-f]+)"/\&H\1UI/g' .cmp/*.vb
TYPES="00000001 80000001 80000002"
./run-in-docker.sh sh -c "cargo build -q --release && for T in $TYPES; do
  PKGVIEW_WRITE_SAMPLE=.cmp/s-\$T.pkg PKGVIEW_SAMPLE_TYPE=0x\$T cargo test -q --release write_sample -- --ignored >/dev/null
  target/release/pkgview extract .cmp/s-\$T.pkg -o .cmp/port >/dev/null 2>&1; done"
docker run --rm -u "$(id -u):$(id -g)" -e HOME=/tmp -v "$PWD/.cmp":/s -w /s mono:6.12 sh -c "
  vbnc -nologo -quiet -optionstrict- -optioninfer+ -main:EntryPoint -r:System.dll -out:oracle.exe \
    Harness.vb pkgmgr.vb AESEngine.vb Archivo.vb >/dev/null
  for T in $TYPES; do mkdir -p orig-\$T && mono oracle.exe s-\$T.pkg orig-\$T >/dev/null; done"
rc=0
if [ -n "$SAMPLES" ]; then
  ./run-in-docker.sh sh -c 'i=0; for f in /samples/*.pkg /samples/*.PKG; do [ -f "$f" ] || continue
    i=$((i+1)); target/release/pkgview extract "$f" -o .cmp/real-port-$i >/dev/null 2>&1; done'
  docker run --rm -u "$(id -u):$(id -g)" -e HOME=/tmp -v "$PWD/.cmp":/s -v "$SAMPLES:/samples:ro" -w /s mono:6.12 \
    sh -c 'i=0; for f in /samples/*.pkg /samples/*.PKG; do [ -f "$f" ] || continue
    i=$((i+1)); mkdir -p real-orig-$i && mono oracle.exe "$f" real-orig-$i >/dev/null; done'
  for o in .cmp/real-orig-*; do
    i=${o##*-}
    port=$(find ".cmp/real-port-$i" -mindepth 2 -maxdepth 2 -type d)
    if diff -r "$o/ROOT" "$port" >/dev/null; then echo "real sample $i: identical"
    else echo "real sample $i: DIFFERENT"; diff -rq "$o/ROOT" "$port" || true; rc=1; fi
  done
fi
for T in $TYPES; do
  if diff -r ".cmp/orig-$T/ROOT" ".cmp/port/s-$T/NPUB12345" >/dev/null; then
    echo "0x$T: identical"
  elif [ "$T" = 00000001 ] && [ "$(diff -rq ".cmp/orig-$T/ROOT" ".cmp/port/s-$T/NPUB12345")" = \
      "Files .cmp/orig-$T/ROOT/PSP/DATA.PSP and .cmp/port/s-$T/NPUB12345/PSP/DATA.PSP differ" ]; then
    echo "0x$T: identical except PSP/DATA.PSP (expected: debug 0x90 deviation)"
  else
    echo "0x$T: DIFFERENT"; diff -rq ".cmp/orig-$T/ROOT" ".cmp/port/s-$T/NPUB12345" || true; rc=1
  fi
done
exit $rc
