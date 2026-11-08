#!/usr/bin/env python3
"""Generate the HTTPS update feed from the exact artifacts uploaded by release CI.

This is an integrity catalog, not an independently signed update channel. Its trust
anchor is the project's GitHub release over HTTPS, like the GitHub API fallback.
"""
import argparse
import hashlib
import json
import re
from pathlib import Path


def manifest(version: str, directory: Path) -> dict:
    if not re.fullmatch(r"[0-9]+\.[0-9]+\.[0-9]+", version):
        raise ValueError("Expected a stable major.minor.patch version")
    names = [
        f"hearthlink-{version}-x86_64.tar.gz",
        "Hearthlink.amd64.deb",
        "Hearthlink.x86_64.rpm",
        f"Hearthlink-{version}-x86_64.AppImage",
        f"Hearthlink-{version}-x86_64.flatpak",
    ]
    assets = []
    for name in names:
        path = directory / name
        if not path.is_file() or path.is_symlink() or not 0 < path.stat().st_size <= 1024**3:
            raise ValueError(f"Missing, empty or oversized release artifact: {name}")
        with path.open("rb") as file:
            checksum = hashlib.file_digest(file, "sha256").hexdigest()
        assets.append({"name": name, "sha256": checksum, "size": path.stat().st_size})
    return {"version": version, "notes": "See the release page for the full changelog.", "assets": assets}


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("version")
    parser.add_argument("directory", type=Path)
    args = parser.parse_args()
    output = manifest(args.version, args.directory)
    (args.directory / "hearthlink-update.json").write_text(json.dumps(output, indent=2) + "\n", encoding="utf-8")
