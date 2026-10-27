#!/bin/sh
# Builds Hearthlink-<version>-x86_64.AppImage from target/release/hearthlink.
# Needs appimagetool in PATH (https://github.com/AppImage/appimagetool/releases).
set -eu
cd "$(dirname "$0")/.."
id=io.github.kushgene.Hearthlink
version="$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)"
app=target/AppDir
rm -rf "$app"
install -Dm755 target/release/hearthlink "$app/usr/bin/hearthlink"
install -Dm644 packaging/$id.desktop "$app/usr/share/applications/$id.desktop"
install -Dm644 packaging/$id.metainfo.xml "$app/usr/share/metainfo/$id.appdata.xml"
for s in 512 256 128 64; do
    install -Dm644 "target/icons/$s/$id.png" "$app/usr/share/icons/hicolor/${s}x${s}/apps/$id.png"
done
cp packaging/$id.desktop "$app/$id.desktop"
cp "target/icons/256/$id.png" "$app/$id.png"
ln -s usr/bin/hearthlink "$app/AppRun"
ARCH=x86_64 appimagetool --no-appstream "$app" "target/Hearthlink-$version-x86_64.AppImage"
