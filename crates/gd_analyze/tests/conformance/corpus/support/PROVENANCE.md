# Harness-support sources

Files vendored from Godot that the corpus fixtures reference but that live **outside** the vendored subtree, so they cannot sit in a suite directory.

`corpus/analyzer/` is a byte-for-byte mirror of `modules/gdscript/tests/scripts/analyzer/`, verified by a bare `diff -rq` (see `../PROVENANCE.md`). Anything added inside it would break that check. Files here are indexed by `conformance.rs::corpus_index` for every suite and are never walked by the fidelity pass, which only descends into the suite directories.

| File | Upstream path | Reference release |
|---|---|---|
| `utils.notest.gd` | `modules/gdscript/tests/scripts/utils.notest.gd` | `4.7.2-stable` (commit `ed1daf0b`) |

`utils.notest.gd` declares `class_name Utils` and is the shared assertion helper for the whole test suite: 364 references across `analyzer/` call `Utils.check(…)` and `Utils.get_type(…)`. Godot's own runner resolves it because it sits at the root of `scripts/`, one level above `analyzer/`. Without it every one of those references reads as an undeclared identifier (#312).

Only the *interface* of these files is used — the index shallow-parses them for `class_name` and signatures, and they are never analyzed — so a 4.6-versus-4.7 body difference cannot change a golden. The newest tag's copy is the one vendored, matching the newest-is-primary rule the dialect guards follow.

Refresh alongside the corpus, in the same commit:

```bash
cp "$GODOT/modules/gdscript/tests/scripts/utils.notest.gd" \
   crates/gd_analyze/tests/conformance/corpus/support/utils.notest.gd
```
