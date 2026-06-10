# Changelog

All notable changes to `gdls` are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project
adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

(nothing yet)

## [1.0.1] — 2026-06-10

The urgent diagnostics-correctness release. A post-v1.0.0 full-project sweep (didOpen every
`.gd`, tally `publishDiagnostics`) found error-level **false positives in ~45–55% of files** on
real layered projects (Pixelorama @ stock 4.6.3: 133/243 files, 1,223 errors; a 2,338-script
production project: 1,051 files, 6,167 errors) — all violations of the "never lie" rule, all
reproducing on vanilla Godot 4.6.3 fixtures. v1.0.1 fixes the four families behind that, closes
both v1.0.0 follow-ups (#13, #14), and removes the last manual setup step (`extension_api.json`).

### Fixed — analyzer false positives
- **Cross-file `extends ClassName` lost native lineage and inherited members** (#15): a new
  shared script-chain resolver (`gd_analyze::script_chain`, memoized, cycle-guarded) walks
  `Interface::extends` links to the native root, mirroring Godot's `native_type` propagation
  (analyzer.cpp:617-619). Fixes the false `Cannot use "$" on a class that isn't a node.`,
  `"@onready" can only be used in classes that inherit "Node".`,
  `Identifier "…" not declared in the current scope.` (inherited members), and
  `Invalid argument … but is "<Class>".` (self-compat — `is_type_compatible` now ports Godot's
  source decomposition, analyzer.cpp:6210-6296). Unknown/unresolvable chains stay permissive.
- **Cross-file `Class.CONST.member` chains errored `Cannot get property from enum value.`**
  (#18): regular consts now take Godot's CONSTANT member arm (typed from their declaration via
  the new interface-TypeExpr resolver); only genuine `enum { … }` hoists type as anonymous-enum
  values (the interface now records them explicitly).
- **Builtin named constants poisoned constant arithmetic** (#16): `Vector3.UP * 3.0` reported
  `Invalid operands to operator *, Nil and float.` because the constant folded as a placeholder
  Nil. `FoldedValue::Opaque(kind)` keeps constancy without a fabricated value; binary ops
  validate by type, dict dup-key treats unknown values as never-equal, and the dump's
  per-constant declared types are now ingested (`Vector3.AXIS_X` is `int`).
- **`Packed*Array` was not iterable** (#17): `for p in paths:` errored
  `Unable to iterate on value of type "PackedStringArray".`; the typed-container element table
  (gdscript_parser.cpp:5508-5530) is now ported into `resolve_for`.
- Cross-file named enums carry their **declared** integer values (literal chains; unknown values
  suppress `INT_AS_ENUM_WITHOUT_MATCH` / `ENUM_VARIABLE_WITHOUT_DEFAULT` instead of judging
  against sequential placeholders).

### Added
- **Cross-file member navigation slice** (#13): member access through typed bases and bare
  inherited identifiers now records `Binding::Use` against the *declaring* file —
  `references` on a signal declaration finds cross-file `obj.sig.emit(…)` sites, `definition`
  on a member-access jumps to the declaration, and hover renders `var`/`const` member shapes.
- **Zero-config native types** (#20): gdls now auto-dumps `extension_api.json` into `.gdls/`
  by running the user's Godot (`godotBinaryPath` → `GDLS_GODOT` → `godot4`/`godot` on PATH)
  with project context — which is what captures GDExtension classes — and keeps staleness
  metadata (binary identity + `.gdextension` set). Opt out with `autoDumpExtensionApi: false`.
  Resolution order: explicit `extensionApiPath` → fresh `.gdls` dump → auto-dump → stale dump →
  unmanaged `<root>/extension_api.json` → dynamic.
- `uid://` autoload script targets resolve through the sidecar map (#19) — definition,
  references, and singleton typing now work for `Name="*uid://…"` autoloads.
- The full-project diagnostics sweep is committed as a release gate
  (`scripts/m6-acceptance/scan_diags.py`) — a nav-row walk is not a diagnostics gate.

### Performance
- **Windows/NTFS startup** (#14): the 7–9 s window blamed on the reconcile walk was mostly the
  file watcher's `FileIdMap` arming scan (a full-tree, handle-per-file walk that exists only to
  pair rename events — which gdls already handles unpaired). The debouncer now runs `NoCache` on
  every platform; reconcile and the warm-load walk reuse the directory enumeration's own
  metadata (zero extra stats on Windows); and with the watcher armed *before* the workspace
  loads, the startup backstop runs in `DiscoverOnly` mode (enumeration-only for known files).

### Cache
- `CACHE_FORMAT_VERSION` 1 → 2 (the interface now carries enum values + unnamed-enum hoists).
  Old caches are ignored; expect one cold start after upgrading.

## [1.0.0] — 2026-06-10

The first release. **M6 has landed** — the ship bar raised at the close of M5 (exposed-capability
parity vs Godot's own LSP + a persistent warm-start cache) is met, and both final acceptance walks
(an OSS Godot 4.6.3 project and a Windows-native walk on a 2,338-script production project) came
back clean. Everything below is what v1.0.0 contains (M0–M6). Full M6 scope:
[`docs/08-m6-v1-ship.md`](docs/08-m6-v1-ship.md).

### Diagnostics
- Parser fidelity 1.0000 (186/186) on the vendored Godot 4.6.3-stable corpus.
- Analyzer fidelity 1.0000 (300/300).
- Full strict mode (45 active + 3 deprecated-gated warnings; godot/strict/off profiles).
- `NATIVE_METHOD_OVERRIDE` correctly exempts engine virtuals (`_ready`, `_process`, …): Godot
  only warns when a real `MethodBind` exists, so overriding a virtual is silent (Phase H fix —
  caught against the full `extension_api.json`, which the trimmed conformance fixture didn't cover).

### LSP surface (per docs/05 §1)
- publishDiagnostics (push, per-file)
- documentSymbol / workspaceSymbol
- definition / references / implementation
- prepareCallHierarchy / incomingCalls / outgoingCalls
- hover (with engine + GDExtension doc prose)

### CLI
- `gdls --version` / `-V` and `gdls --help` / `-h` (terminal probes that print and exit before any
  LSP traffic); `gdls diagnose --reconcile|--path-audit` and `gdls bench --record|--replay` as before.

### Hardening (M5)
- Soft / hard RSS budget enforcer (bench/budget.toml reference caps) with LRU cache eviction.
- $/cancelRequest with cooperative checkpoints.
- Fixpoint loop governor (analyzer_runaway diagnostic).
- tracing-based observability (GDLS_LOG / GDLS_TRACE env vars).
- Local-only differential check tool against godot on PATH (WP-D1; no CI).
- gdls bench --record / --replay local reproducer.
- Manual pre-release walk on a large real-world GDScript project with committed verification report (Phase H).

### Closed milestones
- M0 LSP skeleton
- M1 tokenizer + parser port
- M2 environment + indexing (native DB, project.godot, class_name registry)
- M3 analyzer + warning set + per-file diagnostics + hover + definition + strict mode
- M4 freshness watcher + 4 nav handlers + cross-file cycle detection
- M5 hardening, observability, parity gap closure

### M6 — landed (ships v1.0.0)

The raised ship bar is met (full design: [`docs/08-m6-v1-ship.md`](docs/08-m6-v1-ship.md)):
- **Exposed-capability parity vs Godot's own LSP** — `hover` renders member/call/`preload` signatures
  with parameter names (M6-F); `definition` resolves `class_name`-in-expression (M6-B), `preload`/`load`
  `res://` strings (M6-C1), and autoload names (M6-D); `documentLink` on resolving `res://` literals
  (M6-C2); project-wide `references` through typed vars (M6-E); hierarchical `documentSymbol` with a root
  `Class` (M6-A); `implementation` for method overrides across direct + transitive subclasses (M6-G).
  All res://-resolving navigation gates on index membership — never a link/Location to a file that isn't
  on disk ("never lie").
- **Autoload-singleton typing** — autoload names resolve to their script's instance type, so member
  access through a singleton (`Global.popup_error()`) gives the full hover signature and project-wide
  references, matching Godot (closes a parity gap the OSS acceptance walk surfaced; analyzer-level,
  ratchets unaffected).
- **Persistent warm-start index cache** — serde-serialized `Index` keyed by
  `(format_version, gdls_version, NativeDb::content_hash, project.godot fingerprint)` + a per-file
  `(size, mtime_ns)` table; atomic, multi-instance-safe (temp + rename, last-writer-wins, tolerant
  reads → cold fallback, never crashes) (M6-H / M6-I); `reconcile` re-parses only stat-changed files.
- **Attribute-fallback hover** — `func`/`signal` member signatures also render when the member
  identifier is *not* a call's callee: the signal in `Singleton.sig.emit(…)`/`obj.sig.connect(…)`
  and uncalled references like `var f = obj.method` (caught by the final acceptance walks; the
  Call-gated path alone left these on the degraded type label).
- **Verified (final acceptance):** the parameterized runner (`scripts/m6-acceptance/`) passes every
  capability row on a real Godot 4.6.3 OSS project (Pixelorama, committed session — including
  autoload-singleton member hover/references); a Windows-native walk on a 2,338-script production
  project returns correct data on every row with graceful degradation of doc-less GDExtensions and
  the cache stat-diffing `2338 unchanged, 0 reparsed` over NTFS; warm start **14.7×** faster than
  cold on the 3,000-file synthetic gate (>5× exit criterion); ratchets hold **1.0000 / 1.0000**
  (parser 186/186, analyzer 300/300).
- **Deferred to Phase 2** (documented, not regressions): cross-file instance-member typing for
  signal/var members through typed bases — references/definition on those *member-access* sites
  (#13; method calls and in-file signal uses work, and hover covers func/signal members via the
  attribute fallback); go-to-`definition` on a cross-file method *member* (#13); Windows/NTFS
  startup wall-clock dominated by the reconcile backstop walk, masking the warm-cache win there
  (#14); precise typing of `$`/`%` nodes; `completion`, `signatureHelp`, `rename`,
  `documentHighlight`.

---

**Tag conventions.** `v1.0.0` is tagged with M6 landed (the persistent warm-start cache ships *in* v1,
not later). Subsequent Godot-tracked re-ports become `v1.x.0` minor releases (Godot 4.8, 4.9, …). `v2.0.0`
is reserved for Phase 2 features (`.tscn` node typing for `$`/`%`, `signatureHelp`, `completion`).
