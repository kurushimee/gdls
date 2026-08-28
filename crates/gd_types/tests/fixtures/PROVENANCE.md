# `gd_types` test fixtures — provenance

## `mini_api.json` — synthetic
Hand-authored, **not** generated. A minimal but format-faithful `extension_api.json`
covering every shape the ingester must handle: an inheritance chain (`MiniNode` →
`MiniObject`), method flags (`const`/`static`/`vararg`/`virtual`), an absent
`return_value` (void), and one of every `TypeRef` encoding the dump emits —
`typedarray::`, `typeddictionary::K;V`, `enum::Class.Name`, `enum::Global`,
`bitfield::Class.Name`, `void*`, `Variant`. Small enough to assert exact contents.

## `trimmed_api.json` — real, trimmed by CLASS, complete within each
Extracted from a real in-project dump (see workflow below) with `jq`. The trim is by
class NAME only: it keeps the canonical `Object → RefCounted` / `Node → CanvasItem →
Node2D` chains plus the handful of other classes and builtins the corpus reaches for —
but each one it keeps, it keeps WHOLE (every method, member, constant, enum,
constructor and operator), and the `utility_functions` table is the complete one.
Doc prose (`description` / `brief_description`) is stripped to keep it small;
signatures are real. This is the CI oracle that proves real Godot output parses.

**Why complete-within-each matters (#256).** `NativeDb::from_json` stamps
`ApiProvenance::Exact`, and every negative claim the analyzer makes — `Function "x()"
not found in base self.`, `Cannot find member "x" in base "Vector2".`,
`UNSAFE_METHOD_ACCESS` — is gated on exactly that. So a fixture that claims `Exact`
while carrying nine of the engine's 114 utilities makes every `typeof(…)` in the
conformance corpus read as a typo. Trim classes out; never trim a kept class's members.

Regenerate (from the workspace root, with the full dump at `api/extension_api.json`):

```bash
jq 'walk(if type=="object" then del(.description, .brief_description) else . end)
  | { header,
      global_enums:      [.global_enums[]      | select(.name=="Variant.Operator" or .name=="Error" or .name=="Side")],
      global_constants:  .global_constants,
      utility_functions: .utility_functions,
      builtin_classes:   [.builtin_classes[]   | select(.name | IN("Vector2","Vector2i","Vector3","Array","Dictionary","Color","Callable"))],
      classes:           [.classes[]           | select(.name | IN("Object","RefCounted","Node","CanvasItem","Node2D","Node3D","SpriteBase3D","Sprite3D","Resource","Script","GDScript","Time","TileSet","InstancePlaceholder","MainLoop","SceneTree","Viewport","Window","PhysicsDirectBodyState3D","PhysicsDirectBodyState3DExtension"))],
      singletons:        [.singletons[]        | select(.name | IN("Engine","OS"))] }' \
  api/extension_api.json > crates/gd_types/tests/fixtures/trimmed_api.json
```

A few methods are hand-added on top: the ClassDB-resolvable names the dump omits
(`Object.free`, `Node.free`), which `NativeDb` also seeds at ingest. Re-apply them after
a regeneration, or the `free`-related tests fail.

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
