# gdls, a standalone GDScript language server for Godot 4.6 and 4.7

[![CI](https://github.com/kurushimee/gdls/actions/workflows/ci.yml/badge.svg)](https://github.com/kurushimee/gdls/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)
[![Rust](https://img.shields.io/badge/rust-stable-dea584.svg?logo=rust)](rust-toolchain.toml)
[![Godot conformance](https://img.shields.io/badge/Godot%20conformance-parser%201.0000%20%7C%20analyzer%201.0000-brightgreen.svg)](docs/06-testing-fidelity.md)

One binary that gives Claude Code (or any LSP client) type-aware GDScript diagnostics and navigation over stdio, with no Godot engine or editor running.

`gdls` is a faithful Rust port of the GDScript frontend (tokenizer, parser, analyzer). It exists because the editor's built-in LSP is heavy, goes stale, and needs the engine running, which hurts at 3,000 to 10,000+ `.gd` files. Only the frontend is ported. The compiler, bytecode, and VM half is out of scope, so this is diagnostics and navigation only.

One binary serves both Godot 4.6 and 4.7. Each project is read as the release it targets, taken from `project.godot`, so upgrading gdls does not change what a 4.6 project sees.

## Install

`gdls` does not ship with Claude Code. Grab a release binary or build one, then put it on `PATH`.

Prebuilt binaries: download `gdls` (Linux x86_64) or `gdls.exe` (Windows x86_64) from [GitHub Releases](https://github.com/kurushimee/gdls/releases).

From source, straight from this repo, into `~/.cargo/bin` (no checkout needed):

```sh
cargo install --git https://github.com/kurushimee/gdls gd_server
```

The toolchain is pinned by `rust-toolchain.toml` (stable).

`gdls` speaks JSON-RPC over stdio, so running it bare just waits for a client. To smoke-test the binary without wiring up an editor, point its index pass at a project. It prints a reconcile summary to stderr and exits cleanly:

```sh
gdls diagnose --reconcile --root /path/to/your/godot/project
```

## Quick start

Register the server with your LSP client. For Claude Code, install the plugin from the [`kurushimee/gdls-plugin`](https://github.com/kurushimee/gdls-plugin) marketplace, inside a session:

```
/plugin marketplace add kurushimee/gdls-plugin
/plugin install gdls@gdls-plugin
```

For any other client, the core registration is five lines:

```json
{
  "gdscript": {
    "command": "gdls",
    "extensionToLanguage": { ".gd": "gdscript" }
  }
}
```

Native types need no setup. gdls finds your Godot binary (`godotBinaryPath` option, then the `GDLS_GODOT` env var, then `godot4`/`godot` on `PATH`), runs `--dump-extension-api-with-docs` with project context so the project's GDExtension classes are captured, and keeps the result under `.gdls/`, regenerating only when the binary or the project's `.gdextension` set changes. The dump runs in the background, so it never delays a request; the session re-checks open files as soon as it lands. If no binary is discoverable, a bundled stock class surface for your project's own Godot release keeps builtins like `Node` and `Timer` resolving, and gdls will not invent "unknown type" errors for classes only your engine build knows about.

To pin a hand-made dump instead, set `initializationOptions.extensionApiPath`. To stop gdls from ever spawning Godot, set `autoDumpExtensionApi: false` (or `GDLS_GODOT=off`) and dump manually from inside the project directory:

```sh
godot --dump-extension-api-with-docs
```

[`docs/03-indexing-freshness.md`](docs/03-indexing-freshness.md) §1 and §2 cover the details, including the `doc_classes` XML fallback.

## What it serves

Diagnostics (push and pull), hover, definition, declaration, type definition, references, implementation, call hierarchy, type hierarchy, document and workspace symbols, completion, signature help, rename, document highlight, semantic tokens, inlay hints, document colors, code actions, folding and selection ranges, document links, and file-rename edits. Any editor gets the whole set with no gdls-specific client code and none of the Godot editor LSP's custom protocol. The full surface is in [`docs/05-lsp-cc-integration.md`](docs/05-lsp-cc-integration.md), and the capabilities gdls deliberately does not serve, with reasons, are in [`docs/09-lsp-conventions.md`](docs/09-lsp-conventions.md) §5.

Against Godot's own vendored corpus, both ratchets sit at 1.0000 — the parser at 185/185 and the analyzer at 196/196, with empty known-failures lists on either side.

The latest release is v3.0.0. [`CHANGELOG.md`](CHANGELOG.md) has the release history, and [`docs/08-history.md`](docs/08-history.md) has how the project was built.

## Configuration

Everything is configured through LSP `initializationOptions`, and everything has a working default: `projectRoot`, the auto-dump pair (`godotBinaryPath`, `autoDumpExtensionApi`), `extensionApiPath` to pin a manual dump, the `strict` diagnostics profile (`godot`, `strict`, or `off`) with per-warning overrides, completion and inlay-hint toggles, and the `formatter` command that bridges to gdformat or any other external formatter. Settings can also change mid-session through `workspace/configuration`. The full schema and a worked manifest are in [`docs/05-lsp-cc-integration.md`](docs/05-lsp-cc-integration.md) §3.

## Architecture

[`docs/00-overview.md`](docs/00-overview.md) has the problem statement and the design decisions. [`docs/01-architecture.md`](docs/01-architecture.md) has the components, the control loops, and the crate layering (`gd_syntax` → `gd_types` → `gd_analyze` / `gd_project` → `gd_server`).

## Contributing

Contributions are welcome. See [`CONTRIBUTING.md`](CONTRIBUTING.md), and read the faithful-port rule first: `gdls` mirrors Godot's frontend function for function and matches its diagnostics byte for byte, so fidelity to the upstream source is reviewed ahead of Rust idiom. The dev loop is the CI gate: `cargo fmt --all --check`, `cargo lint`, `cargo build`, `cargo test`.

## License

`gdls` is released under the [MIT License](LICENSE).

It is a faithful port of the GDScript frontend of [Godot Engine](https://github.com/godotengine/godot), which is also MIT-licensed. Substantial portions of this software are derived from Godot's source, and the Godot Engine copyright notice is retained in [`LICENSE`](LICENSE) as that license requires.
