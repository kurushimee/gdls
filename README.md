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

- **From source (cargo)** — builds straight from this repo and installs the `gdls` binary into
  `~/.cargo/bin` (no checkout needed):

  ```sh
  cargo install --git https://github.com/kurushimee/gdls gd_server
  ```

The toolchain is pinned by `rust-toolchain.toml` (stable).

`gdls` speaks JSON-RPC over **stdio**, so a bare `gdls` invocation just waits for an LSP client on
stdin. To smoke-test the binary without wiring up a client, point its index pass at a project — it
exits cleanly and prints a reconcile summary to stderr:

```sh
gdls diagnose --reconcile --root /path/to/your/godot/project
```

## Quick start

1. **Register the server** with your LSP client. For Claude Code, install the official plugin
   from the [`kurushimee/gdls-plugin`](https://github.com/kurushimee/gdls-plugin) marketplace —
   inside a Claude Code session:

   ```
   /plugin marketplace add kurushimee/gdls-plugin
   /plugin install gdls@gdls-plugin
   ```

   For any other LSP client (or a hand-rolled plugin), the core registration is five lines:

   ```json
   {
     "gdscript": {
       "command": "gdls",
       "extensionToLanguage": { ".gd": "gdscript" }
     }
   }
   ```

2. **Native types: nothing to do** (since v1.0.1). gdls finds your Godot binary
   (`godotBinaryPath` option → `GDLS_GODOT` env → `godot4`/`godot` on PATH), runs
   `--dump-extension-api-with-docs` with project context — which is what captures the project's
   GDExtension classes — and manages the result under `.gdls/`, regenerating only when the
   binary or the project's `.gdextension` set changes. Since v1.0.2 the dump runs in the
   background (it never delays a request; the session re-checks open files the moment it lands),
   and when no binary is discoverable at all, a bundled stock 4.6.3 class surface keeps builtins
   (`Node`, `Timer`, …) resolving — without inventing "unknown type" errors for classes only
   your engine build knows. To pin a hand-made dump instead, set
   `initializationOptions.extensionApiPath`; to forbid gdls from ever spawning Godot, set
   `autoDumpExtensionApi: false` (or `GDLS_GODOT=off`) and dump manually from inside the
   project directory:

   ```sh
   godot --dump-extension-api-with-docs
   ```

   Details and the multi-source capture story (incl. `doc_classes` XML fallback) are in
   [`docs/03-indexing-freshness.md`](docs/03-indexing-freshness.md) §1–§2.

## Configuration

The server is configured entirely through LSP `initializationOptions` —
`projectRoot`, the auto-dump pair (`godotBinaryPath`, `autoDumpExtensionApi`),
`extensionApiPath` to pin a manual dump, and the `strict` diagnostics profile
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
**v1.0.4** is the current release — the native-surface completeness release: hover and
`definition` now work on native classes and members (declaration-line hover pinned to Godot's
own LSP detail formats; `definition` jumps into readable API stubs materialized under the user
cache), `workspace/symbol` anchors `class_name` declarations instead of line 0, the analyzer
restores upstream's class→native fall-through so int-typed native surfaces type faithfully, and
`UNSAFE_PROPERTY_ACCESS` — the one deliberately-deferred warning — now fires, provenance-gated
so it never false-positives on incomplete class surfaces. Before it, **v1.0.3** made the full
Godot warning set actually fire (19 silent codes ported function-for-function) with exact
`@warning_ignore` spans, **v1.0.2** made the first run robust (background `extension_api.json`
auto-dump, embedded stock 4.6.3 fallback), and **v1.0.1** fixed the cross-file false-positive
families a full-project diagnostics sweep exposed right after v1.0.0.
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
