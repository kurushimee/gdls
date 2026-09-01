//! #594 — where two analyzer errors attach.
//!
//! Both were being pushed on a child of the node Godot passes, so their range covered less of the
//! source than upstream's does. The messages were already identical; only the extent moved.
//!
//! * `Could not find type "X" in the current scope.` goes on `p_type`, the whole `TypeNode`
//!   (`gdscript_analyzer.cpp:904`). That node starts at the token *before* the name, because
//!   `alloc_node` resets a new node's extents to `previous` (`gdscript_parser.h:1478`) and
//!   `parse_type` allocates right after the caller consumed the `:`, `is`, `as`, or `->`.
//! * `Name "X" called as a function but is a "T".` resolves through the attribute identifier but
//!   reports on `p_call->callee`, the whole subscript (`:3747`).
//!
//! Every expected column here is what the Godot 4.7.2 editor LSP reports for the same source.

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

/// Every error as `(message, the source text its span covers)`. Comparing the covered text rather
/// than raw offsets is what makes a failure readable: the wrong anchor shows up as the wrong
/// substring.
fn anchored(src: &str) -> Vec<(String, String)> {
    let tree = parse(src).tree;
    let db = native_db();
    let policy = WarnPolicy::build(
        &gd_project::WarningConfig::default(),
        &StrictSettings::default(),
        Dialect::DEFAULT,
    );
    let result = analyze(&tree, None, "res://main.gd", &db, &NoCrossFile, &policy);
    result
        .diagnostics
        .iter()
        .filter(|d| d.severity() == Severity::Error)
        .map(|d| {
            let span = d.span();
            (d.message().to_owned(), src[span.start..span.end].to_owned())
        })
        .collect()
}

fn missing_type(name: &str) -> String {
    format!(r#"Could not find type "{name}" in the current scope."#)
}

#[test]
fn a_missing_type_covers_the_token_that_introduces_it() {
    let src =
        "extends Node\n\nfunc f(n: Node) -> void:\n\tvar a: NoSuchType = null\n\tprint(n, a)\n";
    assert_eq!(
        anchored(src),
        vec![(missing_type("NoSuchType"), ": NoSuchType".to_owned())],
        "the declaration's `:` opens the TypeNode"
    );

    let src = "extends Node\n\nfunc f(n: Node) -> void:\n\tif n is NotAClass:\n\t\tpass\n";
    assert_eq!(
        anchored(src),
        vec![(missing_type("NotAClass"), "is NotAClass".to_owned())],
        "a type test opens it at the `is`"
    );

    let src = "extends Node\n\nfunc f(n: Node) -> void:\n\tprint(n as NotAClass2)\n";
    assert_eq!(
        anchored(src),
        vec![(missing_type("NotAClass2"), "as NotAClass2".to_owned())],
        "a cast opens it at the `as`"
    );

    let src = "extends Node\n\nfunc f() -> NoSuchReturn:\n\treturn null\n";
    assert_eq!(
        anchored(src),
        vec![(missing_type("NoSuchReturn"), "-> NoSuchReturn".to_owned())],
        "a return annotation opens it at the `->`"
    );
}

/// A parameter's type has no introducing token of its own beyond the `:`, and a bare `extends`
/// takes the identifier itself — both already matched upstream and must not move.
#[test]
fn the_other_type_positions_are_unchanged() {
    let src = "extends Node\n\nfunc f(p: NoSuchParam) -> void:\n\tprint(p)\n";
    assert_eq!(
        anchored(src),
        vec![(missing_type("NoSuchParam"), ": NoSuchParam".to_owned())]
    );
}

#[test]
fn a_name_called_as_a_function_covers_the_whole_callee() {
    let src = "extends Node\n\nfunc f() -> void:\n\tprint(Vector2.ZERO())\n";
    let got = anchored(src);
    assert!(
        got.contains(&(
            r#"Name "ZERO" called as a function but is a "Vector2"."#.to_owned(),
            "Vector2.ZERO".to_owned(),
        )),
        "an attribute callee reports on the whole subscript; got {got:?}"
    );

    // A bare identifier callee is its own whole callee, so it stays put.
    let src = "extends Node\n\nconst K := 3\n\nfunc f() -> void:\n\tK()\n";
    let got = anchored(src);
    assert!(
        got.contains(&(
            r#"Name "K" called as a function but is a "int"."#.to_owned(),
            "K".to_owned(),
        )),
        "got {got:?}"
    );
}
