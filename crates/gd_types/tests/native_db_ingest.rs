//! Ingestion tests: precise assertions against the synthetic fixture, and real-format coverage
//! against the trimmed real dump.

use gd_types::{NativeClass, NativeDb, TypeRef};

const MINI: &str = include_str!("fixtures/mini_api.json");
const TRIMMED: &str = include_str!("fixtures/trimmed_api.json");

/// Resolve a `Named` ref's interned string against the DB it came from.
fn named<'a>(db: &'a NativeDb, t: &TypeRef) -> &'a str {
    match t {
        TypeRef::Named(s) => db.name_of(*s),
        other => panic!("expected Named, got {other:?}"),
    }
}

fn method<'a>(db: &NativeDb, class: &'a NativeClass, name: &str) -> &'a gd_types::Method {
    class
        .methods
        .iter()
        .find(|m| db.name_of(m.name) == name)
        .unwrap_or_else(|| panic!("no method {name}"))
}

#[test]
fn mini_header_and_inheritance() {
    let db = NativeDb::from_json(MINI).expect("mini parses");
    assert!(!db.is_empty());
    assert_eq!(db.header().version_major, 4);
    assert_eq!(db.header().version_minor, 6);

    assert!(db.is_subclass_of_named("MiniNode", "MiniObject"));
    assert!(db.is_subclass_of_named("MiniObject", "MiniObject")); // reflexive
    assert!(!db.is_subclass_of_named("MiniObject", "MiniNode")); // not upward
    assert!(!db.is_subclass_of_named("Ghost", "MiniObject")); // unknown class
}

#[test]
fn mini_method_flags() {
    let db = NativeDb::from_json(MINI).expect("mini parses");
    let obj = db.class_named("MiniObject").expect("MiniObject");
    assert!(method(&db, obj, "create").is_static);
    assert!(method(&db, obj, "_ready").is_virtual);
    assert!(method(&db, obj, "call_va").is_vararg);
    assert!(method(&db, obj, "get_class").is_const);
    assert!(!obj.is_refcounted);
    assert!(obj.is_instantiable);
}

#[test]
fn mini_return_types_decode_through_ingestion() {
    let db = NativeDb::from_json(MINI).expect("mini parses");
    let obj = db.class_named("MiniObject").expect("MiniObject");

    // void (absent return_value) and Variant
    assert_eq!(method(&db, obj, "_ready").return_type, TypeRef::Void);
    assert_eq!(method(&db, obj, "notify").return_type, TypeRef::Void);
    assert_eq!(method(&db, obj, "call_va").return_type, TypeRef::Variant);
    assert_eq!(
        named(&db, &method(&db, obj, "get_class").return_type),
        "String"
    );

    // typedarray::MiniObject
    match &method(&db, obj, "get_children").return_type {
        TypeRef::TypedArray(inner) => assert_eq!(named(&db, inner), "MiniObject"),
        other => panic!("get_children: {other:?}"),
    }
    // typeddictionary::int;String
    match &method(&db, obj, "get_table").return_type {
        TypeRef::TypedDict(k, v) => {
            assert_eq!(named(&db, k), "int");
            assert_eq!(named(&db, v), "String");
        }
        other => panic!("get_table: {other:?}"),
    }
    // enum::MiniObject.Mode (scoped) and enum::MiniError (global)
    match &method(&db, obj, "get_mode_enum").return_type {
        TypeRef::Enum {
            scope: Some(s),
            name,
        } => {
            assert_eq!(db.name_of(*s), "MiniObject");
            assert_eq!(db.name_of(*name), "Mode");
        }
        other => panic!("get_mode_enum: {other:?}"),
    }
    match &method(&db, obj, "get_error").return_type {
        TypeRef::Enum { scope: None, name } => assert_eq!(db.name_of(*name), "MiniError"),
        other => panic!("get_error: {other:?}"),
    }
    // bitfield::MiniObject.Flags
    match &method(&db, obj, "get_flags").return_type {
        TypeRef::Bitfield {
            scope: Some(s),
            name,
        } => {
            assert_eq!(db.name_of(*s), "MiniObject");
            assert_eq!(db.name_of(*name), "Flags");
        }
        other => panic!("get_flags: {other:?}"),
    }
    // void*
    assert_eq!(
        method(&db, obj, "get_buffer_ptr").return_type,
        TypeRef::Pointer(Box::new(TypeRef::Void))
    );
}

#[test]
fn mini_members_signals_enums_constants() {
    let db = NativeDb::from_json(MINI).expect("mini parses");
    let obj = db.class_named("MiniObject").expect("MiniObject");

    // property with setter/getter
    let mode = obj
        .properties
        .iter()
        .find(|p| db.name_of(p.name) == "mode")
        .unwrap();
    assert_eq!(db.name_of(mode.setter.unwrap()), "set_mode");
    assert_eq!(db.name_of(mode.getter.unwrap()), "get_mode");

    // signal with one MiniObject arg
    let changed = obj
        .signals
        .iter()
        .find(|s| db.name_of(s.name) == "changed")
        .unwrap();
    assert_eq!(changed.params.len(), 1);
    assert_eq!(named(&db, &changed.params[0].ty), "MiniObject");

    // enums (one plain, one bitfield) and a constant
    let flags = obj
        .enums
        .iter()
        .find(|e| db.name_of(e.name) == "Flags")
        .unwrap();
    assert!(flags.is_bitfield);
    assert_eq!(flags.values.len(), 2);
    assert!(obj
        .constants
        .iter()
        .any(|c| db.name_of(c.name) == "VERSION" && c.value == 2));
}

#[test]
fn mini_builtins_globals_singletons() {
    let db = NativeDb::from_json(MINI).expect("mini parses");

    let v2 = db.builtin_named("Vector2").expect("Vector2 builtin");
    assert!(v2.members.iter().any(|m| db.name_of(m.name) == "x"));
    assert!(v2.members.iter().any(|m| db.name_of(m.name) == "y"));

    assert_eq!(db.global_constant("MINI_GLOBAL"), Some(7));
    assert!(db.global_enum("MiniError").is_some());
    assert!(db.utility("mini_max").expect("mini_max").is_vararg);

    // singleton resolves to its class
    let eng = db
        .singleton_type("MiniEngine")
        .expect("MiniEngine singleton");
    assert_eq!(db.name_of(eng.name), "MiniObject");
}

#[test]
fn trimmed_real_fixture_parses_and_chains() {
    let db = NativeDb::from_json(TRIMMED).expect("trimmed real dump parses");
    assert!(db.is_subclass_of_named("Node2D", "CanvasItem"));
    assert!(db.is_subclass_of_named("Node2D", "Node"));
    assert!(db.is_subclass_of_named("Node2D", "Object"));
    assert!(db.is_subclass_of_named("RefCounted", "Object"));
    assert!(!db.is_subclass_of_named("Object", "Node"));
    assert!(db
        .class_named("Node")
        .is_some_and(|c| !c.methods.is_empty()));
}
