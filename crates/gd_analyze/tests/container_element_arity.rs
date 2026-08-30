//! #375 — the arity gate on `Array[T]` / `Dictionary[K, V]`, and the `Variant` slot Godot skips.
//!
//! Two rules live in `resolve_datatype` and gdls used to have neither. Godot stamps element types
//! inside the builtin arm (`gdscript_analyzer.cpp:764-783`) and skips any slot whose resolved type
//! is `Variant`, so `Array[Variant]` carries no element types and renders as a plain `Array`. It
//! then validates arity at the tail (`:941-957`) and returns a bad type, rather than truncating a
//! too-long list down to what fits.
//!
//! Every expectation is pinned against `godot --headless --check-only` at 4.7.2.

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

fn errors(src: &str) -> Vec<String> {
    let dialect = Dialect::DEFAULT;
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
        "a.gd",
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
    .filter(|d| d.warning_code().is_none())
    .map(|d| d.message().to_string())
    .collect()
}

#[test]
fn a_two_element_array_annotation_is_rejected() {
    assert_eq!(
        errors("extends Node\nvar a: Array[int, String] = []\n"),
        vec!["Typed arrays require exactly one collection element type.".to_owned()]
    );
}

#[test]
fn a_one_element_dictionary_annotation_is_rejected() {
    assert_eq!(
        errors("extends Node\nvar d: Dictionary[int] = {}\n"),
        vec!["Typed dictionaries require exactly two collection element types.".to_owned()]
    );
}

#[test]
fn a_non_container_base_cannot_carry_element_types() {
    assert_eq!(
        errors("extends Node\nvar v: Vector2[int]\n"),
        vec!["Only arrays and dictionaries can specify collection element types.".to_owned()]
    );
}

/// The well-formed shapes stay silent, which is what keeps the gate from firing on real code.
#[test]
fn the_well_formed_annotations_draw_nothing() {
    assert_eq!(
        errors("extends Node\nvar a: Array[int] = []\nvar d: Dictionary[int, String] = {}\nvar b: Array = []\nvar e: Dictionary = {}\n"),
        Vec::<String>::new()
    );
}

/// `Array[Variant]` is not an `Array` with a `Variant` element type — Godot leaves the slot unset,
/// so it renders as a bare `Array`. `Dictionary` pads slot 0 only when it has to reach slot 1.
#[test]
fn a_variant_element_slot_is_left_unset() {
    let render = |body: &str| {
        let e = errors(&format!("extends Node\nfunc f() -> void:\n{body}"));
        e.into_iter()
            .filter(|m| m.starts_with("Invalid operand of type"))
            .collect::<Vec<_>>()
    };
    assert_eq!(
        render("\tvar a: Array[Variant] = []\n\tprint(-a)\n"),
        vec![r#"Invalid operand of type "Array" for unary operator "unary-"."#.to_owned()]
    );
    assert_eq!(
        render("\tvar d: Dictionary[int, Variant] = {}\n\tprint(-d)\n"),
        vec![
            r#"Invalid operand of type "Dictionary[int, Variant]" for unary operator "unary-"."#
                .to_owned()
        ]
    );
    assert_eq!(
        render("\tvar e: Dictionary[Variant, int] = {}\n\tprint(-e)\n"),
        vec![
            r#"Invalid operand of type "Dictionary[Variant, int]" for unary operator "unary-"."#
                .to_owned()
        ]
    );
}
