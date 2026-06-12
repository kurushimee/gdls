# gdls — Standalone GDScript Language Server for Godot 4.6.3-stable

> **Codename:** `gdls`. A single self-contained language server providing type-aware GDScript
> diagnostics and navigation to Claude Code **without running any Godot engine or editor instance**.

| | |
|---|---|
| **Spec date** | 2026-05-20 |
| **Status** | Original design spec, retained for rationale. Phase 1 = **M0–M6** = v1 — shipped (v1.0.0 through v1.0.5; see [`08-m6-v1-ship.md`](08-m6-v1-ship.md), [`../CHANGELOG.md`](../CHANGELOG.md), [`../README.md`](../README.md)). Phase 2 = **M7–M11** (the generic-language-server phase) is specified in [`09-phase-2.md`](09-phase-2.md). |
| **Target language** | GDScript 2.0 as shipped in **Godot 4.6.3-stable** |
| **Engine** | Official Godot (`godotengine/godot`) at tag `4.6.3-stable` — the GDScript frontend is the port's source of truth (unchanged from upstream; only native C++ classes differ). Native classes are ingested from the `godot` binary's `extension_api.json`. |
| **Consumer** | Claude Code's native LSP client (stdio) |

---

## 1. Problem statement

The motivating context is a very large game (**3,000–10,000+ `.gd` files**) in Godot, worked on
primarily through Claude Code. Today GDScript intelligence comes from the Godot editor's built-in LSP,
which has three problems:

1. **Weight.** The editor is heavy; at this scale it becomes unresponsive ("wonky").
2. **Staleness.** Diagnostics depend on what the editor has synced/imported. New `class_name`s, renames,
   and deletions are not recognized until the editor regains focus and rescans — a documented Godot bug
   class (see references in §7).
3. **Coupling.** It requires keeping the editor (or a headless engine) running.

## 2. Goal

A standalone server, **gdls**, that:

- Runs as a **single static binary, no Godot process at runtime**.
- Does its own tokenizing, parsing, and **type analysis**, emitting **compile-time diagnostics that match
  Godot's own analyzer** (errors + the GDScript warning set — 45 active + 3 deprecated-gated in Godot),
  plus an opt-in stricter mode.
- Recognizes **all native engine classes and third-party GDExtension classes installed in the
  project**.
- **Auto-recognizes new/renamed/deleted classes live**, via its own filesystem watcher — eliminating the
  staleness problem by construction.
- Speaks LSP to Claude Code over **stdio**.

## 3. Non-goals (v1)

- Running GDScript (no bytecode/VM — diagnostics only; the `compiler`/`codegen` half of Godot's frontend
  is out of scope).
- Parsing `.tscn` for precise `$Node` / `%Unique` typing → **Phase 2** (now specified: `09-phase-2.md` M11).
- `signatureHelp` / `completion` → **Phase 2** (Claude Code does not consume them per its docs; editors do — `09-phase-2.md` M8).
- Any GUI, debugger, formatter, or scene/resource editing. (Phase 2 adds an optional *external*-formatter bridge only — `09-phase-2.md` §5.)

## 4. Locked decisions (from brainstorming)

| Decision | Choice | Rationale |
|---|---|---|
| Godot runtime dependency | **None, ever** | A `--headless` Godot is the whole engine + the same `EditorFileSystem` staleness. Rejected by the user. |
| Build strategy | **Faithful port** of Godot's tokenizer + parser + analyzer | Highest fidelity to "exactly what Godot has"; validated against Godot's own test corpus. |
| Implementation language | **Rust** | Fast on huge projects; single static `.exe`; strong parser/LSP ecosystem. |
| Native-class source | **`extension_api.json`** from the `godot` binary (`--dump-extension-api`), taken **in-project** so installed GDExtensions are captured too; `doc_classes` XML as a static fallback | Engine classes included automatically; GDExtensions enumerated via `res://**/*.gdextension`; static; regenerated on engine rebuild / addon change. See `03-indexing-freshness.md` §1–§2. |
| Diagnostics fidelity | **Match Godot exactly + opt-in always-strict typing** | Validatable via the `.gd/.out` corpus; strict mode = promote typing warnings to errors. |
| Diagnostics trigger | **Per-file, on open/edit** (not whole-project) | Matches Claude Code's push-after-edit model; less noise. |
| Scene/project parsing | `project.godot` **mandatory**; `.tscn` node typing **Phase 2** | Autoloads/paths/warning-config are cheap & essential; scene typing is a heavier subsystem. |
| `$` / `%` in v1 | **Permissive deferred-node type** (zero false positives) | Never feed Claude Code a phantom error before scene typing exists (see `02-frontend-port.md` §11). |

## 5. Why the alternatives were rejected (grounded)

- **Headless Godot LSP.** `--headless` runs the full engine minus rendering; it hosts the LSP among many
  other subsystems, is heavy, and inherits the same `EditorFileSystem`/global-cache staleness. User-confirmed,
  and consistent with the documented external-editor staleness bugs.
- **Embedding Godot's analyzer as a clean library.** No standalone build exists; maintainers state GDScript
  2.0 is "still designed for tight engine integration, using Godot's custom datatypes" (proposal #6199), and
  accurate diagnostics require a **populated `ClassDB`**, which is filled by the very engine modules one would
  strip out. Even initializing only Godot core is "a Godot instance," violating the hard constraint, and the
  LSP type is "bound to Godot's `Object` system" (proposal #11056).
- **Existing external tools.** None are type-aware: gdtoolkit's `gdlint` is syntax + style only;
  tree-sitter-gdscript is parser-only. We are building net-new capability — there is nothing to fork.

## 6. Document map

| File | Contents |
|---|---|
| `01-architecture.md` | Components, control loops, technology choices |
| `02-frontend-port.md` | Tokenizer/parser/analyzer port, type system, name resolution, `$`/`%` policy |
| `03-indexing-freshness.md` | Native-class ingestion, project indexer, incrementalism, filesystem watcher |
| `04-diagnostics-strict-mode.md` | Diagnostics model, per-file policy, strict mode, warning config |
| `05-lsp-cc-integration.md` | LSP surface, Claude Code config & deployment |
| `06-testing-fidelity.md` | Conformance corpus, differential testing, robustness |
| `07-milestones-risks.md` | Phased milestones, effort, risks, maintenance |
| `08-m6-v1-ship.md` | **M6** (the v1 ship milestone): exposed-capability parity gaps vs Godot's own LSP, the warm-start index cache, multi-instance safety, exit criteria |
| `09-phase-2.md` | **Phase 2 (M7–M11)**: the generic-language-server phase — full editor-grade LSP surface (completion, semantic tokens, rename, …), the Godot-LSP weirdness anti-catalog, `.tscn` scene typing, exit criteria |

## 7. References (sources)

- Godot GDScript module overview & pipeline — https://github.com/godotengine/godot/blob/master/modules/gdscript/README.md
- GDScript warning codes (orientation; Godot's `gdscript_warning.h` is authoritative — 45 active + 3 deprecated-gated) — https://github.com/godotengine/godot/blob/master/modules/gdscript/gdscript_warning.h
- GDScript conformance test runner (`.gd`/`.out`) — https://github.com/godotengine/godot/blob/master/modules/gdscript/tests/gdscript_test_runner.cpp
- GDScript warning system docs — https://docs.godotengine.org/en/stable/tutorials/scripting/gdscript/warning_system.html
- GDExtension API / `extension_api.json` — https://deepwiki.com/godotengine/godot/15.1-gdextension-api
- Proposal #6199 — standalone/embeddable GDScript — https://github.com/godotengine/godot-proposals/discussions/6199
- Proposal #11056 — refactor the GDScript Language Server — https://github.com/godotengine/godot-proposals/issues/11056
- Proposal #3300 — headless LSP CLI argument — https://github.com/godotengine/godot-proposals/issues/3300
- gdtoolkit linter (syntax + style only) — https://github.com/Scony/godot-gdscript-toolkit/wiki/3.-Linter
- tree-sitter-gdscript (parser only) — https://github.com/PrestonKnopp/tree-sitter-gdscript
- tree-sitter-godot-resource (`.tscn` / `project.godot`) — https://github.com/PrestonKnopp/tree-sitter-godot-resource
- Autoloads / singletons — https://docs.godotengine.org/en/stable/tutorials/scripting/singletons_autoload.html
- Nodes & scene instances (`$` typing) — https://docs.godotengine.org/en/stable/tutorials/scripting/nodes_and_scene_instances.html
- Claude Code — Plugins Reference (LSP servers) — https://code.claude.com/docs/en/plugins-reference.md
- Claude Code — Tools Reference (LSP tool behavior) — https://code.claude.com/docs/en/tools-reference.md
