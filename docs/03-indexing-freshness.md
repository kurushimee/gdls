# 03 — Symbol environment, project indexing, incrementalism, and freshness

This is where the two "scariest" requirements (native classes, and live recognition of
class changes) are solved — and where they turn out to be the *easy* parts of this design.

## 1. Native class DB (`gd_types`) — engine classes

**Source:** `extension_api.json`, produced by running the **godot binary** once:

```
godot --dump-extension-api
```

Because the dump iterates `ClassDB`, **every class Godot registers** (via the normal
`ClassDB`/`GDREGISTER_CLASS` path) **appears automatically** — no C++ parsing required. This is the canonical
way to feed custom native classes to an external analyzer.

> **Scope note:** the dump is a snapshot of whatever is registered in `ClassDB` *at the moment it runs*. A
> bare-binary dump therefore contains engine classes but **not** third-party GDExtensions installed in
> the project — those are handled in §2.

**Consumed fields** (per class): `name`, `inherits` (→ full inheritance chains), `methods` (each with
`name`, `arguments` [name/type/default], `return_value` type, and flags `is_const` / `is_static` /
`is_virtual` / `is_vararg`), `properties`, `signals`, `enums`, `constants`. Plus builtin/Variant classes,
global enums, and utility functions. This is exactly the signature/return/virtual/inheritance information
the analyzer and `hover` need.

**Build structures:** an interned class graph and per-class method/property/signal tables for O(1) lookup.

**Versioning & reload:** store the dump's version header and a content hash. The watcher reloads the DB when
the file changes (i.e., after you rebuild the engine). If the file is **absent**, degrade gracefully: treat
native types as unknown/dynamic and surface one informational notice — never crash.

**Auto-dump (v1.0.1 — the user does nothing).** When `extensionApiPath` is NOT set, gdls manages the
dump itself: discover a Godot binary (`godotBinaryPath` option → `GDLS_GODOT` env, where empty/`off`
hard-disables → `godot4`/`godot` on PATH), run
`godot --headless --path <root> --dump-extension-api-with-docs` at session startup, and keep the result
plus staleness metadata (binary path+size+mtime, the project's `.gdextension` file set) under `.gdls/`.
Operational facts the implementation is built around (verified on 4.6.3): the dump lands in the
**project root** regardless of the child's cwd (so a pre-existing user `<root>/extension_api.json`
suppresses the dump entirely — never clobber — and the fresh output is moved into
`.gdls/extension_api.json`); Godot may abort on exit *after* writing a complete dump, so the artifact
decides, never the exit status; and a **never-imported** project (no `.godot/extension_list.cfg`) loads
no GDExtensions, so its dump misses their classes — gdls detects the symptom (declared extensions, none
of their class hints resolving) and logs the remediation (open the project in the editor once). The
resolution ladder when no explicit path is set: fresh `.gdls` dump → auto-dump → stale `.gdls` dump →
unmanaged `<root>/extension_api.json` → empty/dynamic. Spawning happens only at session startup, only
for real projects (`project.godot` present), with the child's stdout/stderr piped (stdout is the LSP
wire) and a 60 s kill-timeout.

**Manual workflow (auto-dump disabled or no binary):** after rebuilding Godot, re-run
`--dump-extension-api-with-docs` from inside the project and point gdls at the JSON via
`initializationOptions.extensionApiPath` (an explicit path always wins and never triggers a spawn).

> Alternative source considered: `doc/classes/*.xml`. Rejected as primary for engine classes because it
> is doc-oriented and Godot's classes need their own `doc_classes` XML to appear; `extension_api.json` is
> complete by construction. (Doc XML *is* used for GDExtensions — see §2.)

## 2. Third-party GDExtensions installed in the project

GDExtension classes (addons providing native `.dll`/`.so`/`.dylib` classes) are registered into `ClassDB`
only when their library is **loaded**, after which they are *indistinguishable from core classes* and the
editor autocompletes them. gdls must therefore capture them explicitly; they then occupy the **same
native-symbol lookup tier** as engine classes (`02-frontend-port.md` §7).

**Enumeration.** Scan `res://**/*.gdextension` to discover installed extensions. A `.gdextension` file is
*configuration only* (per-platform `libraries` + `entry_symbol`); it does **not** contain the class API, so
it is used to *enumerate* extensions and locate their docs, not for signatures.

**Capture (robust, multi-source — a project may mix all of these):**

1. **In-project API dump (preferred when available).** Take the `extension_api.json` dump in the project
   context with extensions loaded, so their `ClassDB` entries are captured in the same JSON as engine
   classes. Since you build Godot, the most deterministic form is a small Godot-side command that loads the
   project's extensions and serializes `ClassDB` to JSON. Offline / regenerate-on-change — **not** a runtime
   dependency.
2. **`doc_classes` XML (pure-static fallback).** Many GDExtensions ship documentation in the **same XML
   format as Godot's class reference** (method/property/signal signatures), authored via
   `godot --doctool --gdextension-docs`. When present, ingest it directly — no execution required. The same
   XML reader serves the engine `doc/classes/*.xml` path.
3. **Graceful degradation.** An extension present in neither the dump nor any `doc_classes` XML has its
   classes treated as **unknown/dynamic** (no false positives) with one informational notice — identical to a
   missing `extension_api.json`.

**Reload.** The watcher treats `.gdextension` files and any ingested `doc_classes` XML as inputs: adding or
removing an addon, or updating its docs, re-runs enumeration + ingestion (§6).

## 3. Project globals & `project.godot`

`project.godot` is parsed at startup and on change (it is INI-like; a small dedicated parser, or
tree-sitter-godot-resource, suffices). It supplies:

- **Autoloads** — the `[autoload]` section maps singleton names → script/scene paths; their *type* comes from
  the referenced script's `class_name`/base. (Autoload typing is subtle even in Godot; track the referenced
  script and resolve its type via the normal pipeline.)
- **`res://` root** — needed to resolve `preload`/`load("res://…")` and to map paths.
- **Warning configuration** — which warnings are enabled/disabled/promoted (feeds `04-diagnostics-strict-mode.md`).

The **`class_name` registry** maps each global type name → its script path and resolved type, built from the
eager interface pass (§4).

## 4. Project indexer — eager interfaces, lazy bodies

**Eager interface pass (startup, O(files)):** tokenize + parse + *shallow*-analyze every `.gd` to extract its
interface: `extends`, `class_name`, member signatures (vars/consts/funcs with types), signals, enums, inner
classes. Populate the `class_name` registry and per-script interface tables. This is fast and is all that
cross-file resolution requires.

**Lazy full pass (on demand):** full statement/expression type-checking and the complete warning set run only
when a file's diagnostics or a query need them. Results are cached and invalidated on change.

## 5. Incrementalism (designed for 3,000–10,000+ files)

- **Dependency graph.** Each file records its dependencies: `extends` target, `preload`/`load` targets, used
  `class_name`s, and referenced autoloads. Maintain forward and reverse edges. Lives in
  `gd_project::DepGraph` (`reverse_closure`-driven invalidation); built in M2; driven by the
  watcher in M4.
- **Invalidation.** A change to file Y: (1) re-run Y's interface extraction; (2) if Y's *interface* changed
  (e.g., a renamed method, a new/removed `class_name`), invalidate the cached **full** analysis of Y and of
  Y's reverse-dependents; (3) if only Y's *bodies* changed, invalidate only Y.
- **Memory.** Keep full ASTs only for open documents; for closed files keep just the interface table and
  re-parse on demand. This bounds memory at large scale. `gd_server::Workspace::forget` (called on
  `didClose`) drops both parse and analysis caches; `Workspace::reindex` then re-extracts the
  interface from a fresh disk parse so the index stays accurate.
- **Optional incremental engine.** `salsa`-style memoized queries can manage the dependency/invalidation
  bookkeeping; a hand-rolled invalidation map is the fallback. **Choice made**: hand-rolled (no `salsa`
  dependency). The trait-based [cross-file query seam](02-frontend-port.md#10-cross-file-query-seam-crossfilequery)
  is what carries dependency information across the `gd_analyze` ↔ `gd_project` boundary.
- **Persistent warm-start cache (M6, required for v1).** Serialize the eager-interface index so a second
  launch skips the cold scan. Whole-cache key = `(cache_format_version, gdls_version,
  NativeDb::content_hash, project.godot fingerprint)`; **per-file** validity is a read-free
  `(size, mtime_ns)` stat check (content hash only as a fallback if mtime proves unreliable), so a warm
  start is a stat sweep plus a handful of re-parses rather than a full re-scan. `NativeDb` already carries
  the `content_hash` field. Stored project-local in `<root>/.gdls/` (which M6 adds to the §6 exclusion
  set); writes are atomic (temp + rename, last-writer-wins) so two gdls processes on one project stay
  safe, and any read/verify failure degrades to a cold index. Pulled forward from Phase 2 — it gates v1.
  Full design: [`08-m6-v1-ship.md`](08-m6-v1-ship.md) §3.
- **Cross-file member-initializer cycle detection (M4).** WP-R2 (M3) added
  `CrossFileQuery::member_initializer_xrefs`, inert in the production `SyntacticQuery` impl —
  cycle detection fires only in the conformance harness's `CorpusQuery`. M4 activates it in the
  LSP **without introducing a separate cache structure**: the analyzer records the per-file
  xref set inline on `AnalysisResult.member_xrefs`, and a thin `gd_server::xfile::WorkspaceXFileQuery`
  wrapper over `SyntacticQuery` reads that field from the existing analysis cache. Invalidation
  comes free — when `analysis_cache` evicts an entry on `didChange`, the xrefs go with it. See §7.5.
  (The `Diagnostic.line` null-source-line override stays plumbing-only at the LSP boundary today;
  no downstream LSP renderer needs it.)

## 6. Freshness watcher — the staleness killer

- **`notify`** (with `notify-debouncer-full`) recursively watches the `res://` tree, excluding `.godot/` and
  import caches.
- Events are debounced, then classified as create / delete / modify / rename. Rename is detected from
  create+delete pairs to preserve identity where possible.
- **Reactions:**
  - `.gd` change → re-extract interface; update the `class_name` registry **immediately** (add/rename/remove
    a global type); invalidate dependents per §5. A file *appearing* re-links the consumers waiting on it
    both by **name** (`extends MyBase` referencing a just-added `class_name`) and by **path** (`extends
    "res://b.gd"` whose target file is just created) — the latter via `Index`'s `path_referencers` reverse
    index, the path-keyed analogue of `name_referencers`, so a path-extends consumer's stale "unknown base"
    diagnostics refresh without waiting for the consumer itself to be edited.
  - `project.godot` change → reload autoloads, `res://` root, and warning config. Call
    `gd_server::Workspace::rebuild_policy` to drop the analysis cache so subsequent
    `publishDiagnostics` runs under the new strict configuration.
  - `extension_api.json` change → reload the native class DB (§1).
  - `.gdextension` add/remove, or a watched `doc_classes` XML change → re-enumerate + re-ingest GDExtension
    classes (§2).
  - `.tscn` change → Phase 2 (node-type reindex).
- **Why this fixes the pain:** there is no `EditorFileSystem`, no focus-gated rescan, and no editor sync in
  the loop. A new/renamed/deleted class is reflected in the index as soon as the file changes on disk —
  independent of which file Claude Code currently has open.
- **Status:** the dependency graph and the `Workspace::rebuild_policy` entry point are in code
  (built in M2 / M3 respectively); the `notify` driver and `.gdextension` re-enumeration landed in M4.

### 6.1 Operational specifics (M4)

These are the concrete decisions the spec deferred to M4 kickoff; they live here so the M4
implementation plan and the watcher integration tests pin to one answer.

- **Debouncer.** `notify-debouncer-full` with a **250 ms quiet-time** policy: emit the coalesced
  event set once no further FS event has arrived for the file (or directory) for 250 ms. This
  round-trips burst writes from atomic-write editors (which create a `.tmp`, write, and rename) into
  a single Modify event, matches the cadence users tolerate in editor LSPs, and stays well under
  the 1 s budget for bulk operations (see "Bulk-event budget" below).
- **Exclusion list (always excluded).** Path *components* `.godot/` (engine editor cache + import
  artifacts), `.import/` (per-resource import caches), `.git/`, plus `target/` and `node_modules/`
  (defensive entries for projects that share a directory with non-Godot tooling); and file-name
  *suffixes* `.tmp`, `.bak`, `.swp`, `~`. **`addons/` is deliberately NOT excluded** — Godot installs
  addon `.gd` scripts, `.gdextension` files, and `doc_classes/*.xml` there and the index/watcher must
  surface them. `notify` exposes no per-path ignore API, so this is **not** a watch-time filter: a
  single shared predicate (`gd_project::is_excluded`, used identically by the cold index, by
  `Workspace::reconcile`, and by the watcher) is applied *post-receipt* to every debounced batch, and
  gates `WalkDir::filter_entry` for the cold-scan and reconcile walks. Matching is case-insensitive on
  the component name (macOS HFS+, Windows NTFS default). User-defined exclusions are deferred to Phase 2.
- **Path normalization.** All paths flow through `camino::Utf8PathBuf`; on Windows backslashes
  are converted to forward slashes at the watcher boundary so downstream `Index` keys match the
  cold-scan output (which uses forward-slash paths from `walkdir`). macOS case-folding is handled
  inside `Index::normalize` (`crates/gd_project/src/index.rs`).
- **Concurrency model.** The server is single-threaded by construction — `lsp_server` is synchronous
  and `Workspace` carries no `Mutex` / `RwLock`. `notify-debouncer-full` runs its own internal
  thread and delivers events on a `crossbeam_channel::Receiver<DebouncedEvent>`. The main loop in
  `gd_server::server::run` uses `crossbeam_channel::select!` over the LSP `Connection::receiver`
  and the watcher receiver; the only mutator on `Workspace` is the main loop. No locks, no shared
  mutable state. (This mirrors rust-analyzer's "the event loop accepts an `enum` of possible
  events" pattern — see rust-analyzer's architecture doc, "Observability".)
- **Lifecycle ordering (v1.0.1).** The watcher is constructed in `serve()` after the `initialize`
  *response* has been sent and **before** `Workspace::load` — every filesystem change that lands
  while the load's stat pass runs is then a queued channel event, replayed once the loop arms.
  Arming is cheap since the debouncer runs `NoCache` (the default `FileIdMap` cache walked the
  ENTIRE tree opening a handle per file just to pair rename events — 7–9 s and ~70 MB on a
  2.3k-file NTFS project, issue #14 — and gdls handles unpaired rename halves anyway). The server
  does **not** send an `initialized` notification — `initialized` is a client→server message it
  only receives and logs. (The module doc on `crates/gd_server/src/watcher.rs` is authoritative
  for this ordering.)
- **Reconciliation backstop after load.** After the load settles, the server walks
  `res://**/*.gd` once more and diffs against the index. With a live watcher armed (the normal
  case) this runs in **`DiscoverOnly` mode**: paths already in the stat table were just validated
  by the load itself and modifications in the gap are queued watcher events, so the backstop only
  stats/parses files *added* while the server was off, plus the standard removal pass —
  enumeration-only for everything known (the per-file stat reuses the directory enumeration's own
  metadata, which costs zero extra syscalls on Windows). When no watcher armed (construction
  failed) the backstop runs `FullStat` — the historical stat-diff of every file — as do the
  watcher `need_rescan` overflow path, the watcher-disabled fallback tick, and
  `gdls diagnose --reconcile` (ad-hoc recovery after wake-from-suspend or remote-FS hiccups).
  Logged with the `cold_index_reconciled … mode=discover|full` marker (a `tracing` span since M5
  WP-O1). This split is the load-bearing fix for `notify` dropping events during heavy startup
  AND for the NTFS wall-clock cost of re-statting a large tree at every launch.
- **Atomic-write / rename heuristics.** `notify-debouncer-full`'s rename detection (create+delete
  within the debounce window classified as Rename) is used as-is; pathological cases (`mv a.gd b.gd
  && mv c.gd a.gd` within the same 250 ms window) are treated as two Modify events on the final
  inhabitants of those names, which is correct semantically.
- **Bulk-event budget.** Target: **≥ 500 file change events processed in &lt; 1 s** end-to-end
  (debouncer to `Index::on_file_changed` applied to every event). Measured in M4 integration tests
  and tracked in the M5 perf budget (`bench/budget.toml`).
- **Diagnostic publish on dependency change — explicitly deferred.** The watcher updates the index
  and invalidates the analysis cache for dependents (via `DepGraph::reverse_closure`); it does
  **not** call `publishDiagnostics` for them. The next `didOpen` / `didChange` / `didSave` on a
  dependent re-runs analyze under the now-fresh state — this is the policy in
  `04-diagnostics-strict-mode.md §2` and is load-bearing for noise control.

## 7. M4 navigation indices

The four M4 nav handlers (`references`, `implementation`, `prepareCallHierarchy` +
`callHierarchy/incomingCalls` + `callHierarchy/outgoingCalls`, `workspace/symbol`) share one
architectural choice: **derive from existing structures rather than build new precomputed
indices.** The M2 eager-interface scan already exposes every `class_name` global, member
signatures, and extends edges; the M3 analyzer already records typed bindings at every resolved
call/use site; `Index.name_referencers` (`crates/gd_project/src/index.rs:53`) already maps each
referenced name to the set of files referencing it. M4's handler implementations are projections
over these. Per `05-lsp-cc-integration.md §1` and the LSP 3.17 spec
(<https://microsoft.github.io/language-server-protocol/specifications/lsp/3.17/specification/>).

### 7.1 `references` — `Index.name_referencers` + per-file binding scan

For the identifier at the request's `TextDocumentPositionParams` (per LSP 3.17 `ReferenceParams`):

1. Resolve the identifier to a `(kind, qualified_name)` pair via the analyzer (the same cursor →
   smallest-typed-ancestor walk `hover` and `definition` use,
   `gd_server::handlers::smallest_typed_containing`).
2. Query `Index.name_referencers[name]` for the candidate file set.
3. For each candidate, consult `Workspace::analysis_cache[file]` (parse + analyze lazily on
   cache-miss) and filter the recorded bindings to those whose resolved target matches
   `(kind, qualified_name)`.
4. Map each binding's byte span to LSP `Location` via `PositionMapper`.

The `ReferenceParams.context.includeDeclaration: boolean` from the spec adds the declaration site
to the result when true. Returns `Location[] | null`.

### 7.2 `implementation` — linear walk over `Index.interfaces`

For a class C resolved from the cursor:

1. Linear-scan `Index.interfaces` (~10k entries at scale, sub-millisecond) for any interface whose
   `extends.target` resolves to C — directly or transitively (walk one level at a time, follow
   `class_name` via `ClassNameRegistry`).
2. Return each subclass's declaration site as LSP `Location`.

For a virtual / abstract method M on class C: same scan, plus per-candidate check that the
subclass declares a member with the same name and a compatible signature (the existing
`MemberDecl.kind` / `MemberFlags` / `params` carry what's needed). Returns `Location | Location[]
| LocationLink[] | null` per LSP 3.17.

No precomputed subclass index. At Phase 1 scale the linear scan is faster than the maintenance
cost of an incrementally-invalidated reverse-inheritance map; revisit if M5 soak surfaces it as a
hot path.

### 7.3 `prepareCallHierarchy` — piggyback on analyzer bindings

The analyzer already resolves every call expression during reduce (`gd_analyze::reducer`); M4
adds a typed `Binding::Call { callee_file: FileId, callee_name: String, span: ByteSpan }` variant
to the existing `AnalysisResult.bindings` Vec — recorded for free during the existing walk.

- `textDocument/prepareCallHierarchy`: resolve the symbol under cursor to a
  `CallHierarchyItem[]` per LSP 3.17 (name, kind, uri, range, selectionRange).
- `callHierarchy/outgoingCalls`: filter the caller's `AnalysisResult.bindings` for `Call`
  variants; emit one `CallHierarchyOutgoingCall { to: CallHierarchyItem, fromRanges: Range[] }`
  per unique callee, with `fromRanges` covering all call sites within the caller per the spec
  ("range relative to the caller, e.g. the item passed to `callHierarchy/outgoingCalls`").
- `callHierarchy/incomingCalls`: query `Index.name_referencers[callee_name]`, lazy-analyze each
  candidate file, filter its `Binding::Call` records to those targeting our callee, emit
  `CallHierarchyIncomingCall { from: CallHierarchyItem, fromRanges: Range[] }`.

**Limitations** (intentional, match Godot's editor LSP and rust-analyzer's approach): dynamic
dispatch through `Variant` or `Callable`, signal connections via dynamic name strings, and
lambda invocations through opaque callables are not captured. Static method-resolution and
direct call expressions are.

### 7.4 `workspace/symbol` — fuzzy match over registry + interface tables

LSP 3.17 `workspace/symbol` returns `SymbolInformation[] | WorkspaceSymbol[] | null` for a query
string. Implementation:

1. Flatten `ClassNameRegistry.iter()` to a `(name, kind=Class, location)` list.
2. Flatten `Index.interfaces.iter()` to per-file member tuples `(name, kind ∈ {Function,
   Constant, Variable, Signal, Enum}, location, containerName=class_name)`.
3. Fuzzy-match the union via `nucleo-matcher` (or `fuzzy-matcher`).
4. Order by (a) prefix match on class name, then (b) prefix match on member name, then
   (c) Smith-Waterman fuzzy score.
5. Cap at 256 results (configurable via `initializationOptions.workspaceSymbolMaxResults`).

If the client advertises `workspace.symbol.resolveSupport`, return 3.17 `WorkspaceSymbol[]` with
no `range` (resolved on demand via `workspaceSymbol/resolve`); otherwise return
`SymbolInformation[]` with full `Location` up-front.

### 7.5 Cross-file member-initializer cycle — inline in `AnalysisResult`

WP-R2 (M3) added `CrossFileQuery::member_initializer_xrefs(file, member) -> Vec<(FileId, String)>`,
inert in `SyntacticQuery`. M4 activates it **without a separate cache impl**:

1. The analyzer's reducer, while resolving `const X = B.Y` (and equivalent) expressions, records
   each cross-file xref it walks in
   `AnalysisResult.member_xrefs: FxHashMap<MemberName, Vec<MemberXref>>` (WP-RD15 newtyped the
   former `FxHashMap<String, Vec<(FileId, String)>>`). One HashMap insert per cross-file member
   access on the existing hot path.
2. The production `CrossFileQuery` impl in `gd_server::xfile::WorkspaceXFileQuery` (a thin wrapper
   holding `&SyntacticQuery` + `&Workspace::analysis_cache`) answers `member_initializer_xrefs`
   by reading the cache. Cache-miss returns the default empty `Vec`: detection is
   eventually-consistent, activating once both files have been analyzed at least once. Conformance
   stays green because `CorpusQuery` (the test impl) parses on demand and finds the xrefs
   immediately.
3. **No new cache structure, no new lifecycle, no new invalidation code.** When `analysis_cache`
   evicts an entry on `didChange`, the xrefs go with it.

This replaces the originally-named `DeepResolutionCache`. The 4th `CrossFileQuery` impl is just
the thin `WorkspaceXFileQuery` wrapper — see `02-frontend-port.md §10` table.

### 7.6 `IndexMutation` — post-apply invariant checker

Every mutation to `gd_project::Index` (from `Workspace::reindex`, `::remove`, the M4 watcher, or
any future caller) flows through a thin `IndexMutation` wrapper that:

1. Applies the requested change (delegates to `Index::on_file_changed` / `::on_file_removed`).
2. Runs `Index::verify()`:
   - Every `FileId` in `interfaces` has a path in `paths`.
   - Every `class_name` in `registry` resolves to a `FileId` that exists.
   - `DepGraph.forward` and `DepGraph.reverse` are mutual inverses (every forward edge has its
     reverse counterpart and vice versa).
   - `name_referencers` values are subsets of `interfaces` keys.
3. **Debug:** invariant violation panics with a structured message. **Release:** the violation
   is logged as `index_invariant_violated{file, invariant}` via `tracing`, the offending file is
   dropped from the index (quarantined), and processing continues — never lie, never serve stale
   data, but also never crash mid-session. Aligns with the "never crash, never lie" project
   convention (CLAUDE.md, Project-specific conventions).

## 8. Sources

- `extension_api.json` contents & custom-class inclusion (ClassDB snapshot) — https://deepwiki.com/godotengine/godot/15.1-gdextension-api
- GDExtension classes are indistinguishable from core (editor autocomplete/help) — https://godotengine.org/article/introducing-gd-extensions/
- The `.gdextension` file (config only: `libraries`, `entry_symbol`) — https://docs.godotengine.org/en/stable/tutorials/scripting/gdextension/gdextension_file.html
- GDExtension documentation system (`doc_classes` XML; same format as core; `--doctool --gdextension-docs`) — https://docs.godotengine.org/en/4.4/tutorials/scripting/gdextension/gdextension_docs_system.html
- Optionally include docs in the API dump (`--dump-extension-api-with-docs`) — https://github.com/godotengine/godot/pull/82331
- Autoloads / singletons — https://docs.godotengine.org/en/stable/tutorials/scripting/singletons_autoload.html
- `project.godot` / scene resource format parser — https://github.com/PrestonKnopp/tree-sitter-godot-resource
- Documented external-editor staleness (motivation) — https://github.com/godotengine/godot/issues/69485 · https://github.com/godotengine/godot/issues/107592
