//! A builtin Variant type that a custom engine build adds.
//!
//! A fork can register a new `Variant::Type`, and its `extension_api.json` then lists it under
//! `builtin_classes` beside `Vector2` and `Color`. Godot's tokenizer and analyzer read the builtin
//! names from `Variant::get_type_name`, so in that engine the name is a type, a constructor, and a
//! static-call base. gdls's `VariantType` is fixed at compile time and cannot model the type, so
//! every use of it must degrade to a silent Variant rather than be reported as a typo.

use std::path::Path;

use gd_analyze::{analyze, NoCrossFile, StrictSettings, WarnPolicy};
use gd_project::{FileId, WarningConfig};
use gd_syntax::{parse, Dialect};
use gd_types::NativeDb;

/// The trimmed fixture plus one builtin class no stock engine has.
fn native_db() -> NativeDb {
    let path =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../gd_types/tests/fixtures/trimmed_api.json");
    let text = std::fs::read_to_string(&path).expect("read the native DB fixture");
    let mut api: serde_json::Value = serde_json::from_str(&text).expect("fixture is JSON");
    api["builtin_classes"]
        .as_array_mut()
        .expect("fixture has builtin_classes")
        .push(serde_json::json!({
            "name": "EntityUUID",
            "is_keyed": false,
            "constructors": [{ "index": 0 }],
            "methods": [{
                "name": "from_string",
                "return_type": "EntityUUID",
                "is_vararg": false,
                "is_const": false,
                "is_static": true,
                "hash": 1,
                "arguments": [{ "name": "text", "type": "String" }]
            }]
        }));
    NativeDb::from_json(&api.to_string()).expect("the extended fixture loads")
}

fn errors(src: &str) -> Vec<String> {
    let tree = parse(src).tree;
    let policy = WarnPolicy::build(
        &WarningConfig::default(),
        &StrictSettings::default(),
        Dialect::DEFAULT,
    );
    analyze(
        &tree,
        Some(FileId::new(1)),
        "res://a.gd",
        &native_db(),
        &NoCrossFile,
        &policy,
    )
    .diagnostics
    .iter()
    .filter(|d| d.warning_code().is_none())
    .map(|d| d.message().to_owned())
    .collect()
}

#[test]
fn a_custom_builtin_type_is_never_reported_as_undeclared() {
    let src = "\
extends Node

var held: EntityUUID

func make(text: String) -> EntityUUID:
	var blank := EntityUUID()
	var parsed := EntityUUID.from_string(text)
	var kind = EntityUUID
	print(blank, parsed, kind)
	return parsed
";
    assert_eq!(errors(src), Vec::<String>::new());
}

#[test]
fn a_typo_beside_a_custom_builtin_type_is_still_reported() {
    let src = "extends Node\n\nfunc f() -> void:\n\tprint(EntityUUIDD)\n";
    assert_eq!(
        errors(src),
        [r#"Identifier "EntityUUIDD" not declared in the current scope."#]
    );
}
