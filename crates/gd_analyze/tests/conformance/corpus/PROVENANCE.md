# Analyze-phase conformance corpus — provenance

This directory is a **vendored, fixed copy** of Godot's GDScript golden-file test corpus, used by
`crates/gd_analyze/tests/conformance.rs` as the **analyze-phase** fidelity oracle for M3 — the sibling
of the parse-phase corpus at `crates/gd_syntax/tests/conformance/corpus/parser/`. Per
`docs/06-testing-fidelity.md` §1 we keep the copy fixed and **refresh it deliberately**, not
automatically; we record the reference release tag here.

> **Why it lives under `gd_analyze`, not `gd_syntax`.** The analyze harness must call
> `gd_analyze::analyze`, so it can only live in a crate that depends on `gd_analyze`. `gd_syntax` sits
> *below* `gd_analyze` in the DAG and cannot. (The M3 plan's "`gd_syntax/tests/analyze_conformance.rs`"
> sketch predates that layering check — the corpus and harness belong here.)

## Source

| | |
|---|---|
| Repo | `godotengine/godot` |
| Reference release | `4.6.3-stable` (commit `7d41c59c`) |
| Subtree vendored | `modules/gdscript/tests/scripts/analyzer/` |
| Vendored on | 2026-05-23 |

## What is here (and what is not)

- **Only `analyzer/`** is vendored: `errors/` (170), `features/` (107), `warnings/` (23) =
  **300 testable `.gd`** + **300 `.out`**, plus **28 `*.notest.gd`** multi-file companions (no `.out`;
  they are loaded into the index so the file under test can resolve cross-file references, never run
  standalone).
- `runtime/` is **not** vendored — its `.out` files are dominated by VM stdout, which the frontend
  (diagnostics-only) port does not produce. `completion/` and `lsp/` are intentionally excluded
  (Godot's own runner skips them too).
- The counts here are **larger than the M3 plan's estimates** (the plan guessed ~120/~100/23).
  Godot is the source of truth: grepped at vendor time, the real counts are 170/107/23.

## How the harness reads this (ported from Godot's runner)

The oracle and comparison semantics mirror `modules/gdscript/tests/gdscript_test_runner.cpp`:

- Skip `*.notest.gd` as a primary case (it has no `.out`); keep `*.textonly.gd` / `*.norun.gd`.
- Pair the `.out` by swapping the **final** extension.
- **Classify by the `.out` FIRST LINE, never by directory.** A file under `errors/` may be
  `GDTEST_PARSER_ERROR` (caught before analysis) and is handled by the parser harness, not here.
  - `GDTEST_OK` ⇒ analysis must produce **zero errors** and exactly the `~~ WARNING` set the `.out`
    lists; runtime stdout after the diagnostic lines is stripped.
  - `GDTEST_ANALYZER_ERROR` ⇒ analysis must produce exactly the `>> ERROR` lines (plus any `~~ WARNING`
    lines the `.out` carries), in order.
  - Anything else (`GDTEST_PARSER_ERROR`, compiler/runtime/load phases) ⇒ **skipped** here.
- Diagnostic lines render as Godot does: `>> ERROR at line N: <message>` and
  `~~ WARNING at line N: (CODE) <message>`, where `N` is the 1-based source line of the diagnostic's
  byte span. The whole warning machinery is `DEBUG_ENABLED` in Godot; the runner forces every
  warning to `WARN`, so the harness compares against that level.

## Refreshing this corpus

Deliberate, manual step — do it in its own commit and re-stamp the table above:

```bash
# $GODOT = a local checkout of godotengine/godot at tag 4.6.3-stable
src="$GODOT/modules/gdscript/tests/scripts/analyzer"
dst="crates/gd_analyze/tests/conformance/corpus/analyzer"
rsync -a --delete --include='*/' --include='*.gd' --include='*.out' --exclude='*' "$src/" "$dst/"
git -C "$GODOT" rev-parse HEAD
git -C "$GODOT" rev-parse "HEAD:modules/gdscript/tests/scripts/analyzer"
```

Then re-bless the ratchet
(`GDLS_BLESS_CONFORMANCE=1 cargo test -p gd_analyze --test conformance -- --nocapture`) and review the
diff to `analyze_known_failures.txt` / `analyze_fidelity_floor.txt`.
