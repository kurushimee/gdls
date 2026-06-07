# 01 — Architecture

A single self-contained Rust binary (`gdls`) that Claude Code launches over stdio. No Godot process is
ever spawned. Internally it is layered so each piece is independently testable.

## 1. Component diagram

```
Claude Code ──LSP / JSON-RPC over stdio──►  gdls  (one static Rust binary on PATH)
┌──────────────────────────────────────────────────────────────────────────┐
│ LSP layer (lsp-server + lsp-types): lifecycle, text sync, requests,        │
│            push publishDiagnostics, initializationOptions                   │
├──────────────────┬──────────────────────────────────┬──────────────────────┤
│ Document / VFS   │ Query services                   │ Diagnostics engine    │
│ open buffers     │ documentSymbol, workspaceSymbol, │ (per-file, on-demand) │
│ layered on disk  │ definition, references, hover,   │ runs full analysis of │
│ res:// ↔ path    │ implementation, callHierarchy    │ ONE file, publishes   │
├──────────────────┴──────────────────────────────────┴──────────────────────┤
│ Frontend — the faithful port:   Tokenizer → Parser → Analyzer               │
│   exact Godot codes / messages / ranges; analyzer emits errors + warnings   │
├──────────────────────────────────────────────────────────────────────────┤
│ Symbol / type environment                                                   │
│   • Native class DB   ◄── extension_api.json  (dumped from Godot)           │
│   • Project globals: class_name registry, autoloads, global enums/consts    │
│   • Per-script interface tables: extends, members, signatures               │
├──────────────────────────────────────────────────────────────────────────┤
│ Project indexer   ◄── scans res://**/*.gd  +  project.godot                 │
│ Freshness watcher (notify crate) ◄── res:// create / delete / rename / mod  │
└──────────────────────────────────────────────────────────────────────────┘
```

## 2. Components & responsibilities

1. **LSP layer.** JSON-RPC lifecycle (`initialize`/`initialized`/`shutdown`/`exit`), capability
   advertisement, text-document sync, the request handlers consumed by Claude Code, and **push**
   `publishDiagnostics`. Reads `initializationOptions` (project root, `extension_api.json` path, strict
   config). Protocol travels on stdout; all logging goes to stderr. Detail: `05-lsp-cc-integration.md`.
2. **Document / VFS.** In-memory overlay of unsaved edits over on-disk files; the source of truth per `.gd`
   (open buffer if open, else disk). Owns the `res://` ↔ filesystem mapping. Uses a rope (`ropey`) for cheap
   incremental edits and byte↔line/col↔UTF-16 conversions.
3. **Frontend (faithful port).** `Tokenizer` → `Parser` → `Analyzer`, ported file-for-file from Godot 4.6.3-stable.
   Each stage emits diagnostics with Godot-matching codes, messages, and source ranges. Detail:
   `02-frontend-port.md`.
4. **Symbol / type environment.** Three tiers: the **native class DB** (from `extension_api.json`), the
   **project globals** (`class_name` registry + autoloads), and **per-script interface tables**. Detail:
   `03-indexing-freshness.md`.
5. **Project indexer.** Startup scan of all `.gd` + `project.godot`. Interface-level info is extracted
   eagerly for every file; full body analysis is deferred to on-demand.
6. **Freshness watcher.** Watches `res://` and updates the index live on any file create/delete/rename/modify,
   **regardless of what Claude Code has open**. The component that structurally eliminates staleness.
7. **Diagnostics engine.** On open/edit of file X, fully analyzes **X alone** against the (already-fresh)
   environment and publishes diagnostics for X. No whole-project blast. Detail: `04-diagnostics-strict-mode.md`.
8. **Query services.** `documentSymbol`/`workspaceSymbol` from symbol tables; `definition`/`references`/
   `implementation`/`callHierarchy` from the analyzer's recorded bindings plus a reverse-reference index;
   `hover` from resolved types + native-DB doc strings. Position-based queries (hover/definition)
   walk the AST arena via `gd_syntax::ParseTree::iter_ids` / `innermost_node_at` (byte → smallest
   containing `NodeId`); the typed-ancestor walk that bridges the leaf identifier to the analyzer's
   pinned type lives at `gd_server::handlers::smallest_typed_containing`.

## 3. Two control loops

- **Edit loop:** `didChange(X)` → update VFS buffer → analyze X → `publishDiagnostics(X)`.
- **Watcher loop (independent of what is open):** any on-disk change to Y → re-parse Y's *interface* →
  update the global registry (e.g., a new/renamed/deleted `class_name`) → invalidate dependents' cached
  *full* analysis. The next time a dependent is examined it already sees fresh data.

## 4. The load-bearing principle: eager interfaces, lazy bodies

- **Interface indexing is eager and global.** Every `.gd` is tokenized+parsed+shallow-analyzed at startup to
  extract what it *exposes* (`extends`, `class_name`, member signatures, signals, enums, inner classes).
  This is O(files) and cheap, and it is what cross-file resolution needs.
- **Full type-analysis is lazy and per-file.** Statement/expression checking and the full warning set run
  only when a file's diagnostics or a query require them, then cached and invalidated on change.

This split is what makes 3,000–10,000+ files tractable while keeping cross-file diagnostics correct. It
mirrors Godot's own shallow-vs-full script resolution (the `GDScriptCache` distinction).

## 5. Technology choices (Rust)

| Concern | Candidate crate(s) | Notes |
|---|---|---|
| LSP transport & types | `lsp-server`, `lsp-types` | Minimal, synchronous, well-suited to a hand-driven event loop. `tower-lsp` is an async alternative if concurrency is wanted. |
| Text buffers | `ropey` | Efficient incremental edits; line/column indexing. |
| Filesystem watching | `notify` | Cross-platform; debounced via `notify-debouncer-full`. |
| JSON (API dump, config) | `serde` / `serde_json` | Parse `extension_api.json` and config. |
| Incremental query engine (optional) | `salsa` | For dependency-tracked memoized analysis at scale; a hand-rolled invalidation map is the fallback. |
| Paths | `camino` (UTF-8 paths) | Simplifies `res://` mapping on Windows. |

All chosen to compile to a single dependency-free binary.

## 6. Module boundaries (crate layout sketch)

```
gdls/
├─ crates/
│  ├─ gd_syntax/      # tokenizer + parser + AST (no engine knowledge)
│  ├─ gd_types/       # type model + native-class DB ingestion (extension_api.json)
│  ├─ gd_analyze/     # the analyzer: name resolution, type checking, the warning set, strict mode
│  ├─ gd_project/     # project indexer, project.godot parsing, dependency graph, watcher
│  └─ gd_server/      # LSP layer, VFS, query services, diagnostics engine, main()
└─ docs/              # this spec
```

Each crate answers the brainstorming isolation test — what it does, how it is used, what it depends on —
and is unit-testable in isolation (see `06-testing-fidelity.md`).
