# `gd_types` test fixtures — provenance

## `mini_api.json` — synthetic
Hand-authored, **not** generated. A minimal but format-faithful `extension_api.json`
covering every shape the ingester must handle: an inheritance chain (`MiniNode` →
`MiniObject`), method flags (`const`/`static`/`vararg`/`virtual`), an absent
`return_value` (void), and one of every `TypeRef` encoding the dump emits —
`typedarray::`, `typeddictionary::K;V`, `enum::Class.Name`, `enum::Global`,
`bitfield::Class.Name`, `void*`, `Variant`. Small enough to assert exact contents.

## `trimmed_api.json` — real, trimmed
Extracted from a real in-project dump (see workflow below) with `jq`, keeping the
canonical `Object → RefCounted`/`Node → CanvasItem → Node2D` chain plus a couple of
builtins, globals and utility functions. Doc prose (`description` /
`brief_description`) is stripped to keep it small; method/property/signal *signatures*
are real. This is the CI oracle that proves real Godot output parses.

Regenerate (from the workspace root, with the full dump at `api/extension_api.json`):

```bash
jq 'walk(if type=="object" then del(.description, .brief_description) else . end)
  | { header,
      global_enums:      [.global_enums[]      | select(.name=="Error" or .name=="Side")],
      global_constants:  (.global_constants[0:3]),
      utility_functions: [.utility_functions[] | select(.name | IN("abs","max","print","lerp"))],
      builtin_classes:   [.builtin_classes[]   | select(.name | IN("Vector2","Array"))],
      classes:           [.classes[]           | select(.name | IN("Object","RefCounted","Node","CanvasItem","Node2D"))],
      singletons:        [.singletons[]        | select(.name | IN("Engine","OS"))] }' \
  api/extension_api.json > crates/gd_types/tests/fixtures/trimmed_api.json
```

## Generating the full dump (`api/extension_api.json`, git-ignored)
Run the `godot` binary **inside the project** so its context is loaded:

```bash
cd /path/to/your/godot/project && godot --headless --dump-extension-api-with-docs
# then move it to the dev location gdls reads via initializationOptions.extensionApiPath:
mv /path/to/your/godot/project/extension_api.json api/extension_api.json
```

Two gotchas observed (2026-05): the binary can **crash on
shutdown** *after* writing a complete, valid file — so validate the dump by content
(header + closing brace), not by exit code. And the stock dump **omits installed
GDExtensions** (`ClassDB` is snapshotted before they load) — their types come from the
`doc_classes` XML reader, not this JSON.
