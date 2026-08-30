# 01. Architecture

A single self-contained Rust binary (`gdls`) that an LSP client launches over stdio. No Godot process is ever spawned for a request. Internally it is layered so each piece can be tested on its own.

## 1. Component diagram

```
LSP client ──JSON-RPC over stdio──►  gdls  (one static Rust binary on PATH)
┌──────────────────────────────────────────────────────────────────────────┐
│ LSP layer (lsp-server + lsp-types): lifecycle, text sync, requests,        │
│            push + pull diagnostics, progress, initializationOptions        │
├──────────────────┬──────────────────────────────────┬──────────────────────┤
│ Document / VFS   │ Query services                   │ Diagnostics engine    │
│ open buffers     │ symbols, navigation, completion, │ (per-file, on-demand) │
│ layered on disk  │ presentation, code actions       │ runs full analysis of │
│ res:// ↔ path    │                                  │ ONE file, publishes   │
├──────────────────┴──────────────────────────────────┴──────────────────────┤
│ Frontend, the faithful port:   Tokenizer → Parser → Analyzer                │
│   exact Godot codes / messages / ranges; analyzer emits errors + warnings   │
├──────────────────────────────────────────────────────────────────────────┤
│ Symbol / type environment                                                   │
│   • Native class DB   ◄── extension_api.json  (dumped from Godot)           │
│   • Project globals: class_name registry, autoloads, global enums/consts    │
│   • Per-script interface tables: extends, members, signatures               │
├──────────────────────────────────────────────────────────────────────────┤
│ Project indexer   ◄── scans res://**/*.gd, *.tscn, project.godot            │
│ Freshness watcher (notify crate) ◄── res:// create / delete / rename / mod  │
└──────────────────────────────────────────────────────────────────────────┘
```

## 2. Components and responsibilities

1. **LSP layer.** JSON-RPC lifecycle (`initialize`, `initialized`, `shutdown`, `exit`), capability advertisement gated on what the client says it supports, text-document sync, request dispatch, and both push and pull diagnostics. Reads `initializationOptions` and honors runtime `workspace/configuration`. Protocol travels on stdout; all logging goes to stderr. Detail: `05-lsp-cc-integration.md`.
2. **Document / VFS.** In-memory overlay of unsaved edits over on-disk files, and the source of truth for each `.gd` (the open buffer if open, otherwise disk). Owns the `res://` to filesystem mapping. Uses a rope (`ropey`) for cheap incremental edits and byte to line/column to UTF-16 conversions.
3. **Frontend (the faithful port).** Tokenizer, then parser, then analyzer, ported file for file from Godot, newest supported release first, with older ones reached by `Dialect` guards. Each stage emits diagnostics with Godot-matching codes, messages, and source ranges. Detail: `02-frontend-port.md`.
4. **Symbol / type environment.** Three tiers: the native class DB (from `extension_api.json`), the project globals (`class_name` registry plus autoloads), and per-script interface tables. Detail: `03-indexing-freshness.md`.
5. **Project indexer.** Startup scan of every `.gd`, every `.tscn`, and `project.godot`. Interface-level information is extracted eagerly for every file; full body analysis waits until it is asked for. A persistent warm-start cache turns the second and later launches into a stat sweep.
6. **Freshness watcher.** Watches `res://` and updates the index live on any file create, delete, rename, or modify, no matter what the client has open. This is the component that removes staleness structurally.
7. **Diagnostics engine.** On open or edit of file X, fully analyzes X alone against the already-fresh environment and publishes diagnostics for X. No whole-project blast. Detail: `04-diagnostics-strict-mode.md`.
8. **Query services.** Document and workspace symbols come from symbol tables; navigation (`definition`, `references`, `implementation`, call and type hierarchy) from the analyzer's recorded bindings plus a reverse-reference index; hover, completion, and signature help from resolved types plus the documentation pipeline. Position-based queries walk the AST arena via `gd_syntax::ParseTree::iter_ids` and `innermost_node_at` (byte to smallest containing `NodeId`). The typed-ancestor walk that bridges the leaf identifier to the analyzer's pinned type lives at `gd_server::handlers::smallest_typed_containing`.

## 3. Two control loops

**Edit loop:** `didChange(X)`, update the VFS buffer, analyze X, `publishDiagnostics(X)`.

**Watcher loop, independent of what is open:** any on-disk change to Y, re-parse Y's *interface*, update the global registry (a new, renamed, or deleted `class_name`, say), invalidate dependents' cached *full* analysis. The next time a dependent is examined it already sees fresh data.

## 4. Eager interfaces, lazy bodies

This split is what makes 3,000 to 10,000+ files tractable while keeping cross-file diagnostics correct. It mirrors Godot's own shallow-versus-full script resolution, the `GDScriptCache` distinction.

**Interface indexing is eager and global.** Every `.gd` is tokenized, parsed, and shallow-analyzed at startup to extract what it exposes: `extends`, `class_name`, member signatures, signals, enums, inner classes. This is O(files) and cheap, and it is all that cross-file resolution needs.

**Full type analysis is lazy and per file.** Statement and expression checking, plus the full warning set, run only when a file's diagnostics or a query need them, then get cached and invalidated on change.

**A member with no annotation is read off its initializer, in two steps.** The shallow pass decodes everything syntactically decided into a type on the spot: literals, `Array`/`Dictionary` literals, a builtin constructor or constant, `A.new()` and `A.B.new()`, `x as T`, and `$Path`/`%Unique` (a bare `Node`, per `02-frontend-port.md` §11). What it cannot decode it *records*, as `InitShape` — a dotted chain read as a value (`SOME`, `E.A`, `Other.KONST`, `SomeAutoload.level`), a call whose callee is such a chain (`make()`, `Other.make()`), or a `preload` of a string literal, `res://` or written relative to the reading file (the index joins the relative form against that file's own directory when it walks the dependency edges). The reading file's analyzer resolves that shape lazily against the declaring class, through interfaces only, so nothing has to run the other file's analysis and nothing crosses that a later edit could make stale without invalidating.

Together those two are the floor for every cross-file read of a member. A shape missing from the floor reads as `Variant` from another file while Godot has a real type, and the access on it then goes unchecked, along with everything downstream. Three rules keep the floor honest. Under-reporting is the only safe direction, so a shape with more than one reading — an index, a call through a value, a call whose result depends on its arguments — is not recorded at all. An inferred type is soft: Godot hands `var x = e` its `INFERRED` source and reserves the hard `ANNOTATED_INFERRED` for `:=` and for `const`, so the interface carries which one it was (`MemberFlags::ty_is_soft`), the resolver carries it through every hop it walks, and one soft hop makes the whole answer soft. And the resolution never diagnoses: a shape that resolves to nothing, or comes back to a member already being resolved, leaves the permissive `Variant` an unreadable initializer has always produced.

## 5. Concurrency model

The workspace state has one owner. `lsp-server` is synchronous, and `Workspace` carries no `Mutex` or `RwLock`. A router thread reads the wire so `$/cancelRequest` can preempt in-flight work, and `notify-debouncer-full` runs its own thread delivering events on a `crossbeam_channel::Receiver`. The main loop selects over the LSP receiver and the watcher receiver, and it is the only mutator on `Workspace`. This mirrors rust-analyzer's "the event loop accepts an `enum` of possible events" pattern.

## 6. Technology choices

| Concern | Crate | Notes |
|---|---|---|
| LSP transport and types | `lsp-server`, `lsp-types` | Minimal and synchronous, well-suited to a hand-driven event loop. |
| Text buffers | `ropey` | Efficient incremental edits, line and column indexing. |
| Filesystem watching | `notify`, `notify-debouncer-full` | Cross-platform, debounced, with a `crossbeam-channel` receiver for the main loop's `select!`. |
| JSON (API dump, config) | `serde`, `serde_json` | Parses `extension_api.json`, config, and the warm-start cache. |
| Paths | `camino` (UTF-8 paths), `dunce`, `same-file` | Simplifies `res://` mapping; junction-aware canonicalization and file identity on Windows. |
| Hashing | `rustc-hash` | `FxHashMap` and `FxHashSet` throughout. |
| Bounded caches | `lru` | Parse and analysis caches under the memory-pressure ladder. |
| Fuzzy ranking | `nucleo-matcher` | `workspace/symbol` ordering. |
| Observability | `tracing`, `tracing-subscriber`, `tracing-log`, `sysinfo` | Structured stderr spans and peak-RSS sampling. See `06-testing-fidelity.md` §7. |
| Unicode | `unicode-ident`, `unicode-security` | XID identifier rules, and the UTS #39 confusable check the tokenizer needs. |

Dependency-tracked memoized analysis via `salsa` was considered and not adopted: invalidation is hand-rolled, and the trait-based cross-file query seam carries dependency information across the crate boundary instead. Everything compiles to a single self-contained binary.

## 7. Module boundaries

```
gdls/
├─ crates/
│  ├─ gd_syntax/      # tokenizer + parser + AST (no engine knowledge)
│  ├─ gd_types/       # type model + native-class DB ingestion (extension_api.json)
│  ├─ gd_analyze/     # the analyzer: name resolution, type checking, the warning set, strict mode
│  ├─ gd_project/     # project indexer, project.godot parsing, scene index, dependency graph, watcher
│  └─ gd_server/      # LSP layer, VFS, query services, diagnostics engine, main()
└─ docs/              # this spec
```

Each crate answers the isolation test (what it does, how it is used, what it depends on) and is unit-testable on its own. `gd_server` is a library plus a thin `main.rs`, so the event loop can be driven over an in-memory `Connection` in tests. See `06-testing-fidelity.md`.
