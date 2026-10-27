#!/bin/sh
# Removes what install.sh installed. Settings stay in ~/.config/hearthlink and
# ~/.local/share/hearthlink; delete those folders too for a clean slate.
set -eu
bin="${XDG_BIN_HOME:-$HOME/.local/bin}"
data="${XDG_DATA_HOME:-$HOME/.local/share}"
id=io.github.kushgene.Hearthlink
rm -f "$bin/hearthlink" "$data/applications/$id.desktop" "$data/metainfo/$id.metainfo.xml" \
      "${XDG_CONFIG_HOME:-$HOME/.config}/autostart/hearthlink.desktop"
for s in 512 256 128 64; do rm -f "$data/icons/hicolor/${s}x${s}/apps/$id.png"; done
echo "Hearthlink removed."
