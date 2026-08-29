# 06. Testing and fidelity

"Match Godot exactly" only means something if it is measurable. Each layer of the port is validated against an oracle, and overall fidelity is tracked as a number in CI.

## 1. Primary oracle: Godot's own conformance corpus

Godot ships a golden-file test suite at `modules/gdscript/tests/scripts/`: `.gd` input scripts paired with `.out` expected-output files. Its runner executes five phases per script (Load, Parse, Analyze, Compile, Runtime) and diffs collected output against the `.out` file.

gdls vendors that corpus into its own test fixtures, runs tokenize, parse, and analyze on each `.gd`, and diffs the emitted errors and warnings against the Parse and Analyze phase expectations.

Two caveats shape the harness. `.out` files also carry runtime output, and they include warnings only on debug builds, so a curation layer filters each file down to the frontend-phase diagnostics. And Godot's own runner skips the `completion/` and `lsp/` folders and any `.notest.gd` or `.textonly.gd` file, so gdls mirrors those exclusions.

Skipping a file as a *case* is not the same as hiding it from the *index*, and conflating the two costs fidelity silently. The `.notest.gd` companions are loaded so the file under test can resolve its cross-file references; only the fidelity walk ignores them. Godot's shared assertion helper `utils.notest.gd` needs the same treatment but cannot get it by sitting in the tree, because it lives one level above the vendored subtree and the newest suite is a byte-exact mirror. It is vendored to `corpus/support/` instead, which `corpus_index` feeds into every suite's index and the fidelity walk never descends into. Without it, the 364 `Utils.check(…)` calls across the corpus are unresolvable, and the resulting errors read as analyzer bugs (#312).

The vendored copy is fixed and refreshed deliberately, never automatically. Each corpus directory's `PROVENANCE.md` records the reference tag it came from. Godot regenerates its expected outputs with the `--gdscript-generate-tests` flag.

### Suites, one per supported release

gdls serves several Godot feature releases from one binary, and their goldens differ, so each harness reads a **set** of corpus trees. A suite is a tree plus the dialect its goldens were generated at (`SUITES` in each `conformance.rs`), and every reported path is prefixed with its suite tag, so a `known_failures.txt` line names both the file and the dialect it failed under. One aggregate fidelity number covers all suites, which is what makes it impossible to lose a file by moving it between them.

The newest supported release carries the full vendored tree, byte for byte — `diff -rq` against the Godot checkout must print nothing, so a gdls-authored regression case never lives inside it. Older releases carry only the files whose phase-relevant result actually differs at that tag. Today that is `analyzer-4.6/` with two files (4.7's runner sorts the printed error list by line; 4.6 printed it in emission order) and no parser subset at all, because no `.gd` in the parser corpus parses differently at the two tags. The guarded behaviors that *do* differ are pinned directly instead, in `crates/gd_syntax/tests/dialect_delta.rs` and the analyzer's dialect test files; `docs/02-frontend-port.md` §11c and §11d carry the full delta tables.

Adding support for a newer release therefore *demotes* the current full tree to a subset. `scripts/conformance/demote_corpus.py <corpus> --from <old> --to <new> --godot <checkout>` does the mechanical half: it refuses to run if the vendored tree has drifted from the tag it claims, writes the textual divergences into the subset directory, refreshes the main tree, and prints the suite row and provenance facts. Which of those candidates genuinely diverge is a manual review — a renamed test helper or an added case that behaves the same at both tags belongs in neither tree, and an empty subset is a real outcome.

## 2. Differential oracle: the godot binary, offline

For fidelity against real code, including project-specific native classes, the godot binary runs over a sample corpus of an actual project to produce its diagnostics, and gdls diffs against them. This catches divergences the upstream corpus cannot, especially around Godot's native API and a project's own typing patterns. It runs offline, never at runtime.

It lives in `crates/gd_analyze/tests/differential.rs` and is env-gated: when neither `GDLS_GODOT_BINARY` nor `GDLS_SAMPLE_CORPUS` is set, it logs "skipped" to stderr and returns success. There is no Godot build in CI, so this is a local pass, run before a release and recorded in the header comment of `crates/gd_analyze/tests/conformance/analyze_fidelity_floor.txt` as `# Differential oracle: <PASS|FAIL> against <corpus>, godot binary <path/sha>, at <date> on commit <sha>.`

`GDLS_DIFFERENTIAL_THRESHOLD` (default `0.75`) sets the matched-over-eligible floor when the test does run.

## 3. The fidelity ratchets

Fidelity is the percentage of corpus diagnostics that match on exact code, range, and message, tracked per phase. CI runs the corpus on every change and fails on any regression below the recorded floor. The number makes "match Godot exactly" a visible, defendable target rather than a claim.

Parse sits at 1.0000, 185/185 over the full 4.7.2 tree (`crates/gd_syntax/tests/conformance/fidelity_floor.txt`), with an empty `known_failures.txt`. Analyze also sits at 1.0000, 196/196 (`crates/gd_analyze/tests/conformance/analyze_fidelity_floor.txt`, floor 1.00), with an empty `analyze_known_failures.txt`. Every eligible golden in both suites matches Godot message for message and range for range, so any new entry in either list is a regression. Each floor file carries a header comment documenting the commit that set it and the intentionally excluded fixtures: `.notest.gd` and `.textonly.gd`, the `completion/` and `lsp/` folders, and the runtime and debug lines the curation layer filters.

The two counts moved together when the corpus was re-vendored at 4.7.2. Upstream consolidated its analyzer tests — 4.6.3's 300 mostly one-error files became 194 grouped ones that each cover more — so the denominator fell while the coverage rose, and the rise is what surfaced the two entries the 4.7 vendoring added. They were gaps the older tree never reached, not regressions, and both were equally present at 4.6. One of the two turned out not to be an analyzer gap at all: `invalid_identifier.gd` failed because `reduce_identifier` skipped its undeclared-identifier error for every uppercase-initial name, and that hedge was standing in for two harness gaps rather than a missing port — a `Variant.Type` truncated to four of its 40 values in `trimmed_api.json`, and upstream's shared `utils.notest.gd` helper, which sits one level above the vendored subtree and so was never indexed (#300, #312, #313).

Raising a floor is manual and recorded in the commit message. Nothing outside the ported frontend is under the ratchet: the LSP surface is server glue, tested by protocol-shape tests instead.

## 4. Per-component unit tests

| Crate | What is tested |
|---|---|
| `gd_syntax` | Tokenizer token streams and positions; parser AST shapes; error recovery yielding partial ASTs; arena traversal (`iter_ids`, `innermost_node_at`, `eof_line`); the doc-comment side channel. |
| `gd_types` | `extension_api.json` ingestion (inheritance, signatures, virtuals); type assignability rules; absent-DB degradation; doc-XML merge; the embedded stock surface, including that it still carries prose. |
| `gd_analyze` | Name-resolution order; gradual and `Variant` tracking; each warning firing and not firing on targeted snippets; strict-mode promotion; the precedence chain; cross-file member-initializer cycle detection; provenance gating. |
| `gd_project` | Eager-interface extraction; `project.godot` parsing (autoloads, paths, warning config); scene parsing and node-path resolution; dependency-graph invalidation; watcher event to registry update, including `class_name` add, rename, and delete. |
| `gd_server` | LSP lifecycle over an in-memory `Connection`; incremental sync correctness; byte to UTF-16 conversion; per-file publish policy; every request method's response shape in both its gated and ungated projection; the strict-mode wire path from `initializationOptions` JSON to `publishDiagnostics` severity; cross-file definition jump. |

**Editor-profile walks.** `crates/gd_server/tests/fixtures/client_caps/` holds one JSON per editor, the verbatim `initialize.params.capabilities` that editor sends, captured once and vendored. `tests/editor_profiles.rs` replays the surface against each, so a capability gate that regresses fails against a real client's real capability set rather than a hand-written approximation. That directory's README tracks which profiles are captured.

### Trimmed-API fixture

The analyzer conformance harness does not ship the full multi-MB `extension_api.json`. It carries a hand-curated trimmed native API JSON instead (Script, GDScript, Node3D, Sprite3D, Color, Vector3, Variant, and so on), which keeps the corpus deterministic and quick to load. It grows only when a new fixture needs a native the slice does not have.

The two oracles back each other up: trimmed-API conformance proves analyzer logic, and the differential oracle (§2), which uses the real dump against the real binary, proves coverage gaps in the trimmed slice.

## 5. Robustness: the server must never crash mid-session

**Fuzzing.** Five `cargo-fuzz` targets, each running against random and mutated input, with any panic blocking a release:

| Target | Covers |
|---|---|
| `parse` | Tokenizer and parser on arbitrary `.gd` bytes; error recovery must always return a partial AST. |
| `analyze` | Parser output through `gd_analyze::analyze` with a stub `NoCrossFile` query, covering the resolver and reducer. |
| `index_invariants` | Random `on_file_changed` and `on_file_removed` sequences against `Index::verify()`. |
| `complete_context` | Completion-context classification at arbitrary cursor positions. |
| `scene_parse` | The `.tscn` text parser. |

The fuzz crate is deliberately outside the workspace: libFuzzer needs nightly and cargo-fuzz does not support Windows, so the stable `--workspace` CI on both matrix legs must never compile it. `crates/gd_syntax/tests/fuzz_crate_isolation.rs` guards that.

**Graceful degradation.** A missing `extension_api.json` makes native types dynamic under the provenance rules (`02-frontend-port.md` §11b), with one notice. A missing `project.godot` means treating the root as `res://` with defaults applied. Unreadable files are skipped and logged to stderr at `warn` level, distinguishing `NotFound` (drop the file) from other I/O errors (keep the last-known interface).

A client's `restartOnCrash` is a safety net, not a substitute for any of the above.

## 6. Acceptance sweeps and the release gate

Single-file corpus fixtures and per-capability navigation walks both structurally miss one whole class of defect: an error-level false positive that only appears in a layered real project. A navigation walk that opened only files extending a native class directly once looked clean while a full sweep found bogus errors in 133 of 243 files.

So the standing pre-release gate is a diagnostics sweep, not a navigation walk. `scripts/acceptance/scan_diags.py` opens every `.gd` in a project and tallies every `publishDiagnostics`. It runs on both acceptance projects, comparatively against the previous release binary, with `--strict` plus the warning histogram, and error baselines must hold. The private project is swept on Windows using the Windows binary. `files_with_errors` must be near zero, and every remaining error file has to be justified individually against `godot --check-only --script <file>`, run from inside an imported project since an unimported one lacks the class cache and false-fails. The report's `error_message_histogram` is the fastest way to tell a systematic family apart from genuine project errors.

`scripts/acceptance/README.md` has the runner's arguments and the gate rule.

**Test-rig rule.** Every `gd_server` integration rig that boots via `serve()` must pass `"autoDumpExtensionApi": false` in its `initializationOptions`, or pin `extensionApiPath`. Otherwise a dev machine with `godot` on `PATH` spawns a real dump per test session. Direct `Workspace::load` constructions are spawn-free by design (`ApiDumpPolicy::NeverSpawn`).

**File URIs in tests.** Anything under test that needs a file URI goes through `common::file_uri` (over `gd_server::uri::path_to_file_uri`), never a hand-built `format!("file://…")`. A Windows temp root is a drive path and does not survive that.

## 7. Observability

The server is long-running, so it measures itself. All of it goes to stderr, never stdout.

### 7.1 Structured logs

`tracing` plus `tracing-subscriber`, with `tracing-log` bridging the rest of the workspace's `log::*` callsites so they keep their original target, file, and line. Spans include `analyze{file, version}`, `handle_request{method, id}`, `cold_index{root}`, and `watcher_event{path, kind}`. Each emits an event with elapsed micros on close.

The default filter is `info`, which is cheap in production. `GDLS_LOG` takes per-target directives (`GDLS_LOG=gd_server::api_dump=debug`). `GDLS_TRACE='*>50'` enables a hierarchical-profiler-style filter that logs spans taking more than 50 ms, modelled on rust-analyzer's `RA_PROFILE`. `GDLS_LOG_FORMAT=json` emits the structured JSONL that `scripts/summarize-spans.py` reads.

### 7.2 Latency and the budget file

The `handle_request` span emits `request_latency_us` at close. `bench/budget.toml` records observed p50 and p99 per method, measured against a large real-world project, plus cold index, reconcile, scene index, and peak RSS.

Those numbers are operator-facing reference, not a CI gate: there is no bench job. The file's header records the hardware, the workspace shape, and the capture date for each row, so a row that was estimated rather than measured says so. Re-run the calibration walk (`scripts/lsp-poke.py` plus `scripts/summarize-spans.py`) after any optimization and refresh it.

`gdls bench --record <path>` captures a replayable artifact: the rope snapshot after the cold index, a JSON-RPC trace of the request, and an environment hash (toolchain, target triple, parser and analyzer version). `gdls bench --replay <artifact>` replays it locally, which turns "the editor felt slow" into a one-command repro. `--record` is single-process; two sessions pointed at one path clobber each other.

### 7.3 Memory pressure ladder

Peak RSS is sampled via `sysinfo::Process::memory()` at server start (baseline), end of cold index, and shutdown, and logged as `peak_rss_bytes`. Two thresholds, read from `initializationOptions.memory.softCapMb` and `.hardCapMb` and defaulting to 2× and 4× the calibrated peak:

- **Soft.** Bulk-evict the oldest half of the LRU parse and analysis caches via `pop_lru()`.
- **Hard.** Refuse analysis-priced requests with `ContentModified` and emit `memory_pressure_shed`. Parse- and index-priced features stay served: `foldingRange`, `selectionRange`, `semanticTokens/range`, `documentColor`, and the resolve requests that only read a cached `data` blob.

### 7.4 Fixpoint loop governor

The analyzer's `resolve_*` and `reduce_*` recursive walks have re-entry guards but no natural iteration bound, so `AnalysisContext.iter_count` increments on every reducer entry. Crossing the limit emits `analyzer_runaway{file, last_node_kind}` and bails to a partial type with a diagnostic rather than spinning forever. The default was set empirically against the corpus, where the maximum observed is 7, and is overridable via `initializationOptions.analyzer.iterLimit`.

### 7.5 Cancellation

A router thread reads the wire so `$/cancelRequest` preempts in-flight work rather than waiting for it to finish. A `CancellationToken` is plumbed through the analyzer with cooperative checkpoints, similar to rust-analyzer's salsa-based cancel-by-unwind; `panic = "unwind"` in the root `Cargo.toml` is what makes that cheap. A cancelled request emits `request_cancelled{id, ms_in_flight, last_phase}` and returns `RequestCancelled`; one made stale by an edit returns `ContentModified`.

### 7.6 Index invariant violations

Every mutation to `gd_project::Index` flows through `IndexMutation` (`03-indexing-freshness.md` §7.6). A violation in a release build emits `index_invariant_violated{file, invariant}` and quarantines the file rather than crashing.

## 8. The CI gate

The local dev loop is the CI gate, and it runs on both `ubuntu-latest` and `windows-latest`:

```bash
cargo fmt --all --check
cargo lint                       # clippy --workspace --all-targets -- -D warnings
cargo build --workspace --all-targets
cargo test --workspace
```

CI sets `RUSTFLAGS: -D warnings`, so clippy-clean on first write is the expectation. The Windows leg is the half a Linux dev loop cannot see, and it has gone red for a long stretch before; check `gh run list --branch main` after merging.

## 9. Sources

- [GDScript test runner and the 5-phase pipeline (`.gd`/`.out`, exclusions)](https://github.com/godotengine/godot/blob/master/modules/gdscript/tests/gdscript_test_runner.cpp)
- [Module README, pipeline stages](https://github.com/godotengine/godot/blob/master/modules/gdscript/README.md)
- [Warning codes, for the per-warning unit tests](https://github.com/godotengine/godot/blob/master/modules/gdscript/gdscript_warning.h)
