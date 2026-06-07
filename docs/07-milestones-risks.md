# 07 — Milestones, effort, risks, maintenance

## 1. Phased milestones

Each milestone is independently useful and has explicit exit criteria. Fidelity numbers reference the
oracles in `06-testing-fidelity.md`.

| # | Milestone | Deliverable | Exit criteria |
|---|---|---|---|
| **M0** | Skeleton | LSP lifecycle over stdio; CC connects; empty diagnostics + parse-only `documentSymbol` | CC launches `gdls`, `initialize` handshake succeeds, symbols appear for a trivial file. |
| **M1** | Tokenizer + parser port | Exact syntax-error diagnostics; real `documentSymbol` | Corpus **parse-phase** fidelity ≥ target (closed at **0.9731**, 181/186; see `crates/gd_syntax/tests/conformance/fidelity_floor.txt`); fuzzing finds no panics. |
| **M2** | Environment & indexing | Native DB ingestion; **GDExtension enumeration + ingestion** (`.gdextension` scan, in-project dump and/or `doc_classes` XML); eager interface index; `project.godot` (autoloads, paths, warnings); `class_name` registry | Cross-file names resolve; autoloads & native classes (vanilla + installed GDExtensions) resolvable; cold-index time within budget at scale. |
| **M3** | Analyzer port | Type checking + the warning set (45 active + 3 deprecated-gated); per-file diagnostics; **strict-mode policy layer** (`godot`/`strict`/`off` + per-name `enable`/`disable`/`error` overrides + `@warning_ignore`); `hover`, `definition` | Corpus **analyze-phase** fidelity ≥ target (closed at **1.0000**, 300/300; see `crates/gd_analyze/tests/conformance/analyze_fidelity_floor.txt`); differential oracle agrees on a project sample. |
| **M4** | Freshness & navigation | Closed in M4. `notify` 8.2 + `notify-debouncer-full` 0.6 watcher with a 250 ms quiet-time, wired into the LSP main loop via `crossbeam_channel::select!` (per `docs/03 §6.1`); `Workspace::reconcile()` walks `res://**/*.gd` after cold-index to catch any events `notify` dropped during startup, and again on the `need_rescan` overflow flag. Five reactions dispatched in `server::handle_watcher` — `.gd` Create/Modify/Delete/Renamed, `project.godot`, `extension_api.json`, `.gdextension`, `doc_classes/*.xml` — each funnelled through `IndexMutation::apply` so every mutation runs `Index::verify()` post-apply (debug panics; release quarantines the file). The four nav handlers (`textDocument/references` / `implementation` / `prepareCallHierarchy` + `callHierarchy/{incoming,outgoing}Calls` / `workspace/symbol`) ship as projections over existing structures: `Index::name_referencers` (now public, WP-S2) + parser-level identifier scan for references; `Index::iter_interfaces` extends-walk for implementation; recorded `Binding::Call` for callHierarchy; `nucleo-matcher` 0.3 fuzzy ranking over `ClassNameRegistry::entries` + per-file `Interface.members` for workspace/symbol. Cross-file member-initializer cycle detection (WP-R2) made live in LSP via `AnalysisResult.member_xrefs` (recorded at the `// WP-R2: record_member_xref` marker in `reducer.rs`) consumed by the new `gd_server::xfile::WorkspaceXFileQuery` wrapper that overrides `CrossFileQuery::member_initializer_xrefs`. New CLI subcommand `gdls diagnose --reconcile [--root <path>]` exposes the reconciliation pass for post-suspend / remote-FS recovery without starting a session. See `03-indexing-freshness.md §6–§7`. | All four nav handlers return correct results on `crates/gd_server/tests/watcher_and_nav.rs` (end-to-end tests against a `TempProject` sample); watcher integration test confirms external create + delete reflected in the index within 1.5 s on Windows (debounce 250 ms + reindex); new/renamed/deleted `class_name` reflected without reopening; analyzer fidelity ratchet held at **1.0000** (300/300) after WP-X1+X2 LSP activation; parser ratchet held at **0.972** floor; no regressions in the M0/M1/M2/M3 suites. `IndexMutation` invariants checked on every mutation; new `fuzz_targets/index_invariants.rs` covers random on_file_changed/removed sequences. |
| **M5** | 10k-file hardening, observability, CI, **diagnostics-parity gap closure** | Calibration pass on the 10k-file synthetic corpus (`crates/gd_project/tests/perf_scale.rs`) producing a committed `bench/budget.toml` (cold-index, warm per-file latency p99, peak RSS, watcher throughput, request-latency p50/p99); `tracing` + `tracing-subscriber` instrumentation across the dispatch site, watcher integration, and cold-index; peak-RSS measurement via `sysinfo`; soft/hard RSS budget enforcer; fixpoint loop governor (per-file analyzer iteration cap with `analyzer_runaway` event); `$/cancelRequest` plumbed through cooperative checkpoints; CI bench job ratcheting against `bench/budget.toml`; nightly differential-oracle CI building the godot binary against a committed mini-sample (`crates/gd_analyze/tests/differential_sample/`); `gdls bench --record` reproducer artifact (rope snapshot + request trace + env hash) emitted on regression; **parity gap closure (Phase E, closed)** — five parser-side fixtures ported per the revised M5 plan: `@icon`/`@tool` duplicate-detection now lands in the parser's SCRIPT-annotation branch (`GDScriptParser::icon_annotation` / `tool_annotation`, `gdscript_parser.cpp:4430-4470`), the `@warning_ignore_start` / `@warning_ignore_restore` pair-balance check lands as a parser post-pass mirroring `warning_ignore_region_annotations` (`gdscript_parser.cpp:5182-5219`), and the tokenizer-level visually-similar-to-keyword diagnostic uses the UTS #39 confusable-skeleton algorithm via the `unicode-security` crate (mirrors `TextServer::is_confusable` per `gdscript_tokenizer.cpp:585-602` — **not** `String::similarity()`, which is Sørensen-Dice for "did-you-mean" suggestion text); all five fixtures deleted from `crates/gd_syntax/tests/conformance/known_failures.txt` and the parser floor ratcheted from `0.972` → `1.0` (186/186). See `06-testing-fidelity.md` §5, §8 and the revised M5 plan — whose **Phase F (WP-RD1–RD15)** is the work deferred from M4 into M5 — the hardening findings plus the nav/rename carry-overs (chokepoint encapsulation; `FileId`→`NonZeroU32` to retire the `FileId(0)` placeholder; watcher event-loop testability refactor + dark-branch tests; reconcile/`diagnose` error-injection tests; call-hierarchy assertion strengthening; method-level extends-chain `callee_file` resolution; retirement of the M5-scoped deprecated aliases). The M4 **correctness** fixes (content-addressed parse/analysis cache via `uri::CanonicalKey`, shared `is_excluded`, in-file reference scan) landed during M4. | Soak tests pass against the recorded `bench/budget.toml` with &lt; 5% variance over a 1-hour run on 10k synthetic files; `GDLS_DIFFERENTIAL_THRESHOLD` ≥ 0.85 against the committed mini-sample; CI bench detects ≥ 20% regressions; **analyzer fidelity ratchet at 1.0000 (300/300) AND parser fidelity ratchet at 1.0000 (186/186) — zero entries in either `*_known_failures.txt`**. M5 closed GREEN under the *original* ship bar; that bar was then raised, so v1 ships at **M6** (next row), not M5. |
| **M6** | Exposed-capability parity + warm-start cache (**ships v1**) | Close every gap where an *already-exposed* LSP capability returns incomplete/inaccurate data vs Godot's own GDScript LSP: `hover` member/call/`preload` signatures (M6-F); `definition` for `class_name`-in-expression (M6-B), `preload`/`load` `res://` strings (M6-C) and autoloads (M6-D); project-wide `references` including cross-file member/signal callsites through typed vars (M6-E); hierarchical `documentSymbol` (M6-A); `implementation` for method overrides (M6-G). Plus a persistent per-project **warm-start index cache** keyed on `(cache_format_version, gdls_version, NativeDb::content_hash, project.godot fingerprint)` + per-file `(size, mtime_ns)`, with atomic multi-instance-safe writes (M6-I) and a `reconcile`-by-stat path (M6-H) that stops re-parsing unchanged files. All glue/projection — no analyzer/parser fidelity change. Full per-item design: `08-m6-v1-ship.md`. | Every exposed capability returns output ⊇ Godot's own LSP on the same inputs (re-run of the M5 `scripts/lsp-poke.py` capability walk clean — every row Pass, no "limited"/"null where data exists"); warm start of a large real-world project **> 5× faster** than the cold scan (stat-only); cache validates against `NativeDb::content_hash` + per-file `(size, mtime_ns)` and degrades safely on corruption (verify + quarantine + cold fallback); two gdls processes on one project (e.g. Claude Code + an IDE) run with no cache corruption; both fidelity ratchets still **1.0000 / 1.0000**. On all green: tag **v1.0.0**. See `08-m6-v1-ship.md §6`. |
| **Phase 2** | Scenes & extras (post-v1) | `.tscn` node typing (`$`/`%` precise types); `signatureHelp` + `completion`; `rename` / `documentHighlight` / `declaration`; `$/cancelRequest` *preemption* of in-flight requests | `$`/`%` diagnostics converge with Godot; bonus features usable in a human editor. (The persistent warm-start index cache moved **into M6** — it now gates v1; see the M6 row.) |

**M5 close-out.** M5 landed across eight phases (A–H). Both fidelity ratchets sit at **1.0000** — parser 186/186
(`crates/gd_syntax/tests/conformance/fidelity_floor.txt`) and analyzer 300/300
(`crates/gd_analyze/tests/conformance/analyze_fidelity_floor.txt`), with zero entries in either
`*_known_failures.txt`. The WP-P5 reference numbers committed to `bench/budget.toml` — measured
once locally against a large real-world GDScript project (2 338 `.gd` files): cold-index 3 340 ms, peak RSS 291 MB, soft
cap 582 MB, hard cap 1 164 MB — back WP-H1's memory-pressure ladder. Per the revised scope these
are **reference-only**: the M5 row's original Deliverable/Exit-criteria text above still mentions a
CI bench-budget ratchet, a godot-binary differential CI job, and a 1-hour 10k-file soak — all three
were **dropped** in the revision (a local Godot checkout is read-only inspection; no Godot build
in CI; verification is the Phase H manual walk, not a synthetic soak). M5's own close-out gate was the
Phase H pre-release walk of every exposed LSP capability against the real project corpus,
driven by `scripts/lsp-poke.py` (WP-Q1/Q2/Q3): **ship-recommendation GREEN** under the original
bar — which was then **raised** (full exposed-capability parity + a warm-start cache), moving the
actual v1 ship-gate to **M6** (`08-m6-v1-ship.md`). The same walk caught one v1 blocker the trimmed
conformance fixture couldn't reach —
`NATIVE_METHOD_OVERRIDE` false-firing on engine
virtuals (`_ready`/`_process`/…), since `find_native_class_with_method` matched on name without the
`is_virtual` flag; fixed to mirror Godot's MethodBind-exists gate, analyzer ratchet held 300/300.
Smaller surfaced-and-fixed items (hover `<Script #N>` type-label, missing `--version`/`--help`, two
WP-D1 differential-harness construction bugs) plus the raised-bar backlog are tracked in
`docs/08-m6-v1-ship.md` (the **M6** milestone spec); the optional WP-D1 local differential passes at mean Jaccard 0.8667 ≥ 0.85.

> Targets (the "≥ target" thresholds) are set at the start of M1/M3 against the vendored corpus and ratcheted
> upward in CI; they are intentionally not hard-coded here so they can be calibrated to the corpus once
> measured.

## 2. Effort framing

- **Order of magnitude:** a faithful port (~15k lines of frontend logic) + LSP server + indexer + watcher is
  a **multi-month effort for one experienced engineer**, with the **analyzer (M3) as the long pole**.
- It is **bounded and test-validated**, not open-ended research: the corpus turns fidelity into a
  measurable target, and the parser/tokenizer (M1) and environment (M2) are mechanical ports.
- The two requirements that sounded hardest up front — native-class recognition (incl. GDExtensions) and live
  freshness — are **small, well-understood components** here (`03-indexing-freshness.md`).
- **Calibration from M3 (closed at 1.0):** the analyzer port shipped in ~70 phased
  work-packets across four nested implementation plans,
  roughly **3× the WP count of the original plan**, as each corpus regression bench drove a follow-on
  WP. Budget M4/M5/M6/Phase 2 with the same fidelity-bench-driven follow-on assumption rather than the
  initial WP list alone — corpus and differential oracles routinely surface upstream carve-outs the
  initial design pass did not see.

## 3. Risks & mitigations

| Risk | Likelihood | Mitigation |
|---|---|---|
| Analyzer fidelity gaps (gradual/`Variant` typing especially) | High | Corpus + differential oracle + per-warning unit tests; ratcheted CI fidelity metric. |
| `.out` corpus mixes runtime/debug output | Medium | Curation/filter layer to the parse/analyze subset (M1/M3). |
| GDScript evolves across 4.x | Low–Med | Track the Godot source; re-port from Godot's gdscript-module diffs (GDScript is stable; churn is low). |
| `extension_api.json` drifts from a rebuilt engine | Medium | Watcher reloads on change; documented regen workflow; degrade gracefully if stale/absent. |
| GDExtension classes absent from the API dump (dump timing) or shipping no docs | Medium | Multi-source capture (in-project dump + `doc_classes` XML); enumerate via `.gdextension`; degrade to dynamic with a notice. |
| Performance at 10k files | Medium | Eager-interface/lazy-body split; dependency-tracked invalidation; memory strategy; soak tests in M5. |
| LSP position-encoding / large-file edge cases | Low | UTF-16 conversion tests; incremental sync tests; optional UTF-8 negotiation. |
| Scope creep into running GDScript | Low | Explicit non-goal; compiler/VM excluded by design. |

## 4. Maintenance / upkeep

- **Track Godot.** When Godot's GDScript module changes (e.g. it rebases onto a newer 4.x), re-apply
  the corresponding tokenizer/parser/analyzer diffs from the Godot tree and refresh the vendored corpus.
- **Regenerate `extension_api.json`** whenever Godot's native classes change (engine rebuild).
- **CI fidelity ratchet** prevents silent regressions as the port evolves.

## 5. Out of scope (recap)

Running GDScript (bytecode/VM), `.tscn` typing in v1, `signatureHelp`/`completion` in v1, rename/formatting/
code-actions/semantic-tokens, any GUI/debugger, and any dependency on a running Godot process.

## 6. Sources

- GDScript frontend internals & sizes — https://github.com/godotengine/godot/blob/master/modules/gdscript/README.md · https://github.com/godotengine/godot/blob/master/modules/gdscript/gdscript_analyzer.cpp
- Conformance corpus & generation — https://github.com/godotengine/godot/blob/master/modules/gdscript/tests/gdscript_test_runner.cpp
- `extension_api.json` (incl. GDExtension classes) — https://deepwiki.com/godotengine/godot/15.1-gdextension-api
