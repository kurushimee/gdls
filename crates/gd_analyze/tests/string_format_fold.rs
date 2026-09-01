//! #564 — `"fmt" % value` where the format cannot accept the value.
//!
//! A constant `String % <scalar>` folds through `OperatorEvaluatorStringFormat::do_mod`
//! (core/variant/variant_op.h:724-836), which hands the format and a one-element value span to
//! `String::sprintf` (core/string/ustring.cpp:5182). sprintf returns its error text as the result
//! string, so `reduce_binary_op` reports `<message> in operator %.` and leaves the node constant
//! with that text as its value (gdscript_analyzer.cpp:3149, :3163). gdls stamped the result opaque
//! and validated nothing.
//!
//! Every expected message here is verbatim output from `Godot_v4.6.3-stable` and
//! `Godot_v4.7.2-stable` run over the same source, and every silent row was confirmed silent in
//! both. The one deliberate under-report is `"%d" % Vector2(1, 2)`: Godot errors, gdls folds the
//! vector opaquely and so cannot see the value, and guessing there would be a false positive.

use std::path::Path;

use gd_analyze::{
    analyze_with_options, AnalyzeOptions, NoCrossFile, Severity, StrictSettings, WarnPolicy,
};
use gd_syntax::{parse, Dialect};
use gd_types::NativeDb;

fn native_db() -> NativeDb {
    let path =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../gd_types/tests/fixtures/trimmed_api.json");
    NativeDb::load(path.to_str().expect("utf-8 path"))
        .unwrap_or_else(|e| panic!("load native DB fixture at {}: {e}", path.display()))
}

/// Every error message the source produces, at the given dialect.
fn errors_at(src: &str, dialect: Dialect) -> Vec<String> {
    let policy = WarnPolicy::build(
        &gd_project::WarningConfig::default(),
        &StrictSettings::default(),
        dialect,
    );
    let tree = parse(src).tree;
    analyze_with_options(
        &tree,
        None,
        "res://main.gd",
        &native_db(),
        &NoCrossFile,
        &policy,
        AnalyzeOptions {
            dialect,
            ..Default::default()
        },
    )
    .diagnostics
    .into_iter()
    .filter(|d| d.severity() == Severity::Error)
    .map(|d| d.message().to_owned())
    .collect()
}

fn errors(src: &str) -> Vec<String> {
    errors_at(src, Dialect::DEFAULT)
}

/// Wraps an expression in a body so the reducer reaches it without a declaration in the way.
fn body(expr: &str) -> String {
    format!("extends Node\n\nfunc _ready() -> void:\n\tprint({expr})\n")
}

fn one(expr: &str) -> Vec<String> {
    errors(&body(expr))
}

#[test]
fn an_integer_specifier_wants_a_number() {
    const MSG: &str = "a number is required in operator %.";
    for expr in [
        r#""%d" % "s""#,
        r#""%o" % "s""#,
        r#""%x" % "s""#,
        r#""%X" % "s""#,
        r#""%f" % "s""#,
        r#""%d" % null"#,
        r#""%d" % true"#,
        r#""%d" % &"sn""#,
        r#""%d" % ^"np""#,
    ] {
        assert_eq!(one(expr), vec![MSG.to_owned()], "for {expr}");
    }
}

#[test]
fn a_vector_specifier_wants_a_vector() {
    assert_eq!(
        one(r#""%v" % 1"#),
        vec!["%v requires a vector type (Vector2/3/4/2i/3i/4i) in operator %.".to_owned()]
    );
}

#[test]
fn a_char_specifier_wants_one_character_or_a_code_point() {
    const MSG: &str = "%c requires number or single-character string in operator %.";
    for expr in [
        r#""%c" % "ab""#,
        r#""%c" % """#,
        r#""%c" % &"a""#,
        r#""%c" % true"#,
    ] {
        assert_eq!(one(expr), vec![MSG.to_owned()], "for {expr}");
    }
    assert!(one(r#""%c" % "a""#).is_empty());
}

#[test]
fn a_char_code_point_is_range_checked() {
    assert_eq!(
        one(r#""%c" % -1"#),
        vec!["unsigned integer is lower than minimum in operator %.".to_owned()]
    );
    assert_eq!(
        one(r#""%c" % 55296"#),
        vec!["unsigned integer is invalid Unicode character in operator %.".to_owned()]
    );
    assert_eq!(
        one(r#""%c" % 1114112"#),
        vec!["unsigned integer is greater than maximum in operator %.".to_owned()]
    );
}

/// `int value = values[index]` narrows through a C `int`, so a code point past 2^32 wraps into
/// range instead of failing. Godot is silent on both rows; so is gdls.
#[test]
fn a_char_code_point_narrows_the_way_c_does() {
    assert!(one(r#""%c" % 4294967361"#).is_empty());
    assert!(one(r#""%c" % 65.9"#).is_empty());
}

#[test]
fn a_dangling_percent_is_incomplete() {
    const MSG: &str = "incomplete format in operator %.";
    assert_eq!(one(r#""%" % 1"#), vec![MSG.to_owned()]);
    assert_eq!(one(r#""100%" % 1"#), vec![MSG.to_owned()]);
}

#[test]
fn an_unknown_specifier_is_rejected() {
    assert_eq!(
        one(r#""%z" % 1"#),
        vec!["unsupported format character in operator %.".to_owned()]
    );
}

#[test]
fn a_second_specifier_has_no_argument() {
    assert_eq!(
        one(r#""%d %d" % 1"#),
        vec!["not enough arguments for format string in operator %.".to_owned()]
    );
}

#[test]
fn a_format_with_no_specifier_leaves_the_value_unused() {
    const MSG: &str = "not all arguments converted during string formatting in operator %.";
    assert_eq!(one(r#""" % 1"#), vec![MSG.to_owned()]);
    assert_eq!(one(r#""100%%" % 1"#), vec![MSG.to_owned()]);
}

/// `*` consumes the value as the width, so the specifier after it has nothing left to read.
#[test]
fn a_dynamic_width_consumes_the_only_value() {
    assert_eq!(
        one(r#""%*d" % 4"#),
        vec!["not enough arguments for format string in operator %.".to_owned()]
    );
}

#[test]
fn a_dynamic_width_wants_a_number() {
    assert_eq!(
        one(r#""%*d" % "s""#),
        vec!["* wants number or vector in operator %.".to_owned()]
    );
}

#[test]
fn a_second_decimal_point_is_rejected() {
    assert_eq!(
        one(r#""%.2.2f" % 1.0"#),
        vec!["too many decimal points in format in operator %.".to_owned()]
    );
}

#[test]
fn a_string_name_format_is_checked_too() {
    assert_eq!(
        one(r#"&"%d" % "s""#),
        vec!["a number is required in operator %.".to_owned()]
    );
}

#[test]
fn the_check_runs_at_constant_and_local_sites() {
    assert_eq!(
        errors("extends Node\n\nconst C = \"%d\" % \"s\"\n"),
        vec!["a number is required in operator %.".to_owned()]
    );
    assert_eq!(
        errors("extends Node\n\nfunc _ready() -> void:\n\tvar x := \"%d\" % \"s\"\n\tprint(x)\n"),
        vec!["a number is required in operator %.".to_owned()]
    );
}

/// DIALECT(4.7): `%<n>$` selects an argument by position (ustring.cpp:5521). At 4.6 `$` has no
/// case in the switch and falls through to the default arm.
#[test]
fn positional_selection_arrived_in_4_7() {
    let src = body(r#""%1$s" % "a""#);
    assert!(errors_at(&src, Dialect::Godot4_7).is_empty());
    assert_eq!(
        errors_at(&src, Dialect::Godot4_6),
        vec!["unsupported format character in operator %.".to_owned()]
    );
}

/// The gate. Anything outside it keeps today's silent behaviour, which is also Godot's for every
/// row here except the last.
#[test]
fn a_value_the_fold_cannot_see_is_left_alone() {
    // Not constant at all.
    assert!(errors("extends Node\n\nfunc f(v: String) -> void:\n\tprint(\"%d\" % v)\n").is_empty());
    // An `Array` right operand has no fold — `reduce_array` sets no `reduced_value` upstream
    // either, so Godot is silent on all three.
    for expr in [r#""%d" % [1]"#, r#""%d %d" % [1]"#, r#""%d" % [1, 2]"#] {
        assert!(one(expr).is_empty(), "for {expr}");
    }
    // Folded opaquely: gdls holds the kind but not the value. Godot reports here; gdls does not.
    assert!(one(r#""%d" % Vector2(1, 2)"#).is_empty());
}

#[test]
fn a_well_formed_format_stays_silent() {
    for expr in [
        r#""%s" % 1"#,
        r#""%s" % "s""#,
        r#""%s" % null"#,
        r#""%d" % 1"#,
        r#""%d" % 1.5"#,
        r#""%10.3f" % 1.0"#,
        r#""%-5d" % 1"#,
        r#""%+05d" % 1"#,
        r#""%ud" % 1"#,
        r#""%x" % 255"#,
    ] {
        assert!(one(expr).is_empty(), "for {expr}");
    }
}
