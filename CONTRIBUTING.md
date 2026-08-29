# Contributing to gdls

`gdls` is a standalone GDScript language server built as a faithful Rust port of the GDScript frontend (tokenizer, parser, analyzer) from Godot 4.6.3-stable.

Please read this guide before opening a pull request. The faithful-port rule below is what makes gdls match Godot's diagnostics exactly, and it changes how contributions get reviewed compared to a normal greenfield Rust project.

## Faithful-port discipline

The port crates (`gd_syntax`, `gd_types`, `gd_analyze`, `gd_project`) mirror Godot's C++ frontend function for function. In these crates, fidelity beats idiom:

- Preserve Godot's function decomposition and control flow, so future upstream changes stay applicable as diffs. Do not improve, refactor, modernize, or consolidate the algorithms, even where the Rust-idiomatic version would be cleaner.
- Message strings, warning codes, and source ranges must match Godot's output byte for byte. These are checked against a golden corpus (see *Fidelity* below).
- Keep enums dense and in Godot's declaration order. Several of them (`TokenKind`, for one) are `#[repr(u8)]` indices into parallel tables, guarded by `const _: () = assert!(...)` checks.
- Rename a symbol only when a Rust keyword collides (`SELF` → `SelfKw`, `TK_CONST` → `Const`), and say why in a comment.

The source of truth is the Godot source, not the design docs. Port against a local checkout of official Godot (`godotengine/godot`) at tag `4.6.3-stable`. Derive every enum, count, and message template mechanically from that source. Never hard-code one from memory or from the docs. Where a doc cites a concrete number it may have drifted, and the source wins. Grep to confirm at port time.

The looser Rust conventions further down apply mainly to the `gd_server` LSP glue and to tests.

## Development setup

You need a Rust toolchain. It is pinned by [`rust-toolchain.toml`](rust-toolchain.toml) (`stable` plus `rustfmt` and `clippy`) and `rustup` installs it on first build.

For fidelity work and the local differential-oracle tooling, a local Godot 4.6.3-stable checkout and the `godot` binary are handy (to regenerate `extension_api.json` and to diff against the engine's own output). Neither is required to build or test gdls.

## The dev loop is the CI gate

CI ([`.github/workflows/ci.yml`](.github/workflows/ci.yml)) runs exactly these on every push and PR, on Linux and Windows. Run them locally before opening a PR:

```sh
cargo fmt --all --check                          # format gate (CI fails on any diff)
cargo lint                                        # alias for: clippy --workspace --all-targets -- -D warnings
cargo build --workspace --all-targets
cargo test --workspace
```

CI denies all warnings (`RUSTFLAGS: -D warnings`), so be clippy-clean on the first write. Use the `cargo lint` alias (defined in [`.cargo/config.toml`](.cargo/config.toml)), not bare `cargo clippy`.

To run a single test: `cargo test -p gd_syntax simple_var_declaration` (name substring), or a whole integration file: `cargo test -p gd_server --test lifecycle`. Add `-- --nocapture` for output.

To run the binary: `cargo run --bin gdls` (debug) or `target/release/gdls`. It speaks JSON-RPC over stdio, so a bare invocation just waits for a client.

## Fidelity and the conformance corpus

"Match Godot exactly" is measured against Godot's own golden corpus (`.gd` input plus `.out` expected output), vendored under `crates/*/tests/conformance/`. Two ratchets are enforced in CI and currently hold at 1.0000: parser 186/186 and analyzer 300/300.

A change that lowers a ratchet fails CI. If you are deliberately adding corpus cases, raise the ratchet to match.

Five `cargo-fuzz` targets (`parse`, `analyze`, `index_invariants`, `complete_context`, `scene_parse`) run on nightly Linux. Any panic blocks a release: the parser must always return a partial AST, and position conversions must clamp rather than panic.

[`docs/06-testing-fidelity.md`](docs/06-testing-fidelity.md) covers how the corpus is curated and how fidelity is scored.

## Rust conventions (mostly for `gd_server` glue and tests)

- Dependencies are pinned once in the root `[workspace.dependencies]` and referenced as `<crate>.workspace = true` from members. Use `cargo add` to introduce one, `--dev` for dev-only.
- Error types split by layer: binaries and LSP glue (`gd_server`) use `anyhow::Result` with `.context(...)`; library crates that expose an error type (`gd_types`) use `thiserror`. Don't mix the two in one crate's API.
- `unwrap()` is fine in tests. In production prefer `?`, or `.expect("invariant: <why this can't fail>")` so a stray panic documents the broken assumption.
- Put `#[must_use]` on returns callers must not silently discard.
- stdout is the LSP wire; stderr is logs. All logging goes to stderr (`tracing`, filtered by `GDLS_LOG`). Never write to stdout except LSP protocol, since a stray `println!` corrupts the JSON-RPC stream.

## Architecture orientation

Start with [`docs/00-overview.md`](docs/00-overview.md) for the problem and the design decisions, then [`docs/01-architecture.md`](docs/01-architecture.md) for the crate DAG. The crates layer strictly:

```
gd_syntax → gd_types → { gd_analyze, gd_project } → gd_server (bin: gdls)
```

[`CLAUDE.md`](CLAUDE.md) is the condensed convention reference and is kept in sync with this guide.

## Pull requests

- Keep changes scoped and parity-friendly. In port crates, prefer a diff that mirrors an upstream change over a from-scratch rewrite.
- Reference the relevant `docs/` section, and for port changes the corresponding Godot source location.
- Make sure the full CI gate above passes locally first.
- Say what you verified: which tests, which corpus cases, any fidelity-ratchet change.

## License

By contributing, you agree that your contributions will be licensed under the project's [MIT License](LICENSE).
