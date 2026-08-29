# Analyze-phase conformance corpus provenance

This directory is a vendored, fixed copy of Godot's GDScript golden-file test corpus, used by `crates/gd_analyze/tests/conformance.rs` as the analyze-phase fidelity oracle. It is the sibling of the parse-phase corpus at `crates/gd_syntax/tests/conformance/corpus/parser/`. Per `docs/06-testing-fidelity.md` §1 the copy stays fixed and is refreshed deliberately, not automatically, so the reference release tag is recorded here.

> **Why it lives under `gd_analyze`, not `gd_syntax`.** The analyze harness has to call `gd_analyze::analyze`, so it can only live in a crate that depends on `gd_analyze`. `gd_syntax` sits below `gd_analyze` in the DAG and cannot.

## Source

| | |
|---|---|
| Repo | `godotengine/godot` |
| Subtree vendored | `modules/gdscript/tests/scripts/analyzer/` |

## Suites

gdls serves more than one Godot feature release, and their goldens differ. Each supported release is one **suite**: a corpus tree plus the dialect its goldens were generated at. `conformance.rs` walks every suite and reports one aggregate fidelity number, so no file can be lost by moving between them.

| Suite | Directory | Reference release | Vendored on |
|---|---|---|---|
| 4.6 | `analyzer/` | `4.6.3-stable` (commit `7d41c59c`) | 2026-05-23 |
| 4.7 | `analyzer-4.7/` | `4.7.2-stable` (commit `ed1daf0b`) | 2026-08-29 |

The **oldest** supported release carries the full vendored tree; every newer one carries only the files that actually diverge from it. A version bump is therefore "vendor the new release's full corpus, then demote the previous full tree to its divergence subset" — never a wholesale copy of files that are identical across the two.

## What is here, and what is not

Only `analyzer/` is vendored: at 4.6.3, `errors/` (170), `features/` (107), and `warnings/` (23), giving 300 testable `.gd` plus 300 `.out`, along with 28 `*.notest.gd` multi-file companions. Those companions have no `.out`; they are loaded into the index so the file under test can resolve cross-file references, and are never run standalone.

The 4.7 subset is the files whose goldens 4.7 changed or added: `untyped_override_return_incompatible_type`, `untyped_override_untyped_return`, `untyped_override_return_compatible_type` (an untyped override now inherits the parent's return type) and `constant_expressions` (the new constant-folding fallback reducers).

`runtime/` is not vendored, since its `.out` files are dominated by VM stdout, which the diagnostics-only frontend port does not produce. `completion/` and `lsp/` are intentionally excluded, as Godot's own runner skips them too.

Counts come from grepping Godot at vendor time, never from a plan estimate.

## How the harness reads this

The oracle and comparison semantics mirror `modules/gdscript/tests/gdscript_test_runner.cpp`:

- Skip `*.notest.gd` as a primary case, since it has no `.out`. Keep `*.textonly.gd` and `*.norun.gd`.
- Pair the `.out` by swapping the final extension.
- **Classify by the `.out` first line, never by directory.** A file under `errors/` may be `GDTEST_PARSER_ERROR`, caught before analysis, and is handled by the parser harness rather than here.
  - `GDTEST_OK` means analysis must produce zero errors and exactly the `~~ WARNING` set the `.out` lists. Runtime stdout after the diagnostic lines is stripped.
  - `GDTEST_ANALYZER_ERROR` means analysis must produce exactly the `>> ERROR` lines, plus any `~~ WARNING` lines the `.out` carries, in order.
  - Anything else (`GDTEST_PARSER_ERROR`, or the compiler, runtime, and load phases) is skipped here.
- Diagnostic lines render as Godot does: `>> ERROR at line N: <message>` and `~~ WARNING at line N: (CODE) <message>`, where `N` is the 1-based source line of the diagnostic's byte span. The whole warning machinery is `DEBUG_ENABLED` in Godot, and the runner forces every warning to `WARN`, so the harness compares against that level.

## Refreshing this corpus

A deliberate manual step. Do it in its own commit and re-stamp the table above:

```bash
# $GODOT = a local checkout of godotengine/godot at tag 4.6.3-stable
src="$GODOT/modules/gdscript/tests/scripts/analyzer"
dst="crates/gd_analyze/tests/conformance/corpus/analyzer"
rsync -a --delete --include='*/' --include='*.gd' --include='*.out' --exclude='*' "$src/" "$dst/"
git -C "$GODOT" rev-parse HEAD
git -C "$GODOT" rev-parse "HEAD:modules/gdscript/tests/scripts/analyzer"
```

Then re-bless the ratchet with `GDLS_BLESS_CONFORMANCE=1 cargo test -p gd_analyze --test conformance -- --nocapture` and review the diff to `analyze_known_failures.txt` and `analyze_fidelity_floor.txt`.
