#!/bin/sh
# Renders the app icon in the sizes the packages install (target/icons/<size>/).
set -eu
cd "$(dirname "$0")/.."
for s in 512 256 128 64 32; do
    mkdir -p "target/icons/$s"
    if command -v magick >/dev/null 2>&1; then
        magick assets/icons/app.png -resize "${s}x${s}" "target/icons/$s/io.github.kushgene.Hearthlink.png"
    else
        convert assets/icons/app.png -resize "${s}x${s}" "target/icons/$s/io.github.kushgene.Hearthlink.png"
    fi
done
