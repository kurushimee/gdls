# Conformance corpus provenance

This directory is a vendored, fixed copy of Godot's GDScript golden-file test corpus, used by `crates/gd_syntax/tests/conformance.rs` as the parse-phase fidelity oracle. Per `docs/06-testing-fidelity.md` §1 the copy stays fixed and is refreshed deliberately, not automatically, so the reference release tag is recorded here.

## Source

| | |
|---|---|
| Repo | `godotengine/godot` |
| Reference release | `4.6.3-stable` (commit `7d41c59c`) |
| Subtree vendored | `modules/gdscript/tests/scripts/parser/` |
| Vendored on | 2026-05-21 |

## What is here, and what is not

Only `parser/` is vendored here: `errors/` (76), `features/` (86), and `warnings/` (27), giving 189 `.gd` plus 187 `.out`. The 2 missing `.out` belong to the 2 `*.notest.gd` files, which are skipped.

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
# $GODOT = a local checkout of godotengine/godot at tag 4.6.3-stable
src="$GODOT/modules/gdscript/tests/scripts/parser"
dst="crates/gd_syntax/tests/conformance/corpus/parser"
rsync -a --delete --include='*/' --include='*.gd' --include='*.out' --exclude='*' "$src/" "$dst/"
git -C "$GODOT" rev-parse HEAD
git -C "$GODOT" rev-parse "HEAD:modules/gdscript/tests/scripts/parser"
```

Then re-bless the ratchet with `GDLS_BLESS_CONFORMANCE=1 cargo test -p gd_syntax --test conformance -- --nocapture` and review the diff to `known_failures.txt` and `fidelity_floor.txt`.
