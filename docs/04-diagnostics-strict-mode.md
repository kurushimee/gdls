# 04. Diagnostics model, per-file policy, and strict mode

## 1. What counts as a diagnostic

**Errors** come from the parser (syntax) and the analyzer (type and semantic). They are the same conditions that stop Godot from compiling a script.

**Warnings** come from the analyzer: the 45 active codes, plus 3 deprecated-gated behind `#ifndef DISABLE_DEPRECATED`, ported from Godot's `GDScriptWarning::Code`. Default levels are 33 `WARN`, 8 `IGNORE`, and 4 errors by default (`INFERENCE_ON_VARIANT`, `NATIVE_METHOD_OVERRIDE`, `GET_NODE_DEFAULT_WITHOUT_ONREADY`, `ONREADY_WITH_EXPORT`).

Each diagnostic is reported with the same code, message template, severity, and source range as Godot, with ranges converted to LSP positions at the protocol boundary.

## 2. Per-file and on demand, not whole-project

The Godot editor's LSP tries to diagnose almost the whole project at once. gdls does not. It indexes all scripts and native classes so resolution is always correct (see `03-indexing-freshness.md`), but it runs full diagnostics and publishes them for one file at a time: the file the client opened or edited. That matches Claude Code's model exactly, since it injects diagnostics into context after each file edit for the edited file, and otherwise asks per file.

**Dependents.** When file X changes and a dependent Y could now have new errors, gdls does not eagerly re-publish Y. The global index is already updated by the watcher, so the next time Y is opened, edited, or queried, Y is analyzed fresh against the new state and its diagnostics are correct then. Signal stays focused on the file in hand, and stale results are never served.

**Triggers for (re)publishing a document's diagnostics:** `didOpen`, `didChange` (debounced), and `didSave`. A client that prefers pull can ask via `textDocument/diagnostic` instead; the computation is the same and the items are byte-identical.

Project-wide pull (`workspace/diagnostic`) is deliberately not offered, because it contradicts the per-file principle. Clients fall back to per-file pull or push cleanly.

## 3. Strict mode

Strict mode is a post-analysis policy layer, not new analysis machinery. Godot already has the relevant warnings; strict mode changes whether they are enabled and how severe they are. It lives in `crates/gd_analyze/src/warn_policy.rs`.

**Profiles**, set via `initializationOptions.strict.profile`:

| Profile | Behavior |
|---|---|
| `godot` | Mirror the project's own warning configuration from `project.godot`. Pure Godot parity. |
| `strict` | Start from `godot`, then enable all typing-related warnings and promote them to errors: `UNTYPED_DECLARATION`, `INFERRED_DECLARATION`, and the `UNSAFE_*` family (`UNSAFE_PROPERTY_ACCESS`, `UNSAFE_METHOD_ACCESS`, `UNSAFE_CALL_ARGUMENT`, `UNSAFE_CAST`). Effectively "static typing always". |
| `off` | Errors only; all warnings suppressed. |

**Fine-grained overrides**, optional, layered on top of the profile:

```jsonc
"strict": {
  "profile": "strict",
  "enableWarnings":  ["INTEGER_DIVISION"],   // turn specific warnings on
  "disableWarnings": ["UNUSED_SIGNAL"],       // turn specific warnings off
  "errorWarnings":   ["NARROWING_CONVERSION"] // promote specific warnings to errors
}
```

**Precedence:** built-in defaults, then `project.godot` config, then the profile, then fine-grained overrides, then inline `@warning_ignore(code)`, which always wins for its scope.

Configuration can change mid-session through `workspace/didChangeConfiguration`, which rebuilds the policy and drops the analysis cache so later publishes run under the new settings.

## 4. Interaction with `@warning_ignore`

`@warning_ignore(code)`, and the `@warning_ignore_start`/`@warning_ignore_restore` region pair, suppress the named warning within their scope exactly as in Godot, applied after profile and override resolution. Strict mode does not override an explicit in-source ignore.

## 5. Output shape

Diagnostics carry Godot's warning and error identifiers in the LSP `Diagnostic.code` field, so they are greppable and stable across versions. Where the client advertises support, they also carry `tags`, `relatedInformation`, and a `codeDescription.href` pointing at the ProjectSettings anchor for that warning code (the three deprecated codes point at the overview page instead).

`Diagnostic.data` pairs each diagnostic with its quickfixes, so `codeAction` can offer them without re-analyzing: a `_` prefix for the `UNUSED_*` family, an `@onready` insertion for `GET_NODE_DEFAULT_WITHOUT_ONREADY`, and a clearly-labeled mechanical `@warning_ignore` insertion. `source.fixAll` aggregates the safe ones.

## 6. Sources

- [Warning system: enable, disable, promote-to-error, `@warning_ignore`](https://docs.godotengine.org/en/stable/tutorials/scripting/gdscript/warning_system.html)
- [Warning codes; Godot's `gdscript_warning.h` is authoritative for the 45 active plus 3 deprecated-gated codes, the defaults, and the 4 error-by-default entries](https://github.com/godotengine/godot/blob/master/modules/gdscript/gdscript_warning.h)
- [Claude Code injects diagnostics after each edit](https://code.claude.com/docs/en/tools-reference.md)
