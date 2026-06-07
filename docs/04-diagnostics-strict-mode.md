# 04 — Diagnostics model, per-file policy, and strict mode

> **Status (post-M3 closure):** the strict-mode policy layer described in §3 shipped in M3
> (`crates/gd_analyze/src/warn_policy.rs`), the 4 LSP triggers in §2 are wired in the server
> (`crates/gd_server/src/handlers.rs` + `server.rs`), and analyzer-phase conformance is at
> **1.0000 (300/300)**, including strict-mode wire tests in
> `crates/gd_server/tests/symbols_and_diagnostics.rs`. The text below describes the contract;
> the milestone attribution lives in `07-milestones-risks.md` row M3.

## 1. What counts as a diagnostic

- **Errors** come from the **parser** (syntax) and the **analyzer** (type/semantic). They are the same
  conditions that stop Godot from compiling a script.
- **Warnings** come from the **analyzer**: the **45 active codes** (+ 3 deprecated-gated behind
  `#ifndef DISABLE_DEPRECATED`) ported from Godot's `GDScriptWarning::Code`. Default levels are 33 `WARN`,
  8 `IGNORE`, and 4 **errors by default** (`INFERENCE_ON_VARIANT`, `NATIVE_METHOD_OVERRIDE`,
  `GET_NODE_DEFAULT_WITHOUT_ONREADY`, `ONREADY_WITH_EXPORT`).

Each diagnostic is reported with the same code, message template, severity, and source range as Godot
(ranges converted to LSP UTF-16 positions at the protocol boundary).

## 2. Per-file, on-demand policy (not whole-project)

The built-in LSP tries to diagnose almost the whole project at once; gdls does **not**. Policy:

- gdls **indexes** all scripts and native classes (so resolution is always correct — see
  `03-indexing-freshness.md`), but it **runs full diagnostics and publishes them for one file at a time**:
  the file Claude Code opened or edited.
- This matches Claude Code's model exactly — it injects diagnostics into context **after each file edit**
  for the edited file, and otherwise asks per file.

**Dependents.** When file X changes and a *dependent* Y could now have new errors, gdls does **not** eagerly
re-publish Y. Instead, the global index is already updated (watcher), so the next time Y is opened/edited/
queried, Y is analyzed fresh against the new state and its diagnostics are correct then. This keeps signal
focused on the file in hand while never serving stale results.

**Triggers for (re)publishing a document's diagnostics:** `didOpen`, `didChange` (debounced), and `didSave`.

## 3. Strict mode (the "always statically typed" requirement)

Strict mode is a **post-analysis policy layer**, not new analysis machinery — Godot already has the relevant
warnings; strict mode changes their **enablement and severity**.

**Profiles** (set via `initializationOptions.strict.profile`):

| Profile | Behavior |
|---|---|
| `godot` | Mirror the project's own warning configuration from `project.godot`. Pure Godot parity. |
| `strict` | Start from `godot`, then **enable all typing-related warnings and promote them to errors**: e.g. `UNTYPED_DECLARATION`, `INFERRED_DECLARATION`, and the `UNSAFE_*` family (`UNSAFE_PROPERTY_ACCESS`, `UNSAFE_METHOD_ACCESS`, `UNSAFE_CALL_ARGUMENT`, `UNSAFE_CAST`). Effectively "static typing always." |
| `off` | Errors only; all warnings suppressed. |

**Fine-grained overrides** (optional, layered on top of the profile):

```jsonc
"strict": {
  "profile": "strict",
  "enableWarnings":  ["INTEGER_DIVISION"],   // turn specific warnings on
  "disableWarnings": ["UNUSED_SIGNAL"],       // turn specific warnings off
  "errorWarnings":   ["NARROWING_CONVERSION"] // promote specific warnings to errors
}
```

**Precedence:** built-in defaults → `project.godot` config → profile → fine-grained overrides →
inline `@warning_ignore(code)` (always wins for its scope).

## 4. Interaction with `@warning_ignore`

`@warning_ignore(code)` (and any block/line variants present in 4.6.3) suppress the named warning within their
scope, exactly as in Godot — applied **after** the profile/override resolution above. Strict mode does not
override an explicit in-source ignore.

## 5. Output shape

Diagnostics are delivered via push `textDocument/publishDiagnostics` (see `05-lsp-cc-integration.md`).
Codes are exposed in the LSP `Diagnostic.code` field using Godot's warning/error identifiers so they are
greppable and stable across versions.

## 6. Sources

- Warning system: enable/disable, promote-to-error, `@warning_ignore` — https://docs.godotengine.org/en/stable/tutorials/scripting/gdscript/warning_system.html
- Warning codes (orientation; Godot's `gdscript_warning.h` is authoritative — 45 active + 3 deprecated-gated, defaults, the 4 error-by-default) — https://github.com/godotengine/godot/blob/master/modules/gdscript/gdscript_warning.h
- Claude Code injects diagnostics after each edit — https://code.claude.com/docs/en/tools-reference.md
