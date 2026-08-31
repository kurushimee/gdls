//! `@export_tool_button` (`gdscript_parser.cpp:5047-5091`) — #371.
//!
//! It registers its own apply, so it does not share the `@export*` order. The tool-script check
//! runs FIRST, ahead of the static and duplicate checks every other `@export*` leads with, and the
//! type check accepts only `Callable`. It also sets the "already exported" flag last rather than
//! first, so a rejected tool button does not make the next `@export*` a duplicate.
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

const NOT_TOOL: &str =
    r#"Tool buttons can only be used in tool scripts (add "@tool" to the top of the script)."#;

/// A tool button outside a tool script is rejected whatever else is wrong with it. The static row
/// is the ordering: every other `@export*` would report the static error, and this one does not.
#[test]
fn a_tool_button_needs_a_tool_script() {
    assert_eq!(
        errors("extends Node\n@export_tool_button(\"Go\") var b: Callable\n"),
        vec![NOT_TOOL.to_string()]
    );
    assert_eq!(
        errors("extends Node\n@export_tool_button(\"Go\") static var b: Callable\n"),
        vec![NOT_TOOL.to_string()]
    );
}

/// `@tool` is a property of the SCRIPT, so an inner class's tool button reads the head's flag.
#[test]
fn the_tool_flag_reaches_an_inner_class() {
    assert_eq!(
        errors(
            "@tool\nextends Node\nclass Inner:\n\t@export_tool_button(\"Go\") var b: Callable\n"
        ),
        Vec::<String>::new()
    );
    assert_eq!(
        errors("extends Node\nclass Inner:\n\t@export_tool_button(\"Go\") var b: Callable\n"),
        vec![NOT_TOOL.to_string()]
    );
}

/// Inside a tool script the static and duplicate checks apply as usual, in that order.
#[test]
fn a_tool_script_still_runs_the_static_and_duplicate_checks() {
    assert_eq!(
        errors("@tool\nextends Node\n@export_tool_button(\"Go\") static var b: Callable\n"),
        vec![
            r#"Annotation "@export_tool_button" cannot be applied to a static variable."#
                .to_string()
        ]
    );
    assert_eq!(
        errors("@tool\nextends Node\n@export @export_tool_button(\"Go\") var b: Callable\n"),
        vec![
            r#"Annotation "@export_tool_button" cannot be used with another "@export" annotation."#
                .to_string()
        ]
    );
}

/// The type check takes only `Callable`, and only when the type is hard and not Variant — an
/// untyped variable carries no claim to check.
#[test]
fn a_tool_button_must_be_a_callable() {
    assert_eq!(
        errors("@tool\nextends Node\n@export_tool_button(\"Go\") var b: int\n"),
        vec![
            r#""@export_tool_button" annotation requires a variable of type "Callable", but type "int" was given instead."#
                .to_string()
        ]
    );
    assert_eq!(
        errors("@tool\nextends Node\n@export_tool_button(\"Go\") var b: Callable\n"),
        Vec::<String>::new()
    );
    assert_eq!(
        errors("@tool\nextends Node\n@export_tool_button(\"Go\") var b\n"),
        Vec::<String>::new()
    );
}

/// None of the `@export*` value or type checks run on it — it reads its arguments positionally and
/// its own type check has already had the last word.
#[test]
fn the_export_value_and_type_loops_do_not_run() {
    assert_eq!(
        errors("@tool\nextends Node\n@export_tool_button(\"\", \"a,b\") var b: Callable\n"),
        Vec::<String>::new()
    );
}
