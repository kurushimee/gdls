//! #434 — `NARROWING_CONVERSION` at a call argument.
//!
//! `validate_call_arg`'s ladder ends in one more arm than gdls ported
//! (`gdscript_analyzer.cpp:6115-6117`, byte-identical at 4.6.3-stable): a hard `float` argument
//! passed into an `int` parameter. Godot gates neither side on hardness there, because the soft
//! and Variant arguments already left through the first arm.
//!
//! Every row is verbatim `Godot_v4.7.2-stable --headless --check-only` output, confirmed
//! identical at `Godot_v4.6.3-stable`.

use std::path::Path;

use gd_analyze::{analyze, NoCrossFile, StrictSettings, WarnPolicy};
use gd_project::{FileId, WarningConfig};
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
        &WarningConfig::default(),
        &StrictSettings {
            enable_warnings: vec![
                "NARROWING_CONVERSION".to_owned(),
                "UNSAFE_CALL_ARGUMENT".to_owned(),
            ],
            ..Default::default()
        },
        Dialect::DEFAULT,
    )
}

const NARROWING: &str = "Narrowing conversion (float is converted to int and loses precision).";

/// `(warning code, 1-based line)` for every warning the file draws, plus every hard error as
/// `("error", line)` so a test can assert the shape of both halves at once.
fn rows(body: &str) -> Vec<(String, u32)> {
    let src = format!(
        "extends Node\n\n\
         func take_int(x: int) -> void:\n\tprint(x)\n\n\
         func take_float(x: float) -> void:\n\tprint(x)\n\n\
         func take_any(x: Variant) -> void:\n\tprint(x)\n\n\
         func go() -> void:\n\t{}\n",
        body.replace('\n', "\n\t")
    );
    let tree = parse(&src).tree;
    let result = analyze(
        &tree,
        Some(FileId::new(1)),
        "narrowing.gd",
        &native_db(),
        &NoCrossFile,
        &policy(),
    );
    result
        .diagnostics
        .iter()
        .map(|d| {
            let code = d.warning_code().map_or("error", |_| d.code()).to_owned();
            (code, d.line().unwrap_or(0))
        })
        .collect()
}

fn messages(body: &str) -> Vec<String> {
    let src = format!(
        "extends Node\n\n\
         func take_int(x: int) -> void:\n\tprint(x)\n\n\
         func take_float(x: float) -> void:\n\tprint(x)\n\n\
         func take_any(x: Variant) -> void:\n\tprint(x)\n\n\
         func go() -> void:\n\t{}\n",
        body.replace('\n', "\n\t")
    );
    let tree = parse(&src).tree;
    let result = analyze(
        &tree,
        Some(FileId::new(1)),
        "narrowing.gd",
        &native_db(),
        &NoCrossFile,
        &policy(),
    );
    result
        .diagnostics
        .iter()
        .map(|d| d.message().to_owned())
        .collect()
}

#[test]
fn a_hard_float_into_an_int_parameter_narrows() {
    assert_eq!(
        messages("var f: float = 1.5\ntake_int(f)"),
        vec![NARROWING.to_owned()]
    );
}

#[test]
fn a_constant_float_narrows_too() {
    // Godot converts the constant inside `update_const_expression_builtin_type` and warns there,
    // on the same node; gdls does not convert, so the still-`float` argument draws the row from
    // the call-argument arm. One row either way. An integral-valued float is no exception.
    assert_eq!(messages("take_int(1.5)"), vec![NARROWING.to_owned()]);
    assert_eq!(messages("take_int(1.0)"), vec![NARROWING.to_owned()]);
}

#[test]
fn a_soft_float_draws_the_unsafe_row_and_not_the_narrowing_one() {
    assert_eq!(
        messages("var v = 1.5\ntake_int(v)"),
        vec![
            "The argument 1 of the function \"take_int()\" requires the subtype \"int\" \
             but the supertype \"Variant\" was provided."
                .to_owned()
        ]
    );
}

#[test]
fn widening_and_a_variant_parameter_stay_silent() {
    assert!(messages("take_float(3)").is_empty());
    assert!(messages("var f: float = 1.5\ntake_any(f)").is_empty());
}

#[test]
fn a_native_method_narrows() {
    assert_eq!(
        messages("var f: float = 1.5\nprint(get_child(f))"),
        vec![NARROWING.to_owned()]
    );
}

#[test]
fn a_builtin_method_narrows() {
    assert_eq!(
        messages("var f: float = 1.5\nvar arr: Array = []\narr.resize(f)"),
        vec![NARROWING.to_owned()]
    );
}

#[test]
fn a_super_call_narrows() {
    assert_eq!(
        messages("var f: float = 1.5\nsuper.get_child(f)"),
        vec![NARROWING.to_owned()]
    );
}

#[test]
fn the_row_lands_on_the_argument_not_the_call() {
    let r = rows("var f: float = 1.5\ntake_int(f)");
    assert_eq!(r, vec![("NARROWING_CONVERSION".to_owned(), 14)]);
}

#[test]
fn a_utility_call_is_a_known_under_report() {
    // #440: Godot routes a non-folding utility call through `validate_call_arg` too, so it warns
    // here. gdls's utility arm runs no argument validation at all — no arity, neither UNSAFE arm,
    // and no narrowing. Pinned so the day that arm gains the ladder, this test says so.
    assert!(messages("var f: float = 1.5\nprint(absi(f))").is_empty());
}
