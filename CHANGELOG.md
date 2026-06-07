# Changelog

All notable changes to `gdls` are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project
adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased] — targeting v1.0.0

`1.0.0` is bumped in-tree but **deliberately untagged**: the ship bar was raised at the close of M5, so
v1 now ships with **M6** (exposed-capability parity + a persistent warm-start cache). Everything below
has landed (M0–M5); the **M6** section lists what remains before `1.0.0` is tagged. Full M6 scope:
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

### M6 — remaining for v1 (in progress)

The raised ship bar pulls these into Phase 1 before `1.0.0` is tagged (full design:
[`docs/08-m6-v1-ship.md`](docs/08-m6-v1-ship.md)):
- Exposed-capability parity vs Godot's own LSP — `hover` member/call/`preload` signatures (M6-F);
  `definition` for `class_name`-in-expression (M6-B), `preload`/`load` strings (M6-C), autoloads (M6-D);
  project-wide `references` through typed vars (M6-E); hierarchical `documentSymbol` (M6-A);
  `implementation` for method overrides (M6-G).
- Persistent warm-start index cache — stat-validated `(size, mtime_ns)`, atomic multi-instance-safe
  writes (M6-H / M6-I), plus a reconcile-by-stat path that stops re-parsing unchanged files.
- Exit gate: every exposed capability ⊇ Godot's own LSP; warm start > 5× faster than cold scan; both
  ratchets still 1.0000 / 1.0000; a clean re-run of the capability walk — then tag **v1.0.0**.

---

**Tag conventions.** `1.0.0` is tagged once M6 lands (the persistent warm-start cache ships *in* v1, not
later). Subsequent Godot-tracked re-ports become `v1.x.0` minor releases (Godot 4.8, 4.9, …). `v2.0.0`
is reserved for Phase 2 features (`.tscn` node typing for `$`/`%`, `signatureHelp`, `completion`).
