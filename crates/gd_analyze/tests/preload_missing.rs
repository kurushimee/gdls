//! #555 — `Preload file "X" does not exist.` (`gdscript_analyzer.cpp:4744-4750`).
//!
//! The analyzer never touches a filesystem. It asks the project view
//! ([`CrossFileQuery::preload_missing_path`]) whether the path can be PROVEN absent, and only a
//! view with a live, watcher-fresh picture of the tree answers `Some` — the default is `None`, so
//! `NoCrossFile`, `SyntacticQuery`, and every test stub stay silent by construction. These tests
//! pin both halves: what the analyzer does with a `Some`, and that the default keeps the same
//! source clean.
//!
//! The expected message is verbatim `Godot_v4.7.2-stable --headless --check-only` output.

use std::path::Path;

use gd_analyze::{analyze, CrossFileQuery, NoCrossFile, Severity, StrictSettings, WarnPolicy};
use gd_project::FileId;
use gd_syntax::Dialect;
use gd_types::NativeDb;

fn native_db() -> NativeDb {
    let path =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../gd_types/tests/fixtures/trimmed_api.json");
    NativeDb::load(path.to_str().expect("utf-8 path"))
        .unwrap_or_else(|e| panic!("load native DB fixture at {}: {e}", path.display()))
}

fn policy() -> WarnPolicy {
    WarnPolicy::build(
        &gd_project::WarningConfig::default(),
        &StrictSettings::default(),
        Dialect::DEFAULT,
    )
}

/// A project view that testifies every path is missing, rendering it the way the server's impl
/// would: a `res://` verbatim, anything else joined onto `res://src/`. Stands in for
/// `WorkspaceXFileQuery` without a filesystem.
struct AllMissing;

impl CrossFileQuery for AllMissing {
    fn global_class_file(&self, _name: &str) -> Option<FileId> {
        None
    }
    fn interface(&self, _file: FileId) -> Option<&gd_project::Interface> {
        None
    }
    fn resolve_res_path(&self, _path: &str) -> Option<FileId> {
        None
    }
    fn preload_missing_path(&self, _from: Option<FileId>, raw: &str) -> Option<String> {
        if raw.starts_with("res://") {
            Some(raw.to_owned())
        } else {
            Some(format!("res://src/{raw}"))
        }
    }
}

fn errors_with(src: &str, xfile: &dyn CrossFileQuery) -> Vec<String> {
    let tree = gd_syntax::parse(src).tree;
    let result = analyze(&tree, None, "t.gd", &native_db(), xfile, &policy());
    result
        .diagnostics
        .iter()
        .filter(|d| d.severity() == Severity::Error)
        .map(|d| d.message().to_owned())
        .collect()
}

/// The message, and the anchor: the path argument, not the whole `preload(…)` call.
#[test]
fn a_missing_res_path_reports_on_the_path_argument() {
    let src = "extends Node\n\nconst A = preload(\"res://gone.gd\")\n";
    let tree = gd_syntax::parse(src).tree;
    let result = analyze(&tree, None, "t.gd", &native_db(), &AllMissing, &policy());
    let rows: Vec<_> = result
        .diagnostics
        .iter()
        .filter(|d| d.severity() == Severity::Error)
        .collect();
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(
        rows[0].message(),
        r#"Preload file "res://gone.gd" does not exist."#
    );
    let span = rows[0].span();
    assert_eq!(
        &src[span.start as usize..span.end as usize],
        "\"res://gone.gd\"",
        "anchored on the path argument"
    );
}

/// A relative literal is rendered the way the view resolved it, not echoed back raw — Godot prints
/// the simplified `res://` form.
#[test]
fn a_relative_path_reports_in_its_resolved_form() {
    let src = "extends Node\n\nconst A = preload(\"gone.gd\")\n";
    assert_eq!(
        errors_with(src, &AllMissing),
        vec![r#"Preload file "res://src/gone.gd" does not exist."#.to_owned()]
    );
}

/// `load()` takes the same path but is an ordinary utility call, not a `PreloadNode` — Godot checks
/// only `preload`, and so does this.
#[test]
fn a_load_call_is_not_checked() {
    let src = "extends Node\n\nfunc f() -> void:\n\tvar a = load(\"res://gone.gd\")\n\tprint(a)\n";
    assert_eq!(errors_with(src, &AllMissing), Vec::<String>::new());
}

/// A non-constant path already has its own row; the missing check needs a folded string and never
/// doubles up on one.
#[test]
fn a_non_constant_path_reports_only_its_own_row() {
    let src = "extends Node\n\nfunc f(p: String) -> void:\n\tvar a = preload(p)\n\tprint(a)\n";
    assert_eq!(
        errors_with(src, &AllMissing),
        vec!["Preloaded path must be a constant string.".to_owned()]
    );
}

/// The fail-closed default. Same source, a view that cannot testify — the whole corpus and every
/// stub take this branch, which is why the ratchets are untouched.
#[test]
fn a_view_that_cannot_testify_stays_silent() {
    let src =
        "extends Node\n\nconst A = preload(\"res://gone.gd\")\nconst B = preload(\"gone.gd\")\n";
    assert_eq!(errors_with(src, &NoCrossFile), Vec::<String>::new());
}
