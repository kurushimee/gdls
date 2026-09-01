//! `resolve_function_signature`'s 4.7 parent-return-type adoption (GH-118877), at both tags.
//!
//! The vendored 4.7 corpus covers the *script*-parent half (`untyped_override_*`). The native half
//! — an untyped `func _ready():` inheriting `Node._ready`'s `void` from ClassDB — has no golden
//! file, so it is pinned here, along with the `_get_property_list` compatibility exception.

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

/// Every error message the analyzer produces for `src` under `dialect`.
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
        "inherited_return.gd",
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

const VOID_RETURN: &str = "A void function cannot return a value.";

#[test]
fn an_untyped_override_of_a_script_parent_inherits_its_return_type_only_at_4_7() {
    let src = "class A:\n\tfunc f() -> void: pass\n\nclass B extends A:\n\tfunc f(): return 1\n";
    assert!(
        !errors(src, Dialect::Godot4_6)
            .iter()
            .any(|m| m == VOID_RETURN),
        "4.6 left the untyped override at soft Variant: {:?}",
        errors(src, Dialect::Godot4_6)
    );
    assert!(
        errors(src, Dialect::Godot4_7)
            .iter()
            .any(|m| m == VOID_RETURN),
        "4.7 must inherit the parent's `void`: {:?}",
        errors(src, Dialect::Godot4_7)
    );
}

#[test]
fn an_untyped_override_of_a_native_virtual_inherits_its_return_type_only_at_4_7() {
    // `Node._ready` is declared `void` in ClassDB, so 4.7 rejects a returned value.
    let src = "extends Node\n\nfunc _ready(): return 1\n";
    assert!(!errors(src, Dialect::Godot4_6)
        .iter()
        .any(|m| m == VOID_RETURN));
    assert!(
        errors(src, Dialect::Godot4_7)
            .iter()
            .any(|m| m == VOID_RETURN),
        "4.7 must reach ClassDB for the parent return type: {:?}",
        errors(src, Dialect::Godot4_7)
    );
}

#[test]
fn a_declared_return_type_is_never_overwritten_by_the_parents() {
    // The adoption only fires when the override declares no return type of its own.
    let src = "extends Node\n\nfunc _ready() -> Variant: return 1\n";
    for d in [Dialect::Godot4_6, Dialect::Godot4_7] {
        assert!(
            !errors(src, d).iter().any(|m| m == VOID_RETURN),
            "dialect {d:?}: {:?}",
            errors(src, d)
        );
    }
}

#[test]
fn an_untyped_get_property_list_override_keeps_a_plain_array() {
    // GH-118877's compatibility exception: the declared return is `Array[Dictionary]`, but too
    // much existing code returns a plain array and the mismatch only shows at runtime, so an
    // untyped override inherits bare `Array` instead. The returned `int` is wrong either way —
    // what this pins is *which* type the message names.
    let src = "extends Node\n\nfunc _get_property_list(): return 5\n";
    let at_47 = errors(src, Dialect::Godot4_7);
    assert!(
        at_47.iter().any(|m| m
            == r#"Cannot return value of type "int" because the function return type is "Array"."#),
        "the exception must name bare `Array`, not `Array[Dictionary]`: {at_47:?}"
    );
    // A plain untyped array is what the exception exists to allow.
    assert!(errors(
        "extends Node\n\nfunc _get_property_list(): return [1, 2]\n",
        Dialect::Godot4_7
    )
    .is_empty());
    // And 4.6 adopts nothing at all, so neither return is checked.
    assert!(errors(src, Dialect::Godot4_6).is_empty());
}

#[test]
fn an_untyped_override_of_a_typed_parent_still_rejects_an_incompatible_value_at_4_7() {
    let src =
        "class A:\n\tfunc f() -> int: return 0\n\nclass B extends A:\n\tfunc f(): return \"abc\"\n";
    let at_47 = errors(src, Dialect::Godot4_7);
    assert!(
        at_47
            .iter()
            .any(|m| m.contains(r#"the function return type is "int""#)),
        "4.7: {at_47:?}"
    );
    assert!(errors(src, Dialect::Godot4_6).is_empty());
}

// ===================================================================================================
// `reduce_type_test`'s constant arm: `is_type_compatible_strict_collections` is 4.7-only.
// ===================================================================================================

#[test]
fn a_constant_array_type_test_is_strict_about_collections_at_4_7_only() {
    // 4.7 added `is_type_compatible_strict_collections` to `reduce_type_test`'s constant arm, so
    // `[] is Array[int]` on a constant became an error there and stays silent at 4.6. The arm only
    // runs when the operand really is constant, which needed the array fold (#385) — before it the
    // guard was carried but unreachable, and this test pinned the placeholder silence.
    //
    // Both rows are oracle-pinned against the 4.7.2 and 4.6.3 binaries.
    let strict = "func test():\n\tconst A = []\n\tprint(A is Array[int])\n";
    assert!(
        errors(strict, Dialect::Godot4_7)
            .iter()
            .any(|m| m == r#"Expression is of type "Array" so it can't be of type "Array[int]"."#),
        "4.7: {:?}",
        errors(strict, Dialect::Godot4_7)
    );
    assert!(
        errors(strict, Dialect::Godot4_6).is_empty(),
        "4.6: {:?}",
        errors(strict, Dialect::Godot4_6)
    );

    // An unparameterized `Array` is compatible at both tags — the guard is about the ELEMENT type.
    let loose = "func test():\n\tconst A = []\n\tprint(A is Array)\n";
    for d in [Dialect::Godot4_6, Dialect::Godot4_7] {
        assert!(
            errors(loose, d).is_empty(),
            "dialect {d:?}: {:?}",
            errors(loose, d)
        );
    }
}
