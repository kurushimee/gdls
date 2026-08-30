//! `GDScriptAnalyzer::resolve_annotation` (analyzer.cpp:1673-1727) — an annotation's arguments are
//! reduced, folded, and checked against the types its registration declares. Every row is pinned
//! against `godot --headless --check-only` at 4.7.2-stable.

use gd_analyze::warn_policy::{StrictSettings, WarnPolicy};
use gd_analyze::NoCrossFile;
use gd_project::WarningConfig;
use gd_syntax::Dialect;
use gd_types::NativeDb;

/// The trimmed real dump, so the 114 Variant utilities resolve — a bare `absi(-10)` in an
/// annotation argument would otherwise read as a call to a function that does not exist.
fn native() -> NativeDb {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../gd_types/tests/fixtures/trimmed_api.json");
    NativeDb::load(path.to_str().expect("utf-8 path"))
        .unwrap_or_else(|e| panic!("load native DB fixture at {}: {e}", path.display()))
}

fn diagnose(src: &str, warnings: &[&str]) -> (Vec<String>, Vec<String>) {
    let tree = gd_syntax::parse(src).tree;
    let strict = StrictSettings {
        enable_warnings: warnings.iter().map(|s| (*s).to_string()).collect(),
        ..Default::default()
    };
    let policy = WarnPolicy::build(&WarningConfig::default(), &strict, Dialect::DEFAULT);
    let result = gd_analyze::analyze(&tree, None, "t.gd", &native(), &NoCrossFile, &policy);
    let errors = result
        .diagnostics
        .iter()
        .filter(|d| d.warning_code().is_none())
        .map(|d| d.message().to_owned())
        .collect();
    let warns = result
        .diagnostics
        .iter()
        .filter(|d| d.warning_code().is_some())
        .map(|d| d.message().to_owned())
        .collect();
    (errors, warns)
}

fn errors(src: &str) -> Vec<String> {
    diagnose(src, &[]).0
}

/// The vararg parameter index sticks on the registration's last entry, so every argument past the
/// declared list is checked against that final type.
#[test]
fn a_vararg_annotation_checks_every_argument_against_its_last_parameter() {
    assert_eq!(
        errors("extends Node\n@export_flags(\"A\", \"B\", 0) var flags: int = 0\n"),
        vec![
            r#"Invalid argument for annotation "@export_flags": argument 3 should be "String" but is "int"."#
        ]
    );
}

/// A fixed-arity annotation checks its one argument in place.
#[test]
fn a_wrongly_typed_argument_names_the_expected_and_actual_types() {
    assert_eq!(
        errors("extends Node\n@export_placeholder(3) var a: String = \"\"\n"),
        vec![
            r#"Invalid argument for annotation "@export_placeholder": argument 1 should be "String" but is "int"."#
        ]
    );
}

/// The check reads the folded value, so a constant that folds to the wrong type is caught the same
/// way a literal is.
#[test]
fn a_folded_constant_argument_is_checked_by_its_value() {
    assert_eq!(
        errors("extends Node\nconst C := 5\n@export_placeholder(C) var a: String = \"\"\n"),
        vec![
            r#"Invalid argument for annotation "@export_placeholder": argument 1 should be "String" but is "int"."#
        ]
    );
}

/// A member variable can never be constant, so an argument naming one is blamed by index.
#[test]
fn a_non_constant_argument_is_blamed_by_index() {
    assert_eq!(
        errors("extends Node\nvar num := 1\n@export_range(num, 10) var a\n"),
        vec![r#"Argument 1 of annotation "@export_range" isn't a constant expression."#]
    );
}

/// The walk blames the FIRST offending argument and stops, exactly as Godot's early return does.
#[test]
fn only_the_first_offending_argument_is_reported() {
    assert_eq!(
        errors("extends Node\nvar num := 1\n@export_range(num, num) var a\n"),
        vec![r#"Argument 1 of annotation "@export_range" isn't a constant expression."#]
    );
}

/// A float where an int is declared converts, and warns while it does (analyzer.cpp:1700-1705).
/// The warning is ignore-by-default, so it needs asking for.
#[test]
fn a_float_in_an_int_slot_warns_narrowing_and_still_converts() {
    let (errs, warns) = diagnose(
        "extends Node\n@rpc(\"any_peer\", \"call_local\", \"reliable\", 1.5)\nfunc f():\n\tpass\n",
        &["NARROWING_CONVERSION"],
    );
    assert_eq!(errs, Vec::<String>::new());
    assert_eq!(warns.len(), 1, "{warns:?}");
    assert!(warns[0].contains("Narrowing conversion"), "{warns:?}");
}

/// Well-typed arguments say nothing, at every arity the registrations allow.
#[test]
fn correctly_typed_arguments_are_silent() {
    for src in [
        "extends Node\n@export_range(-10, 10) var a = 0\n",
        "extends Node\n@export_range(1, 2, 3, \"or_greater\") var a: int = 0\n",
        "extends Node\n@export_flags(\"A\", \"B\", \"C\") var f: int = 0\n",
        "extends Node\nconst BEFORE = 1\n@export_range(BEFORE + 1, 10) var c = 5\n",
        "extends Node\n@export_placeholder(\"hint\") var s: String = \"\"\n",
    ] {
        assert_eq!(errors(src), Vec::<String>::new(), "{src}");
    }
}

/// An int in a float slot is a widening conversion, so it is silent and not warned.
#[test]
fn an_int_in_a_float_slot_is_silent() {
    let (errs, warns) = diagnose(
        "extends Node\n@export_range(1, 2) var a: int = 0\n",
        &["NARROWING_CONVERSION"],
    );
    assert_eq!(errs, Vec::<String>::new());
    assert_eq!(warns, Vec::<String>::new());
}

/// Positive identification only: an argument gdls cannot fold but also cannot prove non-constant
/// stays silent rather than reading as either error. `absi(-10)` is constant to Godot.
#[test]
fn an_unfoldable_but_unprovable_argument_stays_silent() {
    assert_eq!(
        errors("extends Node\n@export_range(absi(-10), 10) var a = 0\n"),
        Vec::<String>::new()
    );
}

/// A statement-level annotation resolves too (analyzer.cpp:2076-2080), so its arguments are
/// checked inside a function body just as they are at class level.
#[test]
fn a_statement_level_annotation_resolves_its_arguments() {
    assert_eq!(
        errors(
            "extends Node\nvar num := 1\nfunc f():\n\t@warning_ignore(num)\n\tvar y = 1\n\tprint(y)\n"
        ),
        vec![r#"Argument 1 of annotation "@warning_ignore" isn't a constant expression."#]
    );
}

/// One annotation, one resolve: reaching the same node from two phases must not double-report.
#[test]
fn an_annotation_is_resolved_at_most_once() {
    assert_eq!(
        errors("extends Node\nvar num := 1\n@export_range(num, 10) var a\nfunc f():\n\tpass\n")
            .len(),
        1
    );
}
