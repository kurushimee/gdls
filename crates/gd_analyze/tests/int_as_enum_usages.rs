//! #466 — `INT_AS_ENUM_WITHOUT_MATCH` fires for every usage verb, not just a cast.
//!
//! Upstream has exactly ONE site for this warning, inside the shared helper
//! `update_const_expression_builtin_type` (`gdscript_analyzer.cpp:2762-2766`), and its `p_usage`
//! parameter is what supplies the verb. The helper is called from six places with five distinct
//! verbs — `assign` (a declaration initializer and an assignment), `return`, `pass` (a call
//! argument), `include` (an array element, a dictionary key, a dictionary value), and `cast`.
//!
//! gdls had all six call sites with the right verbs but inlined the check in `reduce_cast` alone,
//! so only `cast` ever fired. The corpus never caught it: its two cases
//! (`analyzer/warnings/cast_enum_bad_enum.gd`, `cast_enum_bad_int.gd`) are both casts.
//!
//! Two properties of the upstream block are load-bearing and pinned below. The value comes from
//! the FOLD, not the static type, so a non-constant int never warns. And the check runs BEFORE the
//! helper re-types the expression to the target enum, which is what stops it reading a value it
//! just narrowed.
//!
//! Every row is pinned against `Godot_v4.7.2-stable --headless --check-only`.

use std::path::Path;

use gd_analyze::{analyze, NoCrossFile, Severity, StrictSettings, WarnPolicy};
use gd_syntax::{parse, Dialect};
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

/// Only the rows this file is about: `INT_AS_ENUM_WITHOUT_CAST` pairs with every one of them and
/// is a different warning with its own coverage.
fn matches(src: &str) -> Vec<String> {
    let tree = parse(src).tree;
    analyze(
        &tree,
        None,
        "res://enummatch.gd",
        &native_db(),
        &NoCrossFile,
        &policy(),
    )
    .diagnostics
    .iter()
    .filter(|d| d.code() == "INT_AS_ENUM_WITHOUT_MATCH")
    .map(|d| d.message().to_owned())
    .collect()
}

fn errors(src: &str) -> Vec<String> {
    let tree = parse(src).tree;
    analyze(
        &tree,
        None,
        "res://enummatch.gd",
        &native_db(),
        &NoCrossFile,
        &policy(),
    )
    .diagnostics
    .iter()
    .filter(|d| d.severity() == Severity::Error)
    .map(|d| d.message().to_owned())
    .collect()
}

fn row(verb: &str, value: &str) -> String {
    format!(r#"Cannot {verb} {value} as Enum "enummatch.gd.E": no enum member has matching value."#)
}

const HEAD: &str = "\
extends Node

enum E { A = 1, B = 2 }
";

/// The issue's repro, whole: one row per usage, each naming its own verb.
#[test]
fn every_usage_verb_reports_the_unmatched_value() {
    let src = format!(
        "{HEAD}
func takes(e: E) -> void:
\tprint(e)

func gives() -> E:
\treturn 5

func f() -> void:
\tvar a: E = 5
\tconst B: E = 5
\ttakes(5)
\tvar arr: Array[E] = [5]
\tvar d: Dictionary[E, int] = {{5: 1}}
\tprint(5 as E)
\tprint(a, B, arr, d)
"
    );
    assert_eq!(
        matches(&src),
        vec![
            row("return", "5"),
            row("assign", "5"),
            row("assign", "5"),
            row("pass", "5"),
            row("include", "5"),
            row("include", "5"),
            row("cast", "5"),
        ]
    );
}

/// The cast row is the one gdls always had. It must survive the move to the shared helper, and it
/// must not double up now that the helper owns the only emission.
#[test]
fn a_cast_still_reports_exactly_once() {
    let src = format!("{HEAD}\nfunc f() -> void:\n\tprint(5 as E)\n");
    assert_eq!(matches(&src), vec![row("cast", "5")]);
}

/// A value the enum does carry is silent through every verb.
#[test]
fn a_matching_value_is_silent_everywhere() {
    let src = format!(
        "{HEAD}
func takes(e: E) -> void:
\tprint(e)

func gives() -> E:
\treturn 2

func f() -> void:
\tvar a: E = 1
\ttakes(2)
\tvar arr: Array[E] = [1, 2]
\tprint(1 as E)
\tprint(a, arr)
"
    );
    assert_eq!(matches(&src), Vec::<String>::new());
}

/// The value is read off the FOLD, so an int that is not a constant expression never warns —
/// upstream reads `p_expression->reduced_value` and a non-constant has none.
#[test]
fn a_non_constant_int_never_warns() {
    let src = format!(
        "{HEAD}
func f(n: int) -> void:
\tvar a: E = n
\tprint(a)
"
    );
    assert_eq!(matches(&src), Vec::<String>::new());
}

/// A folded arithmetic expression still counts as constant, and the message renders the folded
/// value rather than the source text.
#[test]
fn a_folded_expression_reports_its_computed_value() {
    let src = format!("{HEAD}\nfunc f() -> void:\n\tvar a: E = 2 + 3\n\tprint(a)\n");
    assert_eq!(matches(&src), vec![row("assign", "5")]);
}

/// A negative and a zero value are ordinary unmatched values, not edge cases that suppress.
#[test]
fn negative_and_zero_values_report_too() {
    let src =
        format!("{HEAD}\nfunc f() -> void:\n\tvar a: E = 0\n\tvar b: E = -1\n\tprint(a, b)\n");
    assert_eq!(matches(&src), vec![row("assign", "0"), row("assign", "-1")]);
}

/// The warning is a warning: none of these rows is an error, so nothing here changes whether the
/// file compiles.
#[test]
fn no_usage_verb_turns_the_row_into_an_error() {
    let src = format!(
        "{HEAD}
func gives() -> E:
\treturn 5

func f() -> void:
\tvar a: E = 5
\tprint(a)
"
    );
    assert_eq!(errors(&src), Vec::<String>::new());
}
