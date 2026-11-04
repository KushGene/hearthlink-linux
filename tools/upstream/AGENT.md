# Prompt: update a port to a new upstream version

Give an agent this file, the brief (`brief.md` from `check.sh`, or the
"... is out" issue) and nothing else. It is enough for most updates.

---

The Windows program that `src/services/<service>.rs` ports has a new version.
The brief lists what changed in it, found by comparing metadata and IL of
both versions (no decompiling). Update the port to match.

1. Read the brief. "Watched changes" and "Constants" are grouped by where
   they live in the port (the headings name the Rust functions/constants).
   Ignore "Other changes" unless something there clearly affects behaviour
   the port has (UI of the Windows program is not ported).
2. Read only the Rust functions the headings name, not the whole file.
3. Where strings, calls and constants make the change clear, change the port.
   Where they don't (the brief says "decompile to see", or you need the
   exact logic), run the `tools/upstream/decompile.sh` line from the end of
   the brief for that one type and read only the method you need. Don't use
   REA or decompile whole assemblies.
4. Keep the port's style. Add or update unit tests next to the changed code
   (the existing tests in the file show the patterns, e.g. a mock server for
   wire formats).
5. Set `ported_version` and the version in the module doc comment to the new
   version, and add a CHANGELOG entry under "Unreleased" in the existing
   style (user-facing, what changes for the user).
6. `cargo build` and `cargo test` must pass without warnings.
7. When done: `tools/upstream/<service>/check.sh --force --accept` so the
   new version becomes the baseline for the next comparison.

If the brief shows a change you can't port (new server feature, unclear
protocol), say so with the brief's line instead of guessing.
