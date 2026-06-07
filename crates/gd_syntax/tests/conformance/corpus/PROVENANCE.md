# Conformance corpus — provenance

This directory is a **vendored, fixed copy** of Godot's GDScript golden-file test corpus, used by
`crates/gd_syntax/tests/conformance.rs` as the parse-phase fidelity oracle for M1. Per
`docs/06-testing-fidelity.md` §1 we keep the copy fixed and **refresh it deliberately**, not
automatically — we record the reference release tag here.

## Source

| | |
|---|---|
| Repo | `godotengine/godot` |
| Reference release | `4.6.3-stable` (commit `7d41c59c`) |
| Subtree vendored | `modules/gdscript/tests/scripts/parser/` |
| Vendored on | 2026-05-21 |

## What is here (and what is not)

- **Only `parser/`** is vendored for M1: `errors/` (76), `features/` (86), `warnings/` (27) =
  **189 `.gd`** + **187 `.out`** (the 2 missing `.out` are the 2 `*.notest.gd`, which are skipped).
- `analyzer/` and `runtime/` are **not** vendored yet — they arrive with the analyzer at M3. The
  harness classifies by `.out` first line, so it is corpus-superset-safe: dropping those dirs in
  later changes nothing about M1 behavior.
- `completion/` and `lsp/` are intentionally excluded (Godot's own runner skips them too).

## How the harness reads this (ported from Godot's runner)

The oracle and comparison semantics mirror `modules/gdscript/tests/gdscript_test_runner.cpp`:

- Skip `*.notest.gd`. Keep `*.textonly.gd`, `*.bin.gd`, `*.norun.gd` (M1 is text-tokenizer mode;
  Godot only skips `*.textonly.gd` in *binary* mode). `#debug-only`-first-line files are kept (we
  mirror the `DEBUG_ENABLED` build, which is what produces the `~~` warnings the `.out` files carry).
- Pair the `.out` by swapping the **final** extension (`foo.bin.gd` → `foo.bin.out`).
- **Classify by the `.out` FIRST LINE, never by directory.** Three `GDTEST_ANALYZER_ERROR` files live
  inside `parser/errors/` (`export_enum_wrong_array_type`, `export_enum_wrong_type`,
  `export_tool_button_requires_tool_mode`) — they parse cleanly and only fail at analyze; a
  directory-based classifier would mis-bucket them.
- `GDTEST_PARSER_ERROR` `.out` carries only the **first** error message and **no line/column**
  (runner `// TODO: line, column?`), so M1 compares the message **string** only.
- `GDTEST_OK` ⇒ must parse with **zero** parser errors; runtime output and `~~` warning lines are
  ignored at M1 (they enter at M3).

## Refreshing this corpus

Deliberate, manual step — do it in its own commit and re-stamp the table above:

```bash
# $GODOT = a local checkout of godotengine/godot at tag 4.6.3-stable
src="$GODOT/modules/gdscript/tests/scripts/parser"
dst="crates/gd_syntax/tests/conformance/corpus/parser"
rsync -a --delete --include='*/' --include='*.gd' --include='*.out' --exclude='*' "$src/" "$dst/"
git -C "$GODOT" rev-parse HEAD
git -C "$GODOT" rev-parse "HEAD:modules/gdscript/tests/scripts/parser"
```

Then re-bless the ratchet (`GDLS_BLESS_CONFORMANCE=1 cargo test -p gd_syntax --test conformance -- --nocapture`)
and review the diff to `known_failures.txt` / `fidelity_floor.txt`.
