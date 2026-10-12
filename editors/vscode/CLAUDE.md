# editors/vscode

The extension never bundles or downloads gdls; it runs `gdls.path`, else `gdls` on `PATH`. Keep it that way.

- `settings()` in `src/extension.ts` builds both `initializationOptions` and the `workspace/configuration` reply (through the middleware), so the two always agree. Unset values are left out rather than sent as `null`, since `Vec<String>` fields in `config.rs` reject `null`.
- `RESTART_KEYS` mirrors the session-structural list in `apply_runtime_config` (`crates/gd_server/src/server.rs`), plus the extension's own `path` and `dialect`. Update it when that list changes.
- The warning-name enums in `package.json` are the active codes from `crates/gd_analyze/src/warnings.rs`, without the three deprecated ones.
- The grammar and language configuration are vendored from godot-vscode-plugin (MIT, see `THIRD_PARTY_NOTICES.md`).
- `npm test` runs a real VS Code through `@vscode/test-electron` and needs `gdls` on `PATH`. On this WSL box VS Code lacks `libnss3`, `libnspr4`, and `libasound2` and there is no sudo, so extract them with `apt-get download` plus `dpkg-deb -x` and set `LD_LIBRARY_PATH`.
- `vscode.executeDocumentRenameProvider` mangles the new name when the target file has no open model (VS Code passes it as an array), so tests open the file first.
- Releases: bump `version` in `package.json` and `CHANGELOG.md`, then push tag `vscode-v<version>`. The workflow attaches the `.vsix` to that GitHub release and publishes to the Marketplace and Open VSX only when the `VSCE_PAT` and `OVSX_PAT` secrets exist.
