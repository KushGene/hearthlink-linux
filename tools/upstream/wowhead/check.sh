#!/usr/bin/env bash
# Is there a new Windows Wowhead Client, and what in it matters for the port?
#
#   tools/upstream/wowhead/check.sh            check; on a new version download,
#                                              snapshot and write brief.md
#   tools/upstream/wowhead/check.sh --force    the same for the ported version too
#   tools/upstream/wowhead/check.sh --bump     verdict 0: also set ported_version
#                                              in src/services/wowhead.rs and
#                                              move the baseline
#   tools/upstream/wowhead/check.sh --accept   after porting: make the new snapshot
#                                              the baseline
#
# Prints key=value lines (status, upstream, ported, verdict, brief, dir).
# status=uptodate needs no work at all. verdict: 0 = only the version changed,
# 1 = changes outside the watch list, 2 = watched changes (see brief.md).
# Needs python3 and uv (for dnfile). Work files go to
# ${HEARTHLINK_UPSTREAM_DIR:-~/.cache/hearthlink-upstream}/wowhead/VERSION/.
set -euo pipefail
here=$(cd "$(dirname "$0")" && pwd)
root=$(cd "$here/../../.." && pwd)
force= bump= accept=
for a in "$@"; do
    case $a in
        --force) force=--force ;;
        --bump) bump=1 ;;
        --accept) accept=1 ;;
        *) sed -n '2,18p' "$0"; exit 3 ;;
    esac
done
base=${HEARTHLINK_UPSTREAM_DIR:-$HOME/.cache/hearthlink-upstream}/wowhead
mkdir -p "$base"
dl=$(mktemp -d "$base/dl.XXXXXX")
trap 'rm -rf "$dl"' EXIT

info=$(python3 -I "$here/fetch.py" "$dl" $force)
field() { python3 -I -c 'import json,sys; v=json.loads(sys.argv[1]).get(sys.argv[2]); print("" if v is None else str(v).lower() if isinstance(v,bool) else v)' "$info" "$1"; }
upstream=$(field upstream); ported=$(field ported)
echo "upstream=$upstream"
echo "ported=$ported"
if [ -f "$here/baseline.json.gz" ] && [ -z "$accept" ]; then
    baseline=$(python3 -I -c 'import gzip,json,sys; print(json.load(gzip.open(sys.argv[1],"rt"))["version"])' "$here/baseline.json.gz")
    if [ "$baseline" != "$ported" ]; then
        echo "baseline is $baseline but src/services/wowhead.rs ports $ported:" >&2
        echo "after porting, run with --force --accept so both match" >&2
        exit 4
    fi
fi
if [ -z "$(field exe)" ]; then
    echo "status=uptodate"
    exit 0
fi

dir="$base/$upstream"
rm -rf "$dir"; mkdir -p "$dir"
mv "$(field exe)" "$dir/WowheadClient.exe"
uv run -q "$root/tools/upstream/dotnet_snapshot.py" "$dir/WowheadClient.exe" "$dir/snapshot.json" \
    --include '^wowhead' --app "Wowhead Client" --extract "$dir/dll" >&2

verdict=none
if [ -f "$here/baseline.json.gz" ]; then
    set +e
    python3 -I "$root/tools/upstream/compare.py" "$here/baseline.json.gz" "$dir/snapshot.json" \
        --watch "$here/watch.txt" --out "$dir/brief.md" --types-out "$dir/types.tsv"
    verdict=$?
    set -e
    if [ $verdict -eq 2 ] && [ -s "$dir/types.tsv" ]; then
        {
            echo
            echo "## If strings/calls are not enough"
            echo
            echo "Decompile only the type you need (C#, a few KB each):"
            echo
            while IFS=$'\t' read -r asm type; do
                dll="$dir/WowheadClient.exe"
                [ "$asm" = WowheadClient ] || dll="$dir/dll/$(echo "$asm" | tr 'A-Z' 'a-z').dll"
                echo "    tools/upstream/decompile.sh '$dll' '$dir/cs' $type"
            done < "$dir/types.tsv"
        } >> "$dir/brief.md"
    fi
fi
[ "$verdict" != none ] || { [ -n "$accept" ] || { echo "no baseline yet: run with --force --accept" >&2; exit 3; }; verdict=0; }
[ $verdict -le 2 ] || exit $verdict
echo "status=new"
echo "verdict=$verdict"
echo "brief=$dir/brief.md"
echo "dir=$dir"

if [ -n "$bump" ] && [ $verdict -eq 0 ] && [ "$upstream" != "$ported" ]; then
    sed -i "s/$ported/$upstream/g" "$root/src/services/wowhead.rs"
    echo "bumped=src/services/wowhead.rs"
    accept=1  # nothing to port, so this version is the new baseline
fi
if [ -n "$accept" ]; then
    gzip -9n < "$dir/snapshot.json" > "$here/baseline.json.gz"
    echo "accepted=$upstream"
fi
