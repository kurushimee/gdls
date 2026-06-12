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

| Profile | Version captured | Status |
|---|---|---|
| `helix.json` | helix 25.07.1 | ✅ captured 2026-06-13 (WSL, snap) |
| `vscode.json` | — | ⬜ capture during the M7 exit walk |
| `neovim.json` | — | ⬜ capture during the M7 exit walk (≥0.11 built-in client) |
| `zed.json` | — | ⬜ capture during the M7 exit walk |
| `eglot.json` | — | ⬜ capture during the M7 exit walk (≥1.20) |
| `sublime.json` | — | ⬜ capture during the M7 exit walk (Sublime LSP) |
| `claude-code.json` | — | ⬜ capture — decides the absent-`contentFormat` hover default (`crate::docs::ProseFormat` doc) |

The remaining captures need the editors' real environments (most live Windows-side here); they
are part of the interactive milestone-exit walk, not CI. When `claude-code.json` lands, revisit
the markdown-by-default decision documented on `ProseFormat`.
