# Contributing to gdls

Thanks for your interest in `gdls` — a standalone GDScript language server built as a **faithful Rust
port of the GDScript frontend (tokenizer → parser → analyzer) of Godot 4.6.3-stable**.

Please read this guide before opening a pull request. The most important thing to understand up front is
the *faithful-port discipline* (below): it is what makes `gdls` match Godot's diagnostics exactly, and it
changes how contributions are reviewed compared to a typical greenfield Rust project.

## The prime directive: faithful-port discipline

The port crates (`gd_syntax`, `gd_types`, `gd_analyze`, `gd_project`) mirror Godot's C++ frontend
**function-for-function**. In these crates, **fidelity beats idiom**:

- **Mirror upstream structure.** Preserve Godot's function decomposition and control flow so that future
  upstream changes remain re-applicable as diffs. Do **not** "improve," refactor, modernize, or
  consolidate the algorithms — even when the Rust-idiomatic version would be cleaner.
- **Preserve exact error/warning behavior.** Message strings, warning codes, and source ranges must match
  Godot's output byte-for-byte. These are verified against a golden corpus (see *Fidelity* below).
- **Keep enums dense and in Godot's declaration order.** Several enums (e.g. `TokenKind`) are
  `#[repr(u8)]` indices into parallel tables and are guarded by `const _: () = assert!(...)` checks.
- **Rename only when forced.** A symbol is renamed from upstream only when a Rust keyword collides
  (`SELF` → `SelfKw`, `TK_CONST` → `Const`); document why in a comment.

**The source of truth is the Godot source, not the design docs.** Port against a local checkout of
official Godot (`godotengine/godot`) at tag **`4.6.3-stable`**. Derive every enum, count, and message
template *mechanically from the source* — never hard-code them from memory or from the docs (some doc
numbers are stale; the source wins). Grep to confirm at port time.

The looser Rust-idiomatic conventions below apply mainly to the `gd_server` LSP glue and to tests.

## Development setup

You need a Rust toolchain; it is pinned by [`rust-toolchain.toml`](rust-toolchain.toml) (`stable` +
`rustfmt` + `clippy`) and installed automatically by `rustup` on first build.

Optionally, for fidelity work and the local differential-oracle tooling, a local Godot **4.6.3-stable**
checkout and/or the `godot` binary are useful (to regenerate `extension_api.json` and to diff against the
engine's own output), but neither is required to build or test `gdls`.

## The dev loop = the CI gate

CI ([`.github/workflows/ci.yml`](.github/workflows/ci.yml)) runs exactly these on every push and PR, on
Linux and Windows. Run them locally before opening a PR:

```sh
cargo fmt --all --check                          # format gate (CI fails on any diff)
cargo lint                                        # alias for: clippy --workspace --all-targets -- -D warnings
cargo build --workspace --all-targets
cargo test --workspace
```

- **CI denies all warnings** (`RUSTFLAGS: -D warnings`). Be clippy-clean on the first write; use the
  `cargo lint` alias (defined in [`.cargo/config.toml`](.cargo/config.toml)), not bare `cargo clippy`.
- **Run a single test:** `cargo test -p gd_syntax simple_var_declaration` (name substring), or a whole
  integration file: `cargo test -p gd_server --test lifecycle`. Add `-- --nocapture` for output.
- **Run the binary:** `cargo run --bin gdls` (debug) or `target/release/gdls`. It speaks JSON-RPC over
  stdio, so a bare invocation just waits for an LSP client.

## Fidelity and the conformance corpus

"Match Godot exactly" is measured against Godot's own golden corpus (`.gd` input + `.out` expected
output), vendored under `crates/*/tests/conformance/`. Two ratchets are enforced in CI and currently hold
at **1.0000**: parser **186/186** and analyzer **300/300**.

- A change that lowers a ratchet will fail CI. If you are intentionally adding corpus cases, raise the
  ratchet to match.
- The tokenizer and parser are also fuzzed (`cargo-fuzz`, nightly + Linux). **Any panic is a release
  blocker** — the parser must always return a (possibly partial) AST, and position conversions must clamp
  rather than panic.

See [`docs/06-testing-fidelity.md`](docs/06-testing-fidelity.md) for how the corpus is curated and how
fidelity is scored.

## Rust conventions (mostly for `gd_server` glue and tests)

- **Dependencies** are pinned once in the root `[workspace.dependencies]` and referenced as
  `<crate>.workspace = true` from members (`cargo add` to introduce; `--dev` for dev-only).
- **Error-type split:** binaries / LSP glue (`gd_server`) use `anyhow::Result` + `.context(...)`; library
  crates that expose an error type (`gd_types`) use `thiserror`. Don't mix the two in one crate's API.
- **`.unwrap()` vs `.expect("invariant: …")`:** `unwrap()` is fine in tests; in production prefer `?`, or
  `.expect("invariant: <why this can't fail>")` so a stray panic documents the broken assumption.
- **`#[must_use]`** on returns callers must not silently discard.
- **`stdout` is the LSP wire; `stderr` is logs.** All logging goes to stderr (`tracing`; `GDLS_LOG`
  filter). Never write to stdout except LSP protocol — a stray `println!` corrupts the JSON-RPC stream.

## Architecture orientation

Start with [`docs/00-overview.md`](docs/00-overview.md) (problem + locked decisions) and
[`docs/01-architecture.md`](docs/01-architecture.md) (the crate DAG). The crates form a strict layering:

```
gd_syntax → gd_types → { gd_analyze, gd_project } → gd_server (bin: gdls)
```

[`CLAUDE.md`](CLAUDE.md) is the condensed convention reference and is kept in sync with this guide.

## Pull requests

- Keep changes scoped and structural-parity-friendly; in port crates, prefer a diff that mirrors an
  upstream change over a from-scratch rewrite.
- Reference the relevant `docs/0x` section and, for port changes, the corresponding Godot source location.
- Ensure the full CI gate above passes locally first.
- Describe what you verified (which tests, which corpus cases, any fidelity-ratchet change).

## License

By contributing, you agree that your contributions will be licensed under the project's
[MIT License](LICENSE).
