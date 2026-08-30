//! #428: a native property takes its structured type from its getter.
//!
//! Godot's analyzer types a native member off the getter's `PropertyInfo`
//! (`gdscript_analyzer.cpp:4343-4350`), which keeps the enum class name and the container element
//! types. The JSON dump's `properties[].type` flattens all of that to a bare `int` / `Array` /
//! `Dictionary`, so `NativeDb` reads the getter back over a plain named property type.

use gd_types::{NativeDb, TypeRef};

/// One class, every property/getter pairing the recovery has to decide on.
const DUMP: &str = r#"{
  "header": {
    "version_major": 4, "version_minor": 7, "version_patch": 2,
    "version_status": "stable", "version_full_name": "Godot Engine v4.7.2.stable.test"
  },
  "global_constants": [], "global_enums": [], "utility_functions": [],
  "builtin_classes": [], "singletons": [],
  "classes": [
    {
      "name": "Probe", "is_refcounted": false, "is_instantiable": true, "api_type": "core",
      "constants": [],
      "enums": [ { "name": "Mode", "is_bitfield": false,
                   "values": [ { "name": "MODE_A", "value": 0 }, { "name": "MODE_B", "value": 1 } ] } ],
      "signals": [],
      "properties": [
        { "name": "mode",     "type": "int",        "setter": "set_mode",     "getter": "get_mode",     "index": -1 },
        { "name": "items",    "type": "Array",      "setter": "set_items",    "getter": "get_items",    "index": -1 },
        { "name": "lookup",   "type": "Dictionary", "setter": "set_lookup",   "getter": "get_lookup",   "index": -1 },
        { "name": "count",    "type": "int",        "setter": "set_count",    "getter": "get_count",    "index": -1 },
        { "name": "texture",  "type": "Texture2D,-AtlasTexture", "setter": "set_texture", "getter": "get_texture", "index": -1 },
        { "name": "orphan",   "type": "int",        "setter": "set_orphan",   "getter": "get_orphan",   "index": -1 },
        { "name": "readonly", "type": "int" }
      ],
      "methods": [
        { "name": "get_mode", "is_const": true, "is_static": false, "is_vararg": false, "is_virtual": false,
          "hash": 1, "return_value": { "type": "enum::Probe.Mode" }, "arguments": [] },
        { "name": "get_items", "is_const": true, "is_static": false, "is_vararg": false, "is_virtual": false,
          "hash": 2, "return_value": { "type": "typedarray::int" }, "arguments": [] },
        { "name": "get_lookup", "is_const": true, "is_static": false, "is_vararg": false, "is_virtual": false,
          "hash": 3, "return_value": { "type": "typeddictionary::String;int" }, "arguments": [] },
        { "name": "get_count", "is_const": true, "is_static": false, "is_vararg": false, "is_virtual": false,
          "hash": 4, "return_value": { "type": "int" }, "arguments": [] },
        { "name": "get_texture", "is_const": true, "is_static": false, "is_vararg": false, "is_virtual": false,
          "hash": 5, "return_value": { "type": "Texture2D" }, "arguments": [] }
      ]
    }
  ]
}"#;

fn prop(db: &NativeDb, name: &str) -> gd_types::Property {
    db.class_named("Probe")
        .expect("Probe")
        .properties
        .iter()
        .find(|p| db.name_of(p.name) == name)
        .unwrap_or_else(|| panic!("no property {name}"))
        .clone()
}

#[test]
fn an_enum_getter_names_the_enum_the_property_row_flattened() {
    let db = NativeDb::from_json(DUMP).expect("dump parses");
    let mode = prop(&db, "mode");
    match &mode.ty {
        TypeRef::Enum { scope, name } => {
            assert_eq!(db.name_of(scope.expect("class-scoped")), "Probe");
            assert_eq!(db.name_of(*name), "Mode");
        }
        other => panic!("expected an enum, got {other:?}"),
    }
}

#[test]
fn a_container_getter_restores_the_element_types() {
    let db = NativeDb::from_json(DUMP).expect("dump parses");
    match &prop(&db, "items").ty {
        TypeRef::TypedArray(elem) => assert_eq!(
            db.name_of(match **elem {
                TypeRef::Named(s) => s,
                _ => panic!("element"),
            }),
            "int"
        ),
        other => panic!("expected a typed array, got {other:?}"),
    }
    match &prop(&db, "lookup").ty {
        TypeRef::TypedDict(k, v) => {
            assert_eq!(
                db.name_of(match **k {
                    TypeRef::Named(s) => s,
                    _ => panic!("key"),
                }),
                "String"
            );
            assert_eq!(
                db.name_of(match **v {
                    TypeRef::Named(s) => s,
                    _ => panic!("value"),
                }),
                "int"
            );
        }
        other => panic!("expected a typed dictionary, got {other:?}"),
    }
}

#[test]
fn a_plain_getter_leaves_the_property_row_alone() {
    let db = NativeDb::from_json(DUMP).expect("dump parses");
    // Agreement: nothing to recover.
    assert_eq!(
        db.name_of(match prop(&db, "count").ty {
            TypeRef::Named(s) => s,
            _ => panic!(),
        }),
        "int"
    );
    // Disagreement between two plain names is the hint string leaking into `type`. Recovering it
    // would be guessing a class, so the dump's spelling stands.
    assert_eq!(
        db.name_of(match prop(&db, "texture").ty {
            TypeRef::Named(s) => s,
            _ => panic!(),
        }),
        "Texture2D,-AtlasTexture"
    );
}

#[test]
fn a_property_with_no_reachable_getter_is_untouched() {
    let db = NativeDb::from_json(DUMP).expect("dump parses");
    // Named getter, but the class exposes no such method.
    assert_eq!(
        db.name_of(match prop(&db, "orphan").ty {
            TypeRef::Named(s) => s,
            _ => panic!(),
        }),
        "int"
    );
    // No getter at all.
    let readonly = prop(&db, "readonly");
    assert!(readonly.getter.is_none());
    assert_eq!(
        db.name_of(match readonly.ty {
            TypeRef::Named(s) => s,
            _ => panic!(),
        }),
        "int"
    );
}
