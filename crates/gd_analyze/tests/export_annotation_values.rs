//! The `@export*` per-argument value loop (`gdscript_parser.cpp:4680-4740`) — #371.
//!
//! Every `@export*` argument is rendered and then checked: it must not be empty and must not
//! contain a comma (both skipped for `@export_placeholder`, whose argument IS free text), and
//! `@export_flags` additionally parses each argument as `name` or `name:value`. Upstream returns
//! from the whole apply on the first bad argument, so at most one of these fires per annotation.
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
            "classes": [
                {"name": "Object"},
                {"name": "RefCounted", "inherits": "Object"},
                {"name": "Resource", "inherits": "RefCounted"},
                {"name": "Node", "inherits": "Object"}
            ]
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

/// The two checks every `@export*` but `@export_placeholder` runs, on the annotation real projects
/// hit them with.
#[test]
fn an_empty_or_comma_bearing_argument_is_rejected() {
    assert_eq!(
        errors("extends Node\n@export_enum(\"\") var e: int = 0\n"),
        vec![r#"Argument 1 of annotation "@export_enum" is empty."#]
    );
    assert_eq!(
        errors("extends Node\n@export_enum(\"a,b\") var e: int = 0\n"),
        vec![
            r#"Argument 1 of annotation "@export_enum" contains a comma. Use separate arguments instead."#
        ]
    );
}

/// `@export_placeholder`'s argument is the placeholder text, so neither check applies to it.
#[test]
fn export_placeholder_takes_any_text() {
    assert_eq!(
        errors("extends Node\n@export_placeholder(\"\") var p: String = \"\"\n"),
        Vec::<String>::new()
    );
    assert_eq!(
        errors("extends Node\n@export_placeholder(\"a, b\") var p: String = \"\"\n"),
        Vec::<String>::new()
    );
}

/// `@export_flags` parses each argument as `name` or `name:value`. Its five rejections, one per
/// row — upstream returns on the first, so each needs its own source.
#[test]
fn export_flags_checks_each_arguments_shape() {
    assert_eq!(
        errors("extends Node\n@export_flags(\":5\") var f: int = 0\n"),
        vec![r#"Invalid argument 1 of annotation "@export_flags": Expected flag name."#]
    );
    assert_eq!(
        errors("extends Node\n@export_flags(\"A:\") var f: int = 0\n"),
        vec![r#"Invalid argument 1 of annotation "@export_flags": Expected flag value."#]
    );
    assert_eq!(
        errors("extends Node\n@export_flags(\"A:x\") var f: int = 0\n"),
        vec![
            r#"Invalid argument 1 of annotation "@export_flags": The flag value must be a valid integer."#
        ]
    );
    assert_eq!(
        errors("extends Node\n@export_flags(\"A:0\") var f: int = 0\n"),
        vec![
            r#"Invalid argument 1 of annotation "@export_flags": The flag value must be at least 1 and at most 2 ** 32 - 1."#
        ]
    );
    // The upper end of the same range check.
    assert_eq!(
        errors("extends Node\n@export_flags(\"A:4294967296\") var f: int = 0\n"),
        vec![
            r#"Invalid argument 1 of annotation "@export_flags": The flag value must be at least 1 and at most 2 ** 32 - 1."#
        ]
    );
}

/// Past 32 implicit flags there is no bit left to assign, so the value has to be written out.
#[test]
fn export_flags_runs_out_of_implicit_bits_at_thirty_three() {
    let names: Vec<String> = (1..=34).map(|i| format!("\"F{i}\"")).collect();
    let src = format!(
        "extends Node\n@export_flags({}) var f: int = 0\n",
        names.join(", ")
    );
    assert_eq!(
        errors(&src),
        vec![
            r#"Invalid argument 33 of annotation "@export_flags": Starting from argument 33, the flag value must be specified explicitly."#
        ]
    );

    // Exactly 32 is fine, and so is a 33rd that names its own value.
    let ok: Vec<String> = (1..=32).map(|i| format!("\"F{i}\"")).collect();
    assert_eq!(
        errors(&format!(
            "extends Node\n@export_flags({}) var f: int = 0\n",
            ok.join(", ")
        )),
        Vec::<String>::new()
    );
    assert_eq!(
        errors(&format!(
            "extends Node\n@export_flags({}, \"F33:8\") var f: int = 0\n",
            ok.join(", ")
        )),
        Vec::<String>::new()
    );
}

/// An ordinary flags list passes, and a single explicit value passes — the control, so a green run
/// cannot just mean everything is rejected.
#[test]
fn a_well_formed_flags_list_is_accepted() {
    assert_eq!(
        errors("extends Node\n@export_flags(\"Fire\", \"Water:4\") var f: int = 0\n"),
        Vec::<String>::new()
    );
}

/// An argument whose TYPE was already rejected is not checked again by the value loop. The list the
/// loop reads is the folded prefix, truncated where resolution stopped, so the report stays single.
#[test]
fn a_type_rejected_argument_is_reported_once() {
    assert_eq!(
        errors("extends Node\n@export_flags(\"A\", \"B\", 0) var f: int = 0\n"),
        vec![
            r#"Invalid argument for annotation "@export_flags": argument 3 should be "String" but is "int"."#
        ]
    );
}

/// `@export*` is not one family. `@export_storage`, `@export_custom`, and `@export_tool_button`
/// each register their own apply, read their arguments positionally, and never build a hint string
/// out of them — so an empty or comma-bearing argument is legal there. Getting this wrong would
/// invent an error on `@export_custom(hint, "")`, which is ordinary code.
#[test]
fn the_annotations_with_their_own_apply_skip_the_value_loop() {
    assert_eq!(
        errors("extends Node\n@export_custom(0, \"\") var x: int = 1\n"),
        Vec::<String>::new()
    );
    assert_eq!(
        errors("extends Node\n@export_custom(0, \"a,b\") var x: int = 1\n"),
        Vec::<String>::new()
    );
}
