//! The `@export*` TYPE checks (`gdscript_parser.cpp:4744-4965`) — #371.
//!
//! After the argument loop, Godot compares the variable's own type against what the annotation can
//! export. `@export` runs a kind switch over the type (twice, for a typed `Dictionary`); every
//! other `@export*` compares against the builtin type its registration pinned, with a float/int
//! tolerance. gdls drops the `export_info` half of that code — a language server has no consumer
//! for a property hint — and keeps the errors and the control flow that reaches them.
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

const ONLY_BUILTIN: &str = "Export type can only be built-in, a resource, a node, or an enum.";

/// The `@export` kind switch's object leg: a type that reaches neither `Resource` nor `Node` is
/// not exportable. The element and dictionary passes run the same switch, so each container shape
/// needs its own row.
#[test]
fn a_non_resource_non_node_object_is_not_exportable() {
    for src in [
        "extends Node\n@export var x: RefCounted\n",
        "extends Node\n@export var a: Array[RefCounted]\n",
        "extends Node\n@export var d: Dictionary[String, RefCounted]\n",
        "extends Node\n@export var d: Dictionary[RefCounted, int]\n",
    ] {
        assert_eq!(errors(src), vec![ONLY_BUILTIN.to_string()], "{src}");
    }
}

/// A `Resource`, a `Node`, a builtin, an enum, and an unparameterized container all pass the
/// switch. The dictionary row's value leg is a builtin, which upstream never rejects.
#[test]
fn an_exportable_type_is_silent() {
    for src in [
        "extends Node\n@export var r: Resource\n",
        "extends Node\n@export var n: Node\n",
        "extends Node\n@export var i: int = 0\n",
        "extends Node\n@export var arr: Array\n",
        "extends Node\nenum E { A, B }\n@export var e: E\n",
        "extends Node\n@export var d: Dictionary[String, int]\n",
    ] {
        assert_eq!(errors(src), Vec::<String>::new(), "{src}");
    }
}

/// A Node-typed export needs a Node-derived class — including when the Node-ness comes from the
/// dictionary VALUE leg, which runs the check a second time.
#[test]
fn a_node_export_outside_a_node_class_is_rejected() {
    let expected =
        vec![r#"Node export is only supported in Node-derived classes, but the current class inherits "RefCounted"."#.to_string()];
    assert_eq!(
        errors("extends RefCounted\n@export var n: Node\n"),
        expected
    );
    assert_eq!(
        errors("extends RefCounted\n@export var d: Dictionary[String, Node]\n"),
        expected
    );
}

/// `@export_multiline` takes `String` or `Dictionary`. The message prints the variable's OWN type,
/// so a parameterized dictionary reports with its parameters even though the check reads the key.
#[test]
fn export_multiline_takes_a_string_or_a_dictionary() {
    let expect = |given: &str| {
        vec![format!(
            r#""@export_multiline" annotation requires a variable of type "String", "Array[String]", "PackedStringArray", "Dictionary", or "Array[Dictionary]", but type "{given}" was given instead."#
        )]
    };
    assert_eq!(
        errors("extends Node\n@export_multiline var x: int\n"),
        expect("int")
    );
    assert_eq!(
        errors("extends Node\n@export_multiline var x\n"),
        expect("Variant")
    );
    assert_eq!(
        errors("extends Node\n@export_multiline var d: Dictionary[int, String]\n"),
        expect("Dictionary[int, String]")
    );
    assert_eq!(
        errors("extends Node\n@export_multiline var s: String = \"\"\n"),
        Vec::<String>::new()
    );
}

/// `@export_enum` takes `int` or `String`, and its message joins two expected types — eight names
/// after the packed-array expansion.
#[test]
fn export_enum_takes_an_int_or_a_string() {
    assert_eq!(
        errors("extends Node\n@export_enum(\"A\", \"B\") var x: float\n"),
        vec![
            r#""@export_enum" annotation requires a variable of type "int", "Array[int]", "PackedByteArray", "PackedInt32Array", "PackedInt64Array", "String", "Array[String]", or "PackedStringArray", but type "float" was given instead."#
                .to_string()
        ]
    );
    assert_eq!(
        errors("extends Node\n@export_enum(\"A\") var s: String = \"\"\n"),
        Vec::<String>::new()
    );
}

/// The tail check against the `t_type` each `export_annotations<...>` registration pinned. One row
/// per list length so the 2-name, 3-name, 4-name, and 5-name joins are all pinned.
#[test]
fn every_other_export_checks_its_registered_builtin_type() {
    let rows = [
        (
            "extends Node\n@export_node_path var np: int\n",
            r#""@export_node_path" annotation requires a variable of type "NodePath" or "Array[NodePath]", but type "int" was given instead."#,
        ),
        (
            "extends Node\n@export_file var i: int\n",
            r#""@export_file" annotation requires a variable of type "String", "Array[String]", or "PackedStringArray", but type "int" was given instead."#,
        ),
        (
            "extends Node\n@export_color_no_alpha var c: Vector2\n",
            r#""@export_color_no_alpha" annotation requires a variable of type "Color", "Array[Color]", or "PackedColorArray", but type "Vector2" was given instead."#,
        ),
        (
            "extends Node\n@export_range(0, 10) var s: String = \"\"\n",
            r#""@export_range" annotation requires a variable of type "float", "Array[float]", "PackedFloat32Array", or "PackedFloat64Array", but type "String" was given instead."#,
        ),
        (
            "extends Node\n@export_flags(\"A\") var s: String = \"\"\n",
            r#""@export_flags" annotation requires a variable of type "int", "Array[int]", "PackedByteArray", "PackedInt32Array", or "PackedInt64Array", but type "String" was given instead."#,
        ),
        (
            "extends Node\n@export_file var n: Node\n",
            r#""@export_file" annotation requires a variable of type "String", "Array[String]", or "PackedStringArray", but type "Node" was given instead."#,
        ),
    ];
    for (src, message) in rows {
        assert_eq!(errors(src), vec![message.to_string()], "{src}");
    }
}

/// The tolerances the tail check carries: int against a float registration, and the element type of
/// an `Array[T]` or a packed array standing in for `T`.
#[test]
fn the_tail_check_tolerates_int_float_and_container_elements() {
    for src in [
        "extends Node\n@export_range(0, 10) var n: int = 0\n",
        "extends Node\n@export_file var a: Array[String]\n",
        "extends Node\n@export_file var p: PackedStringArray\n",
        "extends Node\n@export_range(0, 10) var q: PackedInt32Array\n",
        "extends Node\n@export_range(0, 10) var v\n",
    ] {
        assert_eq!(errors(src), Vec::<String>::new(), "{src}");
    }
}

/// The three `@export*` names that register their own apply never reach any of this. Running the
/// checks on them would invent errors on ordinary code — `@export_storage var n: Node` in a
/// `RefCounted` is legal, and so is any type under `@export_custom`.
#[test]
fn the_annotations_with_their_own_apply_skip_the_type_checks() {
    for src in [
        "extends RefCounted\n@export_storage var n: Node\n",
        "extends RefCounted\n@export_custom(0, \"\") var n: Node\n",
        "extends Node\n@export_storage var x: RefCounted\n",
    ] {
        assert_eq!(errors(src), Vec::<String>::new(), "{src}");
    }
}

/// fail-open: a native class the loaded dump does not carry answers `false` to BOTH the `Resource`
/// and the `Node` probe, which is the shape of the "not exportable" leg. Without the
/// class-is-known gate, an API dump missing a GDExtension class would invent the error on every
/// `@export` of one.
#[test]
fn an_unknown_native_class_does_not_reach_the_object_leg() {
    let db = NativeDb::from_json(
        r#"{
            "header": {"version_major": 4, "version_minor": 7, "version_patch": 2},
            "classes": [{"name": "Object"}, {"name": "Node", "inherits": "Object"}]
        }"#,
    )
    .expect("valid mini dump");
    let src = "extends Node\n@export var x: RefCounted\n";
    let tree = gd_syntax::parse(src).tree;
    let policy = WarnPolicy::build(
        &WarningConfig::default(),
        &StrictSettings::default(),
        Dialect::DEFAULT,
    );
    let result = gd_analyze::analyze(&tree, None, "t.gd", &db, &NoCrossFile, &policy);
    let errs: Vec<_> = result
        .diagnostics
        .iter()
        .filter(|d| d.warning_code().is_none())
        .map(|d| d.message().to_owned())
        .collect();
    assert!(!errs.contains(&ONLY_BUILTIN.to_string()), "{errs:?}");
}
