//! `reduce_subscript`'s index-validity and result-type tables (analyzer.cpp:4951-5145).
//!
//! Indexing a builtin has a known element type for most bases — `PackedByteArray[i]` is an int,
//! `PackedVector2Array[i]` a Vector2, `String[i]` a String — and the result is exactly as hard as
//! the base. gdls stamped only the `Array[T]` row and left the rest to the silent-Variant tail
//! guard, which under-hard-typed every packed-array element. Nothing read hardness there, so it
//! stayed invisible; it surfaces the moment anything does.
//!
//! The index table is the same slice from the other side: which index types a base accepts. Only
//! the `Array` row was ported, so `Rect2()[0]` — a base that takes a String key — passed silently.
//!
//! Every row is pinned against `godot --headless --check-only` at 4.7.2.

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

fn errors(src: &str, dialect: Dialect) -> Vec<String> {
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
        "sub.gd",
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
    .filter(|d| d.severity() == gd_analyze::Severity::Error)
    .map(|d| d.message().to_string())
    .collect()
}

const TAGS: [Dialect; 2] = [Dialect::Godot4_6, Dialect::Godot4_7];

fn script(body: &str) -> String {
    format!("extends Node\n\nfunc f() -> void:\n{body}\n")
}

/// A deliberately-wrong assignment is what makes the computed element type visible: the message
/// names it. Each row is `(the base expression, the element type Godot computes)`.
#[test]
fn indexing_a_builtin_yields_its_documented_element_type() {
    let rows = [
        ("PackedByteArray([1])", "int"),
        ("PackedInt32Array([1])", "int"),
        ("PackedInt64Array([1])", "int"),
        ("PackedFloat32Array([1.0])", "float"),
        ("PackedFloat64Array([1.0])", "float"),
        ("PackedStringArray([\"a\"])", "String"),
        ("PackedVector2Array([Vector2.ZERO])", "Vector2"),
        ("PackedVector3Array([Vector3.ZERO])", "Vector3"),
        ("PackedVector4Array([Vector4.ZERO])", "Vector4"),
        ("PackedColorArray([Color.RED])", "Color"),
        ("\"hi\"", "String"),
        ("Vector2.ONE", "float"),
        ("Vector2i(1, 1)", "int"),
        ("Vector3.ONE", "float"),
        ("Vector3i(1, 1, 1)", "int"),
        ("Basis()", "Vector3"),
        ("Transform2D()", "Vector2"),
    ];
    for d in TAGS {
        for (base, elem) in rows {
            assert_eq!(
                errors(
                    &script(&format!(
                        "\tvar a := {base}\n\tvar x: Node = a[0]\n\tprint(x)"
                    )),
                    d
                ),
                vec![format!(
                    r#"Cannot assign a value of type {elem} to variable "x" with specified type Node."#
                )],
                "{base} at {d:?}"
            );
        }
    }
}

/// Rows whose element type genuinely depends on the index stay Variant (analyzer.cpp:5116-5123),
/// so a wrong-looking assignment out of one is silent — the same as upstream.
#[test]
fn an_index_dependent_base_stays_variant() {
    for d in TAGS {
        for base in ["Color.RED", "Transform3D()"] {
            assert_eq!(
                errors(
                    &script(&format!(
                        "\tvar a := {base}\n\tvar x: Node = a[\"x\"]\n\tprint(x)"
                    )),
                    d
                ),
                Vec::<String>::new(),
                "{base} at {d:?}"
            );
        }
    }
}

/// A typed container yields its element type; an untyped one stays Variant.
#[test]
fn a_typed_container_yields_its_element_type() {
    for d in TAGS {
        assert_eq!(
            errors(
                &script("\tvar a: Array[int] = [1]\n\tvar x: Node = a[0]\n\tprint(x)"),
                d
            ),
            vec![
                r#"Cannot assign a value of type int to variable "x" with specified type Node."#
                    .to_owned()
            ],
            "{d:?}"
        );
        assert_eq!(
            errors(
                &script(
                    "\tvar a: Dictionary[String, int] = {}\n\tvar x: Node = a[\"k\"]\n\tprint(x)"
                ),
                d
            ),
            vec![
                r#"Cannot assign a value of type int to variable "x" with specified type Node."#
                    .to_owned()
            ],
            "{d:?}"
        );
        for body in [
            "\tvar a := [1]\n\tvar i := 0\n\tvar x: Node = a[i]\n\tprint(x)",
            "\tvar a := {}\n\tvar k := \"k\"\n\tvar x: Node = a[k]\n\tprint(x)",
        ] {
            assert_eq!(
                errors(&script(body), d),
                Vec::<String>::new(),
                "{body} at {d:?}"
            );
        }
    }
}

/// The index table: a base that wants a String key rejects an int one, and vice versa.
#[test]
fn a_base_rejects_an_index_type_it_does_not_accept() {
    let int_indexed = [
        ("Rect2()", "Rect2"),
        ("AABB()", "AABB"),
        ("Quaternion()", "Quaternion"),
    ];
    for d in TAGS {
        for (base, name) in int_indexed {
            let got = errors(
                &script(&format!("\tvar a := {base}\n\tvar x = a[0]\n\tprint(x)")),
                d,
            );
            assert_eq!(
                got.first().map(String::as_str),
                Some(format!(r#"Invalid index type "int" for a base of type "{name}"."#).as_str()),
                "{base} at {d:?} (got {got:?})"
            );
        }
        // A packed array wants a number, not a String.
        assert_eq!(
            errors(
                &script("\tvar a := PackedByteArray([1])\n\tvar x = a[\"k\"]\n\tprint(x)"),
                d
            ),
            vec![r#"Invalid index type "String" for a base of type "PackedByteArray"."#.to_owned()],
            "{d:?}"
        );
        // A Vector takes either, and a Color takes an int or a String — no complaint.
        for body in [
            "\tvar a := Vector2.ONE\n\tvar x = a[\"x\"]\n\tprint(x)",
            "\tvar a := Vector2.ONE\n\tvar x = a[0]\n\tprint(x)",
            "\tvar a := Color.RED\n\tvar x = a[0]\n\tprint(x)",
        ] {
            assert_eq!(
                errors(&script(body), d),
                Vec::<String>::new(),
                "{body} at {d:?}"
            );
        }
    }
}
