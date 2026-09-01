//! #603 — six analyzer diagnostics that attached to a different node than Godot's.
//!
//! None of this is visible to the conformance ratchet: a `.out` golden records the message and
//! the line, never the column, so a diagnostic can sit on the wrong half of a line and still
//! score. Every expected extent below is what the Godot 4.7.2 editor LSP reports for the same
//! source, read off a full-range diff of the analyzer corpus.

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

/// Every diagnostic as `(message, the source text its span covers, its start offset)`. The
/// covered text is what makes a failure readable; the offset is there for the cases where two
/// candidate anchors spell the same thing.
fn anchored(src: &str) -> Vec<(String, String, usize)> {
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
        .map(|d| {
            let span = d.span();
            (
                d.message().to_owned(),
                src[span.start..span.end].to_owned(),
                span.start,
            )
        })
        .collect()
}

/// The covered text of the one diagnostic whose message starts with `prefix`.
fn covered(src: &str, prefix: &str) -> String {
    let hits: Vec<_> = anchored(src)
        .into_iter()
        .filter(|(m, _, _)| m.starts_with(prefix))
        .collect();
    assert_eq!(
        hits.len(),
        1,
        "expected exactly one {prefix:?}, got {hits:?}"
    );
    hits[0].1.clone()
}

#[test]
fn a_bad_utility_argument_covers_the_argument() {
    let src = "func test():\n\tprint(floor(Color()))\n\tprint(len(Color()))\n";
    assert_eq!(covered(src, r#"Invalid argument for "floor()""#), "Color()");
    assert_eq!(covered(src, r#"Invalid argument for "len()""#), "Color()");
}

#[test]
fn a_redefined_enum_value_covers_the_enum_that_holds_it() {
    let src =
        "class A:\n\tenum { V }\n\nclass B extends A:\n\tenum { V }\n\nfunc test():\n\tpass\n";
    assert_eq!(covered(src, r#"The member "V""#), "enum { V }");
}

#[test]
fn a_redefined_enum_value_with_a_custom_value_covers_that_value() {
    let src =
        "class A:\n\tenum { V }\n\nclass B extends A:\n\tenum { V = 7 }\n\nfunc test():\n\tpass\n";
    assert_eq!(covered(src, r#"The member "V""#), "7");
}

#[test]
fn a_rest_parameter_error_covers_its_type_specifier() {
    let bad = "func g(...args: int):\n\tpass\n\nfunc test():\n\tpass\n";
    assert_eq!(covered(bad, "The rest parameter type"), ": int");

    let typed = "func h(...args: Array[int]):\n\tpass\n\nfunc test():\n\tpass\n";
    assert_eq!(covered(typed, "Typed arrays are currently"), ": Array[int]");
}

#[test]
fn the_abstract_native_callable_companion_covers_the_callee() {
    let src = "func test():\n\tInstancePlaceholder.new()\n";
    assert_eq!(
        covered(src, r#"Name "new" is a Callable"#),
        "InstancePlaceholder.new"
    );
    // The error it accompanies keeps the whole call, which is the node Godot passes for it.
    assert_eq!(covered(src, "Native class"), "InstancePlaceholder.new()");
}

#[test]
fn a_member_cycle_reached_through_a_call_covers_the_call() {
    let src = "static func func_0(p := func_0()) -> int:\n\treturn 0\n";
    assert_eq!(covered(src, "Could not resolve member"), "func_0()");
}

#[test]
fn a_confusable_local_covers_the_read_not_the_declaration() {
    let src = "var a = 1\n\nfunc test():\n\tprint(a)\n\tvar a = a + 1\n\tprint(a)\n";
    let read_in_initializer = src.find("a + 1").expect("fixture has the initializer read");
    let declaration = src.find("var a = a").expect("fixture has the declaration") + 4;

    let confusable: Vec<_> = anchored(src)
        .into_iter()
        .filter(|(m, _, _)| m.starts_with(r#"The identifier "a""#))
        .map(|(_, _, at)| at)
        .collect();
    assert_eq!(
        confusable,
        vec![
            src.find("print(a)").expect("fixture prints first") + 6,
            read_in_initializer
        ],
        "both reads, in source order — never the declaration at {declaration}"
    );
}

#[test]
fn a_confusable_local_still_renders_before_the_shadowing_warning_it_precedes() {
    let src = "var a = 1\n\nfunc test():\n\tprint(a)\n\tvar a = a + 1\n\tprint(a)\n";
    let order: Vec<String> = anchored(src).into_iter().map(|(m, _, _)| m).collect();
    let confusable = order
        .iter()
        .rposition(|m| m.starts_with(r#"The identifier "a""#))
        .expect("confusable warning");
    let shadowed = order
        .iter()
        .position(|m| m.starts_with(r#"The local variable "a" is shadowing"#))
        .expect("shadowed warning");
    assert!(
        confusable < shadowed,
        "CONFUSABLE_LOCAL_USAGE comes first even though it now anchors further right: {order:?}"
    );
}

#[test]
fn the_anchors_do_not_change_which_diagnostics_fire() {
    let src = "func test():\n\tprint(len(Color()))\n";
    let messages: Vec<String> = anchored(src).into_iter().map(|(m, _, _)| m).collect();
    assert_eq!(
        messages,
        vec![
            r#"Invalid argument for "len()" function: Value of type 'Color' can't provide a length."#
                .to_owned()
        ]
    );
    let severities: Vec<Severity> = {
        let tree = parse(src).tree;
        let db = native_db();
        let policy = WarnPolicy::build(
            &gd_project::WarningConfig::default(),
            &StrictSettings::default(),
            Dialect::DEFAULT,
        );
        analyze(&tree, None, "res://main.gd", &db, &NoCrossFile, &policy)
            .diagnostics
            .iter()
            .map(|d| d.severity())
            .collect()
    };
    assert_eq!(severities, vec![Severity::Error]);
}
