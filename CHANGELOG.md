# Changelog

All notable changes to `gdls` are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project
adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

(nothing yet)

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
