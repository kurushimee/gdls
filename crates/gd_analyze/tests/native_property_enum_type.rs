//! #428, #432 — a native property carries the type its getter returns, not the flattened one.
//!
//! Godot types a native member off the getter's `PropertyInfo`
//! (`gdscript_analyzer.cpp:4343-4350`), so `process_mode` is a `Node.ProcessMode` and every
//! message that names its type says so. The JSON dump's property row spells it `int`;
//! `gd_types::NativeDb` reads the getter back, and this is the analyzer-side proof.
//!
//! Pinned against `Godot_v4.7.2-stable --headless --check-only`.

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
        &StrictSettings {
            enable_warnings: vec![
                "INT_AS_ENUM_WITHOUT_CAST".to_owned(),
                "UNSAFE_PROPERTY_ACCESS".to_owned(),
                "UNSAFE_METHOD_ACCESS".to_owned(),
            ],
            ..Default::default()
        },
        Dialect::DEFAULT,
    )
}

fn diagnose(src: &str) -> (Vec<String>, Vec<String>) {
    let tree = parse(src).tree;
    let result = analyze(
        &tree,
        None,
        "res://main.gd",
        &native_db(),
        &NoCrossFile,
        &policy(),
    );
    let errors = result
        .diagnostics
        .iter()
        .filter(|d| d.severity() == Severity::Error)
        .map(|d| d.message().to_owned())
        .collect();
    let warnings = result
        .diagnostics
        .iter()
        .filter(|d| d.severity() == Severity::Warning)
        .map(|d| d.message().to_owned())
        .collect();
    (errors, warnings)
}

/// `Name "%s" called as a function but is a "%s".` names the enum, where it used to say `int`.
#[test]
fn a_value_callable_message_names_the_property_enum() {
    let (errors, _) = diagnose("extends Node\n\nfunc f() -> void:\n\tself.process_mode()\n");
    assert_eq!(
        errors,
        vec![r#"Name "process_mode" called as a function but is a "Node.ProcessMode"."#]
    );
}

/// The enum type reaches the assignment check too, so a bare int into the slot warns.
#[test]
fn an_int_assigned_to_the_property_warns_without_a_cast() {
    let (errors, warnings) = diagnose("extends Node\n\nfunc f() -> void:\n\tprocess_mode = 1\n");
    assert_eq!(errors, Vec::<String>::new());
    assert_eq!(
        warnings,
        vec![
            "Integer used when an enum value is expected. If this is intended, cast the integer \
             to the enum type using the \"as\" keyword."
        ]
    );
}

/// An enum member of the same enum assigns cleanly, and the inferred variable keeps the enum type.
#[test]
fn an_enum_member_assigns_cleanly_and_the_read_keeps_the_enum() {
    let (errors, warnings) = diagnose(
        "extends Node\n\nfunc f() -> void:\n\tprocess_mode = Node.PROCESS_MODE_ALWAYS\n\t\
         var m: Node.ProcessMode = process_mode\n\tprint(m)\n",
    );
    assert_eq!(errors, Vec::<String>::new());
    assert_eq!(warnings, Vec::<String>::new());
}

/// #432: the disagreement that was a live false positive. `SceneTree.root` is `Node` in the
/// property table and `Window` from `get_root`, so a `Window` method called on it used to draw
/// `The method "move_to_center()" is not present on the inferred type "Node" …` where Godot is
/// silent.
#[test]
fn a_narrowing_getter_takes_a_window_member_off_the_unsafe_path() {
    let (errors, warnings) =
        diagnose("extends Node\n\nfunc f() -> void:\n\tget_tree().root.move_to_center()\n");
    assert_eq!(errors, Vec::<String>::new());
    assert_eq!(warnings, Vec::<String>::new());
}

/// The same read through the property, so the type itself is on the record.
#[test]
fn the_scene_trees_root_is_a_window() {
    let (errors, warnings) = diagnose("extends Node\n\nfunc f() -> void:\n\tget_tree().root()\n");
    assert_eq!(
        errors,
        vec![r#"Name "root" called as a function but is a "Window"."#]
    );
    assert_eq!(warnings, Vec::<String>::new());
}
