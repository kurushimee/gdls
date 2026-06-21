# Vendored editor `ClientCapabilities` profiles (M7 §7.4)

One JSON per editor: the **verbatim `initialize.params.capabilities`** the editor sends, captured
once from the real client and replayed by `tests/editor_profiles.rs` (every gated projection
asserted per profile) and by `scripts/lsp-poke.py` walks (`"capabilities"` session key). This is
the per-milestone six-profile exit bar from `docs/09-phase-2.md` §7.4 — drop a new capture here
and the harness extends itself; assertions derive from each profile's own capability flags.

## Capture procedure

Point the editor's GDScript language server at a capture shim instead of gdls, open any `.gd`
file, and harvest the dump (capture rig from the M7 work, recreate as needed):

1. A ~40-line python stdio shim that answers `initialize` with `{"capabilities": {}}` and writes
   `params` to `$CAPTURE_OUT` — see PR #88's description for the script.
2. Editor config points `gdscript` at the shim (e.g. Helix: `languages.toml` with
   `language-server.capture = { command = "python3", args = ["capture_server.py"] }`).
3. Open `test.gd`, wait a beat, quit. Vendor `capabilities` (sorted keys) as `<editor>.json`.

## Inventory

Captures are **Claude's job** (headless/scriptable drivers — never assigned to the user); the
user's only check is a single end-of-Phase-2 trial in real work.

| Profile | Version captured | Status |
|---|---|---|
| `helix.json` | helix 25.07.1 | ✅ captured 2026-06-13 (WSL snap, pty-driven) |
| `neovim.json` | NVIM 0.12.3 | ✅ captured 2026-06-13 (`nvim --headless -l` + `vim.lsp.start`) |
| `zed.json` | Zed 1.6.3-stable (Windows) | ✅ captured 2026-06-13 (settings-swapped `lsp.<server>.binary` → `wsl.exe` shim; settings restored byte-identical) |
| `claude-code.json` | — | 🔶 armed: gitignored `.claude/.lsp.json` in this repo routes `.gd` → the capture shim (`~/.local/share/gdls-capture/`); the **next CC session** completes it by touching any `.gd` via the LSP tool, then vendors the JSON, revisits the markdown-by-default decision on `crate::docs::ProseFormat`, and removes the armed config. (Config loads at session start only; not hot-reloadable.) |
| `vscode.json` | vscode-languageclient 9.0.1 (VS Code + godot-tools 2.6.1) | ✅ hand-authored 2026-06-22 from `vscode-languageclient` v9 source (`release/9.0.x`): `computeClientCapabilities` in `client.ts` + all `fillClientCapabilities` across the feature modules; framed as "VS Code + godot-tools 2.6.1" (the Godot VS Code extension uses vscode-languageclient as its LSP transport) |
| `eglot.json` | Eglot (GNU Emacs master) | ✅ synthesized 2026-06-22 from `lisp/progmodes/eglot.el` `cl-defgeneric eglot-client-capabilities` (the `:method` default form); runtime-determined fields fixed to the common configured case (yasnippet present → `snippetSupport: true`; markdown-mode → `["markdown","plaintext"]`; `eglot-report-progress` default → `workDoneProgress: true`; local connection → `didChangeWatchedFiles.dynamicRegistration: true`) |
| `sublime.json` | Sublime Text LSP (sublimelsp/LSP main) | ✅ synthesized 2026-06-22 from `plugin/core/sessions.py` `get_initialize_params`; `didChangeWatchedFiles` included (present when a file-watcher implementation is available) |

These three were synthesized from each client's authoritative capability source (not machine-captured)
because the editor binaries are absent on this machine; a `ClientCapabilities` profile is a pure
fixture, so its fidelity comes from the source, not the local install. The walk runs over every
vendored profile. Tracked: #98.
