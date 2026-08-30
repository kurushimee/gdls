//! #428, #432: a native property takes its type from its getter, always.
//!
//! Godot's analyzer types a native member off the getter's `PropertyInfo`
//! (`gdscript_analyzer.cpp:4343-4350`), and never reads the property table's own type. The JSON
//! dump's `properties[].type` is a flattened `Variant::Type` — a bare `int` for an enum, `Array`
//! for a typed array, and sometimes a `PropertyInfo` hint string that is not a type name at all —
//! so `NativeDb` reads the getter back over it, resolving the getter up the `inherits` chain the
//! way `ClassDB::get_method` does.

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
          "hash": 5, "return_value": { "type": "Texture2D" }, "arguments": [] },
        { "name": "get_inherited", "is_const": true, "is_static": false, "is_vararg": false, "is_virtual": false,
          "hash": 6, "return_value": { "type": "Window" }, "arguments": [] }
      ]
    },
    {
      "name": "Child", "inherits": "Probe", "is_refcounted": false, "is_instantiable": true, "api_type": "core",
      "constants": [], "enums": [], "signals": [], "methods": [],
      "properties": [
        { "name": "inherited_prop", "type": "Node", "setter": "set_inherited", "getter": "get_inherited", "index": -1 }
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
fn an_agreeing_plain_getter_changes_nothing() {
    let db = NativeDb::from_json(DUMP).expect("dump parses");
    assert_eq!(
        db.name_of(match prop(&db, "count").ty {
            TypeRef::Named(s) => s,
            _ => panic!(),
        }),
        "int"
    );
}

/// The disagreement the property table cannot win: its `type` field is carrying a `PropertyInfo`
/// hint string, and the getter spells the real class.
#[test]
fn a_disagreeing_plain_getter_wins() {
    let db = NativeDb::from_json(DUMP).expect("dump parses");
    assert_eq!(
        db.name_of(match prop(&db, "texture").ty {
            TypeRef::Named(s) => s,
            _ => panic!(),
        }),
        "Texture2D"
    );
}

/// The getter lives on a parent class — 63 real properties are served this way, and
/// `ClassDB::get_method` walks the chain for all of them.
#[test]
fn a_getter_declared_on_a_parent_class_is_found() {
    let db = NativeDb::from_json(DUMP).expect("dump parses");
    let child = db.class_named("Child").expect("Child");
    let inherited = child
        .properties
        .iter()
        .find(|p| db.name_of(p.name) == "inherited_prop")
        .expect("inherited_prop");
    assert_eq!(
        db.name_of(match inherited.ty {
            TypeRef::Named(s) => s,
            _ => panic!(),
        }),
        "Window"
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
