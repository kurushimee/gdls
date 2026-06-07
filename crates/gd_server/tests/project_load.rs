//! Regression coverage for `ProjectModel` load-failure surfacing.
//!
//! `ProjectModel::load` used to `.unwrap_or_default()` the `project.godot` read, so a present-but-
//! unreadable file (locked mid-save on Windows, permission-denied, non-UTF-8) silently degraded to
//! empty defaults — and the watcher's `reload_project_and_native` then rebuilt the warning policy
//! from that empty config and republished, with no log. `load_checked` now distinguishes the
//! actionable failure (returns `read_failed = true`, after a `log::warn!`) from a genuinely absent
//! file (the documented standalone-`.gd` degrade, `read_failed = false`), so the reload path can
//! keep the prior policy instead of silently resetting it.
//!
//! Cross-crate by necessity: `gd_project` has no `tempfile` dev-dep, but `ProjectModel` is public
//! and the `gd_server` test rig already provides a throwaway on-disk project.

mod common;

use common::{options_for, TempProject};
use gd_project::{LoadOutcome, ProjectModel};
use gd_server::Workspace;

#[test]
fn load_checked_clean_read_is_loaded() {
    let p = TempProject::new();
    p.write(
        "project.godot",
        "config_version=5\n\n[application]\n\nconfig/name=\"X\"\n",
    );
    let (model, outcome) = ProjectModel::load_checked(&p.root);
    assert_eq!(
        outcome,
        LoadOutcome::Loaded,
        "a readable, well-formed project.godot must load cleanly"
    );
    assert!(!outcome.should_preserve_prior());
    assert_eq!(
        model.config_version, 5,
        "the clean read must actually parse"
    );
}

#[test]
fn load_checked_absent_file_is_not_a_failure() {
    // No project.godot at all: the documented "treat root as res://" degrade for a standalone .gd.
    // This must stay quiet (Absent, not preserve-prior) so a caller never discards a resolved
    // config over a file that was simply never there.
    let p = TempProject::new();
    let (_model, outcome) = ProjectModel::load_checked(&p.root);
    assert_eq!(outcome, LoadOutcome::Absent);
    assert!(
        !outcome.should_preserve_prior(),
        "an absent project.godot is the expected NotFound degrade, not an actionable failure"
    );
}

#[test]
fn load_checked_unreadable_file_is_flagged() {
    // Present but unreadable. Portable trigger: make `project.godot` a DIRECTORY, so
    // `read_to_string` fails with a non-NotFound error on every platform (PermissionDenied on
    // Windows, IsADirectory on Linux). `load_checked` must report `ReadFailed` so the reload path
    // keeps the prior model.
    let p = TempProject::new();
    std::fs::create_dir_all(p.root.join("project.godot").as_std_path())
        .expect("create a directory where project.godot would be");
    let (_model, outcome) = ProjectModel::load_checked(&p.root);
    assert_eq!(outcome, LoadOutcome::ReadFailed);
    assert!(
        outcome.should_preserve_prior(),
        "a present-but-unreadable project.godot must preserve the prior model, not silently \
         degrade to empty defaults"
    );
}

#[test]
fn load_checked_corrupt_but_parseable_is_flagged() {
    // WP-RD13: garbled content the tolerant parser accepts as a near-default "clean" parse. It
    // reads fine (no I/O error) but the confidence signal flags it so the reload path preserves
    // the prior model rather than wiping settings on a save caught mid-write.
    let p = TempProject::new();
    p.write(
        "project.godot",
        "asldkfj\nqwerty zxcv\n%%binary garbage%%\nnot a config at all\nmore junk here\n",
    );
    let (_model, outcome) = ProjectModel::load_checked(&p.root);
    assert_eq!(outcome, LoadOutcome::Corrupt);
    assert!(outcome.should_preserve_prior());
}

/// WP-RD13: reloading over a corrupt `project.godot` must preserve the WHOLE prior state — the
/// project model (autoloads / config), the native DB, and the warning policy — not just the policy.
#[test]
fn reload_over_corrupt_preserves_project_native_and_policy() {
    let p = common::sample_project();
    // sample_project writes a real project.godot + extension_api.json (the native dump).
    let opts = options_for(&p);
    let mut ws = Workspace::load(&p.root, &opts);

    // Capture the loaded-good state.
    let native_before = ws.native.class_count();
    let policy_before = format!("{:?}", ws.policy);
    let root_before = ws.project.root.clone();
    assert!(
        native_before > 0,
        "precondition: the sample project's native DB has classes"
    );

    // Overwrite project.godot with corrupt-but-parseable garbage and reload.
    p.write(
        "project.godot",
        "asldkfj\nqwerty zxcv\n%%binary garbage%%\nnot a config at all\nmore junk here\n",
    );
    ws.reload_project_and_native(&opts);

    assert_eq!(
        ws.native.class_count(),
        native_before,
        "a corrupt reload must NOT wipe the native DB"
    );
    assert_eq!(
        format!("{:?}", ws.policy),
        policy_before,
        "a corrupt reload must NOT reset the warning policy"
    );
    assert_eq!(
        ws.project.root, root_before,
        "a corrupt reload must NOT replace the project model"
    );
}
