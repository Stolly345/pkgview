#!/bin/sh
# `make arch`: point packaging/aur/PKGBUILD at the current .deb (pkgver, sha256sums), then in an archlinux
# container: makepkg with the local .deb standing in for the release download, namcap, pacman -U, run it,
# and write packaging/aur/.SRCINFO. The built package lands in target/arch/.
set -eu
cd "$(dirname "$0")/.."
ver=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
deb=target/debian/pkgview_${ver}-1_amd64.deb
[ -f "$deb" ] || { echo "no $deb: run make deb first" >&2; exit 1; }
sum=$(sha256sum "$deb" | cut -d' ' -f1)
sed -i "s/^pkgver=.*/pkgver=$ver/; s/^pkgrel=.*/pkgrel=1/; s/^sha256sums=.*/sha256sums=('$sum')/" packaging/aur/PKGBUILD

W=target/arch
rm -rf "$W" && mkdir -p "$W"
cp packaging/aur/PKGBUILD "$deb" "$W/"
./run-in-docker.sh env PKGVIEW_WRITE_SAMPLE=/w/$W/sample.pkg cargo test -q write_sample -- --ignored >/dev/null

docker run --rm -v "$PWD/$W:/b" archlinux:latest sh -euc '
    pacman -Syu --noconfirm --needed base-devel namcap >/dev/null 2>&1
    useradd -m builder && chown -R builder /b && cd /b
    . ./PKGBUILD && pacman -S --noconfirm --needed "${depends[@]}" >/dev/null 2>&1
    su builder -c "makepkg -f --noconfirm >makepkg.log 2>&1 && makepkg --printsrcinfo >.SRCINFO" \
        || { tail -20 makepkg.log; exit 1; }
    echo "== namcap PKGBUILD";  namcap PKGBUILD || true
    echo "== namcap package";   namcap pkgview-bin-*.pkg.tar.zst || true
    pacman -U --noconfirm pkgview-bin-*.pkg.tar.zst >/dev/null
    echo "== installed";        pkgview --version && pkgview verify sample.pkg
    pacman -Ql pkgview-bin | grep -E "licenses|site-functions|/bin/|vendor-completions|copyright" || true
    chown -R '"$(id -u):$(id -g)"' /b
'
cp "$W/.SRCINFO" packaging/aur/.SRCINFO
ls -1 "$W"/*.pkg.tar.zst
