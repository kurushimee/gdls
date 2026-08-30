//! The per-annotation apply chain for class member variables — gdscript_parser.cpp's
//! `onready_annotation` (:4527) and the `@export*` family prologue (:4660), driven in source
//! order the way gdscript_analyzer.cpp:1056-1061 drives it. Each row here is pinned against
//! `godot --headless --check-only` at 4.7.2-stable.

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

/// Analyze `src` and return every error message in emission order.
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

#[test]
fn a_second_onready_on_one_variable_is_rejected() {
    assert_eq!(
        errors("extends Node\n@onready @onready var x = 1\n"),
        vec![r#""@onready" annotation can only be used once per variable."#]
    );
}

#[test]
fn every_onready_past_the_first_reports_its_own_error() {
    assert_eq!(
        errors("extends Node\n@onready @onready @onready var x = 1\n"),
        vec![
            r#""@onready" annotation can only be used once per variable."#,
            r#""@onready" annotation can only be used once per variable."#,
        ]
    );
}

#[test]
fn onready_on_a_static_variable_is_rejected() {
    assert_eq!(
        errors("extends Node\n@onready static var x = 1\n"),
        vec![r#""@onready" annotation cannot be applied to a static variable."#]
    );
}

#[test]
fn a_second_export_names_the_annotation_that_lost() {
    assert_eq!(
        errors("extends Node\n@export_range(0, 10) @export var x: int = 1\n"),
        vec![r#"Annotation "@export" cannot be used with another "@export" annotation."#]
    );
    assert_eq!(
        errors("extends Node\n@export_custom(0, \"\") @export_range(0, 1) var x: int = 1\n"),
        vec![r#"Annotation "@export_range" cannot be used with another "@export" annotation."#]
    );
}

#[test]
fn export_storage_counts_as_an_export_annotation() {
    assert_eq!(
        errors("extends Node\n@export_storage @export_storage var x: int = 1\n"),
        vec![r#"Annotation "@export_storage" cannot be used with another "@export" annotation."#]
    );
}

#[test]
fn a_rejected_export_leaves_the_flag_clear_so_the_next_one_fails_too() {
    // Godot reports this twice: neither apply reaches `variable->exported = true`.
    assert_eq!(
        errors("extends Node\n@export @export_storage static var x: int = 1\n"),
        vec![
            r#"Annotation "@export" cannot be applied to a static variable."#,
            r#"Annotation "@export_storage" cannot be applied to a static variable."#,
        ]
    );
}

#[test]
fn simple_export_needs_a_type_or_an_initializer() {
    assert_eq!(
        errors("extends Node\n@export var thing\n"),
        vec![
            r#"Cannot use simple "@export" annotation with variable without type or initializer, since type can't be inferred."#
        ]
    );
}

#[test]
fn simple_export_accepts_a_bare_initializer() {
    assert!(errors("extends Node\n@export var thing = null\n").is_empty());
    assert!(
        errors("extends Node\nfunc f() -> int:\n\treturn 1\n@export var thing = f()\n").is_empty()
    );
}

#[test]
fn onready_outside_a_node_class_is_rejected_before_the_static_check() {
    assert_eq!(
        errors("extends Resource\n@onready static var x = 1\n"),
        vec![r#""@onready" can only be used in classes that inherit "Node"."#]
    );
}

#[test]
fn separate_variables_keep_separate_flags() {
    assert!(errors("extends Node\n@export var a: int = 1\n@export var b: int = 2\n").is_empty());
}
