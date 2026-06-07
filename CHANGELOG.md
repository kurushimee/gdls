# Changelog

All notable changes to `gdls` are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project
adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [1.0.0] — 2026-06-01

First tagged release. Phase 1 complete (M0–M5).

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
- M5 hardening, observability, parity gap closure, v1 release

---

**Tag conventions.** Subsequent Godot-tracked re-ports become `v1.x.0` minor releases
(Godot 4.8, 4.9, …). `v2.0.0` is reserved for the persistent on-disk cache + Phase 2 features
(`.tscn` node typing for `$`/`%`, `signatureHelp`, `completion`).
