# 00. Overview

`gdls` is a standalone GDScript language server: one static Rust binary that gives any LSP client type-aware diagnostics and navigation over stdio, with no Godot engine or editor running.

| | |
|---|---|
| **Target language** | GDScript 2.0 as shipped in Godot 4.6 and 4.7, from one binary, chosen per project |
| **Engine reference** | Official Godot (`godotengine/godot`) at tags `4.6.3-stable` and `4.7.2-stable`. The GDScript frontend is the port's source of truth: it is unchanged from upstream, and only native C++ classes differ. Native classes are ingested from the `godot` binary's `extension_api.json`. |
| **Transport** | LSP over stdio, one server per workspace root |

## 1. What it does

gdls does its own tokenizing, parsing, and type analysis. It emits the same compile-time diagnostics Godot's own analyzer emits, errors plus the GDScript warning set (45 active warnings at 4.6, 46 at 4.7, and 3 more behind Godot's `DISABLE_DEPRECATED` guard), and it adds an opt-in stricter mode on top.

It reads each project as the Godot release that project targets. The version comes from `project.godot`'s `application/config/features`, and everything downstream — tokenizer, parser, analyzer, warning set, and the bundled engine surface — follows it, so a 4.6 project keeps getting 4.6 answers after 4.7 support lands. `02-frontend-port.md` §11c and §11d list every behavior that differs.

It recognizes every native engine class and every third-party GDExtension class installed in the project. Its own filesystem watcher picks up new, renamed, and deleted classes as they land on disk.

On top of diagnostics it serves the full generic LSP surface: hover, definition, declaration, type definition, references, implementation, call hierarchy, type hierarchy, document and workspace symbols, completion, signature help, rename, document highlight, semantic tokens, inlay hints, document colors, code actions, folding and selection ranges, document links, and file-rename edits. `05-lsp-cc-integration.md` has the full list.

## 2. Why it exists

The motivating context is a very large game (3,000 to 10,000+ `.gd` files) worked on mainly through Claude Code. The alternative is the Godot editor's built-in LSP, which has three problems at that scale:

1. **Weight.** The editor is heavy, and at this scale it gets unresponsive.
2. **Staleness.** Its diagnostics depend on what the editor has synced and imported. New `class_name`s, renames, and deletions go unrecognized until the editor regains focus and rescans. This is a documented Godot bug class (references in §7).
3. **Coupling.** It requires keeping the editor, or a headless engine, running.

gdls has no `EditorFileSystem`, no focus-gated rescan, and no editor in the loop, so the staleness problem is gone by construction rather than worked around.

## 3. Out of scope

- **Running GDScript.** No bytecode and no VM. Only Godot's frontend is ported; the `compiler` and `codegen` half of the module is deliberately excluded.
- **Any GUI, debugger, or scene and resource editing.**
- **A built-in formatter.** There is no formatter in Godot's frontend to port. gdls bridges to an external command instead (`09-lsp-conventions.md` §6.6).
- **Godot's custom LSP protocol extensions** (`gdscript/*`, `gdscript_client/*`). Permanently excluded; see the anti-catalog in `09-lsp-conventions.md` §3.

## 4. Design decisions

| Decision | Choice | Rationale |
|---|---|---|
| Godot runtime dependency | None, ever | A `--headless` Godot is the whole engine plus the same `EditorFileSystem` staleness. |
| Build strategy | Faithful port of Godot's tokenizer, parser, and analyzer | Highest fidelity to "exactly what Godot has", and validatable against Godot's own test corpus. |
| Implementation language | Rust | Fast on huge projects, single static executable, strong parser and LSP ecosystem. |
| Native-class source | `extension_api.json` from the `godot` binary (`--dump-extension-api-with-docs`), taken in-project so installed GDExtensions are captured too, with `doc_classes` XML as a static fallback | Engine classes come along automatically; GDExtensions are enumerated via `res://**/*.gdextension`. Static, and regenerated on engine rebuild or addon change. See `03-indexing-freshness.md` §1 and §2. |
| Diagnostics fidelity | Match Godot exactly, plus opt-in always-strict typing | Validatable against the `.gd`/`.out` corpus. Strict mode promotes typing warnings to errors. |
| Diagnostics trigger | Per file, on open and edit, never whole-project | Matches Claude Code's push-after-edit model, and makes less noise. |
| Scene and project parsing | `project.godot` mandatory; `.tscn` parsed as text, never instantiated | Autoloads, paths, and warning config are cheap and essential. Scene knowledge comes from reading the file, not from running the engine. |
| `$` and `%` typing | Bare `NATIVE Node` in the analyzer, precise scene types for navigation only | This is what Godot's own analyzer does. Feeding a precise scene type into the diagnostic path would false-positive on downcasts Godot tolerates. See `02-frontend-port.md` §11. |

Three rules run through every file in this spec:

**Never crash.** The parser always returns a possibly-partial AST, so the server can always respond. Position conversions clamp out-of-range input instead of panicking. `panic = "unwind"` keeps a stray panic to a logged error mid-session, not a dead process.

**Never lie.** A negative claim needs a basis. When the native surface is not project-derived, an unknown type or member stays dynamic rather than becoming an error (`02-frontend-port.md` §11b). A shallow-interface gap degrades to `Variant` instead of surfacing as a warning. Malformed `initializationOptions` fall back to documented defaults with a warning, never failing `initialize`.

**Unknown stays dynamic.** Where Godot would have a populated `ClassDB` and gdls does not, the answer is `Variant`, not a diagnostic.

## 5. Why not the alternatives

**Headless Godot LSP.** `--headless` runs the full engine minus rendering. It hosts the LSP among many other subsystems, it is heavy, and it inherits the same `EditorFileSystem` and global-cache staleness.

**Embedding Godot's analyzer as a library.** No standalone build exists. Maintainers state that GDScript 2.0 is "still designed for tight engine integration, using Godot's custom datatypes" (proposal #6199), and accurate diagnostics need a populated `ClassDB`, which is filled by the very engine modules one would strip out. Even initializing only Godot core is a Godot instance, and the LSP type is "bound to Godot's `Object` system" (proposal #11056).

**Existing external tools.** None are type-aware. gdtoolkit's `gdlint` is syntax and style only; tree-sitter-gdscript is parser-only. There was nothing to fork.

## 6. Document map

| File | Contents |
|---|---|
| `01-architecture.md` | Components, control loops, crate layering |
| `02-frontend-port.md` | Tokenizer, parser, and analyzer port; type system; name resolution; `$` and `%` policy |
| `03-indexing-freshness.md` | Native-class ingestion, project indexer, incrementalism, warm-start cache, filesystem watcher |
| `04-diagnostics-strict-mode.md` | Diagnostics model, per-file policy, strict mode, warning config |
| `05-lsp-cc-integration.md` | The LSP surface, configuration, deployment |
| `06-testing-fidelity.md` | Conformance corpus, differential oracle, fuzzing, observability, perf budgets |
| `07-maintenance.md` | Tracking upstream Godot, regenerating the API dump, the release gate, known risks |
| `08-history.md` | How the project was built, milestone by milestone |
| `09-lsp-conventions.md` | The generic-LSP contract: governing rules, the Godot-LSP anti-catalog, capability gating, per-feature wire conventions |

## 7. Sources

- [Godot GDScript module overview and pipeline](https://github.com/godotengine/godot/blob/master/modules/gdscript/README.md)
- [GDScript warning codes; Godot's `gdscript_warning.h` is authoritative, 45 active plus 3 deprecated-gated](https://github.com/godotengine/godot/blob/master/modules/gdscript/gdscript_warning.h)
- [GDScript conformance test runner (`.gd`/`.out`)](https://github.com/godotengine/godot/blob/master/modules/gdscript/tests/gdscript_test_runner.cpp)
- [GDScript warning system docs](https://docs.godotengine.org/en/stable/tutorials/scripting/gdscript/warning_system.html)
- [GDExtension API and `extension_api.json`](https://deepwiki.com/godotengine/godot/15.1-gdextension-api)
- [Proposal #6199, standalone/embeddable GDScript](https://github.com/godotengine/godot-proposals/discussions/6199)
- [Proposal #11056, refactor the GDScript Language Server](https://github.com/godotengine/godot-proposals/issues/11056)
- [Proposal #3300, headless LSP CLI argument](https://github.com/godotengine/godot-proposals/issues/3300)
- Documented external-editor staleness, the motivation: [godot#69485](https://github.com/godotengine/godot/issues/69485) and [godot#107592](https://github.com/godotengine/godot/issues/107592)
- [gdtoolkit linter, syntax and style only](https://github.com/Scony/godot-gdscript-toolkit/wiki/3.-Linter)
- [tree-sitter-gdscript, parser only](https://github.com/PrestonKnopp/tree-sitter-gdscript)
- [tree-sitter-godot-resource (`.tscn` and `project.godot`)](https://github.com/PrestonKnopp/tree-sitter-godot-resource)
- [Autoloads and singletons](https://docs.godotengine.org/en/stable/tutorials/scripting/singletons_autoload.html)
- [Nodes and scene instances (`$` typing)](https://docs.godotengine.org/en/stable/tutorials/scripting/nodes_and_scene_instances.html)
- [Claude Code, Plugins Reference (LSP servers)](https://code.claude.com/docs/en/plugins-reference.md)
- [Claude Code, Tools Reference (LSP tool behavior)](https://code.claude.com/docs/en/tools-reference.md)
