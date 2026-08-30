# CLAUDE.md

Guidance for Claude Code (claude.ai/code) when working in this repository.

## What this is

`gdls` is a standalone GDScript language server: a faithful Rust port of Godot's GDScript frontend (tokenizer, parser, analyzer), serving both 4.6 and 4.7 from one binary that gives Claude Code type-aware diagnostics and navigation over LSP/stdio, with no Godot process at runtime. It fixes the editor LSP's weight, staleness, and coupling at 3,000 to 10,000+ `.gd` scale. Only the frontend is ported; the compiler, bytecode, and VM half of Godot's module is out of scope, so there is no running of GDScript. On top of diagnostics it serves the full generic LSP surface (`docs/05` §1).

Read `docs/00-overview.md` (problem, design decisions) and `docs/01-architecture.md` first. The full spec is `docs/00` through `docs/09`.

## Commands

The toolchain is pinned by `rust-toolchain.toml` (`stable` plus `rustfmt` and `clippy`). The dev loop is the CI gate (`.github/workflows/ci.yml`):

```bash
cargo fmt --all --check          # format gate (CI fails on diff)
cargo lint                       # alias: clippy --workspace --all-targets -- -D warnings
cargo build --workspace --all-targets
cargo test --workspace
```

CI denies all warnings (`RUSTFLAGS: -D warnings`), so be clippy-clean on first write. Use `cargo lint`, the alias in `.cargo/config.toml`, not bare `cargo clippy`.

To run it: `cargo run --bin gdls` (debug) or `target/release/gdls` (release). It speaks JSON-RPC over stdio, so a bare invocation just waits for an LSP client.

**The local loop is the Linux half of the gate.** CI also runs the same four steps on `windows-latest`, which stayed red for twelve merges once (#279) because nothing in the merge loop surfaces it. After merging, check `gh run list --branch main --limit 5`. Anything under test that needs a file URI goes through `common::file_uri` (over `gd_server::uri::path_to_file_uri`), never a hand-built `format!("file://…")`, since a Windows temp root is a drive path and does not survive that.

## Rust workflow notes

These layer on the gate above. None of them override the faithful-port discipline; in port crates fidelity beats idiom, and these apply mostly to `gd_server` glue and tests.

- **Dependencies.** Pin shared external crates once in the root `[workspace.dependencies]`, then reference them as `<crate>.workspace = true` from members, so versions live in one place. Use `cargo add` to introduce one, with `--dev` for dev-only.
- **Error-type split.** Binaries and LSP glue (`gd_server`) use `anyhow::Result` with `.context(...)`; library crates that expose an error type (`gd_types`) use `thiserror`. Don't mix the two in one crate's API.
- **`.unwrap()` versus `.expect("invariant: …")`.** `unwrap()` is fine in tests. In production prefer `?`, or `.expect("invariant: <why this can't fail>")` so a stray panic documents the broken assumption. With `panic = "unwind"` (root `Cargo.toml`) this keeps the session alive on a logged error.
- **`#[must_use]`** on returns callers must not silently discard: builders, fallible helpers.

## Architecture

A single binary, layered as a strict crate DAG (`crates/<name>`), each unit-testable in isolation:

```
gd_syntax  tokenizer + parser + AST (NO engine knowledge: fuzzable/testable standalone)
   └─ gd_types   GDScript type model + native-class DB (ingested from extension_api.json)
        ├─ gd_analyze  analyzer: name resolution, type checking, warning set, strict mode
        └─ gd_project  project.godot parsing, class_name registry, dep graph, freshness watcher
             └─ gd_server  LSP layer, VFS, position mapping, query services, diagnostics → bin `gdls`
```

Two principles span files:

**Eager interfaces, lazy bodies** (`docs/01` §4). Every `.gd` is shallow-parsed at startup for what it *exposes* (`extends`, `class_name`, signatures), and full type analysis runs lazily per file on demand, cached and invalidated on change. This mirrors Godot's shallow-versus-full `GDScriptCache`, and it is what makes 10k files tractable. Diagnostics are per file on open and edit, never whole-project.

**Three coordinate spaces, never confused** (`crates/gd_server/src/position.rs`): internal byte offsets (`gd_syntax`), Godot's tab-expanded 1-based `(line, column)` (kept only for `.out` fidelity), and LSP `Position` (UTF-16 by default, UTF-8 or 32 if negotiated). Byte to LSP conversion happens only at the protocol boundary, via a `PositionMapper` built per request from the current rope.

`gd_server` is a library plus a thin `main.rs`, so the event loop can be driven over an in-memory `Connection` in tests (`crates/gd_server/tests/lifecycle.rs`). The lexer is pull-based and parser-driven (`crates/gd_syntax/src/lexer.rs`): newline and indent suppression inside `()[]{}` and lambdas is toggled by the parser, so it is not a standalone pre-pass.

## Conventions (these override normal instincts)

**Faithful-port discipline.** Mirror Godot's frontend function for function, and preserve exact error and warning message strings and source ranges. Do not "improve", refactor, or modernize the algorithms: fidelity is the requirement, and structural parity keeps upstream diffs re-applicable. Enums stay dense and in Godot's declaration order (`TokenKind` is `#[repr(u8)]` indexing a parallel names table; see the `const _: () = assert!(...)` guard in `token.rs`). Rename a symbol only when a Rust keyword forces it (`SELF` to `SelfKw`, `TK_CONST` to `Const`), and document why.

**Dialect guards are the one sanctioned departure from faithful porting.** gdls serves both Godot 4.6 and 4.7 from one binary, selected per project from `project.godot`'s `application/config/features`. Godot has no version branching of its own, so every branch here is fenced by three rules that keep upstream diffs re-applicable (`crates/gd_syntax/src/dialect.rs`):

1. **Newest is primary.** The unguarded body of a ported function mirrors the newest supported tag (`Dialect::NEWEST`). The *older* behavior is what gets wrapped, never the other way around, so the next upstream diff applies to the primary text as it always has.
2. **Ordered comparisons only** — `if self.dialect < Dialect::Godot4_7 { … }`, never `==`. A later release then leaves existing guards alone unless it touched the same site again.
3. **One greppable marker** per guard: `// DIALECT(4.7): gdscript_tokenizer.cpp:939 — a tab advances column by 1, not tab_size.` `grep -rn "DIALECT("` is the whole audit surface, the checklist for the next bump, and the deletion list when a dialect is retired.
4. **A no-op is documented, not silent.** Where an upstream change needs no guard because gdls never had the thing it fixes, or already behaved that way, it goes in the delta table in `docs/02` §11c. That table plus `grep -rn "DIALECT("` together cover every difference between the tags, which is what lets an auditor tell "handled" from "missed". Guarded behavior is pinned at both tags by `crates/gd_syntax/tests/dialect_delta.rs` and the analyzer's dialect test files, since a conformance suite only ever runs one tag's goldens.

The dialect rides as a struct field on `Lexer`, `Parser`, and `AnalysisContext`, never as an extra parameter, so ported signatures stay identical to Godot's. `Dialect::DEFAULT` is what "unspecified" means — `NEWEST`, so a bare `parse`, a unit test, and a fuzz target all get the newest port.

**The source of truth is the Godot source, not the design docs.** Port the frontend against a local checkout of official Godot (`godotengine/godot`), which carries both supported tags (`4.6.3-stable`, `4.7.2-stable`) with HEAD at the newest. Diff the two with `git diff 4.6.3-stable 4.7.2-stable -- modules/gdscript/` to see what a dialect guard owes. GDScript's frontend is unchanged from upstream and only native C++ classes differ (`docs/00`), so the official tree is a faithful reference. Derive every enum, count, and message template mechanically from the source, never hard-code them, and grep to confirm at port time. Where a doc cites a concrete number it may have drifted; the source wins. Verified against both tags: `TokenKind` has 100 kinds in each (`gdscript_tokenizer.h`), unchanged between them. The warning set is 45 active plus 3 deprecated (`gdscript_warning.h`, the 3 behind `#ifndef DISABLE_DEPRECATED`) for 48 codes at 4.6, and 46 plus 3 for 49 at 4.7, which added `CONFUSABLE_TEMPORARY_MODIFICATION` mid-enum. Nothing observable is keyed on the ordinal, so the tables carry the newest tag's order and gate older dialects by `WARNING_SINCE`. The 4 error-by-default warnings are `INFERENCE_ON_VARIANT`, `NATIVE_METHOD_OVERRIDE`, `GET_NODE_DEFAULT_WITHOUT_ONREADY`, and `ONREADY_WITH_EXPORT`. Native classes are ingested into `gd_types` from `extension_api.json` (`godot --dump-extension-api-with-docs`).

**Never crash, never lie.** The parser always returns a possibly-partial AST so the server can always respond. Position conversions clamp out-of-range input rather than panic. `panic = "unwind"` keeps a stray panic to a logged error mid-session. Malformed `initializationOptions` fall back to defaults with a warning, never failing `initialize`.

**stdout is the LSP wire; stderr is logs.** All logging goes to stderr (`tracing`, with the `GDLS_LOG` filter and the `GDLS_TRACE` profiler). Never write to stdout except LSP protocol, since a stray `println!` corrupts the JSON-RPC stream.

**`$Node` and `%Unique` type as bare `NATIVE Node`**, faithful to Godot. Godot's *analyzer* types `$`, `%`, and `get_node` as a hard bare `NATIVE Node` (`gdscript_analyzer.cpp:3866-3886`), not a scene-derived precise type; the precise per-node type the editor shows comes from a separate scene-instantiation path. So `reduce_get_node` types a valid `$` or `%` as bare `Node`: a member miss fires `UNSAFE_PROPERTY_ACCESS`, while sibling downcasts and casts stay silent, matching Godot. Precise scene-derived node types are navigation-only: `gd_server::scene_nav` reads the `scene_node_facts` seam for hover, definition, typeDefinition, completion, and signatureHelp — on the access itself and on anything read off it — and that type never enters an `AnalysisResult`, since feeding it into the diagnostic path would false-positive on Godot-tolerated downcasts. See `docs/02` §11.

## Fidelity testing

"Match Godot exactly" is measured against Godot's own golden corpus (`.gd` input plus `.out` expected), vendored and curated down to the frontend phases only. The `.out` files also carry runtime and debug-only output, so filter it, and skip `completion/`, `lsp/`, `*.notest.gd`, and `*.textonly.gd`. Fidelity is a ratcheted percentage in CI.

Each harness reads a **set** of suites, one per supported release: a corpus tree plus the dialect its goldens came from. The newest release carries the full tree, byte for byte against the Godot checkout, so a gdls-authored case never lives inside it; older releases carry only what genuinely diverges (`analyzer-4.6/` is two files, and the parser has no 4.6 subset at all). Adding a release demotes the current tree to a subset — `scripts/conformance/demote_corpus.py` does the mechanical half. Each corpus's `PROVENANCE.md` is the authority.

The tokenizer and parser are also fuzzed (`cargo-fuzz`), and any panic blocks a release. Details: `docs/06`.

## Project state

`gdls` is complete and running. Every capability in `docs/05` §1 is shipped and advertised, and the issue tracker is empty. Both ratchets hold at 1.0000 with empty known-failures lists — parse 185/185, analyze 196/196. The last release is v3.0.0 (2026-08-29), which adds Godot 4.7 alongside 4.6. `CHANGELOG.md` has the release history and `docs/08-history.md` has how it was built.

Three lessons from that work still apply to anything new:

**Mutating consumers need their own firewall.** `references` and `definition` are read-tuned, so their inaccuracies become silent source corruption under `rename`, `codeAction` edits, `willRenameFiles`, and the autoload rename. The pattern that works: a fail-closed positive-project-resolution gate, binding-correct collection (never name-only), and refusing outright rather than half-applying. Widening a candidate set is safe; widening what is collected inside one is not.

**Precise scene types are navigation-only.** See the `$`/`%` convention above, and `docs/02` §11.

**Generic LSP is the contract.** `semanticTokens` advertises the standard legend only, intersected per client at emit time, and `executeCommandProvider` lists exactly the one real command (`gdls.applyWarningIgnore`), never an empty or broken list (anti-catalog W15). The anti-catalog in `docs/09` §3 is binding.

The standing release gate is the `scripts/acceptance/scan_diags.py` diagnostics sweep on both acceptance projects, run comparatively against the previous release binary (`--strict` plus warning histogram, with error baselines holding, and a nav-row walk is not a diagnostics gate), with the private project swept on Windows using the Windows binary. The fuzz gate is five targets: `parse`, `analyze`, `index_invariants`, `complete_context`, `scene_parse`.

Editor capability profiles are captured headlessly; the inventory and this machine's gaps live in `crates/gd_server/tests/fixtures/client_caps/README.md`.
