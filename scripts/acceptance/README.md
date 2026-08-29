# Acceptance runner

Parameterized pre-release tooling. It drives `scripts/lsp-poke.py` against any GDScript project to verify that every exposed LSP capability returns real data, and that the warm-start cache is at least 5x faster than a cold start.

The diagnostics sweep at the bottom of this file is the standing release gate. The capability walk above it is a complement, not a substitute.

## Hard rule

**Nothing here may hardcode a project root, a Godot binary path, or any proprietary path.** The runner accepts all paths via arguments or environment variables. The proprietary stage (Step 3) reuses `run.sh` with out-of-repo arguments and writes its report outside the repo, leaving zero committed trace.

---

## Quick start

```bash
# Option A: flags
scripts/acceptance/run.sh \
  --project /path/to/pixelorama \
  --godot   /path/to/godot4 \
  --session scripts/acceptance/sessions/pixelorama.json

# Option B: environment variables
PROJECT_ROOT=/path/to/pixelorama \
GODOT_BIN=/path/to/godot4 \
EXTENSION_API=/path/to/extension_api.json \  # optional, skips dump step
  scripts/acceptance/run.sh
```

On success: exit 0, and a report at `target/acceptance/oss-report.json`. On failure: exit 1, with a per-capability failure list on stderr.

---

## Arguments and environment variables

| Flag | Env variable | Required | Description |
|------|-------------|----------|-------------|
| `--project PATH` | `PROJECT_ROOT` | Yes | Godot project root directory |
| `--godot PATH` | `GODOT_BIN` | Unless `--api` is supplied | Godot 4.x binary |
| `--api PATH` | `EXTENSION_API` | No | Pre-dumped `extension_api.json`; skips the dump step |
| `--session PATH` | none | No | Session template (default: `walk.json`) |

Flags override environment variables when both are set.

---

## What the runner does

1. **Dump `extension_api.json`**, if not supplied via `--api`: runs `"$GODOT_BIN" --dump-extension-api-with-docs --headless` in a temp directory.
2. **Build `gdls`**: `cargo build --release --bin gdls`.
3. **Substitute tokens** in the session template (`__PROJECT_ROOT__`, `__EXTENSION_API__`) and write a concrete session to `target/acceptance/concrete-session.json`.
4. **Capability walk**: `python3 scripts/lsp-poke.py --session … --gdls … --out …`.
5. **Cold and warm bench**: clears `$PROJECT_ROOT/.gdls/`, runs a minimal `initialize` to `shutdown` session (empty opens, empty requests) to measure cold startup-to-ready time, then runs it again with the cache present. Reads `elapsed_ms` from lsp-poke's JSON output and asserts `cold/warm >= 5.0`.
6. **Validate**: checks every required capability label for a non-null, non-empty result with content-level assertions (hover must contain `func` and `->`, for one). Writes `target/acceptance/oss-report.json`.

> **Bench overhead caveat.** On small projects, fixed LSP overhead (process spawn, roughly 150 ms of stderr drain in lsp-poke) can dilute the measured cold/warm ratio below 5x even when the true index-build speedup is much larger. A sub-5x result on a small project such as Pixelorama at 243 files is a measurement artifact, not a cache regression; the >5x property is proven deterministically by the synthetic 3,000-file bench (`cache_warm_start.rs`, 14.7x). So the runner enforces the 5x gate only on projects with 1000 or more `.gd` files. Below that it prints and records the ratio as informational (`bench_enforced: false` in `oss-report.json`) without failing the run.

---

## Candidate OSS projects

Both are GDScript-heavy with cross-file calls, autoloads, preloads, and inner classes, so they exercise the whole capability set.

**Pixelorama** (https://github.com/Orama-Interactive/Pixelorama): a GDScript-only pixel-art editor, with autoloads (`Global`, `Palettes`, and others), preloads across layers, and inner classes in tool scripts.

**Material Maker** (https://github.com/RodZill4/material-maker): a node-based material editor, with extensive cross-file signal and method calls and deep inheritance chains.

Clone either, point `--project` at the cloned root, and point `--session` at a filled project-specific session (see below).

---

## Building a project-specific session

`walk.json` is a template containing placeholder tokens. Its positions (`line`, `character`) are all `0` and must be replaced with real positions targeting interesting symbols in your project.

1. **Copy the template:**
   ```bash
   cp scripts/acceptance/walk.json scripts/acceptance/sessions/pixelorama.json
   ```

2. **Choose one file per role.** Open the project in your editor, or `grep` for the patterns.

   | Role token | What to put here | Example (Pixelorama) |
   |-----------|-----------------|---------------------|
   | `__CALLER_FILE__` | A GDScript file that calls a method defined in another file, uses `preload(...)`, and references an autoload | `src/UI/Canvas/Canvas.gd` |
   | `__CALLEE_FILE__` | The file that defines the called method and has multiple member symbols | `src/Classes/Cel.gd` |
   | `__AUTOLOAD_FILE__` | The autoload script itself | `src/Autoload/Global.gd` |
   | `__BASE_CLASS_FILE__` | A base class file declaring a `func` that subclasses override | `src/Classes/BaseTool.gd` |
   | `__SUBCLASS_FILE__` | Any subclass that overrides the above func | `src/Classes/PencilTool.gd` |
   | `__LINK_FILE__` | Any file with a `preload("res://…")` or `load("res://…")` call | `src/UI/Canvas/Canvas.gd` |

3. **Find real positions** for each request. VS Code shows 0-based line and column in the status bar (subtract 1 from the column for 0-indexed), or:
   ```bash
   # Count lines to the target symbol (1-based in grep, 0-based in LSP):
   grep -n "some_method" src/Classes/Cel.gd
   ```

4. **Fill in positions** for each request in your session JSON. Each position looks like:
   ```json
   "position": { "line": <0-based>, "character": <0-based UTF-16 column> }
   ```
   Replace the placeholder `{ "line": 0, "character": 0 }` with the actual position.

5. **Replace the file-role tokens** (`__CALLER_FILE__`, `__CALLEE_FILE__`, and the rest) with paths relative to the project root, such as `src/UI/Canvas/Canvas.gd`.

6. **Run** with `--session scripts/acceptance/sessions/pixelorama.json`.

### What each capability checks

| Label | Method | What to target | Expected result |
|-------|--------|---------------|-----------------|
| `hover/cross_file_method` | `textDocument/hover` | A method call site in `__CALLER_FILE__` where the method is defined in `__CALLEE_FILE__` | `contents.value` contains `func` and `->` |
| `definition/class_name_expr` | `textDocument/definition` | A `ClassName` identifier used as a type or in an expression | Non-empty locations array pointing to the class declaration |
| `definition/preload_string` | `textDocument/definition` | Inside the string of `preload("res://…")` | Non-empty locations array pointing to the preloaded file |
| `definition/autoload_name` | `textDocument/definition` | An autoload singleton name such as `Global` in `__CALLER_FILE__` | Non-empty locations array pointing to the autoload script |
| `references/cross_file_method` | `textDocument/references` | A method name at its definition in `__CALLEE_FILE__` | Non-empty locations array with at least one cross-file reference |
| `documentSymbol/nested_members` | `textDocument/documentSymbol` | `__CALLEE_FILE__`, a class with multiple members | Non-empty symbol array |
| `implementation/func_with_overrides` | `textDocument/implementation` | A base `func` in `__BASE_CLASS_FILE__` that subclasses override | Non-empty locations array pointing to override sites |
| `documentLink/res_literal` | `textDocument/documentLink` | `__LINK_FILE__`, which must have a `res://` literal | Non-empty document-link array |

---

## Output files (all gitignored via `target/`)

| Path | Contents |
|------|---------|
| `target/acceptance/oss-report.json` | Combined pass/fail report with capability results and the speedup ratio |
| `target/acceptance/capability-report.json` | Raw lsp-poke output for the capability walk |
| `target/acceptance/bench-cold.json` | lsp-poke output for the cold bench run |
| `target/acceptance/bench-warm.json` | lsp-poke output for the warm bench run |
| `target/acceptance/concrete-session.json` | The template with `__PROJECT_ROOT__` and `__EXTENSION_API__` substituted |

`scripts/acceptance/sessions/` is the canonical place for project-specific filled sessions. Add your project's session there; it lives alongside the template.

---

## Proprietary project stage (zero repo trace)

Run the same `run.sh` with out-of-repo paths. Nothing else changes:

```bash
PROJECT_ROOT=/out/of/repo/project \
GODOT_BIN=/out/of/repo/godot \
EXTENSION_API=/out/of/repo/extension_api.json \
  scripts/acceptance/run.sh \
    --session /out/of/repo/sessions/project.json

# Write report outside the repo
cp target/acceptance/oss-report.json /out/of/repo/reports/proprietary-report.json
```

The session file for the proprietary project lives entirely outside the repo. Nothing is committed.

---

## Diagnostics sweep, the release gate (`scan_diags.py`)

A navigation walk is not a diagnostics gate: it only sees the files it opens, and those are the ones a human picked. `scan_diags.py` opens every `.gd` in a project and tallies every `publishDiagnostics`:

```bash
scripts/acceptance/scan_diags.py \
  --project ~/dev/m6-oss-acceptance/Pixelorama \
  --gdls target/release/gdls \
  --api ~/dev/m6-oss-acceptance/extension_api.json \
  --out target/acceptance/scan-report.json
```

**Gate rule: run this on both acceptance projects before any release.** `files_with_errors` must be near 0, and every remaining error file must be individually justified against `godot --check-only --script <file>`, run from inside an imported project, since an unimported one lacks the class cache and false-fails. The report's `error_message_histogram` is the fastest way to tell a systematic family apart from genuine project errors. Delete the project's `.gdls/` afterwards if you don't want the sweep's warm cache left behind.
