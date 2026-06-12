# CLAUDE.md

Guidance for Claude Code (claude.ai/code) when working in this repository.

## What this is

`gdls` is a standalone GDScript language server: a **faithful Rust port of Godot 4.6.3-stable's
GDScript frontend** (tokenizer → parser → analyzer) giving Claude Code type-aware diagnostics and navigation
over LSP/stdio with **no Godot process at runtime**. It fixes the editor LSP's weight, staleness, and
coupling at 3,000–10,000+ `.gd` scale. Only the frontend is ported — the compiler/bytecode/VM half of
Godot's module is out of scope (diagnostics only).

Read `docs/00-overview.md` (problem, locked decisions) and `docs/01-architecture.md` first; the full
spec is `docs/00`…`docs/08`.

## Commands

Toolchain is pinned by `rust-toolchain.toml` (`stable` + `rustfmt`/`clippy`). The dev loop = the CI gate
(`.github/workflows/ci.yml`):

```bash
cargo fmt --all --check          # format gate (CI fails on diff)
cargo lint                       # alias: clippy --workspace --all-targets -- -D warnings
cargo build --workspace --all-targets
cargo test --workspace
```

- **CI denies all warnings** (`RUSTFLAGS: -D warnings`); be clippy-clean on first write. Use `cargo lint`
  (the alias in `.cargo/config.toml`), not bare `cargo clippy`.
- **Run it:** `cargo run --bin gdls` (debug) or `target/release/gdls` (release). It speaks JSON-RPC over
  stdio, so a bare invocation just waits for an LSP client.

## Rust workflow notes

Layered on the gate above; none override the faithful-port discipline (in port crates fidelity beats
idiom — these apply mostly to `gd_server` glue and tests).

- **Dependencies:** pin shared external crates once in the root `[workspace.dependencies]`, then
  reference as `<crate>.workspace = true` from members so versions live in one place (`cargo add` to
  introduce; `--dev` for dev-only).
- **Error-type split:** binaries / LSP glue (`gd_server`) use `anyhow::Result` + `.context(...)`; library
  crates that expose an error type (`gd_types`) use `thiserror`. Don't mix the two in one crate's API.
- **`.unwrap()` vs `.expect("invariant: …")`:** `unwrap()` is fine in tests; in production prefer `?`, or
  `.expect("invariant: <why this can't fail>")` so a stray panic documents the broken assumption. With
  `panic = "unwind"` (root `Cargo.toml`) this keeps the session alive on a logged error.
- **`#[must_use]`** on returns callers must not silently discard (builders, fallible helpers).

## Architecture

A single binary, layered as a strict crate DAG (`crates/<name>`), each unit-testable in isolation:

```
gd_syntax  tokenizer + parser + AST (NO engine knowledge — fuzzable/testable standalone)
   └─ gd_types   GDScript type model + native-class DB (ingested from extension_api.json)
        ├─ gd_analyze  analyzer: name resolution, type checking, warning set, strict mode
        └─ gd_project  project.godot parsing, class_name registry, dep graph, freshness watcher
             └─ gd_server  LSP layer, VFS, position mapping, query services, diagnostics → bin `gdls`
```

Two load-bearing principles that span files:

- **Eager interfaces, lazy bodies** (`docs/01` §4). Every `.gd` is shallow-parsed at startup for what it
  *exposes* (`extends`, `class_name`, signatures); full type analysis runs lazily per-file on demand and
  is cached/invalidated on change — mirroring Godot's shallow-vs-full `GDScriptCache`, and what makes 10k
  files tractable. Diagnostics are **per-file on open/edit**, never whole-project.
- **Three coordinate spaces, never confused** (`crates/gd_server/src/position.rs`): internal **byte
  offsets** (`gd_syntax`), Godot's tab-expanded 1-based `(line, column)` (kept only for `.out` fidelity),
  and LSP `Position` (UTF-16 default; UTF-8/32 if negotiated). Byte↔LSP conversion happens **only** at the
  protocol boundary, via a `PositionMapper` built per-request from the current rope.

`gd_server` is a library + thin `main.rs` so the event loop can be driven over an in-memory `Connection`
in tests (`crates/gd_server/tests/lifecycle.rs`). The lexer is **pull-based and parser-driven**
(`crates/gd_syntax/src/lexer.rs`): newline/indent suppression inside `()[]{}`/lambdas is toggled by the
parser, so it is not a standalone pre-pass.

## Conventions (these override normal instincts)

- **Faithful-port discipline.** Mirror Godot's frontend **function-for-function**; preserve exact error/
  warning **message strings and source ranges**. Do **not** "improve," refactor, or modernize the
  algorithms — fidelity is the requirement and structural parity keeps upstream diffs re-applicable.
  Enums stay dense and **in Godot's declaration order** (`TokenKind` is `#[repr(u8)]` indexing a parallel
  names table — see the `const _: () = assert!(...)` guard in `token.rs`). Rename a symbol only when a
  Rust keyword forces it (`SELF`→`SelfKw`, `TK_CONST`→`Const`) and document why.

- **Source of truth = the Godot source, not the design docs.** Port the frontend against a local
  checkout of official Godot (`godotengine/godot`) at tag **`4.6.3-stable`**. GDScript's frontend is
  **unchanged from upstream** — only native C++ classes differ (`docs/00`) — so the official tree is a
  faithful reference. **Derive every enum, count, and message template mechanically from the source —
  never hard-code them; grep to confirm at port time.** The design docs cite some stale numbers; the
  source wins. Verified against `4.6.3-stable`: `TokenKind` = **100** kinds (`gdscript_tokenizer.h`); the warning
  set = **45 active + 3 deprecated** (`gdscript_warning.h`; the 3 behind `#ifndef DISABLE_DEPRECATED`) =
  48 codes; the 4 error-by-default warnings are `INFERENCE_ON_VARIANT`, `NATIVE_METHOD_OVERRIDE`,
  `GET_NODE_DEFAULT_WITHOUT_ONREADY`, `ONREADY_WITH_EXPORT`. Native classes are ingested into `gd_types`
  from `extension_api.json` (`godot --dump-extension-api-with-docs`).

- **Never crash, never lie.** The parser always returns a (possibly partial) AST so the server can always
  respond; position conversions **clamp** out-of-range input rather than panic; `panic = "unwind"` keeps a
  stray panic to a logged error mid-session; malformed `initializationOptions` fall back to defaults with
  a warning, never failing `initialize`.

- **stdout is the LSP wire; stderr is logs.** All logging goes to stderr (`tracing`; `GDLS_LOG` filter,
  `GDLS_TRACE` profiler). Never write to stdout except LSP protocol — a stray `println!` corrupts the
  JSON-RPC stream.

- **`$Node`/`%Unique` v1 policy (deliberate deviation, not a bug).** Until `.tscn` typing lands in Phase
  2, `$`/`%` yield a *permissive deferred-node type* (assignable to any `Node`-derived var, dynamic on
  member access) so the tool never false-positives on node access. Do not "fix" this to match Godot
  exactly. See `docs/02` §11.

- **Intentional deferrals, not dead code.** Phase 2 features are deliberately stubbed/deferred — check the
  milestone status below before assuming something is missing.

## Fidelity testing

"Match Godot exactly" is measured against Godot's own golden corpus (`.gd` input + `.out` expected),
vendored and curated to the **frontend phases only** (the `.out` files also carry runtime + debug-only
output — filter it; skip `completion/`, `lsp/`, `*.notest.gd`, `*.textonly.gd`). Fidelity is a ratcheted
% in CI. The tokenizer/parser are also fuzzed (`cargo-fuzz`); any panic is a release blocker. Details:
`docs/06`.

## Milestone status

Phase 1 = v1 = **M0–M6**: M0 LSP skeleton · M1 tokenizer + parser · M2 environment/indexing (native DB,
`project.godot`, `class_name` registry) · M3 analyzer + warnings + per-file diagnostics + `hover`/
`definition` + strict-mode · M4 freshness watcher + dep-graph invalidation + nav (`references`,
`implementation`, `prepareCallHierarchy`/`callHierarchy`, `workspace/symbol`) · M5 10k-file hardening
(memory pressure ladder, perf budgets), observability (`tracing`), differential-oracle harness · **M6
(the milestone that ships v1)** exposed-capability parity vs Godot's own LSP (`hover`/`definition`/
`references`/`documentSymbol`/`implementation` gaps) + a persistent warm-start index cache, per
[`docs/08-m6-v1-ship.md`](docs/08-m6-v1-ship.md). Phase 2 = **M7–M11** (the generic-language-server
phase, fully specified in [`docs/09-phase-2.md`](docs/09-phase-2.md)): M7 protocol foundations
(cancel preemption, progress, configuration, pull diagnostics, doc-comment pipeline) · M8
`completion` + `signatureHelp` · M9 navigation/refactoring (`rename`, `documentHighlight`,
`declaration`/`typeDefinition`, `typeHierarchy`, folding/selection) · M10 presentation
(`semanticTokens` standard-legend-only, `inlayHint`, `documentColor`, `codeAction`) · M11 scenes
(`.tscn` typing for `$`/`%`, scene-aware completion, `willRenameFiles`, external-formatter bridge).
Governing principle (issue #30): generic LSP first — Godot-specific data additive, never instead;
no custom protocol; every feature capability-gated. The Godot-LSP anti-catalog in `docs/09 §3` is
binding.

**Current:** Phase 1 complete — **v1.0.5 shipped** (2026-06-12). Release notes and the full
history live in `CHANGELOG.md`; per-milestone exit criteria in `docs/07`. Both conformance
ratchets hold at **1.0** with empty known-failures lists; CI is green on both legs with the
three-layer fuzz gate (`parse` + `analyze` + `index_invariants`). Standing release gate: the
`scripts/m6-acceptance/scan_diags.py` diagnostics sweep on both acceptance projects, run
comparatively against the previous release binary (`--strict` + warning histogram; error
baselines must hold; a nav-row walk is NOT a diagnostics gate), with the private project swept
on Windows with the Windows binary.
