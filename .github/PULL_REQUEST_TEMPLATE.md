<!-- Thanks for contributing to gdls! Keep PRs scoped and parity-friendly. -->

## What & why

<!-- What does this change, and why? Link any issue, e.g. "Closes #N". -->

## Faithful-port discipline

gdls mirrors Godot 4.6.3-stable's GDScript frontend **function-for-function** and matches its
diagnostics byte-for-byte. See
[CONTRIBUTING.md](https://github.com/kurushimee/gdls/blob/main/CONTRIBUTING.md).

- [ ] Port-crate changes (`gd_syntax` / `gd_types` / `gd_analyze` / `gd_project`) mirror the upstream
      Godot structure (no refactor / modernize / consolidate), and reference the corresponding Godot
      source location.
- [ ] Error/warning **message strings, codes, and source ranges** are unchanged (or the change matches Godot).
- [ ] N/A — this only touches `gd_server` glue, tests, docs, or CI.

## CI gate (run locally — identical to CI)

- [ ] `cargo fmt --all --check`
- [ ] `cargo lint` (clippy, `-D warnings`)
- [ ] `cargo build --workspace --all-targets`
- [ ] `cargo test --workspace`

## Fidelity ratchets

- [ ] Both fidelity ratchets still hold (or I intentionally raised a ratchet and updated the floor file).

## Verification

<!-- Which tests / corpus cases did you run? Any fidelity-ratchet change? -->
