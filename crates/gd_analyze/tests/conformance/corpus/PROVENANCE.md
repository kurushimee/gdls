# Analyze-phase conformance corpus provenance

This directory is a vendored, fixed copy of Godot's GDScript golden-file test corpus, used by `crates/gd_analyze/tests/conformance.rs` as the analyze-phase fidelity oracle. It is the sibling of the parse-phase corpus at `crates/gd_syntax/tests/conformance/corpus/parser/`. Per `docs/06-testing-fidelity.md` §1 the copy stays fixed and is refreshed deliberately, not automatically, so the reference release tag is recorded here.

> **Why it lives under `gd_analyze`, not `gd_syntax`.** The analyze harness has to call `gd_analyze::analyze`, so it can only live in a crate that depends on `gd_analyze`. `gd_syntax` sits below `gd_analyze` in the DAG and cannot.

## Source

| | |
|---|---|
| Repo | `godotengine/godot` |
| Reference release | `4.6.3-stable` (commit `7d41c59c`) |
| Subtree vendored | `modules/gdscript/tests/scripts/analyzer/` |
| Vendored on | 2026-05-23 |

## What is here, and what is not

Only `analyzer/` is vendored: `errors/` (170), `features/` (107), and `warnings/` (23), giving 300 testable `.gd` plus 300 `.out`, along with 28 `*.notest.gd` multi-file companions. Those companions have no `.out`; they are loaded into the index so the file under test can resolve cross-file references, and are never run standalone.

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
