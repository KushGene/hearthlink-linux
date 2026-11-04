"""Check the Windows Wowhead Client version and download it when it changed.

Reads the client version and MD5 from https://www.wowhead.com/client/update
(the file the client itself checks, ~1 KB), compares it with
`ported_version` in src/services/wowhead.rs, and only then downloads
WowheadClient.exe into OUTDIR and checks its MD5.

Prints one JSON line: {"ported", "upstream", "md5", "new", "exe"}.
Exit code 0 also when nothing is new; non-zero only on errors.

Usage: python3 -I tools/upstream/wowhead/fetch.py OUTDIR [--force]
"""

import argparse
import hashlib
import json
import os
import re
import sys
import urllib.request

UPDATE_URL = "https://www.wowhead.com/client/update"
EXE_URL = "https://www.wowhead.com/client/files/WowheadClient.exe"
PAGE_URL = "https://www.wowhead.com/client"
# The download answers an HTML page without a browser-like request.
BROWSER = {"User-Agent": "Mozilla/5.0 (Windows NT 10.0; Win64; x64)", "Referer": PAGE_URL}
ROOT = os.path.dirname(os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__)))))


def get(url, headers):
    req = urllib.request.Request(url, headers=headers)
    with urllib.request.urlopen(req, timeout=60) as r:
        return r.read()


def parse_update(d):
    """Same layout as parse_update() in src/services/wowhead.rs."""
    if len(d) < 48 or d[:4] != b"MTWC":
        raise ValueError("unknown update file format")
    p = 8
    client_hash = d[p:p + 32].decode("ascii", "replace").lower()
    p += 32 + 4
    count = max(0, int.from_bytes(d[p:p + 4], "little", signed=True))
    p += 4

    def cstr(p):
        end = d.index(b"\0", p)
        return d[p:end].decode("utf-8", "replace"), end + 1

    _, p = cstr(p)  # source
    _, p = cstr(p)  # client file
    for _ in range(count):
        p += 32 + 8
        _, p = cstr(p)
        _, p = cstr(p)
    version, _ = cstr(p)
    return version, client_hash


def ported_version():
    src = open(os.path.join(ROOT, "src/services/wowhead.rs"), encoding="utf-8").read()
    return re.search(r'ported_version:\s*"([^"]+)"', src).group(1)


def main():
    ap = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    ap.add_argument("outdir")
    ap.add_argument("--force", action="store_true", help="download even if the version is the ported one")
    args = ap.parse_args()

    version, md5 = parse_update(get(UPDATE_URL, {"User-Agent": "hearthlink-upstream-check"}))
    ported = ported_version()
    info = {"ported": ported, "upstream": version, "md5": md5, "new": version != ported, "exe": None}
    if info["new"] or args.force:
        os.makedirs(args.outdir, exist_ok=True)
        data = get(EXE_URL, BROWSER)
        got = hashlib.md5(data).hexdigest()
        if data[:2] != b"MZ":
            sys.exit(f"download is not a Windows program (got {data[:40]!r})")
        if got != md5:
            print(f"warning: MD5 {got} differs from the update file's {md5}", file=sys.stderr)
        exe = os.path.join(args.outdir, f"WowheadClient-{version}.exe")
        with open(exe, "wb") as f:
            f.write(data)
        info["exe"] = exe
        info["md5_ok"] = got == md5
    print(json.dumps(info))


if __name__ == "__main__":
    main()
