# Changelog

All notable changes to `gdls` are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project
adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased] — targeting v1.0.0

`1.0.0` is bumped in-tree and **M6 has landed** — the ship bar raised at the close of M5
(exposed-capability parity vs Godot's own LSP + a persistent warm-start cache) is met. Everything below
has landed (M0–M6); `1.0.0` is tagged from this branch once merged. Full M6 scope:
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
- **Verified:** a clean capability walk on a real Godot 4.6.3 OSS project (every exposed capability
  returns complete data); warm start **14.7×** faster than cold on a 3,000-file synthetic project
  (>5× exit gate); ratchets hold **1.0000 / 1.0000** (parser 186/186, analyzer 300/300).
- **Deferred to Phase 2** (documented, not regressions): go-to-`definition` on a cross-file method
  *member* (hover/references on members work); precise typing of `$`/`%` nodes; `completion`,
  `signatureHelp`, `rename`, `documentHighlight`.

---

**Tag conventions.** `1.0.0` is tagged once M6 lands (the persistent warm-start cache ships *in* v1, not
later). Subsequent Godot-tracked re-ports become `v1.x.0` minor releases (Godot 4.8, 4.9, …). `v2.0.0`
is reserved for Phase 2 features (`.tscn` node typing for `$`/`%`, `signatureHelp`, `completion`).
