//! Guard: keep the `fuzz/` cargo-fuzz crate OUT of the gdls workspace.
//!
//! libFuzzer needs the nightly toolchain and is unsupported on Windows by cargo-fuzz, so the
//! stable `--workspace` CI (`cargo build/clippy/test --workspace`) must never try to compile it. If
//! someone adds `fuzz` to `[workspace] members` (or drops the parent `exclude = ["fuzz"]` and the
//! fuzz crate's own `[workspace]` table), the fuzz package would appear in this workspace's metadata
//! and this test fails — a fast tripwire instead of a confusing CI compile error.

use std::process::Command;

#[test]
fn fuzz_crate_is_not_a_workspace_member() {
    let out = Command::new(env!("CARGO"))
        .args(["metadata", "--no-deps", "--format-version", "1"])
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .expect("failed to run `cargo metadata`");
    assert!(
        out.status.success(),
        "cargo metadata failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    // `--no-deps` reports only workspace members; the fuzz crate's package name must be absent.
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        !stdout.contains("gdls-fuzz"),
        "the `fuzz` crate was pulled into the workspace — keep it isolated; it needs nightly + \
         libFuzzer, which break the stable `--workspace` CI"
    );
}
