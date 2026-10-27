#!/bin/sh
# Installs Hearthlink for the current user (no root needed): ~/.local/bin and the
# desktop menu. Run it from the unpacked release folder.
set -eu
here="$(cd "$(dirname "$0")" && pwd)"
bin="${XDG_BIN_HOME:-$HOME/.local/bin}"
data="${XDG_DATA_HOME:-$HOME/.local/share}"
id=io.github.kushgene.Hearthlink

install -Dm755 "$here/hearthlink" "$bin/hearthlink"
sed "s|^Exec=hearthlink|Exec=$bin/hearthlink|" "$here/$id.desktop" > "$here/.desktop.tmp"
install -Dm644 "$here/.desktop.tmp" "$data/applications/$id.desktop"
rm -f "$here/.desktop.tmp"
install -Dm644 "$here/$id.metainfo.xml" "$data/metainfo/$id.metainfo.xml"
for s in 512 256 128 64; do
    install -Dm644 "$here/icons/$s/$id.png" "$data/icons/hicolor/${s}x${s}/apps/$id.png"
done
command -v update-desktop-database >/dev/null 2>&1 && update-desktop-database -q "$data/applications" || true
command -v gtk-update-icon-cache >/dev/null 2>&1 && gtk-update-icon-cache -q -t "$data/icons/hicolor" || true
echo "Hearthlink installed. Start it from your app menu or with: $bin/hearthlink"
