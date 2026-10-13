#!/bin/sh
# Build dist/orbit_<version>_<arch>.deb (and a tar.gz of the binaries).
# Everything is staged and packed with --root-owner-group, so the root-run scan
# helper is root-owned in the package and cannot be replaced by a normal user.
set -e
cd "$(dirname "$0")/.."

VERSION="$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)"
ARCH="$(dpkg --print-architecture)"
PKG="orbit_${VERSION}_${ARCH}"
STAGE="dist/$PKG"

cargo build --release --workspace
rm -rf "$STAGE"
install -d "$STAGE/DEBIAN"

install -Dm755 target/release/orbit              "$STAGE/usr/bin/orbit"
install -Dm755 target/release/orbit-cli          "$STAGE/usr/bin/orbit-cli"
install -Dm755 target/release/orbit-scan-helper  "$STAGE/usr/lib/orbit/orbit-scan-helper"
install -Dm644 packaging/orbit.desktop           "$STAGE/usr/share/applications/orbit.desktop"
install -Dm644 packaging/io.github.atasoglu.orbit.policy \
    "$STAGE/usr/share/polkit-1/actions/io.github.atasoglu.orbit.policy"
install -Dm644 LICENSE "$STAGE/usr/share/doc/orbit/copyright"
for s in 16 24 32 48 64 128 256 512; do
    install -Dm644 "assets/icons/png/orbit-$s.png" "$STAGE/usr/share/icons/hicolor/${s}x${s}/apps/orbit.png"
done
install -Dm644 assets/icons/orbit.svg "$STAGE/usr/share/icons/hicolor/scalable/apps/orbit.svg"

SIZE="$(du -sk "$STAGE" | cut -f1)"
cat > "$STAGE/DEBIAN/control" <<CONTROL
Package: orbit
Version: $VERSION
Section: utils
Priority: optional
Architecture: $ARCH
Installed-Size: $SIZE
Depends: libc6 (>= 2.35), libfontconfig1, libfreetype6, libwayland-client0, libx11-6, libxcb1, libxkbcommon0
Recommends: pkexec | policykit-1
Maintainer: atasoglu <noreply@users.noreply.github.com>
Homepage: https://github.com/atasoglu/orbit
Description: Lightweight disk usage analyzer and cleaner
 Orbit shows disk usage as an interactive sunburst chart, finds caches and
 build artifacts that are safe to remove, and lists the largest files.
 It renders on the CPU, so it needs no GPU.
CONTROL

dpkg-deb --root-owner-group --build "$STAGE" "dist/$PKG.deb" >/dev/null
tar -C target/release -czf "dist/orbit-${VERSION}-linux-${ARCH}.tar.gz" orbit orbit-cli orbit-scan-helper
rm -rf "$STAGE"
echo "dist/$PKG.deb"
echo "dist/orbit-${VERSION}-linux-${ARCH}.tar.gz"
