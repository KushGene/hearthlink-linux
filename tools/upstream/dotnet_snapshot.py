# /// script
# requires-python = ">=3.10"
# dependencies = ["dnfile>=0.15"]
# ///
"""Snapshot of a .NET program: what a port depends on, without decompiling.

Reads the main assembly and the assemblies Costura.Fody packs into it
(`costura.*.dll.compressed`, raw deflate). For every type it records the
fields and methods; for every method a hash of its IL (metadata tokens
resolved to names, so adding something elsewhere doesn't change the hash),
the strings it uses and the members it calls.

Comparing two snapshots (compare.py) then shows exactly which methods
changed, without an LLM, REA or a decompiler.

Usage: uv run tools/upstream/dotnet_snapshot.py PROGRAM.exe OUT.json [--include REGEX]
"""

import argparse
import hashlib
import json
import os
import re
import sys
import zlib

import dnfile

# ECMA-335 III operand kinds: n = none, number = that many bytes,
# T = metadata token, S = string token, W = switch table.
ONE = {}
for op in range(0x00, 0xE1):
    ONE[op] = "n"
for op in range(0x0E, 0x14):
    ONE[op] = 1
ONE[0x1F] = 1
ONE[0x20] = 4
ONE[0x21] = 8
ONE[0x22] = 4
ONE[0x23] = 8
for op in (0x27, 0x28, 0x29, 0x6F, 0x70, 0x71, 0x73, 0x74, 0x75, 0x79, 0x7B, 0x7C, 0x7D, 0x7E,
           0x7F, 0x80, 0x81, 0x8C, 0x8D, 0x8F, 0xA3, 0xA4, 0xA5, 0xC2, 0xC6, 0xD0):
    ONE[op] = "T"
ONE[0x72] = "S"
for op in range(0x2B, 0x38):
    ONE[op] = 1
for op in range(0x38, 0x45):
    ONE[op] = 4
ONE[0x45] = "W"
ONE[0xDD] = 4
ONE[0xDE] = 1
TWO = {op: "n" for op in range(0x00, 0x1F)}
for op in (0x06, 0x07, 0x15, 0x16, 0x1C):
    TWO[op] = "T"
for op in range(0x09, 0x0F):
    TWO[op] = 2
TWO[0x12] = 1
TWO[0x19] = 1

GENERATED_DIGITS = re.compile(r"\d+")


def norm(name):
    """Compiler-generated names (<Foo>d__12, <>c__DisplayClass3_0) renumber
    whenever anything is added; keep only their shape."""
    return GENERATED_DIGITS.sub("N", name) if "<" in name else name


def compressed_uint(b, i):
    x = b[i]
    if x & 0x80 == 0:
        return x, i + 1
    if x & 0xC0 == 0x80:
        return ((x & 0x3F) << 8) | b[i + 1], i + 2
    return ((x & 0x1F) << 24) | (b[i + 1] << 16) | (b[i + 2] << 8) | b[i + 3], i + 4


def param_count(sig):
    if not sig:
        return 0
    i = 1
    if sig[0] & 0x10:  # generic
        _, i = compressed_uint(sig, i)
    n, _ = compressed_uint(sig, i)
    return n


def blob(v):
    if v is None:
        return b""
    if isinstance(v, (bytes, bytearray)):
        return bytes(v)
    return bytes(getattr(v, "value", b"") or b"")


class Assembly:
    def __init__(self, data, label):
        self.label = label
        self.pe = dnfile.dnPE(data=data)
        net = self.pe.net
        if net is None or net.mdtables is None:
            raise ValueError(f"{label}: not a .NET assembly")
        self.t = net.mdtables
        self.us = net.user_strings
        self.type_names = {}
        self.method_owner = {}
        self.field_owner = {}
        self._index_types()
        self._index_data()

    def rows(self, name):
        table = getattr(self.t, name, None)
        return table.rows if table is not None else []

    def _index_types(self):
        enclosing = {}
        for r in self.rows("NestedClass"):
            enclosing[r.NestedClass.row_index] = r.EnclosingClass.row_index

        def full(i, depth=0):
            r = self.rows("TypeDef")[i - 1]
            name = norm(str(r.TypeName))
            if i in enclosing and depth < 10:
                return f"{full(enclosing[i], depth + 1)}/{name}"
            ns = str(r.TypeNamespace or "")
            return f"{ns}.{name}" if ns else name

        for i, r in enumerate(self.rows("TypeDef"), 1):
            self.type_names[i] = full(i)
            for m in r.MethodList or []:
                self.method_owner[m.row_index] = i
            for f in r.FieldList or []:
                self.field_owner[f.row_index] = i

    def method_key(self, i):
        m = self.rows("MethodDef")[i - 1]
        return f"{norm(str(m.Name))}/{param_count(blob(m.Signature))}"

    def coded_name(self, ci):
        if ci is None or ci.row is None:
            return "?"
        table = ci.table.name if hasattr(ci.table, "name") else str(ci.table)
        return self.row_name(table, ci.row_index)

    def row_name(self, table, i):
        try:
            if table == "TypeDef":
                return self.type_names.get(i, "?")
            if table == "TypeRef":
                r = self.rows("TypeRef")[i - 1]
                ns = str(r.TypeNamespace or "")
                return f"{ns}.{r.TypeName}" if ns else str(r.TypeName)
            if table == "TypeSpec":
                sig = blob(self.rows("TypeSpec")[i - 1].Signature)
                # GENERICINST (0x15) CLASS|VALUETYPE TypeDefOrRef: name the generic type
                if len(sig) > 2 and sig[0] == 0x15:
                    coded, _ = compressed_uint(sig, 2)
                    tab = ("TypeDef", "TypeRef", "TypeSpec")[coded & 3]
                    return self.row_name(tab, coded >> 2) + "<>"
                return "spec"
            if table == "MethodDef":
                owner = self.type_names.get(self.method_owner.get(i), "?")
                return f"{owner}::{self.method_key(i)}"
            if table == "Field":
                if i in self.field_data:
                    return self.field_data[i]
                owner = self.type_names.get(self.field_owner.get(i), "?")
                return f"{owner}::{norm(str(self.rows('Field')[i - 1].Name))}"
            if table == "MemberRef":
                r = self.rows("MemberRef")[i - 1]
                return f"{self.coded_name(r.Class)}::{r.Name}"
            if table == "MethodSpec":
                return self.coded_name(self.rows("MethodSpec")[i - 1].Method) + "<>"
            if table == "ModuleRef":
                return str(self.rows("ModuleRef")[i - 1].Name)
        except (IndexError, AttributeError):
            pass
        return table

    def _index_data(self):
        """Field -> its static data (array initialisers), decoded where it looks like int32s."""
        self.field_data = {}
        for r in self.rows("FieldRva"):
            fi = r.Field.row_index
            sig = blob(self.rows("Field")[fi - 1].Signature)
            size = None
            if len(sig) > 2 and sig[1] == 0x11:
                coded, _ = compressed_uint(sig, 2)
                if coded & 3 == 0:
                    m = re.search(r"Size=(\d+)", self.type_names.get(coded >> 2, ""))
                    size = int(m.group(1)) if m else None
            if not size or size > 4096:
                continue
            data = self.pe.get_data(r.Rva, size)
            if size % 4 == 0 and size <= 256:
                ints = [int.from_bytes(data[i:i + 4], "little", signed=True) for i in range(0, size, 4)]
                self.field_data[fi] = "int32[" + ",".join(map(str, ints)) + "]"
            else:
                self.field_data[fi] = "data:" + hashlib.sha256(data).hexdigest()[:16]

    def constants(self):
        """Values of const fields and enum members (they are not in the IL)."""
        out = {}
        for r in self.rows("Constant"):
            parent = r.Parent
            table = parent.table.name if hasattr(parent.table, "name") else str(parent.table)
            if table != "Field":
                continue
            raw = blob(r.Value)
            t = r.Type
            if t == 0x0E:
                v = raw.decode("utf-16-le", "replace")
            elif t in (0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0A, 0x0B):
                v = int.from_bytes(raw, "little", signed=t in (0x04, 0x06, 0x08, 0x0A))
            elif t in (0x0C, 0x0D):
                import struct
                v = struct.unpack("<f" if t == 0x0C else "<d", raw)[0]
            else:
                v = None
            out[self.row_name("Field", parent.row_index)] = v
        return out

    def token_name(self, tok):
        tables = {0x01: "TypeRef", 0x02: "TypeDef", 0x04: "Field", 0x06: "MethodDef",
                  0x0A: "MemberRef", 0x11: "StandAloneSig", 0x1A: "ModuleRef",
                  0x1B: "TypeSpec", 0x2B: "MethodSpec"}
        return self.row_name(tables.get(tok >> 24, hex(tok >> 24)), tok & 0xFFFFFF)

    def user_string(self, tok):
        try:
            s = self.us.get(tok & 0xFFFFFF)
            return s.value if hasattr(s, "value") else str(s)
        except Exception:
            return "?"

    def method_body(self, rva):
        if not rva:
            return None
        pe = self.pe
        head = pe.get_data(rva, 12)
        if head[0] & 3 == 2:
            return pe.get_data(rva + 1, head[0] >> 2)
        size = int.from_bytes(head[4:8], "little")
        header = (int.from_bytes(head[0:2], "little") >> 12) * 4
        return pe.get_data(rva + header, size)

    def walk_il(self, code):
        """Hashable IL with names instead of tokens, plus strings and refs used."""
        out, strings, refs = [], [], []
        i, n = 0, len(code)
        while i < n:
            op = code[i]
            i += 1
            if op == 0xFE and i < n:
                op2 = code[i]
                i += 1
                kind = TWO.get(op2, "n")
                out.append(f"FE{op2:02X}")
            else:
                kind = ONE.get(op, "n")
                out.append(f"{op:02X}")
            if kind == "n":
                continue
            if kind == "W":
                count = int.from_bytes(code[i:i + 4], "little")
                out.append(code[i:i + 4 + 4 * count].hex())
                i += 4 + 4 * count
            elif kind == "T":
                name = self.token_name(int.from_bytes(code[i:i + 4], "little"))
                out.append(name)
                refs.append(name)
                i += 4
            elif kind == "S":
                s = self.user_string(int.from_bytes(code[i:i + 4], "little"))
                out.append(repr(s))
                strings.append(s)
                i += 4
            else:
                out.append(code[i:i + kind].hex())
                i += kind
        return "\n".join(out), strings, refs

    def snapshot(self):
        asm = self.rows("Assembly")
        version = None
        name = self.label
        if asm:
            a = asm[0]
            name = str(a.Name)
            version = f"{a.MajorVersion}.{a.MinorVersion}.{a.BuildNumber}.{a.RevisionNumber}"
        types = {}
        all_strings = set()
        for ti, r in enumerate(self.rows("TypeDef"), 1):
            tname = self.type_names[ti]
            if tname == "<Module>" and not r.MethodList:
                continue
            fields = sorted(norm(str(self.rows("Field")[f.row_index - 1].Name)) for f in r.FieldList or [])
            methods = {}
            for m in r.MethodList or []:
                key = self.method_key(m.row_index)
                while key in methods:
                    key += "+"
                row = self.rows("MethodDef")[m.row_index - 1]
                entry = {}
                try:
                    code = self.method_body(row.Rva)
                except Exception:
                    code = None
                if code is not None:
                    il, strings, refs = self.walk_il(code)
                    entry["il"] = hashlib.sha256(il.encode()).hexdigest()[:16]
                    if strings:
                        entry["strings"] = sorted(set(strings))
                        all_strings.update(strings)
                    if refs:
                        entry["refs"] = sorted(set(refs))
                methods[key] = entry
            types[tname] = {"fields": fields, "methods": methods}
        return name, {"version": version, "types": types, "strings": sorted(all_strings),
                      "constants": self.constants()}


def costura_parts(pe):
    for r in pe.net.resources or []:
        name = str(r.name)
        if name.startswith("costura.") and name.endswith(".dll.compressed") and isinstance(r.data, bytes):
            yield name[len("costura."):-len(".compressed")], zlib.decompress(r.data, -15)


def main():
    ap = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    ap.add_argument("program")
    ap.add_argument("out")
    ap.add_argument("--include", default=".", help="regex on assembly names to keep (others only listed)")
    ap.add_argument("--app", default=None)
    ap.add_argument("--extract", metavar="DIR", help="also write the Costura-packed DLLs there (for decompile.sh)")
    args = ap.parse_args()
    keep = re.compile(args.include, re.I)

    data = open(args.program, "rb").read()
    main_asm = Assembly(data, args.program)
    parts = [(args.program, main_asm)]
    if args.extract:
        os.makedirs(args.extract, exist_ok=True)
    for label, raw in costura_parts(main_asm.pe):
        if args.extract:
            with open(os.path.join(args.extract, label), "wb") as f:
                f.write(raw)
        try:
            parts.append((label, Assembly(raw, label)))
        except Exception as e:  # noqa: BLE001
            print(f"skip {label}: {e}", file=sys.stderr)

    snap = {"app": args.app, "assemblies": {}, "other_assemblies": {}, "version": None}
    for label, a in parts:
        name, s = a.snapshot()
        if a is main_asm:
            snap["version"] = s["version"]
        if keep.search(name):
            snap["assemblies"][name] = s
        else:
            snap["other_assemblies"][name] = s["version"]
    with open(args.out, "w") as f:
        json.dump(snap, f, indent=1, sort_keys=True, ensure_ascii=False)
        f.write("\n")
    total = sum(len(t["methods"]) for s in snap["assemblies"].values() for t in s["types"].values())
    print(f"{snap['version']}: {len(snap['assemblies'])} assemblies, {total} methods -> {args.out}")


if __name__ == "__main__":
    main()
