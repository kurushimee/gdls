//! Three analyzer errors that had no port: a call on an enum VALUE (#377), a bare `super()` inside
//! a lambda (#378), and a non-constant dictionary match-pattern key (#379).
//!
//! What ties them together is that gdls already had each one's *neighbour* — the property twin of
//! the enum check, the lambda cursor stack, the pattern-constancy gate — so each was a single
//! missing arm rather than a missing subsystem. Two of the three left gdls answering with Godot's
//! follow-on line while dropping the line that explains it.
//!
//! Every expectation is pinned against `godot --headless --check-only` at 4.7.2.

use std::path::Path;

use gd_analyze::{analyze_with_options, AnalyzeOptions, NoCrossFile, StrictSettings, WarnPolicy};
use gd_project::{FileId, WarningConfig};
use gd_syntax::{Dialect, ParseOptions};
use gd_types::NativeDb;

fn native_db() -> NativeDb {
    let path =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../gd_types/tests/fixtures/trimmed_api.json");
    NativeDb::load(path.to_str().expect("utf-8 path"))
        .unwrap_or_else(|e| panic!("load native DB fixture at {}: {e}", path.display()))
}

fn errors(src: &str) -> Vec<String> {
    let dialect = Dialect::DEFAULT;
    let tree = gd_syntax::parse_with_options(
        src,
        &ParseOptions {
            dialect,
            script_path: "",
        },
    )
    .tree;
    let db = native_db();
    let policy = WarnPolicy::build(
        &WarningConfig::default(),
        &StrictSettings::default(),
        dialect,
    );
    analyze_with_options(
        &tree,
        Some(FileId::new(1)),
        "a.gd",
        &db,
        &NoCrossFile,
        &policy,
        AnalyzeOptions {
            dialect,
            ..Default::default()
        },
    )
    .diagnostics
    .iter()
    .filter(|d| d.warning_code().is_none())
    .map(|d| d.message().to_string())
    .collect()
}

/// Godot pushes the call error, returns false, and then hands the callee to
/// `reduce_identifier_from_base` — which is where the property line comes from. Both, in order.
#[test]
fn a_call_on_an_enum_value_draws_both_lines() {
    assert_eq!(
        errors("extends Node\nenum E { A, B }\nfunc f() -> void:\n\tE.A.foo()\n"),
        vec![
            "Cannot call function on enum value.".to_owned(),
            "Cannot get property from enum value.".to_owned(),
        ]
    );
}

/// The enum TYPE keeps its methods — Godot treats it as a dictionary. Only the value is rejected.
#[test]
fn a_call_on_the_enum_type_itself_is_not_the_value_error() {
    let e = errors("extends Node\nenum E { A, B }\nfunc f() -> void:\n\tprint(E.keys())\n");
    assert!(
        !e.iter().any(|m| m == "Cannot call function on enum value."),
        "the enum type is not an enum value: {e:?}"
    );
}

/// A bare `super()` has no method to name from inside a lambda. The gate is the lambda cursor at
/// the call site, so the follow-on that gdls used to report alone now has its explanation above it.
#[test]
fn a_bare_super_call_inside_a_lambda_is_an_error() {
    assert_eq!(
        errors("extends Node\nfunc f() -> void:\n\tvar c := func() -> void: super()\n\tc.call()\n"),
        vec![
            "Cannot use `super()` inside a lambda.".to_owned(),
            r#"Function "<anonymous>()" not found in base Node."#.to_owned(),
        ]
    );
}

/// Two shapes that must stay silent: `super()` in the method itself, even when that method also
/// declares a lambda, and `super.foo()` — which carries a callee and names a real parent method.
#[test]
fn a_super_call_outside_a_lambda_stays_silent() {
    let sibling = errors(
        "extends Node\nfunc _ready() -> void:\n\tsuper()\n\tvar c := func() -> void: pass\n\tc.call()\n",
    );
    assert!(
        !sibling
            .iter()
            .any(|m| m == "Cannot use `super()` inside a lambda."),
        "declaring a lambda elsewhere in the method does not put the `super()` inside one: {sibling:?}"
    );
    let qualified =
        errors("extends Node\nfunc _ready() -> void:\n\tvar c := func() -> void: super._ready()\n\tc.call()\n");
    assert!(
        !qualified
            .iter()
            .any(|m| m == "Cannot use `super()` inside a lambda."),
        "a qualified super call carries a callee and is not the bare form: {qualified:?}"
    );
}

/// A dictionary pattern matches its key by value, so the key has to be known at analysis time.
#[test]
fn a_non_constant_dictionary_pattern_key_is_an_error() {
    assert_eq!(
        errors("extends Node\nfunc f(x: Variant, k: int) -> void:\n\tmatch x:\n\t\t{k: 1}:\n\t\t\tpass\n"),
        vec!["Expression in dictionary pattern key must be a constant.".to_owned()]
    );
}

/// A literal key and a `const` key are both constant, and neither draws the error.
#[test]
fn a_constant_dictionary_pattern_key_stays_silent() {
    assert_eq!(
        errors("extends Node\nconst K := \"a\"\nfunc f(x: Variant) -> void:\n\tmatch x:\n\t\t{\"lit\": 1, K: 2}:\n\t\t\tpass\n"),
        Vec::<String>::new()
    );
}
