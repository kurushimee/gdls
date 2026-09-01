//! The constant-subscript miss (`gdscript_analyzer.cpp:4920-4930`) — #385.
//!
//! When the base AND the index are both constant, Godot just tries the index and, on failure,
//! renders BOTH values into the message. gdls could not reach that arm at all: its fold table had
//! no array or dictionary, so a `const` collection was never a constant operand.
//!
//! Three things had to land together. A collection representation in the fold table, produced by
//! `make_expression_reduced_value` at the constant sites — NOT by `reduce_array` /
//! `reduce_dictionary`, which is why `{[1, 2]: 1, [1, 2]: 2}` is still not a duplicate-key error.
//! A `Variant::stringify` port, so the message matches byte for byte. And `Variant::get` itself,
//! restricted to the bases whose indexing is unambiguous.
//!
//! Every row is pinned against `Godot_v4.7.2-stable --headless --check-only`.

use gd_analyze::warn_policy::{StrictSettings, WarnPolicy};
use gd_analyze::NoCrossFile;
use gd_project::WarningConfig;
use gd_syntax::Dialect;
use gd_types::NativeDb;

fn mini_native() -> NativeDb {
    NativeDb::from_json(
        r#"{
            "header": {"version_major": 4, "version_minor": 7, "version_patch": 2},
            "classes": [{"name": "Object"}, {"name": "Node", "inherits": "Object"}]
        }"#,
    )
    .expect("valid mini dump")
}

fn errors(src: &str) -> Vec<String> {
    let tree = gd_syntax::parse(src).tree;
    let policy = WarnPolicy::build(
        &WarningConfig::default(),
        &StrictSettings::default(),
        Dialect::DEFAULT,
    );
    let result = gd_analyze::analyze(&tree, None, "t.gd", &mini_native(), &NoCrossFile, &policy);
    result
        .diagnostics
        .iter()
        .filter(|d| d.warning_code().is_none())
        .map(|d| d.message().to_owned())
        .collect()
}

fn miss(index: &str, base: &str) -> String {
    format!(r#"Cannot get index "{index}" from "{base}"."#)
}

/// The issue's own case, and the rendering that made it hard: the message carries Godot's own
/// printing of the whole base, nested collections and all.
#[test]
fn a_constant_dictionary_miss_is_reported_with_both_values() {
    assert_eq!(
        errors("extends Node\nconst D := {\"a\": 1}\nfunc f() -> void:\n\tvar _x = D[\"zz\"]\n"),
        vec![miss("zz", r#"{ "a": 1 }"#)]
    );
    assert_eq!(
        errors(
            "extends Node\nconst D := {\"a\": 1, \"b\": [1, 2.5], \"c\": {\"d\": &\"n\"}}\nfunc f() -> void:\n\tvar _x = D[\"zz\"]\n"
        ),
        vec![miss("zz", r#"{ "a": 1, "b": [1, 2.5], "c": { "d": &"n" } }"#)]
    );
    // An empty dictionary renders with Godot's two spaces.
    assert_eq!(
        errors("extends Node\nconst D := {}\nfunc f() -> void:\n\tvar _x = D[\"zz\"]\n"),
        vec![miss("zz", "{  }")]
    );
}

/// A dictionary index is a key lookup: a hit of any key type is silent, a miss is reported, and the
/// `String`/`StringName` equivalence applies to the lookup exactly as it does to duplicate keys.
#[test]
fn a_dictionary_index_is_a_key_lookup() {
    let d = "extends Node\nconst D := {\"a\": 1, 2: \"b\"}\nfunc f() -> void:\n\tvar _x = D[";
    assert_eq!(errors(&format!("{d}\"a\"]\n")), Vec::<String>::new());
    assert_eq!(errors(&format!("{d}2]\n")), Vec::<String>::new());
    assert_eq!(errors(&format!("{d}&\"a\"]\n")), Vec::<String>::new());
    assert_eq!(
        errors(&format!("{d}0]\n")),
        vec![miss("0", r#"{ "a": 1, 2: "b" }"#)]
    );
    assert_eq!(
        errors(&format!("{d}\"zz\"]\n")),
        vec![miss("zz", r#"{ "a": 1, 2: "b" }"#)]
    );
}

/// An array index counts from the end when negative, truncates a float, and rejects a bool or a
/// string outright — all four oracle-pinned, and the bool row is why a float cannot simply be
/// "any numeric".
#[test]
fn an_array_index_is_an_integer_position() {
    let a = "extends Node\nconst A := [10, 20, 30]\nfunc f() -> void:\n\tvar _x = A[";
    for ok in ["0", "-1", "1.0", "1.5", "2"] {
        assert_eq!(errors(&format!("{a}{ok}]\n")), Vec::<String>::new(), "{ok}");
    }
    for (bad, shown) in [("3", "3"), ("-4", "-4"), ("\"x\"", "x"), ("true", "true")] {
        assert_eq!(
            errors(&format!("{a}{bad}]\n")),
            vec![miss(shown, "[10, 20, 30]")],
            "{bad}"
        );
    }
}

/// A constant string indexes by position too.
#[test]
fn a_constant_string_indexes_by_position() {
    assert_eq!(
        errors("extends Node\nfunc f() -> void:\n\tvar _x = \"abc\"[0]\n"),
        Vec::<String>::new()
    );
    assert_eq!(
        errors("extends Node\nfunc f() -> void:\n\tvar _x = \"abc\"[9]\n"),
        vec![miss("9", "abc")]
    );
}

/// A successful index folds, so the result is itself constant and indexable — which is what makes
/// a nested lookup work and a nested miss report against the INNER value.
#[test]
fn a_successful_index_stays_constant() {
    assert_eq!(
        errors(
            "extends Node\nconst D := {\"a\": [1, 2]}\nfunc f() -> void:\n\tvar _x = D[\"a\"][0]\n"
        ),
        Vec::<String>::new()
    );
    assert_eq!(
        errors(
            "extends Node\nconst D := {\"a\": [1, 2]}\nfunc f() -> void:\n\tvar _x = D[\"a\"][9]\n"
        ),
        vec![miss("9", "[1, 2]")]
    );
}

/// The collection operations `Variant::evaluate` performs. `Array + Array` concatenates, and the
/// result is still constant; `Dictionary + Dictionary` is rejected by the TYPE check with its own
/// message and never evaluated, which is why it must not take the value path.
#[test]
fn the_collection_operators_godot_evaluates_are_evaluated() {
    assert_eq!(
        errors(
            "extends Node\nconst A := [1]\nconst C = A + A\nfunc f() -> void:\n\tvar _x = C[1]\n"
        ),
        Vec::<String>::new()
    );
    assert_eq!(
        errors(
            "extends Node\nconst A := [1]\nconst C = A + A\nfunc f() -> void:\n\tvar _x = C[2]\n"
        ),
        vec![miss("2", "[1, 1]")]
    );
    assert_eq!(
        errors("extends Node\nconst A := [1]\nconst C = A == A\n"),
        Vec::<String>::new()
    );
    assert_eq!(
        errors("extends Node\nconst B := {\"a\": 1}\nconst C = B + B\n"),
        // Godot adds `Assigned value for constant "C" isn't a constant expression.` here. gdls
        // does not track constancy through a rejected operator, so that second line is a standing
        // under-report — it only ever accompanies the error above, which gdls does report.
        vec![r#"Invalid operands "Dictionary" and "Dictionary" for "+" operator."#.to_string()]
    );
}

/// The fold is produced at the CONSTANT site only, never by `reduce_array` / `reduce_dictionary`.
/// Godot behaves the same way, and the visible proof is that two equal array literals used as
/// dictionary keys are not a duplicate — they carry no value when the check runs.
#[test]
fn a_collection_literal_key_is_not_a_duplicate() {
    assert_eq!(
        errors("extends Node\nconst D := {[1, 2]: 1, [1, 2]: 2}\n"),
        Vec::<String>::new()
    );
    assert_eq!(
        errors("extends Node\nconst E := {{\"a\": 1}: 1, {\"a\": 1}: 2}\n"),
        Vec::<String>::new()
    );
    // A scalar key still is one, so the check itself has not been disabled.
    assert_eq!(
        errors("extends Node\nconst F := {1.5: 1, 1.5: 2}\n"),
        vec![r#"Key "1.5" was already used in this dictionary (at line 2)."#.to_string()]
    );
}

/// A base gdls cannot index stays silent rather than reporting a miss it cannot prove.
#[test]
fn an_undecidable_base_reports_nothing() {
    for src in [
        // A non-constant base.
        "extends Node\nfunc f() -> void:\n\tvar a := [1]\n\tvar _x = a[9]\n",
        // A non-constant index.
        "extends Node\nconst A := [1]\nfunc f(i: int) -> void:\n\tvar _x = A[i]\n",
    ] {
        assert_eq!(errors(src), Vec::<String>::new(), "{src}");
    }
}
