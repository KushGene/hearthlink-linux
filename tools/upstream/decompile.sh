#!/usr/bin/env bash
# Decompile single types of a .NET assembly to C#, only when the brief says
# "decompile this method". Uses ilspycmd from PATH, or else in a podman/docker
# container (the image is built once and kept; no .NET install on the host).
#
# Usage: tools/upstream/decompile.sh ASSEMBLY.dll OUTDIR TYPE [TYPE...]
#   TYPE = full name as in the brief, e.g. Wowhead.Client.Core.Managers.SyncManager
set -euo pipefail
[ $# -ge 3 ] || { sed -n '2,8p' "$0"; exit 3; }
dll=$(realpath "$1"); out=$(realpath -m "$2"); shift 2
mkdir -p "$out"
image=localhost/hearthlink-ilspycmd:9

run() {
    if command -v ilspycmd >/dev/null; then
        ilspycmd "$@"
        return
    fi
    eng=$(command -v podman || command -v docker) || { echo "need ilspycmd, podman or docker" >&2; exit 1; }
    if ! "$eng" image exists "$image" 2>/dev/null && ! "$eng" image inspect "$image" >/dev/null 2>&1; then
        printf 'FROM mcr.microsoft.com/dotnet/sdk:8.0\nRUN dotnet tool install -g ilspycmd --version 9.*\nENV PATH=$PATH:/root/.dotnet/tools\nENTRYPOINT ["ilspycmd"]\n' \
            | "$eng" build -t "$image" -f - >&2
    fi
    "$eng" run --rm --network=none -v "$(dirname "$dll"):/in:ro,Z" -v "$out:/out:Z" "$image" "$@"
}

name=$(basename "$dll")
for t in "$@"; do
    t=${t%%::*}; t=${t%%/*}   # Type::Method or Outer/<Gen>d__N -> outer type
    file="$out/${t##*.}.cs"
    if command -v ilspycmd >/dev/null; then
        run -t "$t" "$dll" > "$file"
    else
        run -t "$t" "/in/$name" > "$file"
    fi
    echo "$file"
done
