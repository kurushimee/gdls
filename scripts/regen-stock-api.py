#!/usr/bin/env python3
"""Regenerate an embedded stock-Godot API asset from a `--dump-extension-api-with-docs` dump.

The assets in `crates/gd_server/assets/` are gdls's last-resort native surface: what a user gets on
a fresh install with no Godot binary anywhere. There is one per supported feature release, since a
4.6 project asking a 4.7 surface about its engine classes gets wrong answers. They must carry the
DOCUMENTATION fields, or hover and completion on every engine class show correct signatures and no
prose at all (#259).

The output filename comes from the dump's own header, so running this against a 4.7.2 binary writes
`extension_api_4.7.2_stock.min.json.gz` and cannot silently overwrite another release's asset. When
you add a release, add its arm to `EMBEDDED` in `crates/gd_server/src/api_dump.rs` too.

The transform keeps exactly the fields `gd_types::api` deserializes and drops everything else —
`builtin_class_sizes`, `builtin_class_member_offsets` and `native_structures` are GDExtension ABI
data gdls never reads, and each method's `hash` / `hash_compatibility` / `is_required` are for
binding generators. Dropping those pays for a good part of the prose.

Usage:
    godot --headless --dump-extension-api-with-docs     # writes ./extension_api.json
    scripts/regen-stock-api.py extension_api.json

Run it from a stock binary of a supported release, NOT from inside a project: the asset is the
STOCK surface, and a project dump would bake in that project's GDExtensions.
"""

import gzip
import json
import os
import sys

ASSET_DIR = "crates/gd_server/assets"

ARG_KEYS = ("name", "type", "default_value")


def arg(a):
    return {k: v for k, v in a.items() if k in ARG_KEYS}


def method(m):
    o = {k: m[k] for k in ("name", "is_const", "is_static", "is_vararg", "is_virtual") if k in m}
    if "return_value" in m:
        o["return_value"] = {"type": m["return_value"]["type"]}
    if "arguments" in m:
        o["arguments"] = [arg(a) for a in m["arguments"]]
    if m.get("description"):
        o["description"] = m["description"]
    return o


def prop(p):
    o = {k: p[k] for k in ("name", "type", "setter", "getter") if k in p}
    if p.get("description"):
        o["description"] = p["description"]
    return o


def signal(s):
    o = {"name": s["name"]}
    if "arguments" in s:
        o["arguments"] = [arg(a) for a in s["arguments"]]
    if s.get("description"):
        o["description"] = s["description"]
    return o


def klass(c):
    o = {k: c[k] for k in ("name", "inherits", "is_refcounted", "is_instantiable", "api_type") if k in c}
    for k in ("brief_description", "description"):
        if c.get(k):
            o[k] = c[k]
    if "methods" in c:
        o["methods"] = [method(m) for m in c["methods"]]
    if "properties" in c:
        o["properties"] = [prop(p) for p in c["properties"]]
    if "signals" in c:
        o["signals"] = [signal(s) for s in c["signals"]]
    if "enums" in c:
        o["enums"] = [
            {
                "name": e["name"],
                "is_bitfield": e.get("is_bitfield", False),
                "values": [{"name": v["name"], "value": v["value"]} for v in e["values"]],
            }
            for e in c["enums"]
        ]
    if "constants" in c:
        o["constants"] = [{"name": k["name"], "value": k["value"]} for k in c["constants"]]
    return o


def builtin_method(m):
    o = {k: m[k] for k in ("name", "return_type", "is_const", "is_static", "is_vararg") if k in m}
    if "arguments" in m:
        o["arguments"] = [arg(a) for a in m["arguments"]]
    return o


def builtin(b):
    o = {k: b[k] for k in ("name", "is_keyed", "indexing_return_type") if k in b}
    if "members" in b:
        o["members"] = [{"name": m["name"], "type": m["type"]} for m in b["members"]]
    if "constants" in b:
        o["constants"] = [
            {"name": c["name"], "type": c.get("type"), "value": c.get("value")} for c in b["constants"]
        ]
    if "enums" in b:
        o["enums"] = b["enums"]
    if "constructors" in b:
        o["constructors"] = [{"arguments": [arg(a) for a in c.get("arguments", [])]} for c in b["constructors"]]
    if "methods" in b:
        o["methods"] = [builtin_method(m) for m in b["methods"]]
    return o


def utility(u):
    o = {k: u[k] for k in ("name", "return_type", "category", "is_vararg") if k in u}
    if "arguments" in u:
        o["arguments"] = [arg(a) for a in u["arguments"]]
    return o


def main():
    if len(sys.argv) != 2:
        sys.exit(__doc__)
    src = json.load(open(sys.argv[1]))
    header = src["header"]
    if not any(c.get("description") for c in src["classes"]):
        sys.exit(
            "the dump carries no descriptions — regenerate it with "
            "`--dump-extension-api-with-docs`, not `--dump-extension-api`"
        )
    out = {
        "header": {
            k: header[k]
            for k in ("version_major", "version_minor", "version_patch", "version_status", "version_full_name")
            if k in header
        },
        "global_constants": src.get("global_constants", []),
        "global_enums": src.get("global_enums", []),
        "utility_functions": [utility(u) for u in src.get("utility_functions", [])],
        "builtin_classes": [builtin(b) for b in src.get("builtin_classes", [])],
        "classes": [klass(c) for c in src.get("classes", [])],
        "singletons": src.get("singletons", []),
    }
    version = ".".join(
        str(header[k]) for k in ("version_major", "version_minor", "version_patch")
    )
    asset = os.path.join(ASSET_DIR, f"extension_api_{version}_stock.min.json.gz")
    text = json.dumps(out, separators=(",", ":"))
    blob = gzip.compress(text.encode("utf-8"), 9)
    with open(asset, "wb") as f:
        f.write(blob)
    print(f"{asset}: {len(blob) / 1024:.0f} KB gzipped, {len(text) / 1e6:.1f} MB raw")
    print(f"  {len(out['classes'])} classes, {len(out['builtin_classes'])} builtins, "
          f"{len(out['utility_functions'])} utilities, from {header['version_full_name']}")


if __name__ == "__main__":
    main()
