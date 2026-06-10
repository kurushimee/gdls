# 06 — Testing & fidelity strategy

"Match Godot exactly" is only meaningful if it is measurable. This is how we make it so. Each layer of the
port is validated against an oracle, and overall fidelity is tracked as a number in CI.

## 1. Primary oracle — Godot's own conformance corpus

Godot ships a golden-file test suite at `modules/gdscript/tests/scripts/`: `.gd` input scripts paired with
`.out` expected-output files. Its runner executes five phases per script — **Load, Parse, Analyze, Compile,
Runtime** — and diffs collected output against the `.out` file.

**Plan:**

- **Vendor** the corpus from Godot's `modules/gdscript/tests` into `gdls` test fixtures.
- Run gdls's `Tokenize → Parse → Analyze` on each `.gd` and diff emitted **errors + warnings** against the
  **Parse/Analyze-phase** expectations.
- **Known caveat (from research):** `.out` files also contain *runtime* output and include warnings only on
  debug builds. The harness must **filter to the frontend-phase diagnostics**, which requires a curation step
  (a small allow/transform layer mapping `.out` lines to the parse/analyze subset). The runner also skips
  `completion/` and `lsp/` folders and `.notest.gd`/`.textonly.gd` files — mirror those exclusions.
- Expected outputs in Godot are regenerated via its `--gdscript-generate-tests` flag; we keep the vendored
  copy fixed and refresh it deliberately when Godot's corpus changes.

## 2. Differential oracle — the godot binary (offline)

For ongoing fidelity against *real* code (including project-specific native classes):

- Offline (never at runtime), run the **godot binary** over a sample corpus of the actual project to
  produce its diagnostics, and diff against gdls.
- This catches divergences that the upstream corpus cannot — especially around Godot's native API and the
  project's own typing patterns.

**Implementation & CI status (post-M3).** Lives in `crates/gd_analyze/tests/differential.rs`. The
test is **env-gated** — when neither `GDLS_GODOT_BINARY` nor `GDLS_SAMPLE_CORPUS` is set, it logs
"skipped" to stderr and returns success. The exit-criterion text in §6 below (and `docs/07` M3
row) is therefore satisfied two ways:

1. **Manual milestone pass** — at every milestone close, a developer runs the differential oracle
   against a large real-world project (during M3) and the pass is recorded in the header
   comment of `crates/gd_analyze/tests/conformance/analyze_fidelity_floor.txt`, with the form:
   `# Differential oracle: <PASS|FAIL> against <corpus>, godot binary <path/sha>, at <date> on
   commit <sha>.`
2. **CI integration (planned)** — a separate CI job that builds the godot binary against a
   committed mini-sample. Tracked under M5 hardening (the godot-binary build itself is the
   blocker, not the test infrastructure).

`GDLS_DIFFERENTIAL_THRESHOLD` (default `0.75`) sets the matched/eligible floor when the test does
run. Tightening this in CI is the post-M5 step.

## 3. Fidelity metric in CI

- Define **fidelity = % of corpus diagnostics that match** (exact code + range + message), tracked per
  phase (parse, analyze) and overall.
- CI runs the corpus on every change and fails on regressions below a ratchet threshold. The number makes
  "match Godot exactly" a visible, defendable target rather than a claim.
- **Current floors (post-M5 Phase E):** parse-phase **1.0000** (186/186, see
  `crates/gd_syntax/tests/conformance/fidelity_floor.txt` — ratcheted from the post-M3 floor of
  0.9731 once WP-F1..F5 ported the five parser-side diagnostics: duplicate `@icon` / `@tool`, the
  `@warning_ignore_start` / `_restore` pair-balance pass, and the UTS #39 visually-similar-to-
  keyword tokenizer check) and analyze-phase **1.0000** (300/300, see
  `crates/gd_analyze/tests/conformance/analyze_fidelity_floor.txt`). Each ratchet file carries a
  header comment documenting the close-out commit and a list of intentionally-excluded fixtures
  (e.g. `.notest.gd`/`.textonly.gd`, the `completion/` and `lsp/` folders, and the runtime/debug
  lines filtered by the parse/analyze curation layer). The per-file `known_failures.txt` for the
  parser is intentionally empty after Phase E close-out.
- **M6 holds both ratchets.** M6 (capability-parity + warm-start cache — `07-milestones-risks.md` /
  `08-m6-v1-ship.md`) is glue/projection over existing structures; it adds no analyzer/parser fidelity
  fixtures and keeps parse **1.0000** / analyze **1.0000** green as a gate on every step.

## 4. Per-component unit tests

| Crate | What is tested |
|---|---|
| `gd_syntax` | Tokenizer token streams & positions; parser AST shapes; error-recovery yields partial ASTs; arena traversal (`iter_ids`, `innermost_node_at`, `eof_line`). |
| `gd_types` | `extension_api.json` ingestion (inheritance, signatures, virtuals); type assignability rules; absent-DB degradation; doc-XML merge. |
| `gd_analyze` | Name-resolution order; gradual/`Variant` tracking; each of the 45 warnings fires/does-not-fire on targeted snippets; strict-mode promotion; precedence chain; cross-file member-initializer cycle detection. |
| `gd_project` | Eager-interface extraction; `project.godot` parsing (autoloads, paths, warning config); dependency-graph invalidation; watcher event → registry update (incl. `class_name` add/rename/delete). |
| `gd_server` | LSP lifecycle; incremental sync correctness; byte↔UTF-16 conversion; per-file publish policy; each implemented request method; strict-mode wire path (initializationOptions JSON → publishDiagnostics severity); cross-file definition jump. |

### Trimmed-API fixture (analyzer conformance harness)

The conformance harness in `crates/gd_analyze/tests/conformance.rs` does **not** ship the full
multi-MB `extension_api.json`. Instead it carries a hand-curated **trimmed** native API JSON
(Script, GDScript, Node3D, Sprite3D, Color, Vector3, Variant, etc., assembled across WP-Q6 /
WP-Q7 / WP-Q13 / WP-Q16). The trimmed fixture keeps the conformance corpus deterministic and
quick to load, and only grows when a new corpus fixture demands a native that isn't yet in the
slice. The differential oracle (§2) uses the *real* `extension_api.json` against the godot binary,
so the two oracles back each other: trimmed-API conformance proves analyzer logic; differential
proves coverage gaps in the trimmed slice.

## 5. Robustness (the server must never crash mid-session)

- **Panic-free on malformed input.** Fuzz the tokenizer/parser (e.g., `cargo-fuzz`) on random and mutated
  `.gd` input; any panic is a release blocker. Parser error-recovery must always return a partial AST.
  A second fuzz target (`fuzz/fuzz_targets/analyze.rs`) feeds parser output through `gd_analyze::analyze`
  with a stub `NoCrossFile` xfile — covers the resolver/reducer code paths added in M3. Both targets
  share the same nightly `cargo-fuzz` job.
- **Graceful degradation:** missing `extension_api.json` (native types → dynamic + one log notice
  written via `log::warn!` to **stderr** — not surfaced to the LSP client as `window/showMessage`,
  since most editors render that as a modal popup and the degradation is a deploy-time configuration
  issue, not a per-edit signal); missing `project.godot` (treat root as `res://`, defaults applied);
  unreadable files (skip + log to stderr at `warn` level, distinguishing `NotFound` ⇒ drop from
  other I/O ⇒ keep last-known interface).
- **Soak/perf tests** at 10k synthetic files: cold-index time, warm per-file analysis latency, memory
  ceiling, and watcher throughput under bulk file operations (e.g., a branch switch / mass rename).
  Owned by M5 (`07-milestones-risks.md`).
- CC's `restartOnCrash` is a safety net, not a substitute for the above.

## 6. Acceptance gates (tie-in)

Milestone exit criteria in `07-milestones-risks.md` reference these oracles directly — e.g., "M1: corpus
parse-phase fidelity meets its calibrated target", "M3: analyze-phase fidelity meets its calibrated target" — thresholds calibrated against the vendored corpus and ratcheted in CI.

**Full-project diagnostics sweep (v1.0.1 lesson).** The corpus (single-file fixtures) and the
nav-row acceptance walks both structurally missed an error-level false-positive epidemic on real
layered projects (the v1.0.0 → v1.0.1 cross-file families: 133/243 Pixelorama files carried bogus
errors). `scripts/m6-acceptance/scan_diags.py` didOpens **every** `.gd` in a project and tallies
`publishDiagnostics`; running it on both acceptance projects is a standing pre-release gate
(`files_with_errors` ~0, remainder justified against `godot --check-only`). See
`scripts/m6-acceptance/README.md` for the gate rule.

**Test-rig rule (auto-dump).** Every `gd_server` integration rig that boots via `serve()` must
pass `"autoDumpExtensionApi": false` in its `initializationOptions` (or pin `extensionApiPath`) —
otherwise a dev machine with `godot` on PATH spawns a real dump per test session. Direct
`Workspace::load` constructions are spawn-free by design (`ApiDumpPolicy::NeverSpawn`).

## 7. M3 calibration record (closed at 1.0)

The M3 analyzer port closed at **300/300 = 1.0000** analyze-phase conformance on the vendored
corpus, ratcheted in `crates/gd_analyze/tests/conformance/analyze_fidelity_floor.txt`. The port
shipped in ~70 phased work-packets across four nested implementation plans, roughly **3× the WP
count of the initial design**. Each follow-on was driven by a corpus regression — upstream carve-outs the design
pass didn't see (the lambda body queue order, the cross-file member-initializer cycle, the
synthetic post-EOF line for `match`-pattern subscript-Index emission). See `docs/07 §2`
"Calibration from M3" for the budgeting consequence: M4/M5/Phase 2 should budget for the same
fidelity-bench-driven follow-on assumption rather than the initial WP list alone.

## 8. Observability (M5)

The server is long-running. To verify M5's "within budget" exit criterion at all, the binary
must measure itself; to debug a 3am-on-call session, structured signals must be in the logs;
to enforce a regression ratchet in CI, the measurements must be machine-readable. M5 adds the
infrastructure for all three. None of this exists in M3; it lands in M5 (see `07-milestones-risks.md` row M5).

### 8.1 Structured logs via `tracing`

Adopt `tracing` + `tracing-subscriber` (cited as the de-facto Rust observability crate by the
crate ecosystem; `tokio` and `rust-analyzer` both use it). Stays on stderr — never stdout, per
the project convention. Initial spans:

- `analyze{file, version}` around the analyzer's per-file entry point in `gd_analyze::analyze`.
- `handle_request{method, id}` around the dispatch site in `gd_server::server::run`.
- `cold_index{root}` around `Workspace::load`.
- `watcher_event{path, kind}` around the watcher integration point.

Each span emits an `info!` event with elapsed micros on close. Default subscriber filter is
`info` (production-cheap); the env var `GDLS_TRACE='*>50'` enables a hierarchical-profiler-style
filter ("log spans taking > 50 ms"), modelled on rust-analyzer's `RA_PROFILE` pattern (see
rust-analyzer architecture doc, "Observability" section).

### 8.2 Per-request latency

`handle_request` span emits `request_latency_us` at close. Soak-test assertions read these from
the trace stream and compute p50 / p99 / p999 per method. Budgets live in `bench/budget.toml`;
the CI bench job fails when a method's p99 exceeds the recorded budget by &gt; 20%.

### 8.3 Peak RSS

Sampled via `sysinfo::Process::memory()` at three points: server start (baseline), end of
cold-index, server shutdown. Delta from baseline is logged as `peak_rss_bytes`. M5 also adds a
soft-cap evictor (LRU on `parse_cache` + `analysis_cache` when RSS &gt; soft_cap) and a hard-cap
shedder (refuse new full analyses + emit `memory_pressure_shed` when RSS &gt; hard_cap). Caps are
read from `initializationOptions.memory.softCapMb` / `.hardCapMb` with defaults of 2× and 4×
the peak observed during M5's calibration soak.

### 8.4 Fixpoint loop governor

The analyzer's `resolve_*` and `reduce_*` recursive walks have re-entry guards but no hard
iteration cap. M5 adds a per-file `AnalysisContext.iter_count: u32` incremented on every
reducer entry; crossing N=64 emits `analyzer_runaway{file, last_node_kind}` and bails to a
partial type with a diagnostic rather than spinning forever. The threshold is empirically set
against the existing 300-fixture corpus (max observed: 7).

### 8.5 `IndexMutation` invariant violations

Every mutation to `gd_project::Index` flows through `IndexMutation` (see
`03-indexing-freshness.md §7.6`). Violations in release builds emit
`index_invariant_violated{file, invariant}` and quarantine the file rather than crashing.

### 8.6 `$/cancelRequest`

Per LSP 3.17 `$/cancelRequest`, a cancelled request must still return — gdls returns
`ErrorCodes.RequestCancelled`. M5 plumbs a `CancellationToken` through the analyzer with
cooperative checkpoints every N AST nodes (similar to rust-analyzer's salsa-based cancellation
via panic-and-unwind — the project already keeps `panic = "unwind"` in the root Cargo.toml,
which makes this cheap to add). Emits `request_cancelled{id, ms_in_flight, last_phase}` on
cancel.

### 8.7 `gdls bench --record` reproducer

When the CI bench job detects a regression, the job artifact captures: the input rope snapshot
(post-cold-index), a replay-able JSON-RPC trace of the request that regressed, and the env hash
(toolchain, target triple, parser/analyzer version). `gdls bench --replay <artifact>` replays
locally — turns "user reports a slowdown" into a one-command repro.

### 8.8 Budget ratchet — `bench/budget.toml`

Numeric budgets are calibrated, not pre-specified (per `07-milestones-risks.md §2`). The M5
calibration pass runs `crates/gd_project/tests/perf_scale.rs` against the 10k synthetic corpus
three times, takes the median for each metric, and writes:

```toml
# bench/budget.toml — ratcheted in CI; tighten only after a confirmed soak pass.
[cold_index]
files_10k_p99_ms = 30000   # 30s ceiling for 10k cold scan

[warm_analyze]
single_file_p99_ms = 250

[hover]
p99_ms = 50

[memory]
peak_rss_after_10k_cold_index_mb = 1024
soft_cap_mb = 2048
hard_cap_mb = 4096

[watcher]
events_500_p99_ms = 1000   # 500 events processed in ≤ 1s
```

CI re-runs `perf_scale.rs` on PRs touching `crates/gd_{syntax,types,analyze,project,server}/src/`
and fails when any metric exceeds the recorded budget by &gt; 20% (giving room for noise but
catching real regressions). Tightening is manual and recorded in the commit message, same
discipline as the fidelity ratchet.

## 9. Sources

- GDScript test runner & 5-phase pipeline (`.gd`/`.out`, exclusions) — https://github.com/godotengine/godot/blob/master/modules/gdscript/tests/gdscript_test_runner.cpp
- Module README (pipeline stages) — https://github.com/godotengine/godot/blob/master/modules/gdscript/README.md
- Warning codes for per-warning unit tests — https://github.com/godotengine/godot/blob/master/modules/gdscript/gdscript_warning.h
