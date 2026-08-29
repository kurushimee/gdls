//! Reproduce-first + regression net for #124: `reduce_get_node` must render the context-error
//! glyph from the access form — `$` for `$Node`, `%` for `%Unique` — matching Godot
//! `gdscript_analyzer.cpp:3869/3875` (`p_get_node->use_dollar ? '$' : '%'`). gdls previously
//! hardcoded `"$"`, so a `%Name` access in a static function / non-Node class printed `("$")`
//! where Godot prints `("%")`. No corpus `.out` covers `%` in these contexts (the conformance
//! ratchet cannot catch this), so these direct message assertions are the only coverage.

use gd_syntax::Dialect;
use std::path::Path;

use gd_analyze::{analyze, NoCrossFile, StrictSettings, WarnPolicy};
use gd_project::{FileId, WarningConfig};
use gd_syntax::parse;
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
        &StrictSettings::default(),
        Dialect::DEFAULT,
    )
}

/// All diagnostic messages produced for `src`.
fn messages(src: &str) -> Vec<String> {
    let tree = parse(src).tree;
    let db = native_db();
    let result = analyze(
        &tree,
        Some(FileId::new(1)),
        "gn.gd",
        &db,
        &NoCrossFile,
        &policy(),
    );
    result
        .diagnostics
        .iter()
        .map(|d| d.message().to_owned())
        .collect()
}

/// Does some diagnostic carry the get_node shorthand context error with the given glyph + context?
fn has_shorthand_error(src: &str, glyph: char, context: &str) -> bool {
    let needle = format!(r#"notation ("{glyph}")"#);
    messages(src)
        .iter()
        .any(|m| m.contains(&needle) && m.contains(context))
}

#[test]
fn percent_shorthand_in_static_function_uses_percent_glyph() {
    // `%Unique` in a static function of a Node-derived class → ONLY the static-function error,
    // and it must read ("%"), not ("$"). (Reproduce-first: fails on the hardcoded-"$" code.)
    let src = "\
extends Node

static func f():
\tvar _n = %Foo
";
    assert!(
        has_shorthand_error(src, '%', "in a static function"),
        "expected a %-glyph static-function shorthand error; got: {:?}",
        messages(src)
    );
}

#[test]
fn percent_shorthand_on_non_node_class_uses_percent_glyph() {
    // `%Unique` in a non-Node class → ONLY the not-a-node error, glyph ("%").
    let src = "\
extends RefCounted

func f():
\tvar _n = %Foo
";
    assert!(
        has_shorthand_error(src, '%', "isn't a node"),
        "expected a %-glyph not-a-node shorthand error; got: {:?}",
        messages(src)
    );
}

#[test]
fn dollar_shorthand_in_static_function_keeps_dollar_glyph() {
    // Regression guard: the `$` form must still print ("$").
    let src = "\
extends Node

static func f():
\tvar _a = $Node
";
    assert!(
        has_shorthand_error(src, '$', "in a static function"),
        "expected a $-glyph static-function shorthand error; got: {:?}",
        messages(src)
    );
}

#[test]
fn dollar_shorthand_on_non_node_class_keeps_dollar_glyph() {
    // Regression guard: the `$` form must still print ("$").
    let src = "\
extends RefCounted

func f():
\tvar _a = $Node
";
    assert!(
        has_shorthand_error(src, '$', "isn't a node"),
        "expected a $-glyph not-a-node shorthand error; got: {:?}",
        messages(src)
    );
}

#[test]
fn non_node_static_function_fires_only_the_non_node_error() {
    // A class that is BOTH non-Node AND inside a static function: Godot checks the non-Node case
    // first and early-returns (gdscript_analyzer.cpp:3868-3872), so EXACTLY ONE context error
    // fires — the not-a-node one, never also the static-function one. (Reproduce-first: the code
    // that checked both contexts without returning double-fired here.)
    let src = "\
extends RefCounted

static func f():
\tvar _n = $Node
";
    let shorthand: Vec<String> = messages(src)
        .into_iter()
        .filter(|m| m.contains(r#"shorthand "get_node()""#))
        .collect();
    assert_eq!(
        shorthand.len(),
        1,
        "exactly one get_node context error must fire (Godot early-returns); got: {shorthand:?}"
    );
    assert!(
        shorthand[0].contains("isn't a node"),
        "the single error must be the not-a-node one (checked first); got: {:?}",
        shorthand[0]
    );
}
