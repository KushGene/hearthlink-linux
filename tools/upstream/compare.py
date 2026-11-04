"""Compare two snapshots from dotnet_snapshot.py and write a short brief.

The brief lists only what changed, and puts first what the port depends on
(the patterns in a watch file). It is meant as the whole input for an agent
that updates the port, so it is kept small: async state machines and lambdas
are folded into the method that made them, compiler plumbing is left out,
trivial members (accessors, delegate stubs) are only counted.

Exit code: 0 = no code changes (only version numbers), 1 = changes, none of
them watched, 2 = watched changes, 3 = usage error.

Watch file: one pattern per line, `type:`, `string:` or `ref:` followed by a
regex, optionally `=> note` (where in the port that lives). `#` comments.

Usage: python3 -I tools/upstream/compare.py OLD.json[.gz] NEW.json[.gz] --watch FILE
           [--out brief.md] [--types-out types.tsv]
"""

import argparse
import gzip
import json
import re
import sys

NOISE = re.compile(
    r"^System\.Runtime\.CompilerServices\.|^System\.Collections\.(Generic\.)?I?Enumera|^Enumerator<>"
    r"|::GetEnumerator$|^System\.IDisposable|^System\.Object::\.ctor$|<>N__|<>t__builder|<>u__N"
    r"|k__BackingField|__wrapN|::GetAwaiter$|^System\.(Int32|Int64|Char|Boolean|String|Object|Nullable`1<>)$"
    r"|>d__N::|>d::|DisplayClassN_N::|/<>c::|^System\.Delegate::|^System\.Threading\.Interlocked::"
)
TRIVIAL = re.compile(r"^(get_|set_|add_|remove_)|^(\.ctor|\.cctor|Invoke|BeginInvoke|EndInvoke|SetStateMachine)/")


def load(path):
    opener = gzip.open if path.endswith(".gz") else open
    with opener(path, "rt", encoding="utf-8") as f:
        return json.load(f)


def load_watch(path):
    rules = []
    for line in open(path, encoding="utf-8"):
        line = line.strip()
        if not line or line.startswith("#"):
            continue
        kind, _, rest = line.partition(":")
        pattern, _, note = rest.partition("=>")
        rules.append((kind.strip(), re.compile(pattern.strip()), note.strip() or pattern.strip()))
    return rules


def flatten(snap):
    """{"Asm!Type::method": entry}, {"Asm!Type": fields}"""
    methods, types = {}, {}
    for asm, a in snap["assemblies"].items():
        for tname, t in a["types"].items():
            types[f"{asm}!{tname}"] = t["fields"]
            for mname, m in t["methods"].items():
                methods[f"{asm}!{tname}::{mname}"] = m
    return methods, types


def split(key):
    asm, _, rest = key.partition("!")
    tname, _, mname = rest.partition("::")
    return asm, tname, mname


def logical(key):
    """(asm, outer type, method name): compiler-made types and lambdas
    (<Send>d__N, <>c::<Foo>b__N_N, DisplayClass) count as the method that made them."""
    asm, tname, mname = split(key)
    outer, _, inner = tname.partition("/<")
    if inner:
        m = re.search(r"<([A-Za-z_][\w.]*)>", "<" + inner)
        if m:
            return asm, outer, m.group(1)
    m = re.match(r"<+([A-Za-z_][\w.]*)>", mname)
    if m:
        return asm, outer, m.group(1)
    return asm, outer, mname.split("/")[0]


def q(s):
    return json.dumps(s, ensure_ascii=False)


def capped(items, limit):
    items = list(items)
    return items[:limit] + [f"... {len(items) - limit} more"] if len(items) > limit else items


class Matcher:
    def __init__(self, rules):
        self.rules = rules

    def notes(self, key, strings, refs):
        hits = []
        name = key.partition("!")[2]
        for kind, rx, note in self.rules:
            if (kind == "type" and rx.search(name)) or \
               (kind == "string" and any(rx.search(s) for s in strings)) or \
               (kind == "ref" and any(rx.search(r) for r in refs)):
                if note not in hits:
                    hits.append(note)
        return hits

    def string_watched(self, s):
        return any(kind == "string" and rx.search(s) for kind, rx, _ in self.rules)


class Group:
    def __init__(self):
        self.kinds = set()
        self.notes = []
        self.s_plus, self.s_minus, self.r_plus, self.r_minus = set(), set(), set(), set()
        self.trivial = 0
        self.blind = False  # changed, but no visible difference in strings/calls
        self.moved = ""

    def add(self, kind, mname, o, n, notes):
        os_, ns = set(o.get("strings", [])), set(n.get("strings", []))
        orf = {r for r in o.get("refs", []) if not NOISE.search(r)}
        nrf = {r for r in n.get("refs", []) if not NOISE.search(r)}
        for x in notes:
            if x not in self.notes:
                self.notes.append(x)
        if TRIVIAL.search(mname) and not (os_ | ns) and kind != "changed":
            self.trivial += 1
            return
        self.kinds.add(kind)
        self.s_plus |= ns - os_
        self.s_minus |= os_ - ns
        self.r_plus |= nrf - orf
        self.r_minus |= orf - nrf
        if kind == "changed" and ns == os_ and nrf == orf:
            self.blind = True


def ref_rank(r):
    if r.startswith("Wowhead.") or not r.startswith(("System.", "Microsoft.", "Newtonsoft.")):
        return 0
    if re.match(r"System\.(Net|IO|Text\.RegularExpressions|Security\.Cryptography)\.|Newtonsoft\.", r):
        return 1
    return 2


def calls(refs, limit=12):
    return ", ".join(f"`{r}`" for r in capped(sorted(refs, key=lambda r: (ref_rank(r), r)), limit))


def group_lines(tname, mname, v, shown):
    kinds = "/".join(sorted(v.kinds))
    line = f"- {kinds} `{tname}::{mname}`"
    if v.moved:
        line += f" (moved {v.moved})"
    if len(v.notes) > 1:
        line += " (also: " + "; ".join(v.notes[1:]) + ")"
    if v.trivial:
        line += f" +{v.trivial} trivial"
    lines = [line]
    if v.s_plus:
        lines.append("  - strings +: " + ", ".join(q(x) for x in capped(sorted(v.s_plus), 30)))
    if v.s_minus:
        lines.append("  - strings -: " + ", ".join(q(x) for x in capped(sorted(v.s_minus), 30)))
    if v.r_plus:
        lines.append("  - calls +: " + calls(v.r_plus))
    if v.r_minus:
        lines.append("  - calls -: " + calls(v.r_minus))
    if v.blind:
        lines.append("  - logic/constants changed with the same strings and calls: decompile to see")
    shown |= v.s_plus | v.s_minus
    return lines


def detect_moves(groups):
    """Code that moved to another method (or type): show only what differs."""
    gone = [(g, v) for g, v in groups.items() if v.s_minus or v.r_minus]
    came = [(g, v) for g, v in groups.items() if v.s_plus or v.r_plus]
    for g1, a in gone:
        best, score = None, 0.0
        for g2, b in came:
            if g2 == g1 or b.moved:
                continue
            x, y = a.s_minus | a.r_minus, b.s_plus | b.r_plus
            if len(x) < 4:
                continue
            j = len(x & y) / len(x | y)
            if j > score:
                best, score = (g2, b), j
        if best and score >= 0.6:
            g2, b = best
            sm, sp = a.s_minus - b.s_plus, b.s_plus - a.s_minus
            rm, rp = a.r_minus - b.r_plus, b.r_plus - a.r_minus
            a.s_minus, b.s_plus, a.r_minus, b.r_plus = sm, sp, rm, rp
            a.moved = f"to `{g2[1]}::{g2[2]}`"
            b.moved = f"from `{g1[1]}::{g1[2]}`"


def main():
    ap = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    ap.add_argument("old")
    ap.add_argument("new")
    ap.add_argument("--watch", required=True)
    ap.add_argument("--out")
    ap.add_argument("--types-out", help="write 'assembly<TAB>type' of watched changed types (for decompile.sh)")
    args = ap.parse_args()
    old, new = load(args.old), load(args.new)
    match = Matcher(load_watch(args.watch))
    om, ot = flatten(old)
    nm, nt = flatten(new)

    events = [("changed", k) for k in om.keys() & nm.keys() if om[k] != nm[k]]
    events += [("added", k) for k in nm.keys() - om.keys()]
    events += [("removed", k) for k in om.keys() - nm.keys()]

    groups = {}
    for kind, k in events:
        o, n = om.get(k) or {}, nm.get(k) or {}
        strings = set(o.get("strings", [])) | set(n.get("strings", []))
        refs = set(o.get("refs", [])) | set(n.get("refs", []))
        lg = logical(k)
        groups.setdefault(lg, Group()).add(kind, split(k)[2], o, n, match.notes(k, strings, refs))

    types_added = sorted(nt.keys() - ot.keys())
    types_removed = sorted(ot.keys() - nt.keys())
    fields = []
    for k in sorted(ot.keys() & nt.keys()):
        plus, minus = sorted(set(nt[k]) - set(ot[k])), sorted(set(ot[k]) - set(nt[k]))
        plus, minus = [f for f in plus if "<" not in f], [f for f in minus if "<" not in f]
        if (plus or minus) and "/<" not in k:
            fields.append((k, plus, minus))
    os_all = {s for a in old["assemblies"].values() for s in a["strings"]}
    ns_all = {s for a in new["assemblies"].values() for s in a["strings"]}
    watched_strings = [("+", s) for s in sorted(ns_all - os_all) if match.string_watched(s)]
    watched_strings += [("-", s) for s in sorted(os_all - ns_all) if match.string_watched(s)]

    oc = {f"{a}!{k}": v for a, x in old["assemblies"].items() for k, v in x.get("constants", {}).items()}
    nc = {f"{a}!{k}": v for a, x in new["assemblies"].items() for k, v in x.get("constants", {}).items()}
    const_changes = []
    for k in sorted(oc.keys() | nc.keys()):
        if oc.get(k, "<none>") != nc.get(k, "<none>"):
            name = k.partition("!")[2]
            hit = match.notes(k, set(), set())
            const_changes.append((name, oc.get(k, "<none>"), nc.get(k, "<none>"), hit))

    deps = []
    oo, no = old.get("other_assemblies", {}), new.get("other_assemblies", {})
    for name in sorted(set(oo) | set(no)):
        if oo.get(name) != no.get(name):
            deps.append(f"{name}: {oo.get(name)} -> {no.get(name)}")

    detect_moves(groups)
    watched = {g: v for g, v in groups.items() if v.notes}
    other = {g: v for g, v in groups.items() if not v.notes}
    any_change = events or types_added or types_removed or fields or os_all != ns_all
    watched_consts = [c for c in const_changes if c[3]]
    any_change = any_change or const_changes
    code = 2 if (watched or watched_strings or watched_consts) else (1 if any_change else 0)
    verdict = {
        0: "No code changes. Raising the version number in the port is enough.",
        1: "Code changed, but nothing the port depends on (per watch list). Most likely raising the version is enough; skim 'Other changes'.",
        2: "Changes in parts the port depends on. Update the port as listed under 'Watched changes'.",
    }[code]

    out = [f"# {new.get('app') or 'upstream'}: {old.get('version')} -> {new.get('version')}", "",
           f"**Verdict:** {verdict}", ""]
    if deps:
        out += ["Bundled libraries: " + "; ".join(deps), ""]
    shown = set()

    if const_changes:
        out += ["## Constants (const fields, enum members; old -> new)", ""]
        by_note = {}
        for name, a, b, hit in const_changes:
            by_note.setdefault(hit[0] if hit else "not watched", []).append((name, a, b))
        for note in sorted(by_note, key=lambda n: n == "not watched"):
            items = by_note[note]
            out.append(f"- **{note}**")
            for name, a, b in items[:40]:
                out.append(f"  - `{name}`: " + (q(b) if a == "<none>" else f"{q(a)} -> {q(b)}"
                                                if b != "<none>" else f"{q(a)} -> removed"))
            if len(items) > 40:
                out.append(f"  - ... {len(items) - 40} more")
        out.append("")

    if watched or watched_strings:
        out += ["## Watched changes", "",
                "Grouped by where it lives in the port. strings/calls +/- = what the method gained/lost;"
                " calls list the program's own and I/O/HTTP calls first.", ""]
        by_note = {}
        for g, v in watched.items():
            by_note.setdefault(v.notes[0], []).append((g, v))
        for note, items in by_note.items():
            out.append(f"### {note}")
            trivial = {}
            for (asm, tname, mname), v in sorted(items, key=lambda x: x[0]):
                if not v.kinds:
                    trivial.setdefault(tname, []).append(mname)
                    continue
                out += group_lines(tname, mname, v, shown)
            for tname, names in sorted(trivial.items()):
                out.append(f"- trivial `{tname}`: " + ", ".join(sorted(names)))
            out.append("")
        rest = [(sign, x) for sign, x in watched_strings if x not in shown]
        if rest:
            out += ["Watched strings elsewhere: " + ", ".join(sign + q(x) for sign, x in rest), ""]

    if types_added or types_removed or fields:
        out += ["## Types and fields", ""]
        out += [f"- type + `{t.partition('!')[2]}`" for t in types_added if "/<" not in t and "<Private" not in t]
        out += [f"- type - `{t.partition('!')[2]}`" for t in types_removed if "/<" not in t and "<Private" not in t]
        for k, plus, minus in fields[:40]:
            if "<Private" in k:
                continue
            out.append(f"- `{k.partition('!')[2]}` fields " + " ".join([f"+{f}" for f in plus] + [f"-{f}" for f in minus]))
        out.append("")

    if other:
        out += ["## Other changes (not watched)", ""]
        names = sorted(f"{'/'.join(sorted(v.kinds)) or 'trivial'} `{t}::{m}`" for (_, t, m), v in other.items())
        out += [f"- {n}" for n in capped(names, 60)]
        out.append("")

    unwatched_strings = [x for x in sorted(ns_all - os_all) if not match.string_watched(x) and x not in shown]
    if unwatched_strings:
        out += ["## Other new strings", "",
                ", ".join(q(x) for x in capped(unwatched_strings, 80)), ""]

    text = "\n".join(out).rstrip() + "\n"
    if args.out:
        open(args.out, "w", encoding="utf-8").write(text)
    else:
        sys.stdout.write(text)
    if args.types_out:
        with open(args.types_out, "w", encoding="utf-8") as f:
            for asm, tname in sorted({(a, t) for (a, t, _), v in watched.items() if v.kinds & {"changed", "added"}}):
                f.write(f"{asm}\t{tname}\n")
    print(f"verdict {code}: {len(watched)} watched, {len(other)} other changed methods", file=sys.stderr)
    sys.exit(code)


if __name__ == "__main__":
    main()
