//! doc-XML reader tests: real engine XML parses, a synthetic GDExtension class normalizes correctly,
//! and merging it into a JSON-built DB resolves cross-tier with JSON-dump-wins precedence.

use gd_types::{parse_doc_class, Method, NativeClass, NativeDb, TypeRef};

const EXT: &str = include_str!("fixtures/ext_class.xml");
const MARKER: &str = include_str!("fixtures/marker2d.xml");
const TRIMMED: &str = include_str!("fixtures/trimmed_api.json");

fn find<'a>(db: &NativeDb, class: &'a NativeClass, name: &str) -> &'a Method {
    class
        .methods
        .iter()
        .find(|m| db.name_of(m.name) == name)
        .unwrap_or_else(|| panic!("no method {name}"))
}

#[test]
fn parses_real_engine_xml() {
    let c = parse_doc_class(MARKER).expect("Marker2D parses");
    assert_eq!(c.name, "Marker2D");
    assert_eq!(c.inherits.as_deref(), Some("Node2D"));
    assert!(c.properties.iter().any(|p| p.name == "gizmo_extents"));
}

#[test]
fn parses_synthetic_gdextension_xml() {
    let c = parse_doc_class(EXT).expect("GDExtNode parses");
    assert_eq!(c.name, "GDExtNode");
    assert_eq!(c.inherits.as_deref(), Some("Node2D"));
    assert_eq!(c.api_type, "extension");
    // typed-array return normalized to the dump's prefix encoding
    let peers = c.methods.iter().find(|m| m.name == "get_peers").unwrap();
    assert_eq!(
        peers.return_value.as_ref().unwrap().ty,
        "typedarray::GDExtNode"
    );
    // void return becomes an absent return_value; vararg qualifier parsed
    let flush = c.methods.iter().find(|m| m.name == "flush").unwrap();
    assert!(flush.return_value.is_none());
    assert!(flush.is_vararg);
    // enum member normalized; bitfield member flagged
    let mode = c.properties.iter().find(|p| p.name == "mode").unwrap();
    assert_eq!(mode.ty, "enum::GDExtNode.Mode");
    let layers = c.properties.iter().find(|p| p.name == "layers").unwrap();
    assert_eq!(layers.ty, "bitfield::GDExtNode.Layers");
    // enum-grouped constants vs a flat constant
    assert!(c
        .enums
        .iter()
        .any(|e| e.name == "Mode" && e.values.len() == 2));
    assert!(c
        .constants
        .iter()
        .any(|k| k.name == "MAX_PEERS" && k.value == 32));
}

#[test]
fn doc_class_merges_and_resolves_cross_tier() {
    let mut db = NativeDb::from_json(TRIMMED).expect("trimmed parses");
    let ext = parse_doc_class(EXT).expect("ext parses");
    assert!(db.merge_doc_class(ext), "GDExtNode is new, should merge");

    // cross-tier inheritance: GDExtNode (XML) → Node2D (JSON) → … → Object (JSON)
    assert!(db.is_subclass_of_named("GDExtNode", "Node2D"));
    assert!(db.is_subclass_of_named("GDExtNode", "Object"));

    let ext_class = db.class_named("GDExtNode").expect("merged class present");

    // typed-array return decoded through the shared ingester
    match &find(&db, ext_class, "get_peers").return_type {
        TypeRef::TypedArray(inner) => match inner.as_ref() {
            TypeRef::Named(s) => assert_eq!(db.name_of(*s), "GDExtNode"),
            other => panic!("element: {other:?}"),
        },
        other => panic!("get_peers: {other:?}"),
    }
    // enum param decoded
    let set_mode = find(&db, ext_class, "set_mode");
    match &set_mode.params[0].ty {
        TypeRef::Enum {
            scope: Some(s),
            name,
        } => {
            assert_eq!(db.name_of(*s), "GDExtNode");
            assert_eq!(db.name_of(*name), "Mode");
        }
        other => panic!("set_mode param: {other:?}"),
    }
    assert!(find(&db, ext_class, "flush").is_vararg);
}

#[test]
fn json_dump_wins_over_doc_xml() {
    let mut db = NativeDb::from_json(TRIMMED).expect("trimmed parses");
    // A doc class named "Node" already exists from the JSON dump — merging must be refused.
    let fake =
        parse_doc_class(r#"<?xml version="1.0"?><class name="Node" inherits="Bogus"></class>"#)
            .expect("parses");
    assert!(!db.merge_doc_class(fake), "Node already exists; JSON wins");
    // Node still chains to its real JSON base, not the bogus XML one.
    assert!(db.is_subclass_of_named("Node", "Object"));
    assert!(!db.is_subclass_of_named("Node", "Bogus"));
}
