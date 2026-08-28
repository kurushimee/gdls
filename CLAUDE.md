# CLAUDE.md

Guidance for Claude Code (claude.ai/code) when working in this repository.

## What this is

`gdls` is a standalone GDScript language server: a **faithful Rust port of Godot 4.6.3-stable's
GDScript frontend** (tokenizer → parser → analyzer) giving Claude Code type-aware diagnostics and navigation
over LSP/stdio with **no Godot process at runtime**. It fixes the editor LSP's weight, staleness, and
coupling at 3,000–10,000+ `.gd` scale. Only the frontend is ported — the compiler/bytecode/VM half of
Godot's module is out of scope (diagnostics only).

Read `docs/00-overview.md` (problem, locked decisions) and `docs/01-architecture.md` first; the full
spec is `docs/00`…`docs/09`.

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

- **`$Node`/`%Unique` typing — bare `NATIVE Node` (M11 #76, faithful to Godot).** Godot's *analyzer*
  types `$`/`%`/`get_node` as a hard bare `NATIVE Node` (`gdscript_analyzer.cpp:3866-3886`), NOT a
  scene-derived precise type — the precise per-node type the editor shows comes from a separate
  scene-instantiation path. So `reduce_get_node` types valid `$`/`%` as bare `Node` (M11 retired the
  old permissive-`Variant` deviation): a member miss fires `UNSAFE_PROPERTY_ACCESS`, sibling
  downcasts/casts stay silent — matching Godot. Precise scene-derived node types are **navigation-only**:
  `gd_server::scene_nav` reads the `scene_node_facts` seam for hover/definition/typeDefinition (#125) and
  completion, and that type never enters an `AnalysisResult` — feeding it into the diagnostic path would
  false-positive on Godot-tolerated downcasts. See `docs/02` §11.

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

**Current:** Phase 1 complete — **v1.0.7 shipped** (2026-06-13; the utility-as-Callable
follow-ups #92, tagged on `release/v1.0.7` = v1.0.6 + #92, so the unreleased Phase 2 work on
`main` stayed unreleased — #91's code was already in v1.0.6; v1.0.6 itself was the #88 hotfix off
v1.0.5); **Phase 2 COMPLETE — M7–M11 shipped and closed (#57–#80), no release cut** (deferred
until the phase matures): **M7 (protocol foundations), M8 (editing core — `completion` +
`signatureHelp`), M9 (navigation & refactoring — `documentHighlight`, `foldingRange`/
`selectionRange`, `declaration`/`typeDefinition`, `typeHierarchy`, `workspaceSymbol/resolve`,
`rename`/`prepareRename`), M10 (presentation & code actions — `semanticTokens` full/delta/range
standard-legend-only, `inlayHint`+resolve, `documentColor`/`colorPresentation`, `codeAction`
warning quickfixes + `source.fixAll`), and M11 (scenes & file operations — `.tscn` scene index +
`$`/`%` typing, scene-aware completion, autoload `uid://`→scene→root-script typing, `willRenameFiles`,
external-formatter bridge)** — M7's #57–#63 and M8's #64/#65 merged 2026-06-13,
M9's #66–#71 and M10's #72–#75 merged 2026-06-14, M11's #76–#80 merged 2026-06-15; all five
milestones closed (M8 via PRs #94/#95, M9 via #100–#105, M10 via #110/#112/#116/#117, all
adversarially reviewed; a Godot-headless-LSP differential showed 99.6% completion agreement on
member access). M9 `rename` (#66) took **six
adversarial review rounds** to close every proven source-corruption path — the lesson, captured in
memory: `references`/`definition` are read-tuned, so their inaccuracies become silent corruption
under rename, the first mutating consumer; the fix is a fail-closed positive-project-resolution
firewall + binding-correct local resolution (excludes `self.x`-attribute over-capture). It
round-trips on Pixelorama with zero stale-version edits; the additive `ParseResult.comments` field
(foldingRange) kept both ratchets at 1.0000. M10's `codeAction` (#75) inherited that mutating-consumer
lesson — its warning quickfixes ship edits behind a fail-closed ERROR/shadow backstop (what the
offer-time gate proved safe is exactly what the client applies). `semanticTokens` advertises the
STANDARD LSP legend only (zero custom token names — the #30 generic-LSP target), intersecting it
per-client at emit time; `executeCommandProvider` lists EXACTLY the one real command
(`gdls.applyWarningIgnore`), never an empty/broken list (anti-catalog W15). **M11's key lesson —
the bare-Node `$`/`%` premise correction (#76):** a fusion review (confirmed against the real
4.6.3 binary) caught that Godot's analyzer types `$`/`%` as a hard bare `NATIVE Node`
(`gdscript_analyzer.cpp:3866-3886`), NOT scene-precise — so feeding precise scene-derived types into
the *diagnostic* path would manufacture false positives Godot never emits (sibling downcasts). The
`.tscn` text-parsed `SceneIndex` ships, but `reduce_get_node` types valid `$`/`%` as bare `Node`
(retiring the over-permissive `Variant` deviation, `docs/02 §11`); the precise scene types are kept
DORMANT for phase-3 navigation, explicitly out of diagnostics. `willRenameFiles` (#79) reaffirmed
the mutating-consumer firewall — a fusion review fixed an over-capture so it rewrites ONLY
positively-identified `preload`/`load` args, never an arbitrary `res://` value string. M9 deferred
items filed: #106 (enum-value/autoload rename refuse), #107 (for-loop/match/inner-shadow). M10
deferred items filed: #111 (semanticTokens bare-call/for-loop/match-pattern decl sites), #114/#115
(inlayHint script-owned enum / `Array[<named-script>]` container hints), #118/#119 (codeAction
`_`-prefix for `UNUSED_PRIVATE_CLASS_VARIABLE` / over-refuse when `_name` exists in an unrelated
scope); plus #113 (signatureHelp wrong inner-class-method signature — an M8 carry-over surfaced
during M10 review). M11 deferred items filed: #123 (`UNSAFE_METHOD_ACCESS` analyzer-wide
under-emission), #124 (`$`/`%` glyph), #125 (precise `$`/`%` navigation typing — the dormant
substrate's consumer), #126 (`%` mid-string completion span), #127 (`res://` asset completion),
#129 (scriptless autoload as a type annotation), #131 (`.tscn` `ext_resource` rewrite on rename),
#132 (`willRenameFiles` write-set misses), #135/#136 (formatter head-of-line / cancel).
Per-milestone interactive checks are batched into ONE end-of-Phase-2 trial by the user
in real work (capability captures are Claude's, headless — inventory + gaps in
`crates/gd_server/tests/fixtures/client_caps/README.md`; deferred feel-check items in
`docs/09 §7.4`). Phase 2 shipped no per-milestone releases; now that the phase is COMPLETE, a
single release will be cut from `main` as a separate step (the v1.0.7 tag stands as the last
release). Release notes and the full history live in
`CHANGELOG.md`; per-milestone exit criteria in `docs/07`. Both conformance ratchets hold at
**1.0** with empty known-failures lists; CI is green on both legs with the five-layer fuzz
gate (`parse` + `analyze` + `index_invariants` + `complete_context` + `scene_parse`). Standing release gate: the
`scripts/m6-acceptance/scan_diags.py` diagnostics sweep on both acceptance projects, run
comparatively against the previous release binary (`--strict` + warning histogram; error
baselines must hold; a nav-row walk is NOT a diagnostics gate), with the private project swept
on Windows with the Windows binary.
