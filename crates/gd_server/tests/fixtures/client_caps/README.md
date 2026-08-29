# Vendored editor `ClientCapabilities` profiles

One JSON per editor, holding the verbatim `initialize.params.capabilities` that editor sends. They are replayed by `tests/editor_profiles.rs`, which asserts every gated projection per profile, and by `scripts/lsp-poke.py` walks through the `"capabilities"` session key.

Every gdls capability gates on an exact `ClientCapabilities` path (`docs/09-lsp-conventions.md` §7.1), and these profiles are what prove those gates against real clients rather than a hand-written approximation. Drop a new capture here and the harness extends itself, since assertions derive from each profile's own capability flags.

## Capture procedure

Point the editor's GDScript language server at a capture shim instead of gdls, open any `.gd` file, and harvest the dump.

1. A roughly 40-line python stdio shim that answers `initialize` with `{"capabilities": {}}` and writes `params` to `$CAPTURE_OUT`.
2. Point the editor config's `gdscript` entry at the shim. For Helix that is `languages.toml` with `language-server.capture = { command = "python3", args = ["capture_server.py"] }`.
3. Open `test.gd`, wait a beat, quit. Vendor `capabilities` with sorted keys as `<editor>.json`.

## Inventory

| Profile | Version | Source |
|---|---|---|
| `helix.json` | helix 25.07.1 | Machine-captured (WSL snap, pty-driven) |
| `neovim.json` | NVIM 0.12.3 | Machine-captured (`nvim --headless -l` plus `vim.lsp.start`) |
| `zed.json` | Zed 1.6.3-stable (Windows) | Machine-captured (settings-swapped `lsp.<server>.binary` to a `wsl.exe` shim; settings restored byte-identical) |
| `vscode.json` | vscode-languageclient 9.0.1 (VS Code plus godot-tools 2.6.1) | Derived from `vscode-languageclient` v9 source (`release/9.0.x`): `computeClientCapabilities` in `client.ts` plus every `fillClientCapabilities` across the feature modules. Framed as "VS Code plus godot-tools 2.6.1", since the Godot VS Code extension uses vscode-languageclient as its LSP transport |
| `eglot.json` | Eglot (GNU Emacs master) | Derived from `lisp/progmodes/eglot.el` `cl-defgeneric eglot-client-capabilities`, the `:method` default form. Runtime-determined fields are fixed to the common configured case: yasnippet present gives `snippetSupport: true`, markdown-mode gives `["markdown","plaintext"]`, the `eglot-report-progress` default gives `workDoneProgress: true`, and a local connection gives `didChangeWatchedFiles.dynamicRegistration: true` |
| `sublime.json` | Sublime Text LSP (sublimelsp/LSP main) | Derived from `plugin/core/sessions.py` `get_initialize_params`. `didChangeWatchedFiles` is included, since it is present when a file-watcher implementation is available |

The last three were derived from each client's authoritative capability source rather than machine-captured, because those editor binaries are absent on this machine. A `ClientCapabilities` profile is a pure fixture, so its fidelity comes from the source, not from a local install. The walk runs over every vendored profile either way.

`claude-code.json` is not captured. Claude Code's LSP config loads at session start and is not hot-reloadable, so capturing it means arming a `.lsp.json` that routes `.gd` to the capture shim, touching any `.gd` through the LSP tool in a fresh session, then vendoring the JSON and removing the armed config.
