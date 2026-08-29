# `gd_types` test fixtures provenance

## `mini_api.json`, synthetic

Hand-authored, not generated. A minimal but format-faithful `extension_api.json` covering every shape the ingester has to handle: an inheritance chain (`MiniNode` to `MiniObject`), method flags (`const`, `static`, `vararg`, `virtual`), an absent `return_value` for void, and one of every `TypeRef` encoding the dump emits (`typedarray::`, `typeddictionary::K;V`, `enum::Class.Name`, `enum::Global`, `bitfield::Class.Name`, `void*`, `Variant`). Small enough to assert exact contents.

## `trimmed_api.json`, real, trimmed by class, complete within each

Extracted from a real in-project dump (see the workflow below) with `jq`. The trim is by class name only. It keeps the canonical `Object` to `RefCounted` and `Node` to `CanvasItem` to `Node2D` chains, plus the handful of other classes and builtins the corpus reaches for, and every class it keeps it keeps whole: every method, member, constant, enum, constructor, and operator. The `utility_functions` table is the complete one. Doc prose (`description`, `brief_description`) is stripped to keep it small; the signatures are real. This is the CI oracle proving that real Godot output parses.

**Why complete-within-each matters (#256).** `NativeDb::from_json` stamps `ApiProvenance::Exact`, and every negative claim the analyzer makes is gated on exactly that: `Function "x()" not found in base self.`, `Cannot find member "x" in base "Vector2".`, `UNSAFE_METHOD_ACCESS`. So a fixture claiming `Exact` while carrying nine of the engine's 114 utilities makes every `typeof(…)` in the conformance corpus read as a typo. Trim classes out, and never trim a kept class's members.

Regenerate from the workspace root, with the full dump at `api/extension_api.json`:

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

`Line2D` and the `PackedVector2Array` builtin are kept for the 4.7
`CONFUSABLE_TEMPORARY_MODIFICATION` fixture, which needs a native property whose type is a packed
array plus that array's method list (to tell the mutating `clear()` from the `const` `size()`).

A few methods are hand-added on top, and must be re-applied after a regeneration:

- `Object.free` and `Node.free`, ClassDB-resolvable names the dump omits, which `NativeDb` also seeds at ingest. Without them the `free`-related tests fail.
- `Object._get_property_list`, returning `typedarray::Dictionary`. The dump omits `Object`'s script virtuals entirely, and `seed_dump_omitted_methods` deliberately synthesizes every seeded method as `Variant`-returning, since only the name takes part in the existence lookup. The real signature is what 4.7's `_get_property_list` return-type exception turns on, so this fixture carries it and `crates/gd_analyze/tests/inherited_return_type.rs` pins the behavior. A stock dump therefore leaves that exception inert in a real session; the code is still correct, and goes live the moment a DB carries the real return type.

## Generating the full dump (`api/extension_api.json`, git-ignored)

Run the `godot` binary inside the project, so its context is loaded:

```bash
cd /path/to/your/godot/project && godot --headless --dump-extension-api-with-docs
# then move it to the dev location gdls reads via initializationOptions.extensionApiPath:
mv /path/to/your/godot/project/extension_api.json api/extension_api.json
```

Two gotchas observed in 2026-05. The binary can crash on shutdown *after* writing a complete, valid file, so validate the dump by content (the header plus the closing brace) rather than by exit code. And the stock dump omits installed GDExtensions, since `ClassDB` is snapshotted before they load; their types come from the `doc_classes` XML reader, not this JSON.
