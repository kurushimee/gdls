//! #463 — comparisons are registered per type pair, not accepted wholesale.
//!
//! `Variant::evaluate` is a bare table lookup (variant_op.cpp:1041-1057), and
//! `_register_variant_operators` registers each comparison over an explicit pair list. gdls
//! accepted every pair, in both halves: the type-only path answered `Bool` for anything, and the
//! constant path folded `1 == "a"` to `false` rather than failing the way `1 < "a"` already did.
//!
//! What bounds the blast radius is upstream's own gate: an unregistered pair is an ERROR only when
//! both operands are hard-typed (`hard_operation` at gdscript_analyzer.cpp:6299, `r_valid =
//! !hard_operation` at :6320), and `reduce_binary_op` never reaches the table at all when either
//! side is Variant (:3179-3182). Both already exist in gdls and serve the arithmetic tables.
//!
//! Every row is verbatim `Godot_v4.7.2-stable --headless --check-only` output, pinned at both
//! supported tags.

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
    let policy = WarnPolicy::build(
        &WarningConfig::default(),
        &StrictSettings::default(),
        dialect,
    );
    analyze_with_options(
        &tree,
        Some(FileId::new(1)),
        "ops.gd",
        &native_db(),
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

/// A function whose parameters cover every operand kind the rows below need, so one harness serves
/// the whole matrix. Hard annotations throughout: `hard_operation` is what turns an unregistered
/// pair into an error, and `u` is the deliberate soft one.
const PARAMS: &str = "i: int, s: String, n: Node, o: Object, v: Vector2, v3: Vector3, \
                      c: Color, sn: StringName, np: NodePath, fl: float, e: E463, g: G463, \
                      a: Array, b: Array, u";

fn script(body: &str) -> String {
    format!(
        "extends Node\n\nenum E463 {{ A, B }}\nenum G463 {{ X, Y }}\n\n\
         func f({PARAMS}) -> void:\n\t{body}\n"
    )
}

/// The type-only template (gdscript_analyzer.cpp:3187), whose operand names come from
/// `DataType::to_string`.
fn type_pair(a: &str, b: &str, op: &str) -> String {
    format!(r#"Invalid operands "{a}" and "{b}" for "{op}" operator."#)
}

/// The constant-fold template (gdscript_analyzer.cpp:3151-3156), whose operand names come from
/// `Variant::get_type_name` — so `Nil`, not `null`.
fn const_pair(op: &str, a: &str, b: &str) -> String {
    format!("Invalid operands to operator {op}, {a} and {b}.")
}

fn assert_errors(body: &str, want: &[String]) {
    for d in TAGS {
        assert_eq!(errors(&script(body), d), want.to_vec(), "{body} at {d:?}");
    }
}

fn assert_silent(body: &str) {
    assert_errors(body, &[]);
}

#[test]
fn a_mismatched_pair_of_hard_operands_is_an_error() {
    assert_errors("print(i == s)", &[type_pair("int", "String", "==")]);
    assert_errors("print(i != s)", &[type_pair("int", "String", "!=")]);
    assert_errors("print(i < s)", &[type_pair("int", "String", "<")]);
    assert_errors("print(v == v3)", &[type_pair("Vector2", "Vector3", "==")]);
    assert_errors("print(s == np)", &[type_pair("String", "NodePath", "==")]);
}

#[test]
fn a_type_that_registers_equality_but_not_ordering_errors_on_ordering() {
    // `Color` is registered for `==` (variant_op.cpp:509) and for nothing else; `Object` likewise
    // (:514), so every object ordering fails whatever the two classes are.
    assert_silent("print(c == c)");
    assert_errors("print(c < c)", &[type_pair("Color", "Color", "<")]);
    assert_silent("print(n == o)");
    assert_errors("print(n < o)", &[type_pair("Node", "Object", "<")]);
}

#[test]
fn the_string_crossings_register_for_equality_only() {
    // `register_string_op` (variant_op.cpp:196-203) expands the equality registration to all four
    // String/StringName pairs; the ordering ops are registered same-type only.
    assert_silent("print(s == sn)");
    assert_errors("print(s < sn)", &[type_pair("String", "StringName", "<")]);
    assert_errors(
        r#"print("a" < &"b")"#,
        &[const_pair("<", "String", "StringName")],
    );
    assert_silent(r#"print("a" < "b")"#);
}

#[test]
fn bool_orders_under_less_and_greater_but_not_under_their_equal_forms() {
    // `OP_LESS` / `OP_GREATER` register `BOOL × BOOL` (variant_op.cpp:731/:762); `OP_LESS_EQUAL` /
    // `OP_GREATER_EQUAL` (:747-760/:778-791) do not. The asymmetry is upstream's, not a typo.
    assert_silent("print(true < false)");
    assert_silent("print(true > false)");
    assert_errors("print(true <= false)", &[const_pair("<=", "bool", "bool")]);
    assert_errors("print(true >= false)", &[const_pair(">=", "bool", "bool")]);
}

#[test]
fn a_bool_against_a_number_registers_nowhere() {
    assert_errors("print(true == 1)", &[const_pair("==", "bool", "int")]);
    assert_errors("print(true < 1)", &[const_pair("<", "bool", "int")]);
    assert_errors("print(1.0 <= true)", &[const_pair("<=", "float", "bool")]);
}

#[test]
fn a_constant_mismatch_fails_the_fold_instead_of_answering_false() {
    // The whole constant-path hole: `compare` widened or fabricated an answer where
    // `Variant::evaluate` sets `r_valid = false`.
    assert_errors(r#"print(1 == "a")"#, &[const_pair("==", "int", "String")]);
}

#[test]
fn null_compares_for_equality_with_anything_and_orders_with_nothing() {
    // Every type carries an `X × NIL` equality row (variant_op.cpp:533-607/:665-729), and the
    // analyzer short-circuits a builtin-NIL operand to BOOL before the table anyway (:3168-3173).
    // Ordering registers no NIL at all.
    assert_silent("print(n == null)");
    assert_silent("print(i == null)");
    assert_silent("print(v == null)");
    assert_silent("print(null == null)");
    // The two spellings of nothing: `Nil` from `Variant::get_type_name`, `null` from `DataType`.
    assert_errors("print(null < null)", &[const_pair("<", "Nil", "Nil")]);
    assert_errors("print(i < null)", &[type_pair("int", "null", "<")]);
}

#[test]
fn the_registered_pairs_stay_silent() {
    assert_silent("print(i == fl)");
    assert_silent("print(1 == 1.0)");
    assert_silent("print(a < b)");
    assert_silent("print(i == i)");
    assert_silent("print(v == v)");
    assert_silent("print(np == np)");
}

#[test]
fn an_enum_value_compares_as_an_int() {
    // `get_operation_type` coerces an ENUM operand to INT, or to DICTIONARY when it is a meta
    // (gdscript_analyzer.cpp:6283-6296), so two unrelated enums compare fine as values.
    assert_silent("print(i == e)");
    assert_silent("print(e < i)");
    assert_silent("print(e == g)");
    assert_silent("print(E463 == E463)");
    assert_errors("print(E463 == 1)", &[type_pair("ops.gd.E463", "int", "==")]);
}

#[test]
fn a_meta_enum_does_not_order() {
    // Two metas collapse to `DICTIONARY × DICTIONARY`, which registers equality only.
    //
    // 4.6 renders this one row differently: its fold gate had no `is_shared()` check, so Godot
    // 4.6 folds the two constant metas and reports the CONSTANT template with the coerced names
    // (`Dictionary and Dictionary`). gdls has no Dictionary fold to materialize, so it emits the
    // 4.7 wording at both tags — detection parity, one wording divergence, recorded in the
    // `docs/02` §11c delta table.
    assert_errors(
        "print(E463 < E463)",
        &[type_pair("ops.gd.E463", "ops.gd.E463", "<")],
    );
}

#[test]
fn a_soft_operand_buys_silence_on_both_sides() {
    // `hard_operation` is the firewall: an unregistered pair is only an error when BOTH operands
    // are hard. Every gdls degrade is a soft Variant, so this is also what keeps the change from
    // reporting on a line whose type gdls simply could not see.
    assert_silent("print(u == s)");
    assert_silent("print(u < n)");
    assert_silent("print(s == u)");
}

#[test]
fn a_degraded_variant_operand_stays_silent() {
    // The same trade `invalid_operands.rs` documents: a member read off an untyped local is a
    // degrade to gdls, so no comparison through it is claimed either way.
    let src = "extends Node\n\nfunc f() -> void:\n\
               \tvar dict = {}\n\tprint(dict.k == 1)\n\tprint(dict.k < \"x\")\n";
    for d in TAGS {
        assert_eq!(errors(src, d), Vec::<String>::new(), "{d:?}");
    }
}

#[test]
fn a_registered_comparison_still_folds_to_a_constant() {
    // The gate must not cost the fold: a `const` fed by a comparison is still a constant
    // expression.
    let src = "extends Node\n\nconst A: bool = \"a\" < \"b\"\nconst B: bool = 1 == 1.0\n";
    for d in TAGS {
        assert_eq!(errors(src, d), Vec::<String>::new(), "{d:?}");
    }
}
