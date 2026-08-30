//! #376 — the two subscript errors that sat beside tables gdls had already ported.
//!
//! `reduce_subscript` carried Godot's can't-index base list and its index-type table but pushed no
//! diagnostic from either; the comment in the code said as much. Indexing an `int` and indexing a
//! `Node` by number both passed in silence.
//!
//! Godot's third subscript error, `Cannot get index "X" from "Y".` (`gdscript_analyzer.cpp:4926`),
//! is not here. It fires only when the base AND the index are both constant, and its message
//! interpolates Godot's `Variant` rendering of both values — `{ "a": 1 }` for a dictionary. gdls's
//! fold table has no collection representation to render, so the message cannot be produced
//! faithfully for the shapes that actually reach it.
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

fn errors(body: &str) -> Vec<String> {
    let src = format!("extends Node\nfunc f(n: Node, v: Variant, s: String) -> void:\n{body}");
    let dialect = Dialect::DEFAULT;
    let tree = gd_syntax::parse_with_options(
        &src,
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

#[test]
fn a_base_that_cannot_be_indexed_at_all_is_an_error() {
    assert_eq!(
        errors("\tvar i := 5\n\tprint(i[\"a\"])\n"),
        vec![r#"Cannot use subscript operator on a base of type "int"."#.to_owned()]
    );
    assert_eq!(
        errors("\tvar b := true\n\tprint(b[0])\n"),
        vec![r#"Cannot use subscript operator on a base of type "bool"."#.to_owned()]
    );
}

#[test]
fn an_object_base_takes_only_a_string_index() {
    assert_eq!(
        errors("\tprint(n[1])\n"),
        vec![
            r#"Only "String" or "StringName" can be used as index for type "Node", but received "int"."#
                .to_owned()
        ]
    );
}

/// The shapes that must stay silent: a name index on an object, a `Variant` index (the
/// gradual-typing fallback is indistinguishable from an unannotated string), and the two
/// collection bases that index normally.
#[test]
fn the_legal_subscripts_draw_nothing() {
    assert_eq!(
        errors("\tprint(n[\"ok\"])\n\tprint(n[v])\n\tprint(n[s])\n"),
        Vec::<String>::new()
    );
    assert_eq!(
        errors("\tvar arr := [1, 2]\n\tprint(arr[0])\n\tvar d := {\"k\": 1}\n\tprint(d[\"k\"])\n"),
        Vec::<String>::new()
    );
}

/// The guard that opens Godot's whole block (analyzer.cpp:4937): a `Variant` base is unsafe, not
/// wrong, so nothing about its index is checked. Dropping this fired 241 false errors across the
/// Pixelorama acceptance project, where `v[0]` on an untyped value is ordinary code.
#[test]
fn a_variant_base_is_never_index_checked() {
    assert_eq!(
        errors("\tprint(v[0])\n\tprint(v[true])\n"),
        Vec::<String>::new()
    );
    assert_eq!(
        errors("\tvar u = n.get(\"x\")\n\tprint(u[0])\n"),
        Vec::<String>::new()
    );
}
