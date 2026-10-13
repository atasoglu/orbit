#!/bin/sh
# Install Orbit for the current user (no root needed): binary, icons, desktop entry.
set -e
cd "$(dirname "$0")/.."
PREFIX="${PREFIX:-$HOME/.local}"
ICONS="$PREFIX/share/icons/hicolor"

cargo build --release -p diskutil-app
install -Dm755 target/release/orbit "$PREFIX/bin/orbit"

for s in 16 24 32 48 64 128 256 512; do
    install -Dm644 "assets/icons/png/orbit-$s.png" "$ICONS/${s}x${s}/apps/orbit.png"
done
install -Dm644 assets/icons/orbit.svg "$ICONS/scalable/apps/orbit.svg"
install -Dm644 assets/icons/orbit-symbolic.svg "$ICONS/symbolic/apps/orbit-symbolic.svg"
install -Dm644 packaging/orbit.desktop "$PREFIX/share/applications/orbit.desktop"
# A per-user icon theme is not always picked up by the shell right away, so the
# user-level desktop entry points at the icon file directly and uses the full binary path.
sed -i "s|^Icon=.*|Icon=$ICONS/256x256/apps/orbit.png|; s|^Exec=.*|Exec=$PREFIX/bin/orbit|" "$PREFIX/share/applications/orbit.desktop"

gtk-update-icon-cache -qtf "$ICONS" 2>/dev/null || true
update-desktop-database -q "$PREFIX/share/applications" 2>/dev/null || true
echo "Installed to $PREFIX (make sure $PREFIX/bin is on PATH)"
