# Following upstream programs

Hearthlink ports Windows programs. When one of them has a new version, the
question is: what changed that matters for the port? This answers it without
an LLM, REA or a decompiler, and gives an agent a short brief to work from.

```
fetch (version + MD5 from the upstream's own update file, ~1 KB)
  └─ new version? download, check MD5
       └─ dotnet_snapshot.py: types, methods, IL hash per method (tokens
          resolved to names), strings and calls per method, const/enum values,
          static arrays; also from Costura-packed DLLs
            └─ compare.py against baseline.json.gz + watch.txt
                 └─ brief.md + verdict
```

| verdict | meaning | what to do |
|---|---|---|
| `status=uptodate` | same version as ported | nothing (0 tokens) |
| 0 | only version numbers changed | `--bump` sets `ported_version` (0 tokens) |
| 1 | code changed, nothing in `watch.txt` | skim brief, usually bump |
| 2 | watched parts changed | agent with `AGENT.md` + `brief.md` |

The brief folds async state machines and lambdas into their methods, drops
compiler plumbing and accessors, recognises moved code and shows only what
differs, and lists `decompile.sh` lines for the changed types in case strings
and calls are not enough.

## Wowhead

```sh
tools/upstream/wowhead/check.sh            # check, brief on a new version
tools/upstream/wowhead/check.sh --bump     # verdict 0: bump ported_version too
tools/upstream/wowhead/check.sh --force --accept   # after porting: new baseline
```

Needs `python3` and [`uv`](https://docs.astral.sh/uv/) (pulls `dnfile`).
`decompile.sh` uses `ilspycmd` if installed, else builds a podman/docker
image once (.NET SDK + ilspycmd) and runs it without network.
Work files: `~/.cache/hearthlink-upstream/wowhead/VERSION/`.

`.github/workflows/upstream-watch.yml` runs the check daily and opens an
issue (label `upstream`) with the brief for a new version. It only reads the
repository; bumping or porting happens locally.

`watch.txt` lists what the port depends on (types, strings, calls), each with
where it lives in the port. Keep it in step when the port grows a feature.

## Another .NET program

Copy `wowhead/` and change `fetch.py` (where the version and download come
from), `watch.txt` and the `--include` regex in `check.sh`. For non-.NET
programs only `dotnet_snapshot.py` needs a counterpart (e.g. `asar extract` +
a JS AST dump for Electron); `compare.py` works on the same JSON.

The baseline holds names, hashes, strings and constants of the upstream
program, not its code.
