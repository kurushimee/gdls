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
| 4.7 | `analyzer/` | `4.7.2-stable` (commit `ed1daf0b`, subtree `0c8c912e`) | 2026-08-29 |
| 4.6 | `analyzer-4.6/` | `4.6.3-stable` (commit `35e80b3a`, subtree `50c20a36`) | 2026-08-29 |

The **newest** supported release carries the full vendored tree, byte for byte, verifiable with a single `diff -rq`. Every older release carries only the files whose analyze-phase result actually differs at that tag. A version bump is therefore "vendor the new release's full corpus, then demote the previous full tree to its divergence subset" — never a wholesale copy of files that behave the same at both. `scripts/conformance/demote_corpus.py` does the mechanical half; deciding which candidates genuinely diverge is a manual review.

**The 4.6 subset is two files**, both for the same reason: `errors/abstract_methods` and `errors/variadic_functions`. 4.7's test *runner* stable-sorts the error list by start line before printing (`gdscript_test_runner.cpp:578-591`); 4.6 printed them in emission order. These two are the only corpus files where the two orders differ. Nothing else in the 4.6 goldens diverges once the guarded behaviors are accounted for, and those are pinned directly instead — the parser and lexer ones in `crates/gd_syntax/tests/dialect_delta.rs`, the analyzer ones in `crates/gd_analyze/tests/inherited_return_type.rs` and its siblings. See `docs/02-frontend-port.md` §11c and §11d for the full delta tables, including the no-ops.

## What is here, and what is not

Only `analyzer/` is vendored: at 4.7.2, `errors/` (63), `features/` (107), and `warnings/` (24), giving 194 testable `.gd` plus 194 `.out`, along with 28 `*.notest.gd` multi-file companions. Those companions have no `.out`; they are loaded into the index so the file under test can resolve cross-file references, and are never run standalone.

That is 103 fewer `.gd` than 4.6.3 carried, which is a consolidation rather than a loss: 4.7 merged many one-error files into grouped ones, so `cyclic_ref_const` / `cyclic_ref_enum` / `cyclic_ref_enum_value` and the rest now live inside `errors/cyclic_reference.gd`, the two `bitwise_float_*_operand` files inside `errors/bitwise_float.gd`, the four `cast_*` files inside `errors/invalid_cast.gd`, and so on. The merged files also cover more than the originals did, which is what surfaced the two gaps recorded in `analyze_known_failures.txt`.

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
# $GODOT = a local checkout of godotengine/godot at the newest supported tag
src="$GODOT/modules/gdscript/tests/scripts/analyzer"
dst="crates/gd_analyze/tests/conformance/corpus/analyzer"
rsync -a --delete --include='*/' --include='*.gd' --include='*.out' --exclude='*' "$src/" "$dst/"
diff -rq "$src" "$dst"   # must print nothing
git -C "$GODOT" rev-parse HEAD
git -C "$GODOT" rev-parse "HEAD:modules/gdscript/tests/scripts/analyzer"
```

Then re-bless the ratchet with `GDLS_BLESS_CONFORMANCE=1 cargo test -p gd_analyze --test conformance -- --nocapture` and review the diff to `analyze_known_failures.txt` and `analyze_fidelity_floor.txt`.

## Adding support for a newer release

Bringing in the next release demotes the current tree to a subset:

```bash
scripts/conformance/demote_corpus.py analyzer --from 4.7 --to 4.8 --godot "$GODOT"
```

That leaves `analyzer-4.7/` holding the candidate divergences, refreshes `analyzer/` from the new tag, and prints the `SUITES` row to add. Then review every candidate by hand and delete the ones whose analyze-phase result does not actually differ — a renamed helper or an added case that behaves the same at both tags belongs in neither tree.
