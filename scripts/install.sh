#!/bin/bash
# Build the release and install it for everyday use: the app in lutris-applications/umber, its
# icons beside it, and a menu launcher.
#
#   scripts/install.sh
#
# The launcher names its icon "umber" and the user's icon theme links to the files here, rather
# than pointing at a file path: GNOME keeps an icon it has loaded from a path and never notices
# the file changing, but it does reload icons from the theme when the theme changes.
set -euo pipefail
cd "$(dirname "$0")/.."

DEST=/mnt/109313D2109313D2/lutris-applications/umber
THEME=$HOME/.local/share/icons/hicolor
LAUNCHER=$HOME/.local/share/applications/umber.desktop

cargo build --release
mkdir -p "$DEST/icons"
# Copy then rename, so a running Umber keeps its own file.
cp target/release/umber "$DEST/umber.new"
mv -f "$DEST/umber.new" "$DEST/umber"

cp assets/icon/umber.svg "$DEST/icons/umber.svg"
mkdir -p "$THEME/scalable/apps"
ln -sfn "$DEST/icons/umber.svg" "$THEME/scalable/apps/umber.svg"
for s in 16 24 32 48 64 128 256 512; do
    # The small cut has a heavier stroke that still shows at taskbar sizes.
    src=assets/icon/umber.svg
    [ "$s" -le 32 ] && src=assets/icon/umber-small.svg
    rsvg-convert -w "$s" -h "$s" "$src" -o "$DEST/icons/umber-$s.png"
    mkdir -p "$THEME/${s}x${s}/apps"
    ln -sfn "$DEST/icons/umber-$s.png" "$THEME/${s}x${s}/apps/umber.png"
done
[ -f "$THEME/index.theme" ] || cp /usr/share/icons/hicolor/index.theme "$THEME/"

mkdir -p "$(dirname "$LAUNCHER")"
cat > "$LAUNCHER" <<EOF
[Desktop Entry]
Type=Application
Name=Umber
GenericName=Texture Painter
Comment=UV unwrapping and texture painting for game models
Exec="$DEST/umber" %f
Icon=umber
Terminal=false
StartupWMClass=umber
Categories=Graphics;3DGraphics;
Keywords=uv;unwrap;texture;paint;pbr;godot;
EOF

# Tell the desktop the theme and the launcher changed.
touch "$THEME"
gtk-update-icon-cache -f -t "$THEME" >/dev/null 2>&1 || true
update-desktop-database "$(dirname "$LAUNCHER")" 2>/dev/null || true
echo "Installed $(git log --oneline -1 | cut -c1-8) to $DEST/umber"
