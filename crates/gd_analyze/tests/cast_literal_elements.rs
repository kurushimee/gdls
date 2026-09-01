//! #562 — a literal cast to a typed container is element-checked.
//!
//! `reduce_cast` (`gdscript_analyzer.cpp:3808-3815`) pushes the cast target's element types into an
//! array or dictionary LITERAL operand before the validity check, which is what makes the
//! per-element rows fire. gdls ran the annotated road (`var a: Array[String] = [1]`) but not the
//! cast one, so `[1] as Array[String]` passed silently.
//!
//! Every expected row is verbatim output from Godot 4.7.2's editor language server.

use std::path::Path;

use gd_analyze::{analyze, NoCrossFile, Severity, StrictSettings, WarnPolicy};
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

fn errors(body: &str) -> Vec<String> {
    let src = format!("extends Node\n\nfunc f() -> void:\n{body}");
    let tree = gd_syntax::parse(&src).tree;
    analyze(&tree, None, "t.gd", &native_db(), &NoCrossFile, &policy())
        .diagnostics
        .iter()
        .filter(|d| d.severity() == Severity::Error)
        .map(|d| d.message().to_owned())
        .collect()
}

/// Both rows, in the order Godot emits them.
#[test]
fn an_array_literal_cast_reports_a_bad_element() {
    assert_eq!(
        errors("\tvar a := [1, 2] as Array[String]\n\tprint(a)\n"),
        vec![
            r#"Cannot include a value of type "int" as "String"."#.to_owned(),
            r#"Cannot have an element of type "int" in an array of type "Array[String]"."#
                .to_owned(),
        ]
    );
}

/// The dictionary half, on the key.
#[test]
fn a_dictionary_literal_cast_reports_a_bad_key() {
    assert_eq!(
        errors("\tvar d := {1: 2} as Dictionary[String, int]\n\tprint(d)\n"),
        vec![
            r#"Cannot include a value of type "int" as "String"."#.to_owned(),
            r#"Cannot have a key of type "int" in a dictionary of type "Dictionary[String, int]"."#
                .to_owned(),
        ]
    );
}

/// A matching literal, and an empty one, have nothing to report.
#[test]
fn a_matching_or_empty_literal_is_silent() {
    assert_eq!(
        errors("\tvar b := [\"a\"] as Array[String]\n\tvar c := [] as Array[String]\n\tvar e := {\"k\": 2} as Dictionary[String, int]\n\tprint(b, c, e)\n"),
        Vec::<String>::new()
    );
}

/// An UNtyped container target has no element type to push, so the literal keeps its own.
#[test]
fn an_untyped_container_target_is_silent() {
    assert_eq!(
        errors("\tvar g := [1] as Array\n\tvar h := {1: 2} as Dictionary\n\tprint(g, h)\n"),
        Vec::<String>::new()
    );
}

/// The narrowing applies to a LITERAL operand only — a variable already has its own type, and the
/// ordinary cast-validity rules own that case.
#[test]
fn a_non_literal_operand_keeps_the_plain_cast_rules() {
    assert_eq!(
        errors("\tvar v := [1]\n\tvar w := v as Array[String]\n\tprint(w)\n"),
        Vec::<String>::new()
    );
}
