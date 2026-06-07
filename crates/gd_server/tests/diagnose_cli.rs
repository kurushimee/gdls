//! M4 (WP-T3) `gdls diagnose --reconcile` subcommand integration tests.
//!
//! The CLI is documented as the "post-suspend / remote-FS recovery" tool. These tests guard
//! the contract that:
//!   1. Stdout is the LSP wire (any byte to stdout corrupts JSON-RPC); the summary line goes
//!      to stderr exclusively.
//!   2. No-args / wrong-args path exits with code 2 and prints usage to stderr.
//!   3. Successful reconcile against a clean project exits 0.
//!   4. Reconcile that hits walk errors exits nonzero so wrapper scripts can detect
//!      "found nothing because the walk wasn't authoritative" vs "really clean".
//!   5. `--path-audit` runs `Index::verify()` and exits nonzero on any identity-invariant
//!      violation; on a clean index it prints `path-audit: OK` and exits 0.

mod common;

use std::process::Command;

use common::{sample_project, MINI_API};

fn gdls_bin() -> &'static str {
    env!("CARGO_BIN_EXE_gdls")
}

#[test]
fn diagnose_no_args_exits_two_with_usage_on_stderr() {
    let out = Command::new(gdls_bin())
        .arg("diagnose")
        .output()
        .expect("spawn gdls binary");
    assert_eq!(
        out.status.code(),
        Some(2),
        "no-args diagnose must exit code 2"
    );
    assert!(
        out.stdout.is_empty(),
        "stdout MUST be empty in the no-args path — it's the LSP wire; stdout bytes corrupt \
         JSON-RPC clients. Got: {:?}",
        String::from_utf8_lossy(&out.stdout)
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("usage: gdls diagnose"),
        "stderr should contain the usage line; got: {stderr}"
    );
}

#[test]
fn diagnose_reconcile_clean_project_exits_zero() {
    let project = sample_project();
    let out = Command::new(gdls_bin())
        .args(["diagnose", "--reconcile", "--root", project.root.as_str()])
        .output()
        .expect("spawn gdls binary");
    assert_eq!(
        out.status.code(),
        Some(0),
        "clean reconcile must exit 0; stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("cold_index_reconciled"),
        "stderr should contain the marker summary line; got: {stderr}"
    );
    assert!(
        stderr.contains("walk_errors=0"),
        "clean reconcile should report walk_errors=0; got: {stderr}"
    );
    assert!(
        out.stdout.is_empty(),
        "stdout MUST be empty in the success path. Got: {:?}",
        String::from_utf8_lossy(&out.stdout)
    );
}

#[test]
fn diagnose_indexes_extension_api() {
    // Sanity-check that the workspace is loaded with the native DB before reconciling. A
    // regression that loaded an empty NativeDb would show 0 native classes in the load
    // line.
    let project = sample_project();
    // Overwrite with a known-good API so we can assert on the class count.
    project.write("extension_api.json", MINI_API);

    let out = Command::new(gdls_bin())
        .args(["diagnose", "--reconcile", "--root", project.root.as_str()])
        .output()
        .expect("spawn gdls binary");
    assert_eq!(out.status.code(), Some(0));
    let stderr = String::from_utf8_lossy(&out.stderr);
    // The sample project ships hero.gd + enemy.gd → 2 scripts.
    assert!(
        stderr.contains("loaded 2 script"),
        "should report 2 scripts loaded; got: {stderr}"
    );
}

#[test]
fn diagnose_reconcile_nonzero_exit_on_unreadable_file() {
    // Reconcile contract: a reconcile that walked a `.gd` it then
    // couldn't read must exit nonzero so wrapper scripts distinguish "couldn't read the tree" from
    // "really clean". Provoke `skipped_unreadable` with a `.gd` whose bytes are not valid UTF-8
    // (the cross-platform way to make `read_to_string` fail without fiddling file permissions).
    let project = sample_project();
    let bad = project.root.join("src/bad.gd");
    std::fs::write(bad.as_std_path(), [0xFFu8, 0xFE, 0x00, 0x9C]).expect("write invalid-UTF-8 .gd");

    let out = Command::new(gdls_bin())
        .args(["diagnose", "--reconcile", "--root", project.root.as_str()])
        .output()
        .expect("spawn gdls binary");

    assert_eq!(
        out.status.code(),
        Some(1),
        "reconcile with an unreadable .gd must exit 1; stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("skipped_unreadable=1"),
        "stderr should report exactly one unreadable file; got: {stderr}"
    );
    assert!(
        out.stdout.is_empty(),
        "stdout MUST stay empty (it's the LSP wire). Got: {:?}",
        String::from_utf8_lossy(&out.stdout)
    );
}

/// WP-RD4 (Windows): a reconcile whose walk hits an unreadable SUBDIR (the `walk_errors` path, not
/// the `skipped_unreadable` per-file path covered above) must exit nonzero AND report a nonzero
/// `walk_errors` in the summary, so the data-safety removal-pass skip is observable at the CLI.
/// `icacls /deny` on a subdir induces the walk error; skips if the deny is bypassed (admin /
/// backup-privilege environment where DENY ACEs are ignored).
#[cfg(windows)]
#[test]
fn diagnose_reconcile_nonzero_exit_on_walk_error_subdir() {
    let project = sample_project();
    project.write("locked/secret.gd", "class_name Secret\nextends Node\n");
    let locked = project.root.join("locked");

    let denied = Command::new("icacls")
        .args([locked.as_str(), "/inheritance:r", "/deny", "Everyone:(RX)"])
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if !denied || std::fs::read_dir(locked.as_std_path()).is_ok() {
        let _ = Command::new("icacls")
            .args([locked.as_str(), "/reset"])
            .status();
        eprintln!(
            "skipping: could not deny read on the subdir (admin/backup-privilege environment)"
        );
        return;
    }

    let out = Command::new(gdls_bin())
        .args(["diagnose", "--reconcile", "--root", project.root.as_str()])
        .output()
        .expect("spawn gdls binary");
    let _ = Command::new("icacls")
        .args([locked.as_str(), "/reset"])
        .status();

    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_ne!(
        out.status.code(),
        Some(0),
        "a reconcile that hit a walk error must exit nonzero; stderr: {stderr}"
    );
    assert!(
        stderr.contains("cold_index_reconciled") && !stderr.contains("walk_errors=0\n"),
        "the reconcile summary must report a nonzero walk_errors count; got: {stderr}"
    );
    assert!(
        out.stdout.is_empty(),
        "stdout MUST stay empty (it's the LSP wire). Got: {:?}",
        String::from_utf8_lossy(&out.stdout)
    );
}

#[test]
fn diagnose_path_audit_clean_project_exits_zero() {
    // `--path-audit` runs `Index::verify()` and reports identity health. A
    // freshly loaded clean project must pass and exit 0.
    let project = sample_project();
    let out = Command::new(gdls_bin())
        .args(["diagnose", "--path-audit", "--root", project.root.as_str()])
        .output()
        .expect("spawn gdls binary");

    assert_eq!(
        out.status.code(),
        Some(0),
        "clean path-audit must exit 0; stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("path-audit: OK"),
        "stderr should report a clean audit; got: {stderr}"
    );
    assert!(
        out.stdout.is_empty(),
        "stdout MUST stay empty (it's the LSP wire). Got: {:?}",
        String::from_utf8_lossy(&out.stdout)
    );
}
