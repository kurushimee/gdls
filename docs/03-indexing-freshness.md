# 03. Symbol environment, project indexing, incrementalism, and freshness

Two requirements sound scary up front, recognizing every native class and noticing class changes live, and both turn out to be small well-understood components. This is where they live.

## 1. Native class DB (`gd_types`): engine classes

The source is `extension_api.json`, produced by running the godot binary once:

```
godot --dump-extension-api-with-docs
```

Because the dump iterates `ClassDB`, every class Godot registers through the normal `ClassDB`/`GDREGISTER_CLASS` path appears automatically, with no C++ parsing. This is the canonical way to feed custom native classes to an external analyzer.

> **Scope note:** the dump snapshots whatever is registered in `ClassDB` at the moment it runs. A bare-binary dump therefore contains engine classes but not third-party GDExtensions installed in the project. Those are handled in §2.

**Consumed fields**, per class: `name`, `inherits` (which gives full inheritance chains), `methods` (each with `name`, `arguments` carrying name, type, and default, `return_value` type, and the `is_const`, `is_static`, `is_virtual`, `is_vararg` flags), `properties`, `signals`, `enums`, `constants`, plus the `description` and `brief_description` prose. Plus builtin and Variant classes, global enums, and utility functions. That is exactly the signature, return, virtual, inheritance, and documentation information the analyzer, hover, and completion need.

**Structures:** an interned class graph plus per-class method, property, and signal tables for O(1) lookup.

**Versioning and reload:** the DB stores the dump's version header and a content hash. The watcher reloads it when the file changes, which is to say after an engine rebuild. If the file is absent, resolution degrades gracefully per the provenance rules in `02-frontend-port.md` §11b: types stay dynamic, one informational notice goes out, and nothing crashes.

**Auto-dump, where the user does nothing.** When `extensionApiPath` is not set, gdls manages the dump itself. It discovers a Godot binary (`godotBinaryPath` option, then the `GDLS_GODOT` env var where empty or `off` hard-disables, then `godot4`/`godot` on `PATH`), runs `godot --headless --path <root> --dump-extension-api-with-docs` at session startup, and keeps the result plus staleness metadata (binary path, size and mtime, and the project's `.gdextension` file set) under `.gdls/`.

Operational facts the implementation is built around, verified on 4.6.3:

- The dump lands in the project root regardless of the child's cwd. So a pre-existing user `<root>/extension_api.json` suppresses the dump entirely (never clobber it), and the fresh output is moved into `.gdls/extension_api.json`.
- Godot may abort on exit *after* writing a complete dump, so the artifact decides, never the exit status.
- A never-imported project (no `.godot/extension_list.cfg`) loads no GDExtensions, so its dump misses their classes. gdls detects the symptom (declared extensions, none of their class hints resolving) and logs the remediation: open the project in the editor once.

The resolution ladder when no explicit path is set: fresh `.gdls` dump, then auto-dump, then stale `.gdls` dump, then unmanaged `<root>/extension_api.json`, then the embedded stock surface, then empty and dynamic. Spawning happens only at session startup, only for real projects (`project.godot` present), with the child's stdout and stderr piped (stdout is the LSP wire) and a 5 minute kill-timeout. The dump runs on a background thread and is adopted mid-session, so the budget is deliberately generous, and a deadline-killed dump that already wrote its artifact is still adopted.

**Manual workflow**, when auto-dump is disabled or no binary is found: after rebuilding Godot, re-run `--dump-extension-api-with-docs` from inside the project and point gdls at the JSON with `initializationOptions.extensionApiPath`. An explicit path always wins and never triggers a spawn.

> `doc/classes/*.xml` was considered and rejected as the primary source for engine classes. It is doc-oriented, and a Godot class needs its own `doc_classes` XML to appear at all, whereas `extension_api.json` is complete by construction. Doc XML *is* used for GDExtensions; see §2.

## 2. Third-party GDExtensions installed in the project

GDExtension classes (addons providing native `.dll`, `.so`, or `.dylib` classes) are registered into `ClassDB` only when their library is loaded, after which they are indistinguishable from core classes and the editor autocompletes them. gdls captures them explicitly, and they then occupy the same native-symbol lookup tier as engine classes (`02-frontend-port.md` §7).

**Enumeration.** A scan of `res://**/*.gdextension` discovers installed extensions. A `.gdextension` file is configuration only (per-platform `libraries` plus `entry_symbol`); it does not contain the class API, so it locates extensions and their docs, never signatures.

**Capture** is multi-source, since a project may mix all of these:

1. **In-project API dump, preferred.** The `extension_api.json` dump taken in the project context, with extensions loaded, so their `ClassDB` entries land in the same JSON as engine classes. This is what the auto-dump in §1 produces.
2. **`doc_classes` XML, the pure-static fallback.** Many GDExtensions ship documentation in the same XML format as Godot's class reference (method, property, and signal signatures), authored with `godot --doctool --gdextension-docs`. When it is present, gdls ingests it directly, no execution required. The same XML reader serves the engine `doc/classes/*.xml` path.
3. **Graceful degradation.** An extension present in neither the dump nor any `doc_classes` XML has its classes treated as unknown and dynamic, so no false positives, with one informational notice. Identical to a missing `extension_api.json`.

**Reload.** The watcher treats `.gdextension` files and any ingested `doc_classes` XML as inputs, so adding or removing an addon, or updating its docs, re-runs enumeration and ingestion (§6).

## 3. Project globals and `project.godot`

`project.godot` is parsed at startup and on change. It is INI-like, so a small dedicated parser is enough. It supplies:

- **Autoloads.** The `[autoload]` section maps singleton names to script or scene paths; the *type* comes from the referenced script's `class_name` or base. Autoload typing is subtle even in Godot, so gdls tracks the referenced script and resolves its type through the normal pipeline. A scene target resolves through its `uid://` to the scene's root script; a scriptless root falls to the bare `Node` floor.
- **The `res://` root**, needed to resolve `preload` and `load("res://…")` and to map paths.
- **Warning configuration**: which warnings are enabled, disabled, or promoted. Feeds `04-diagnostics-strict-mode.md`.

The `class_name` registry maps each global type name to its script path and resolved type, built from the eager interface pass (§4).

## 4. Project indexer: eager interfaces, lazy bodies

**Eager interface pass, at startup, O(files):** tokenize, parse, and shallow-analyze every `.gd` to extract its interface, meaning `extends`, `class_name`, member signatures (vars, consts, funcs with types), signals, enums, and inner classes. This populates the `class_name` registry and the per-script interface tables. It is fast, and it is all cross-file resolution requires.

**Lazy full pass, on demand:** full statement and expression type checking plus the complete warning set run only when a file's diagnostics or a query need them. Results are cached and invalidated on change.

**Two sibling indices** are built by the same scan. The `SceneIndex` parses every `.tscn` as text, never by instantiating anything, into the node tree plus each node's `type=` and `script=`, which is what `$`/`%` navigation and node-path completion read (`02-frontend-port.md` §11). The `AssetIndex` enumerates every other project file, so `load` and `preload` completion can offer real `res://` paths. Assets are defined by exclusion, meaning everything that is not a script, scene, or engine-managed file, since `res://LICENSE` is a perfectly listable asset.

## 5. Incrementalism at 3,000 to 10,000+ files

**Dependency graph.** Each file records its dependencies: the `extends` target, `preload`/`load` targets, used `class_name`s, and referenced autoloads. Forward and reverse edges are both maintained. Lives in `gd_project::DepGraph`, with `reverse_closure` driving invalidation.

**Invalidation.** On a change to file Y: re-run Y's interface extraction; if Y's *interface* changed (a renamed method, a new or removed `class_name`), invalidate the cached full analysis of Y and of Y's reverse-dependents; if only Y's *bodies* changed, invalidate only Y.

**Memory.** Full ASTs are kept only for open documents. A closed file keeps just its interface table and re-parses on demand, which bounds memory at large scale. `gd_server::Workspace::forget` (called on `didClose`) drops both parse and analysis caches, and `Workspace::reindex` then re-extracts the interface from a fresh disk parse so the index stays accurate. Both caches are LRU-bounded and sit under the memory-pressure ladder in `06-testing-fidelity.md` §7.3.

**Cold start is paid once.** The eager-interface index is serialized to disk so later launches skip the scan; see §8.

**Cross-file member-initializer cycles.** The analyzer records the per-file xref set inline on `AnalysisResult.member_xrefs`, and the `gd_server::xfile::WorkspaceXFileQuery` wrapper over `SyntacticQuery` reads that field from the existing analysis cache to answer `CrossFileQuery::member_initializer_xrefs`. Invalidation comes free, since the xrefs go with the entry when `analysis_cache` evicts it on `didChange`. See §7.5.

## 6. The freshness watcher

`notify` (with `notify-debouncer-full`) recursively watches the `res://` tree, excluding `.godot/` and import caches. Events are debounced, then classified as create, delete, modify, or rename. Rename is detected from create plus delete pairs, to preserve identity where possible.

Reactions:

- **A `.gd` change** re-extracts the interface, updates the `class_name` registry immediately (adding, renaming, or removing a global type), and invalidates dependents per §5. A file *appearing* re-links the consumers waiting on it both by name (`extends MyBase` referencing a just-added `class_name`) and by path (`extends "res://b.gd"` whose target file was just created). The path case goes through `Index`'s `path_referencers` reverse index, the path-keyed analogue of `name_referencers`, so a path-extends consumer's stale "unknown base" diagnostics refresh without waiting for the consumer itself to be edited.
- **A `project.godot` change** reloads autoloads, the `res://` root, and warning config, then calls `gd_server::Workspace::rebuild_policy` to drop the analysis cache so later `publishDiagnostics` runs under the new strict configuration.
- **An `extension_api.json` change** reloads the native class DB (§1).
- **A `.gdextension` add or remove, or a watched `doc_classes` XML change**, re-enumerates and re-ingests GDExtension classes (§2).
- **A `.tscn` change** reindexes the scene (`reindex_scene`, `remove_scene`). It does not re-diagnose the scene's attached scripts, because a `$` or `%` type is scene-independent and republishing would be byte-identical churn (`02-frontend-port.md` §11).
- **Any other file** updates the asset index, so `load` and `preload` completion stay current.

**Why this fixes the pain:** there is no `EditorFileSystem`, no focus-gated rescan, and no editor sync in the loop. A new, renamed, or deleted class shows up in the index as soon as the file changes on disk, no matter which file the client has open.

### 6.1 Operational specifics

**Debouncer.** `notify-debouncer-full` with a 250 ms quiet-time policy: emit the coalesced event set once no further filesystem event has arrived for the file or directory for 250 ms. This turns burst writes from atomic-write editors (create a `.tmp`, write, rename) into a single Modify event, matches the cadence users tolerate in editor LSPs, and stays well under the bulk-event budget below.

**Exclusion list, always excluded.** Path *components* `.godot/` (engine editor cache and import artifacts), `.import/` (per-resource import caches), `.git/`, and `.gdls/` (gdls's own managed dump and warm-start cache), plus `target/` and `node_modules/` as defensive entries for projects sharing a directory with non-Godot tooling. Plus file-name *suffixes* `.tmp`, `.bak`, `.swp`, and `~`. `addons/` is deliberately not excluded, since Godot installs addon `.gd` scripts, `.gdextension` files, and `doc_classes/*.xml` there and the index and watcher must surface them. `notify` exposes no per-path ignore API, so this is not a watch-time filter: a single shared predicate (`gd_project::is_excluded`, used identically by the cold index, by `Workspace::reconcile`, and by the watcher) is applied post-receipt to every debounced batch, and gates `WalkDir::filter_entry` for the cold-scan and reconcile walks. Matching is case-insensitive on the component name, for macOS HFS+ and Windows NTFS defaults.

**Path normalization.** All paths flow through `camino::Utf8PathBuf`. On Windows, backslashes are converted to forward slashes at the watcher boundary so downstream `Index` keys match the cold-scan output, which uses forward-slash paths from `walkdir`. macOS case-folding is handled inside `Index::normalize` (`crates/gd_project/src/index.rs`). A file reached through an NTFS junction or a differently-cased path interns to one `FileId`, via `dunce`-backed canonicalization in `gd_server::uri::CanonicalKey`.

**Threading.** The watcher's debouncer runs its own thread and delivers events on a `crossbeam_channel::Receiver<DebouncedEvent>`. The main loop selects over that and the LSP receiver, and is the only mutator on `Workspace`. See `01-architecture.md` §5.

**Lifecycle ordering.** The watcher is constructed in `serve()` after the `initialize` *response* has been sent and before `Workspace::load`, so every filesystem change landing during the load's stat pass is a queued channel event, replayed once the loop arms. Arming is cheap because the debouncer runs `NoCache`: the default `FileIdMap` cache walked the entire tree opening a handle per file purely to pair rename events, costing 7 to 9 s and about 70 MB on a 2.3k-file NTFS project, and gdls handles unpaired rename halves anyway. The server does not send an `initialized` notification; `initialized` is a client-to-server message it only receives and logs. The module doc on `crates/gd_server/src/watcher.rs` is authoritative for this ordering.

**Reconciliation backstop after load.** After the load settles, the server walks `res://**/*.gd` once more and diffs against the index. With a live watcher armed (the normal case) this runs in `DiscoverOnly` mode: paths already in the stat table were just validated by the load itself, and modifications in the gap are queued watcher events, so the backstop only stats and parses files *added* while the server was off, plus the standard removal pass. Everything known is enumeration-only, and the per-file stat reuses the directory enumeration's own metadata, which costs zero extra syscalls on Windows. When no watcher armed (construction failed) the backstop runs `FullStat`, the stat-diff of every file, as do the watcher `need_rescan` overflow path, the watcher-disabled fallback tick, and `gdls diagnose --reconcile` for ad-hoc recovery after wake-from-suspend or remote-filesystem hiccups. Logged with the `cold_index_reconciled … mode=discover|full` marker. This split handles both `notify` dropping events during heavy startup and the NTFS wall-clock cost of re-statting a large tree at every launch.

**Atomic-write and rename heuristics.** `notify-debouncer-full`'s rename detection (create plus delete within the debounce window classified as Rename) is used as-is. Pathological cases such as `mv a.gd b.gd && mv c.gd a.gd` within the same 250 ms window are treated as two Modify events on the final inhabitants of those names, which is semantically correct.

**Bulk-event budget.** At least 500 file change events processed in under 1 s end to end, from debouncer to `Index::on_file_changed` applied to every event. Covered by integration tests and tracked in `bench/budget.toml`.

**Dependency changes do not trigger a publish.** The watcher updates the index and invalidates the analysis cache for dependents via `DepGraph::reverse_closure`; it does not call `publishDiagnostics` for them. The next `didOpen`, `didChange`, or `didSave` on a dependent re-runs analysis under the now-fresh state. This is the policy in `04-diagnostics-strict-mode.md` §2, and it is what keeps noise down.

## 7. Navigation indices

The navigation handlers share one architectural choice: derive from existing structures instead of building new precomputed indices. The eager-interface scan already exposes every `class_name` global, member signatures, and extends edges. The analyzer already records typed bindings at every resolved call and use site. `Index.name_referencers` already maps each referenced name to the set of files referencing it. The handlers are projections over those. Protocol shapes are in `05-lsp-cc-integration.md` §1.

### 7.1 `references`: `Index.name_referencers` plus a per-file binding scan

For the identifier at the request's `TextDocumentPositionParams`:

1. Resolve the identifier to a `(kind, qualified_name)` pair via the analyzer, using the same cursor-to-smallest-typed-ancestor walk `hover` and `definition` use (`gd_server::handlers::smallest_typed_containing`).
2. Query `Index.name_referencers[name]` for the candidate file set.
3. For each candidate, consult `Workspace::analysis_cache[file]` (parse and analyze lazily on a cache miss) and filter the recorded bindings to those whose resolved target matches `(kind, qualified_name)`. For a method or signal target, matching `Binding::Call` call sites are projected too, so a cross-file `c.get_current_value()` through a typed local is found; the callee-identifier sub-span is extracted to avoid wide duplicates, and results are de-duped against the identifier scan.
4. Map each binding's byte span to an LSP `Location` via `PositionMapper`.

`ReferenceParams.context.includeDeclaration` adds the declaration site when true.

### 7.2 `implementation`: a linear walk over `Index.interfaces`

For a class C resolved from the cursor, a linear scan of `Index.interfaces` (about 10k entries at scale, sub-millisecond) finds any interface whose `extends.target` resolves to C, directly or transitively, walking one level at a time and following `class_name` through `ClassNameRegistry`. Each subclass's declaration site becomes an LSP `Location`.

For a virtual or abstract method M on class C: the same scan, plus a per-candidate check that the subclass declares a member with the same name and a compatible signature. `MemberDecl.kind`, `MemberFlags`, and `params` carry what is needed.

There is no precomputed subclass index. At this scale the linear scan is faster than the maintenance cost of an incrementally-invalidated reverse-inheritance map.

### 7.3 Call hierarchy: piggyback on analyzer bindings

The analyzer resolves every call expression during reduce (`gd_analyze::reducer`), and records a typed `Binding::Call { callee_file, callee_name, span }` in `AnalysisResult.bindings` for free during that walk.

- `textDocument/prepareCallHierarchy` resolves the symbol under the cursor to a `CallHierarchyItem[]` (name, kind, uri, range, selectionRange).
- `callHierarchy/outgoingCalls` filters the caller's bindings for `Call` variants and emits one outgoing call per unique callee, with `fromRanges` covering every call site within the caller.
- `callHierarchy/incomingCalls` queries `Index.name_referencers[callee_name]`, lazy-analyzes each candidate file, filters its `Binding::Call` records to those targeting the callee, and emits one incoming call per unique caller.

**Limitations**, intentional and matching both Godot's editor LSP and rust-analyzer's approach: dynamic dispatch through `Variant` or `Callable`, signal connections via dynamic name strings, and lambda invocations through opaque callables are not captured. Static method resolution and direct call expressions are.

### 7.4 `workspace/symbol`: fuzzy match over the registry plus interface tables

1. Flatten `ClassNameRegistry.iter()` to a `(name, kind=Class, location)` list.
2. Flatten `Index.interfaces.iter()` to per-file member tuples `(name, kind ∈ {Function, Constant, Variable, Signal, Enum}, location, containerName=class_name)`.
3. Fuzzy-match the union via `nucleo-matcher`.
4. Order by prefix match on class name, then prefix match on member name, then fuzzy score.
5. Cap at 256 results, to bound latency on projects with 10k+ symbols. An empty query returns everything, capped.

If the client advertises `workspace.symbol.resolveSupport`, gdls returns `WorkspaceSymbol[]` with no `range`, resolved on demand via `workspaceSymbol/resolve`. Otherwise it returns `SymbolInformation[]` with the full `Location` up front.

### 7.5 Cross-file member-initializer xrefs, inline in `AnalysisResult`

`CrossFileQuery::member_initializer_xrefs(file, member) -> Vec<(FileId, String)>` is answered without a separate cache:

1. The reducer, while resolving `const X = B.Y` and equivalents, records each cross-file xref it walks in `AnalysisResult.member_xrefs: FxHashMap<MemberName, Vec<MemberXref>>`. That is one HashMap insert per cross-file member access on the existing hot path.
2. `gd_server::xfile::WorkspaceXFileQuery`, a thin wrapper holding `&SyntacticQuery` plus `&Workspace::analysis_cache`, answers the query by reading the cache. A cache miss returns the default empty `Vec`, so detection is eventually consistent, activating once both files have been analyzed at least once. Conformance stays green because `CorpusQuery`, the test impl, parses on demand and finds the xrefs immediately.
3. No new cache structure, no new lifecycle, no new invalidation code. When `analysis_cache` evicts an entry on `didChange`, the xrefs go with it.

### 7.6 `IndexMutation`, the post-apply invariant checker

Every mutation to `gd_project::Index` (from `Workspace::reindex`, `::remove`, or the watcher) flows through a thin `IndexMutation` wrapper that:

1. Applies the requested change, delegating to `Index::on_file_changed` or `::on_file_removed`.
2. Runs `Index::verify()`, which checks that every `FileId` in `interfaces` has a path in `paths`; that every `class_name` in `registry` resolves to a `FileId` that exists; that `DepGraph.forward` and `DepGraph.reverse` are mutual inverses, every forward edge having its reverse counterpart and the other way round; and that `name_referencers` values are subsets of `interfaces` keys.
3. Reacts to a violation by build profile. In debug it panics with a structured message. In release it logs `index_invariant_violated{file, invariant}` via `tracing`, quarantines the offending file by dropping it from the index, and keeps going: never lie, never serve stale data, but also never crash mid-session.

## 8. Persistent warm-start cache

Without a cache, every launch pays the full startup cost twice. `Index::build` reads, parses, and interface-extracts every `.gd`. Then `Workspace::reconcile` re-reads and re-parses every file again to diff `signature_hash`. The event loop only arms after both, so on a 2,338-file project that was roughly 12 s of unresponsiveness at every start. Paying that once is fine; paying it every launch is not.

**What is persisted.** The eager-interface `Index`: the per-file `Interface` table plus `ClassNameRegistry`. The reverse indexes (`name_referencers`, `path_referencers`, `deps.reverse`, `file_refs`) are all derivable, so the forward data is stored and the edges are rebuilt on load.

**Whole-cache key.** `(cache_format_version, gdls_version, NativeDb::content_hash, project.godot fingerprint, dialect)`. Any mismatch discards the whole cache, since a changed native lattice, config, or Godot release means every interface is stale. The dialect is in the key because the two supported releases do not parse identically, so an interface extracted under one is not reusable under the other.

**Per-file validity** is a read-free `(size, mtime_ns)` stat check. Unchanged means reuse the cached `Interface`; changed or new means re-parse just those; missing means drop. This is the win: warm start becomes a stat sweep plus a handful of re-parses instead of thousands of full parses. A content hash is the fallback if mtime ever proves unreliable. The same stat table lets `reconcile` skip its full re-parse, which is what removes the second startup block.

**Storage** is project-local, `<root>/.gdls/index.<format-ver>.bin`, which is simple, discoverable, and user-clearable. `.gdls/` is in the §6.1 exclusion set so the cache never re-enters the index.

**Load path safety**, non-negotiable under "never crash, never lie":

1. Read the cache. On any parse or IO error, log and fall back to a full cold index. Never trust blind.
2. Validate the whole-cache key. On mismatch, discard and cold-index.
3. Preserve `FileId` stability. The `paths` arena is append-only and ids must not shift, so `paths` deserializes in exact stored order and new files append.
4. Run per-file stat validation, and re-parse the deltas.
5. Run `Index::verify()` and quarantine violators exactly as `Index::build` does. A corrupt cache degrades; it never poisons the session.
6. Still run the cheap stat-based reconcile as the drift backstop for changes made while the server was off.

**Write timing.** The cache is written after the initial index is built and reconciled, a one-time few-ms serialize, guarded so a write failure only logs.

**Multi-instance safety.** Two gdls processes on one project (a Claude Code session plus an IDE, say) is the normal case, and the rest of the server is already safe for it: a session writes nothing, each process holds its own in-memory `Index`, VFS, and caches, and multiple `ReadDirectoryChangesW` or inotify watches on one tree coexist. The cache is the one shared writable artifact, so:

- **Atomic writes, last writer wins.** Serialize to a unique temp file in the same directory, then atomically rename over the target (`tempfile::NamedTempFile::persist`). A reader only ever sees a complete old file or a complete new one, never a torn one. Two concurrent writers both write a valid full cache and the last rename wins; since the cache is derived from identical on-disk project state, either is correct. On Windows the replace tolerates the target being open by another reader. A replace error skips the write rather than failing the session, and the next start cold-indexes.
- **No hard lock.** A cross-process advisory lock is unnecessary and fragile, since a killed process leaves a stale-lock deadlock. The cache is throwaway: a lost write costs exactly one cold index next launch.
- **Tolerant reads**, per the safety list above. Any read, parse, key, or `verify()` failure falls back to a cold index.
- **Per-process temp-file names** so two simultaneous writers do not collide on the temp path. `NamedTempFile` already randomizes.

## 9. Sources

- [`extension_api.json` contents and custom-class inclusion (a ClassDB snapshot)](https://deepwiki.com/godotengine/godot/15.1-gdextension-api)
- [GDExtension classes are indistinguishable from core, per editor autocomplete and help](https://godotengine.org/article/introducing-gd-extensions/)
- [The `.gdextension` file: config only, `libraries` and `entry_symbol`](https://docs.godotengine.org/en/stable/tutorials/scripting/gdextension/gdextension_file.html)
- [GDExtension documentation system: `doc_classes` XML, same format as core, `--doctool --gdextension-docs`](https://docs.godotengine.org/en/4.4/tutorials/scripting/gdextension/gdextension_docs_system.html)
- [Optionally include docs in the API dump (`--dump-extension-api-with-docs`)](https://github.com/godotengine/godot/pull/82331)
- [Autoloads and singletons](https://docs.godotengine.org/en/stable/tutorials/scripting/singletons_autoload.html)
- [`project.godot` and scene resource format parser](https://github.com/PrestonKnopp/tree-sitter-godot-resource)
- Documented external-editor staleness, the motivation: [godot#69485](https://github.com/godotengine/godot/issues/69485) and [godot#107592](https://github.com/godotengine/godot/issues/107592)
