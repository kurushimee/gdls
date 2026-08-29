//! `CONFUSABLE_TEMPORARY_MODIFICATION`, the one warning 4.7 adds.
//!
//! The vendored 4.7 golden covers the positive cases against `Line2D.points`. What it cannot show
//! is the dialect gate — the corpus only ever runs one tag at a time — or the shapes that must
//! stay silent, so those live here.

use std::path::Path;

use gd_analyze::{
    analyze_with_options, AnalyzeOptions, NoCrossFile, StrictSettings, WarnPolicy, WarningCode,
};
use gd_project::{FileId, WarningConfig};
use gd_syntax::{Dialect, ParseOptions};
use gd_types::NativeDb;

fn native_db() -> NativeDb {
    let path =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../gd_types/tests/fixtures/trimmed_api.json");
    NativeDb::load(path.to_str().expect("utf-8 path"))
        .unwrap_or_else(|e| panic!("load native DB fixture at {}: {e}", path.display()))
}

/// How many `CONFUSABLE_TEMPORARY_MODIFICATION` diagnostics `src` produces under `dialect`.
fn count(src: &str, dialect: Dialect) -> usize {
    let tree = gd_syntax::parse_with_options(
        src,
        &ParseOptions {
            dialect,
            script_path: "",
        },
    )
    .tree;
    let db = native_db();
    let mut config = WarningConfig::default();
    config.levels.insert(
        "confusable_temporary_modification".to_owned(),
        gd_project::WarnLevel::Warn,
    );
    let policy = WarnPolicy::build(&config, &StrictSettings::default(), dialect);
    analyze_with_options(
        &tree,
        Some(FileId::new(1)),
        "ctm.gd",
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
    .filter(|d| d.warning_code() == Some(WarningCode::ConfusableTemporaryModification))
    .count()
}

/// `Line2D.points` is a `PackedVector2Array` property, so its getter hands back a copy.
const WRITE_THROUGH: &str = "extends Line2D\n\nfunc _ready() -> void:\n\tpoints[0] = Vector2.ONE\n";

#[test]
fn the_warning_does_not_exist_at_4_6() {
    assert_eq!(count(WRITE_THROUGH, Dialect::Godot4_6), 0);
    assert_eq!(count(WRITE_THROUGH, Dialect::Godot4_7), 1);
}

#[test]
fn a_mutating_builtin_method_warns_but_a_const_one_does_not() {
    let mutating = "extends Line2D\n\nfunc _ready() -> void:\n\tpoints.clear()\n";
    let readonly = "extends Line2D\n\nfunc _ready() -> void:\n\tvar _n := points.size()\n";
    assert_eq!(count(mutating, Dialect::Godot4_7), 1);
    assert_eq!(count(readonly, Dialect::Godot4_7), 0);
}

#[test]
fn a_scripts_own_property_never_warns() {
    // The value is the script's own member, not a native getter's copy, so writing through it
    // works and there is nothing to warn about.
    let src = "extends Node\n\nvar points: PackedVector2Array\n\nfunc _ready() -> void:\n\tpoints[0] = Vector2.ONE\n\tpoints.clear()\n";
    for d in [Dialect::Godot4_6, Dialect::Godot4_7] {
        assert_eq!(count(src, d), 0, "dialect {d:?}");
    }
}

#[test]
fn a_local_copy_never_warns() {
    // Assigning the property to a local first is exactly the fix the message suggests.
    let src = "extends Line2D\n\nfunc _ready() -> void:\n\tvar p := points\n\tp[0] = Vector2.ONE\n\tp.clear()\n";
    assert_eq!(count(src, Dialect::Godot4_7), 0);
}

#[test]
fn a_non_packed_native_property_never_warns() {
    // `Node2D.position` is a `Vector2`, not a packed array — `is_typed_container_type()` is false,
    // and writing through it goes via the setter.
    let src = "extends Node2D\n\nfunc _ready() -> void:\n\tposition.x = 1.0\n";
    assert_eq!(count(src, Dialect::Godot4_7), 0);
}

#[test]
fn an_explicit_base_warns_the_same_as_the_implicit_one() {
    let src = "extends Line2D\n\nfunc _ready() -> void:\n\tvar base: Line2D = self\n\tbase.points[0] = Vector2.ONE\n\tbase.points.clear()\n";
    assert_eq!(count(src, Dialect::Godot4_7), 2);
    assert_eq!(count(src, Dialect::Godot4_6), 0);
}
