# gdls — a standalone GDScript language server for Godot 4.6.3-stable

[![CI](https://github.com/kurushimee/gdls/actions/workflows/ci.yml/badge.svg)](https://github.com/kurushimee/gdls/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)
[![Rust](https://img.shields.io/badge/rust-stable-dea584.svg?logo=rust)](rust-toolchain.toml)
[![Godot conformance](https://img.shields.io/badge/Godot%20conformance-1.0000-brightgreen.svg)](docs/06-testing-fidelity.md)

A single self-contained language server providing **type-aware GDScript diagnostics and
navigation** to Claude Code (and any LSP client) over stdio — **with no Godot engine or editor
process running at runtime**.

`gdls` is a faithful Rust port of the GDScript frontend (tokenizer → parser → analyzer) of
Godot 4.6.3-stable. It exists to fix the editor LSP's weight, staleness, and engine coupling at
the 3,000–10,000+ `.gd` scale. Only the frontend is ported — the compiler/bytecode/VM half is out
of scope (diagnostics only).

## Install

`gdls` is not shipped by Claude Code; you grab a release binary (or build one) and put it on `PATH`.

- **Prebuilt binaries** — download `gdls` (Linux x86_64) or `gdls.exe` (Windows x86_64) from
  [GitHub Releases](https://github.com/kurushimee/gdls/releases).

- **From source (cargo)** — installs the `gdls` binary into `~/.cargo/bin`:

  ```sh
  cargo install --path crates/gd_server
  ```

- **From source (build in place)** — produces `target/release/gdls`:

  ```sh
  cargo build --release --bin gdls
  ```

The toolchain is pinned by `rust-toolchain.toml` (stable).

`gdls` speaks JSON-RPC over **stdio**, so a bare `gdls` invocation just waits for an LSP client on
stdin. To smoke-test the binary without wiring up a client, point its index pass at a project — it
exits cleanly and prints a reconcile summary to stderr:

```sh
gdls diagnose --reconcile --root /path/to/your/godot/project
```

## Quick start

1. **Register the server** with your LSP client. For Claude Code, drop a plugin manifest
   ([`examples/.lsp.json`](examples/.lsp.json)) at the plugin root — the core is five lines:

   ```json
   {
     "gdscript": {
       "command": "gdls",
       "extensionToLanguage": { ".gd": "gdscript" }
     }
   }
   ```

2. **Generate `extension_api.json`** once per engine rebuild so native and installed
   GDExtension classes resolve. Run the `godot` binary **from inside your project directory** (so
   the dump captures the project's GDExtensions), with docs included for hover prose:

   ```sh
   godot --dump-extension-api-with-docs
   ```

   Point `initializationOptions.extensionApiPath` at the resulting file. Details and the
   multi-source capture story (incl. `doc_classes` XML fallback) are in
   [`docs/03-indexing-freshness.md`](docs/03-indexing-freshness.md) §1–§2.

## Configuration

The server is configured entirely through LSP `initializationOptions` —
`projectRoot`, `extensionApiPath`, and the `strict` diagnostics profile
(`godot` / `strict` / `off`) plus per-warning overrides. The full schema and a worked manifest are
in [`docs/05-lsp-cc-integration.md`](docs/05-lsp-cc-integration.md) §3.

## Architecture

- **Problem, goal, and locked decisions** — [`docs/00-overview.md`](docs/00-overview.md).
- **Components, control loops, and the crate DAG** (`gd_syntax` → `gd_types` →
  `gd_analyze` / `gd_project` → `gd_server`) — [`docs/01-architecture.md`](docs/01-architecture.md).

## Status

**Phase 1 = M0–M6 = v1. Complete.** Both fidelity ratchets are at **1.0000** (parser 186/186, analyzer
300/300) against the vendored Godot 4.6.3-stable conformance corpus. **M6** closed the exposed-capability
parity gaps vs Godot's own LSP (hover member signatures, `definition`/`documentLink` on
`class_name`/`preload`/autoloads, project-wide `references`, hierarchical `documentSymbol`,
`implementation` overrides, autoload-singleton typing) and added a persistent, multi-instance-safe
warm-start index cache (a warm relaunch is **>5×** faster than a cold scan). Verified by capability
walks against a real Godot 4.6.3 OSS project and a Windows-native 2,338-script production project.
**v1.0.0** is the first tagged release.
See [`docs/08-m6-v1-ship.md`](docs/08-m6-v1-ship.md) for the M6 scope and
[`CHANGELOG.md`](CHANGELOG.md) for the milestone history.

Phase 2 (post-v1): `.tscn` node typing for `$`/`%`, `signatureHelp`, `completion`, and `rename` /
`documentHighlight`. (The persistent warm-start index cache was pulled forward into **M6** — it now
gates v1.) Roadmap in [`docs/07-milestones-risks.md`](docs/07-milestones-risks.md).

## Contributing

Contributions are welcome — see [`CONTRIBUTING.md`](CONTRIBUTING.md). The one thing to read first is the
*faithful-port discipline*: `gdls` mirrors Godot's frontend function-for-function and matches its
diagnostics byte-for-byte, so fidelity to the upstream source is reviewed ahead of Rust idiom. The dev
loop is the CI gate (`cargo fmt --all --check`, `cargo lint`, `cargo build`, `cargo test`).

## License

`gdls` is released under the [MIT License](LICENSE).

It is a faithful port of the GDScript frontend of [Godot Engine](https://github.com/godotengine/godot),
which is also MIT-licensed; substantial portions of this software are derived from Godot's source, and the
Godot Engine copyright notice is retained in [`LICENSE`](LICENSE) as that license requires.
