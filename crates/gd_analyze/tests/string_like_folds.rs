//! #424 — `String`, `StringName`, and `NodePath` fold as themselves, not as one coalesced
//! `String`.
//!
//! The fold table used to store all three as `FoldedValue::String`, on the grounds that Godot
//! treats `&"k"` and `"k"` as the same dictionary key. It does — and only those two.
//! `StringLikeVariantComparator::compare` (`core/variant/variant.cpp:3400-3411`) exempts exactly
//! that pair and falls through to `Variant::hash_compare` (`:3176`) for everything else, and
//! `hash_compare` requires the two variants to be the same type. So a `NodePath` key matched a
//! `String` key in gdls and reported a duplicate Godot never reports.
//!
//! The operator table draws the same lines, and the split makes those reachable too:
//! `register_string_op` (`variant_op.cpp:196-202`) registers all four String/StringName pairs for
//! `OP_ADD`, `OP_EQUAL`, and `OP_NOT_EQUAL`, while the ORDERED comparisons are registered
//! same-type only (`:737-783`) and `NodePath` gets nothing but equality against itself
//! (`:511`/`:633`).
//!
//! Every row is pinned against `Godot_v4.7.2-stable --headless --check-only`.

use std::path::Path;

use gd_analyze::{analyze, FoldedValue, NoCrossFile, Severity, StrictSettings, WarnPolicy};
use gd_syntax::Dialect;
use gd_types::NativeDb;

fn native_db() -> NativeDb {
    let path =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../gd_types/tests/fixtures/trimmed_api.json");
    NativeDb::load(path.to_str().expect("utf-8 path"))
        .unwrap_or_else(|e| panic!("load native DB fixture at {}: {e}", path.display()))
}

fn policy() -> WarnPolicy {
    WarnPolicy::build(
        &gd_project::WarningConfig::default(),
        &StrictSettings::default(),
        Dialect::DEFAULT,
    )
}

fn errors(src: &str) -> Vec<String> {
    let tree = gd_syntax::parse(src).tree;
    analyze(&tree, None, "t.gd", &native_db(), &NoCrossFile, &policy())
        .diagnostics
        .iter()
        .filter(|d| d.severity() == Severity::Error)
        .map(|d| d.message().to_owned())
        .collect()
}

/// The fold of the initializer of the last `const` in `src`.
fn last_const_fold(src: &str) -> Option<FoldedValue> {
    use gd_syntax::ast::NodeKind;
    let tree = gd_syntax::parse(src).tree;
    let result = analyze(&tree, None, "t.gd", &native_db(), &NoCrossFile, &policy());
    let mut found = None;
    for id in tree.iter_ids() {
        if let NodeKind::Constant(c) = &tree.get(id).kind {
            if let Some(init) = c.initializer {
                found = result.folds.get(init).cloned();
            }
        }
    }
    found
}

const MSG_X: &str = r#"Key "x" was already used in this dictionary (at line 4)."#;

// ===================================================================================================
// Dictionary keys — Godot's one cross-type exemption, and nothing beyond it.
// ===================================================================================================

/// The headline row. Oracle: one error, for the `&"x"` pair only.
#[test]
fn a_stringname_key_duplicates_a_string_key_and_a_nodepath_key_does_not() {
    let src = "\
extends Node

func f() -> void:
\tvar a := {\"x\": 1, &\"x\": 2}
\tvar b := {\"y\": 1, ^\"y\": 2}
\tprint(a, b)
";
    assert_eq!(errors(src), vec![MSG_X.to_owned()]);
}

/// Each string-like still duplicates itself.
#[test]
fn each_string_like_duplicates_its_own_kind() {
    for (key, kind) in [
        ("\"x\"", "String"),
        ("&\"x\"", "StringName"),
        ("^\"x\"", "NodePath"),
    ] {
        let src = format!(
            "extends Node\n\nfunc f() -> void:\n\tvar a := {{{key}: 1, {key}: 2}}\n\tprint(a)\n"
        );
        assert_eq!(errors(&src), vec![MSG_X.to_owned()], "{kind}");
    }
}

/// Oracle: `Variant::stringify` at the top level quote-wraps nothing and renders all three
/// string-likes as their bare text, so every one of these reports `Key "x"`.
#[test]
fn the_duplicate_key_message_renders_a_string_like_bare() {
    let src = "\
extends Node

func f() -> void:
\tvar a := {^\"x\": 1, ^\"x\": 2}
\tprint(a)
";
    assert_eq!(errors(src), vec![MSG_X.to_owned()]);
}

/// A Lua-style key folds as a `StringName` in both of Godot's branches
/// (`gdscript_parser.cpp:3331-3336`), so it duplicates a Python-style `String` key.
#[test]
fn a_lua_style_key_is_a_stringname_and_still_matches_a_string_key() {
    let src = "\
extends Node

func f() -> void:
\tvar a := {x = 1, x = 2}
\tprint(a)
";
    assert_eq!(errors(src), vec![MSG_X.to_owned()]);
}

// ===================================================================================================
// The operator table.
// ===================================================================================================

/// `register_string_op` registers all four String/StringName pairs for `OP_ADD` and for equality,
/// and every concat returns a `String` (`ReturnType = String`, `variant_op.h:717`).
#[test]
fn concat_and_equality_cross_string_and_stringname() {
    assert_eq!(
        errors("extends Node\nconst A = \"a\" + &\"b\"\n"),
        Vec::<String>::new()
    );
    assert_eq!(
        last_const_fold("extends Node\nconst A = \"a\" + &\"b\"\n"),
        Some(FoldedValue::String("ab".to_owned()))
    );
    assert_eq!(
        last_const_fold("extends Node\nconst A = \"a\" == &\"a\"\n"),
        Some(FoldedValue::Bool(true))
    );
    assert_eq!(
        last_const_fold("extends Node\nconst A = \"a\" != &\"b\"\n"),
        Some(FoldedValue::Bool(true))
    );
}

/// Oracle: `Invalid operands to operator <, String and StringName.` — the ordered comparisons are
/// registered same-type only, so the cross pair has no evaluator.
#[test]
fn an_ordered_comparison_across_string_and_stringname_is_invalid() {
    assert_eq!(
        errors("extends Node\nconst A = \"a\" < &\"b\"\n"),
        vec![r#"Invalid operands to operator <, String and StringName."#.to_owned()]
    );
}

/// Oracle: `Invalid operands to operator +, NodePath and String.` — `NodePath` has no concat
/// registration at all.
#[test]
fn a_nodepath_has_no_concat() {
    assert_eq!(
        errors("extends Node\nconst A = ^\"a\" + \"b\"\n"),
        vec![r#"Invalid operands to operator +, NodePath and String."#.to_owned()]
    );
}

/// Same-type ordered comparisons still fold.
#[test]
fn same_type_ordered_comparisons_still_fold() {
    assert_eq!(
        last_const_fold("extends Node\nconst A = \"a\" < \"b\"\n"),
        Some(FoldedValue::Bool(true))
    );
    assert_eq!(
        last_const_fold("extends Node\nconst A = &\"a\" < &\"b\"\n"),
        Some(FoldedValue::Bool(true))
    );
}

// ===================================================================================================
// Everything downstream of the fold still reads all three.
// ===================================================================================================

/// Each literal folds as its own kind now, and `%` still routes a `StringName` format through the
/// type tail (`register_string_modulo_op` registers a `StringName` left operand,
/// `variant_op.cpp:205-210`).
#[test]
fn each_literal_folds_as_its_own_kind() {
    assert_eq!(
        last_const_fold("extends Node\nconst A = \"x\"\n"),
        Some(FoldedValue::String("x".to_owned()))
    );
    assert_eq!(
        last_const_fold("extends Node\nconst A = &\"x\"\n"),
        Some(FoldedValue::StringName("x".to_owned()))
    );
    assert_eq!(
        last_const_fold("extends Node\nconst A = ^\"x\"\n"),
        Some(FoldedValue::NodePath("x".to_owned()))
    );
}

/// A preload path reads through all three: Godot checks `Variant::STRING`, but
/// `can_convert_strict` has already coerced a `StringName`/`NodePath` argument by then.
#[test]
fn a_preload_path_reads_every_string_like() {
    for path in ["\"res://x.gd\"", "&\"res://x.gd\"", "^\"res://x.gd\""] {
        let src = format!("extends Node\nconst A = preload({path})\n");
        assert!(
            !errors(&src)
                .iter()
                .any(|e| e == "Preloaded path must be a constant string."),
            "{path} is a constant string"
        );
    }
}
