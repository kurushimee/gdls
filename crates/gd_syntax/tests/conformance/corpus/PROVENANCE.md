# Conformance corpus provenance

This directory is a vendored, fixed copy of Godot's GDScript golden-file test corpus, used by `crates/gd_syntax/tests/conformance.rs` as the parse-phase fidelity oracle. Per `docs/06-testing-fidelity.md` §1 the copy stays fixed and is refreshed deliberately, not automatically, so the reference release tag is recorded here.

## Suites

gdls supports more than one Godot feature release, so the harness reads a *set* of trees, each paired with the dialect its goldens were generated at (`SUITES` in `conformance.rs`). The newest supported release carries the full vendored tree; an older release carries only the files whose **parse-phase** result actually differs at that tag.

| Suite | Directory | Dialect | Contents |
|---|---|---|---|
| `4.7` | `parser/` | `Godot4_7` | The full upstream tree, byte for byte. |
| `4.6` | — | `Godot4_6` | No subset. See below. |

**4.6 has no subset.** The two tags' parser corpora differ in exactly five places, and none of them changes a parse-phase result: `annotations.gd`, `export_arrays.gd`, and `export_enum.gd` only swap a test-harness helper call (`Utils.print_property_extended_info(…)` became `print(Utils.get_property_extended_info(…))`); `export_variable.gd` gained enum cases; and `multiline_preload.gd` is new in 4.7. All of them are `GDTEST_OK` at both tags, so the parse phase sees no divergence at all. The guarded parser and lexer behaviors that *do* differ are pinned directly instead, in `crates/gd_syntax/tests/dialect_delta.rs`.

## Source

| | |
|---|---|
| Repo | `godotengine/godot` |
| Reference release | `4.7.2-stable` (commit `ed1daf0b`, subtree `c640d08d`) |
| Subtree vendored | `modules/gdscript/tests/scripts/parser/` |
| Vendored on | 2026-08-29 |

## What is here, and what is not

Only `parser/` is vendored here: `errors/` (76), `features/` (87), and `warnings/` (27), giving 190 `.gd` plus 188 `.out`. The 2 missing `.out` belong to the 2 `*.notest.gd` files, which are skipped.

The tree is a byte-exact mirror of upstream, verifiable with a single `diff -rq`. gdls-authored regression cases do **not** live here — `Nested typed collections are not supported.`, which Godot's own corpus never covers, is pinned as a unit test in `crates/gd_syntax/src/parser.rs` instead.

`analyzer/` is vendored separately, under `crates/gd_analyze/tests/conformance/corpus/`, since the analyze harness has to call `gd_analyze` and `gd_syntax` sits below it in the crate DAG. `runtime/` is not vendored at all: its `.out` files are dominated by VM stdout, which a diagnostics-only frontend does not produce. The harness classifies by the `.out` first line rather than by directory, so it is corpus-superset-safe.

`completion/` and `lsp/` are intentionally excluded, since Godot's own runner skips them too.

## How the harness reads this

The oracle and comparison semantics mirror `modules/gdscript/tests/gdscript_test_runner.cpp`:

- Skip `*.notest.gd`. Keep `*.textonly.gd`, `*.bin.gd`, and `*.norun.gd`: this harness runs the text tokenizer, and Godot only skips `*.textonly.gd` in *binary* mode. Files whose first line is `#debug-only` are kept, mirroring the `DEBUG_ENABLED` build, which is what produces the `~~` warnings the `.out` files carry.
- Pair the `.out` by swapping the final extension, so `foo.bin.gd` pairs with `foo.bin.out`.
- **Classify by the `.out` first line, never by directory.** Three `GDTEST_ANALYZER_ERROR` files live inside `parser/errors/` (`export_enum_wrong_array_type`, `export_enum_wrong_type`, `export_tool_button_requires_tool_mode`). They parse cleanly and only fail at analyze, so a directory-based classifier would mis-bucket them.
- A `GDTEST_PARSER_ERROR` `.out` carries only the first error message and no line or column (the runner has a `// TODO: line, column?`), so this harness compares the message string only.
- `GDTEST_OK` means the file must parse with zero parser errors. Runtime output and `~~` warning lines are ignored here; the analyze-phase harness owns them.

## Refreshing this corpus

A deliberate manual step. Do it in its own commit and re-stamp the table above:

```bash
# $GODOT = a local checkout of godotengine/godot at the newest supported tag
src="$GODOT/modules/gdscript/tests/scripts/parser"
dst="crates/gd_syntax/tests/conformance/corpus/parser"
rsync -a --delete --include='*/' --include='*.gd' --include='*.out' --exclude='*' "$src/" "$dst/"
diff -rq "$src" "$dst"   # must print nothing
git -C "$GODOT" rev-parse HEAD
git -C "$GODOT" rev-parse "HEAD:modules/gdscript/tests/scripts/parser"
```

Then re-bless the ratchet with `GDLS_BLESS_CONFORMANCE=1 cargo test -p gd_syntax --test conformance -- --nocapture` and review the diff to `known_failures.txt` and `fidelity_floor.txt`.

## Adding support for a newer release

The tree above always tracks the newest supported release, so bringing in the next one *demotes* the current tree to a subset:

```bash
scripts/conformance/demote_corpus.py parser --from 4.7 --to 4.8 --godot "$GODOT"
```

That leaves `parser-4.7/` holding only the files whose parse-phase result differs at 4.7, refreshes `parser/` from the new tag, and prints the `SUITES` row to add. If the subset comes out empty, delete the directory and record why in the suite table above, the way 4.6 is recorded.
