# 08. How gdls was built

A record of the milestones, kept because the reasoning behind several design decisions only makes sense next to the problem that forced them. Nothing here describes current behavior; for that, read `00` through `07` and `09`. For release-by-release detail, read [`CHANGELOG.md`](../CHANGELOG.md).

The original design spec was written 2026-05-20 and covered two phases.

## Phase 1: the diagnostics oracle (M0 through M6)

Phase 1 built gdls into a faithful diagnostics-and-navigation oracle for Claude Code. Its ship bar was exposed-capability parity against Godot's own LSP.

| # | Milestone | What it delivered |
|---|---|---|
| M0 | Skeleton | LSP lifecycle over stdio, an `initialize` handshake, parse-only `documentSymbol` |
| M1 | Tokenizer and parser port | Exact syntax-error diagnostics; real `documentSymbol`. Closed at 0.9731 parse-phase fidelity, 181/186 |
| M2 | Environment and indexing | Native DB ingestion, GDExtension enumeration, the eager interface index, `project.godot`, the `class_name` registry |
| M3 | Analyzer port | Type checking plus the warning set, per-file diagnostics, the strict-mode policy layer, `hover` and `definition`. Closed at 1.0000 analyze-phase fidelity, 300/300 |
| M4 | Freshness and navigation | The `notify` watcher wired into the main loop, dependency-graph invalidation, `references`, `implementation`, call hierarchy, `workspace/symbol`, and the `gdls diagnose --reconcile` subcommand |
| M5 | Hardening and observability | `tracing` instrumentation, the peak-RSS sampler and memory-pressure ladder, the fixpoint governor, `$/cancelRequest`, the differential-oracle harness, and the calibration pass behind `bench/budget.toml` |
| M6 | Capability parity and warm start | Every exposed capability brought to a superset of Godot's own LSP, plus the persistent index cache. Shipped v1.0.0 on 2026-06-10 |

### What M3 cost, and what that taught

The analyzer port shipped in about 70 phased work packets across four nested implementation plans, roughly 3× the count of the initial design. Every follow-on was driven by a corpus regression, upstream carve-outs the design pass simply did not see: the lambda body queue order, the cross-file member-initializer cycle, the synthetic post-EOF line for `match`-pattern subscript-`Index` emission.

The budgeting lesson stuck for the rest of the project. A fidelity-bench-driven port should be planned around the assumption that the bench will find things the plan did not, not around the initial work-packet list.

### The parser floor, closed in M5

Five parser-side fixtures were ported to take the parse ratchet from 0.972 to 1.0000 (186/186): duplicate `@icon` and duplicate `@tool` detection in the parser's SCRIPT-annotation branch, the `@warning_ignore_start`/`@warning_ignore_restore` pair-balance post-pass, and the tokenizer's visually-similar-to-keyword check. The last one uses the UTS #39 confusable-skeleton algorithm via the `unicode-security` crate, mirroring `TextServer::is_confusable`, and specifically not `String::similarity()`, which is Sørensen-Dice for "did you mean" suggestion text.

### Why the ship bar moved from M5 to M6

M5's pre-release walk of every exposed LSP capability came back green under the original bar: no crashes, no wrong answers, safe-but-sometimes-incomplete acceptable. The bar was then raised to require that every exposed capability be fully correct, with no incomplete data on any input and no regressions against Godot's own LSP, plus a cached startup so a long index happens once rather than every launch.

That raise created M6. Seven capability items closed the genuine gaps (hierarchical `documentSymbol`; `definition` on `class_name` in expression position, `preload` strings, and autoloads; project-wide `references` through typed vars; `hover` member, call, and `preload` signatures; `implementation` for method overrides), and the persistent warm-start cache closed the startup cost, measuring 14.7× on a 3,000-file synthetic project. `v1.0.0` was tagged 2026-06-10.

That same walk also caught a blocker the trimmed conformance fixture could not reach: `NATIVE_METHOD_OVERRIDE` false-firing on engine virtuals (`_ready`, `_process`, and the rest), because the native-method lookup matched on name without the `is_virtual` flag. Fixed to mirror Godot's MethodBind-exists gate, with the analyzer ratchet holding at 300/300.

### The v1.0.1 lesson: sweeps, not walks

The v1.0.0 capability walks opened only files that extended a native class directly, and looked clean. A full diagnostics sweep then found error-level false positives in 133 of 243 Pixelorama files, the cross-file families fixed in v1.0.1.

Single-file corpus fixtures and per-capability navigation walks both structurally miss that class of defect. `scripts/acceptance/scan_diags.py` was written in response, and the comparative sweep on both acceptance projects became the standing release gate (`06-testing-fidelity.md` §6).

The last Phase 1 release was v1.0.7, on 2026-06-13.

## Phase 2: the generic language server (M7 through M11)

Phase 2 reframed Phase 1's ship bar as the problem. Parity with Godot's LSP is a low bar, because Godot's LSP is exotic: full functionality needs Godot-aware clients, custom notifications, and a running editor. The goal became working the way rust-analyzer works for Rust, tracked under umbrella issue [#30](https://github.com/kurushimee/gdls/issues/30) against a v1.0.5 baseline, specified 2026-06-12.

| # | Milestone | What it delivered |
|---|---|---|
| M7 | Protocol foundations | Concurrent dispatch so `$/cancelRequest` truly preempts, `workDoneProgress`, `workspace/configuration`, dynamic `didChangeWatchedFiles`, pull diagnostics, the `##` doc-comment and BBCode pipeline, diagnostic `codeDescription` (#57 through #63) |
| M8 | Editing core | `completion` plus `completionItem/resolve`, and `signatureHelp`, with the full convention set. The Phase 2 long pole (#64, #65) |
| M9 | Navigation and refactoring | `rename` and `prepareRename`, `documentHighlight`, `declaration`, `typeDefinition`, `typeHierarchy`, `foldingRange`, `selectionRange`, `workspaceSymbol/resolve` (#66 through #71) |
| M10 | Presentation and actions | `semanticTokens` full, delta, and range on the standard legend; `inlayHint`; `documentColor`; `codeAction` quickfixes plus `source.fixAll` (#72 through #75) |
| M11 | Scenes and file operations | The `.tscn` scene index feeding `$`/`%` typing, scene-aware completion, autoload `uid://` scene typing, `willRenameFiles`, the external-formatter bridge (#76 through #80) |

M7 through M11 shipped and closed by 2026-06-15. A post-phase hardening wave then closed every remaining follow-up (#99, #125, #132, #157, #161, #189, #193, #204, #246, plus the M9, M10, and M11 deferral lists), and the issue tracker reached zero on 2026-08-28. Phase 2 shipped as **v2.0.0** on 2026-08-29, after two rounds of end-to-end verification against the release binary found fourteen more issues (#255 through #265, then #277, #279, #280) and the release gate itself found two more (#284, #286).

### The `$`/`%` premise correction

M11 was specified around precise scene-derived types for `$Node` and `%Unique`, retiring what Phase 1 had documented as a permissive-`Variant` deviation. Reading Godot's analyzer closed that question differently: `reduce_get_node` types every `$` and `%` as a hard bare `NATIVE Node`, and the precise per-node type the editor shows comes from a separate scene-instantiation path, not from the analyzer.

Feeding a precise type into gdls's analyzer would therefore have been the *less* faithful choice. A `DataType` is used symmetrically in compatibility checks, so a precise `$Health: Node2D` would turn `var c: Control = $Health` into a false-positive error that Godot does not emit. The scene index shipped, and its resolution seam drives navigation only. The reasoning is now in `02-frontend-port.md` §11.

## Two releases at once (v3.0.0)

By the time Phase 2 shipped, stable Godot had moved to 4.7. Supporting it by re-porting would have dropped 4.6, and shipping two binaries would have doubled every future re-port, so the release became a per-project setting instead: one binary, the dialect read from `project.godot`, and every ported function carrying the newest behavior with the older one wrapped in a guard.

`git diff 4.6.3-stable 4.7.2-stable -- modules/gdscript/` produced 24 behavioral differences. Nine turned out to be no-ops for gdls — either the change fixed an engine bug gdls never had (the `ParserError` range, the retargeted tokenizer error sites), or it touched machinery gdls does not port (`cursor_position`, `GDScriptWarning`'s columns, the `-1` extent sentinel). Writing each of those down beside the guards, in the delta tables in `02-frontend-port.md` §11c and §11d, mattered as much as the guards themselves: a no-op that is not recorded looks like an omission on the next re-port.

Two findings were worth more than the delta itself. Vendoring the 4.7.2 corpus byte for byte against the Godot checkout revealed that the tree filed as "4.6.3" contained a gdls-authored test pair that exists in neither tag, and one file that was already 4.7.2 content — which is why the byte-exact-mirror rule is now in `PROVENANCE.md` and why gdls-authored cases live in unit tests. And upstream had consolidated its analyzer corpus, 300 mostly one-error files down to 194 grouped ones, so coverage rose while the denominator fell. That rise is what exposed three real analyzer gaps, all of them equally wrong at 4.6: enum values could not name their siblings, an override against a purely native ancestor was never signature-checked, and native `Array[T]` return types were flattening to `Variant`.

The default for a project that declares no version is the **newest** port, which is why this is a major release: an existing undeclared project can see different diagnostics after the upgrade. Godot writes that entry itself, so a real project always has one.

## Three lessons that still apply

**Mutating consumers need their own firewall.** `references` and `definition` are read-tuned, so their inaccuracies become silent source corruption under `rename` (which took six adversarial review rounds), `codeAction` edits, `willRenameFiles`, and the autoload rename. The pattern that works: a fail-closed positive-project-resolution gate, binding-correct collection (never name-only), and refusing outright rather than half-applying. Widening a candidate set is safe; widening what is collected inside one is not.

**Precise scene types are navigation-only.** See the premise correction above, and `02-frontend-port.md` §11.

**Generic LSP is the contract.** `semanticTokens` advertises the standard legend only, intersected per client at emit time, and `executeCommandProvider` lists exactly the one real command, never an empty or broken list. The anti-catalog in `09-lsp-conventions.md` §3 is binding.
