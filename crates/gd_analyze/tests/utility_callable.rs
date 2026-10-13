//! Regression tests for #88 — utility functions referenced as first-class Callables
//! (the `reduce_identifier` arm at analyzer.cpp:4641-4652).
//!
//! A bare (non-callee) reference to a Variant utility (`print`, `abs`, `floor`, …) or a
//! GDScript-only utility (`len`, `range`, …) must reduce to a constant Callable instead of
//! `Identifier "X" not declared in the current scope.`, and `const PRINTER = print` must stay
//! a constant initializer. Every positive/negative expectation here is oracle-pinned against
//! godot 4.6.3-stable `--headless --check-only` (see #88).

use gd_syntax::Dialect;
use std::path::Path;

use gd_analyze::{
    analyze, DataType, DtKind, NoCrossFile, Severity, StrictSettings, TypeSource, VariantType,
    WarnPolicy,
};
use gd_syntax::ast::NodeKind;
use gd_types::NativeDb;

/// The committed native-DB fixture (carries the Variant utility table: `print`, `abs`,
/// `floor`, `lerp`, …), loaded once per call.
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

/// Analyze `src` and return the Error-severity diagnostic messages (warnings excluded — the
/// godot CLI oracle only surfaces errors, so errors are what the fidelity claim covers).
fn errors(src: &str) -> Vec<String> {
    let tree = gd_syntax::parse(src).tree;
    let result = analyze(&tree, None, "t.gd", &native_db(), &NoCrossFile, &policy());
    result
        .diagnostics
        .iter()
        .filter(|d| d.severity() == Severity::Error)
        .map(|d| d.message().to_owned())
        .collect()
}

/// The type and folded-constant flag of the LAST occurrence of identifier `name` that got a
/// type set (mirrors autoload_typing.rs's scan helper).
fn ident_info(src: &str, name: &str) -> (DataType, bool) {
    let tree = gd_syntax::parse(src).tree;
    let result = analyze(&tree, None, "t.gd", &native_db(), &NoCrossFile, &policy());
    let mut found = (DataType::default(), false);
    for node_id in tree.iter_ids() {
        if let NodeKind::Identifier(ident) = &tree.get(node_id).kind {
            if ident.name == name {
                let dt = result.types.get(node_id);
                if dt.is_set() {
                    found = (dt.clone(), result.folds.is_reduced(node_id));
                }
            }
        }
    }
    found
}

/// The issue's minimal repro: `print.call_deferred(message)` is a builtin-method call on the
/// constant Callable `print` reduces to — godot accepts it with zero diagnostics. The type
/// assertion keeps the test discriminating: zero errors alone would hold even if `print`
/// mis-resolved to some base whose members gdls cannot introspect. Since #256 the fixture
/// carries `Callable`'s full method table, so `call_deferred` resolving is a real hit and a
/// mis-resolution would show up as `Function "call_deferred()" not found in base Callable.`
#[test]
fn print_call_deferred_is_clean() {
    let src =
        "extends Node\n\n\nfunc test(message: String) -> void:\n\tprint.call_deferred(message)\n";
    assert_eq!(errors(src), Vec::<String>::new());
    let (dt, _) = ident_info(src, "print");
    assert_eq!(dt.builtin_type, VariantType::Callable);
}

/// A bare Variant-utility reference carries Godot's `make_callable_type` shape: hard BUILTIN
/// Callable, constant, and reduced (the `reduced_value = Callable(...)` stand-in).
#[test]
fn bare_utility_reference_types_as_constant_callable() {
    let src =
        "extends Node\n\n\nfunc test() -> void:\n\tvar c: Callable = print\n\tc.call(\"x\")\n";
    assert_eq!(errors(src), Vec::<String>::new());
    let (dt, reduced) = ident_info(src, "print");
    assert_eq!(dt.kind, DtKind::Builtin);
    assert_eq!(dt.builtin_type, VariantType::Callable);
    assert_eq!(dt.type_source, TypeSource::AnnotatedExplicit);
    assert!(dt.is_constant, "utility Callable must be constant");
    assert!(
        reduced,
        "utility Callable must fold (Godot sets reduced_value)"
    );
}

/// `:=` inference from a utility reference lands on Callable, not an inference error.
#[test]
fn inferred_declaration_from_utility_is_clean() {
    let src = "extends Node\n\n\nfunc test() -> void:\n\tvar f := floor\n\tf.call(1.5)\n";
    assert_eq!(errors(src), Vec::<String>::new());
}

/// GDScript-only utilities (`len`, `range` — the `gd_utility_return_type` table, not the
/// NativeDb) previously escaped the not-declared error via a step-10 guard but stayed
/// untyped; they must now carry the same constant-Callable type (#88's secondary scope).
#[test]
fn gdscript_only_utility_gets_callable_type() {
    let src = "extends Node\n\n\nfunc test() -> void:\n\tvar l = len\n\tl.call(\"abc\")\n\tvar r := range\n\tr.call(3)\n";
    assert_eq!(errors(src), Vec::<String>::new());
    let (dt, reduced) = ident_info(src, "len");
    assert_eq!(dt.kind, DtKind::Builtin);
    assert_eq!(dt.builtin_type, VariantType::Callable);
    assert!(dt.is_constant, "GDScript-only utility must be constant");
    assert!(reduced, "GDScript-only utility must fold");
}

/// The latent edge from #88's follow-up comment: `const PRINTER = print` is legal in 4.6.3
/// (the arm sets `is_constant` AND `reduced_value`), so it must pass gdls's
/// constant-expression gates too.
#[test]
fn const_assigned_utility_callable_is_clean() {
    let src =
        "extends Node\n\nconst PRINTER = print\n\n\nfunc test() -> void:\n\tPRINTER.call(\"x\")\n";
    assert_eq!(errors(src), Vec::<String>::new());
}

/// Assignment to a utility name hits the constant gate — godot 4.6.3:
/// `Cannot assign a new value to a constant.` (oracle-pinned for BOTH families), NOT
/// "not declared".
#[test]
fn assigning_to_utility_is_constant_error() {
    for stmt in ["print = 5", "len = 5"] {
        let src = format!("extends Node\n\n\nfunc test() -> void:\n\t{stmt}\n");
        assert_eq!(
            errors(&src),
            vec!["Cannot assign a new value to a constant.".to_owned()],
            "for `{stmt}`"
        );
    }
}

/// Control: a genuinely undeclared identifier in the same shape still errors.
#[test]
fn undeclared_identifier_still_errors() {
    let src = "extends Node\n\n\nfunc test() -> void:\n\tblah.call_deferred(1)\n";
    assert_eq!(
        errors(src),
        vec![r#"Identifier "blah" not declared in the current scope."#.to_owned()]
    );
}

/// Utilities passed as call arguments (`arr.map(floor)`) reduce in non-callee position.
#[test]
fn utility_as_call_argument_is_clean() {
    let src = "extends Node\n\n\nfunc test() -> void:\n\tvar arr := [1.5, 2.5]\n\tvar floored := arr.map(floor)\n\tprint(floored)\n";
    assert_eq!(errors(src), Vec::<String>::new());
}

/// Direct utility calls keep dispatching by name through `reduce_call`'s utility arms — the
/// new identifier arm is skipped in callee position, so no
/// `Name "print" is a Callable. You can call it with "print.call()" instead.` regression.
#[test]
fn direct_utility_call_unaffected() {
    // `absi`, not `abs`: the dump types `abs` as returning Variant, so `:=` on it draws
    // INFERENCE_ON_VARIANT — a true positive that has nothing to do with what this test pins.
    let src =
        "extends Node\n\n\nfunc test() -> void:\n\tprint(\"x\")\n\tvar n := absi(-1)\n\tprint(n)\n";
    assert_eq!(errors(src), Vec::<String>::new());
}

/// Ladder order: a local shadowing a utility name still wins (suite locals resolve before
/// the utility arm, mirroring Godot's lookup order).
#[test]
fn local_shadowing_wins_over_utility() {
    let src = "extends Node\n\n\nfunc test() -> void:\n\tvar print := 1\n\tvar y := print + 1\n\tassert(y == 2)\n";
    assert_eq!(errors(src), Vec::<String>::new());
    let (dt, _) = ident_info(src, "print");
    assert_eq!(
        dt.builtin_type,
        VariantType::Int,
        "the int local must shadow the utility Callable"
    );
}

// --- Absent-DB provenance ------------------------------------------------------------------------
// With NO native dump (`ApiProvenance::Absent`), `Variant::has_utility_function` is still
// compile-time true in Godot, so a bare Variant utility must STILL reduce to a constant Callable
// rather than fall through to `Identifier "X" not declared`. The DB-independent registry
// (`gd_types::is_variant_utility`) is what makes this hold with no dump present.

/// Error-severity messages analyzing `src` against an empty (Absent) DB.
fn errors_absent(src: &str) -> Vec<String> {
    let tree = gd_syntax::parse(src).tree;
    let result = analyze(
        &tree,
        None,
        "t.gd",
        &NativeDb::empty(),
        &NoCrossFile,
        &policy(),
    );
    result
        .diagnostics
        .iter()
        .filter(|d| d.severity() == Severity::Error)
        .map(|d| d.message().to_owned())
        .collect()
}

/// Type + folded flag of the last typed occurrence of `name`, analyzed against an empty DB.
fn ident_info_absent(src: &str, name: &str) -> (DataType, bool) {
    let tree = gd_syntax::parse(src).tree;
    let result = analyze(
        &tree,
        None,
        "t.gd",
        &NativeDb::empty(),
        &NoCrossFile,
        &policy(),
    );
    let mut found = (DataType::default(), false);
    for node_id in tree.iter_ids() {
        if let NodeKind::Identifier(ident) = &tree.get(node_id).kind {
            if ident.name == name {
                let dt = result.types.get(node_id);
                if dt.is_set() {
                    found = (dt.clone(), result.folds.is_reduced(node_id));
                }
            }
        }
    }
    found
}

/// The empty-DB gap: a Variant utility absent from the (empty) DB still reduces to a constant,
/// reduced Callable — `print`/`floor` no longer false-positive `not declared` without a dump.
#[test]
fn variant_utility_resolves_under_absent_db() {
    let floor_src = "extends Node\n\n\nfunc test() -> void:\n\tvar f := floor\n\tf.call(1.5)\n";
    assert_eq!(errors_absent(floor_src), Vec::<String>::new());

    let print_src =
        "extends Node\n\n\nfunc test() -> void:\n\tvar c: Callable = print\n\tc.call(\"x\")\n";
    assert_eq!(errors_absent(print_src), Vec::<String>::new());
    let (dt, reduced) = ident_info_absent(print_src, "print");
    assert_eq!(dt.kind, DtKind::Builtin);
    assert_eq!(dt.builtin_type, VariantType::Callable);
    assert!(
        dt.is_constant,
        "utility Callable must be constant under an Absent DB"
    );
    assert!(reduced, "utility Callable must fold under an Absent DB");
}

/// The negative at `undeclared_identifier_still_errors` above fires against a project-derived
/// dump — and only there. Under `Absent` provenance every native lookup
/// misses, so the analyzer cannot tell a typo from a real native member and must say nothing —
/// the #256 rule that already governs `Cannot find member "x" in base "Y".`. `position` is the
/// case that proves it: it is a real `Node2D` property, and an empty DB knows nothing about it,
/// so an ungated step 10 would report a valid script as undeclared. #300.
#[test]
fn undeclared_identifier_stays_silent_under_absent_db() {
    let typo = "extends Node\n\n\nfunc test() -> void:\n\tblah.call_deferred(1)\n";
    assert_eq!(errors_absent(typo), Vec::<String>::new());

    let real = "extends Node2D\n\n\nfunc test() -> void:\n\tprint(position)\n";
    assert_eq!(errors_absent(real), Vec::<String>::new());
}

// --- Same-utility dictionary keys ----------------------------------------------------------------
// A bare utility folds to a constant Callable carrying its identity, so two same-utility keys are a
// provable duplicate. Every message + line below is oracle-pinned against godot 4.6.3-stable
// `--headless --check-only`.

/// `{print: 1, print: 2}` — both keys are the Variant utility `print`, reported with the callable's
/// `@GlobalScope::print` text form (the duplicate key is silent before utilities fold).
#[test]
fn duplicate_variant_utility_key_errors() {
    let src = "extends Node\n\n\nfunc test() -> void:\n\tvar d := {print: 1, print: 2}\n";
    assert_eq!(
        errors(src),
        vec![
            r#"Key "@GlobalScope::print" was already used in this dictionary (at line 5)."#
                .to_owned()
        ]
    );
}

/// The GDScript-only family qualifies under `@GDScript` (`{len: 1, len: 2}`), matching
/// `GDScriptUtilityCallable`'s constructor precedence.
#[test]
fn duplicate_gdscript_utility_key_errors() {
    let src = "extends Node\n\n\nfunc test() -> void:\n\tvar d := {len: 1, len: 2}\n";
    assert_eq!(
        errors(src),
        vec![r#"Key "@GDScript::len" was already used in this dictionary (at line 5)."#.to_owned()]
    );
}

/// Distinct utilities are distinct keys — the identity match must not over-fire.
#[test]
fn distinct_utility_keys_are_clean() {
    let src = "extends Node\n\n\nfunc test() -> void:\n\tvar d := {print: 1, abs: 2}\n";
    assert_eq!(errors(src), Vec::<String>::new());
}

/// Regression: a non-utility opaque constant (`Vector3.UP`) still compares never-equal, so distinct
/// builtin-constant keys do not false-positive — the documented Opaque policy is preserved.
#[test]
fn distinct_builtin_constant_keys_are_clean() {
    let src =
        "extends Node\n\n\nfunc test() -> void:\n\tvar d := {Vector3.UP: 1, Vector3.DOWN: 2}\n";
    assert_eq!(errors(src), Vec::<String>::new());
}

/// `folded_variant_type` still reports Callable for a utility callable, so an invalid operator use
/// routes through Godot's reduced-operand template, not the type-only tail.
#[test]
fn utility_callable_operator_error_names_callable() {
    let src = "extends Node\n\n\nfunc test() -> void:\n\tvar x := print + 1\n";
    assert_eq!(
        errors(src),
        vec![
            "Invalid operands to operator +, Callable and int.".to_owned(),
            r#"Cannot infer the type of "x" variable because the value is "null"."#.to_owned(),
        ]
    );
}

// --- #300: `@GlobalScope` integer constants come from the dump ------------------------------------
// 4.7 added `UINT8_MAX` / `INT64_MIN` and nine siblings to the dump's `global_constants` array
// (4.6's is empty). `reduce_identifier` step 8 used to hard-code only the float set
// (`PI`/`TAU`/`INF`/`NAN`), so every one of these read as undeclared once the uppercase hedge came
// out. The `trimmed_api.json` fixture is a 4.6 dump and carries none of them, so the DB is built
// inline here.

/// A minimal `Exact` DB carrying one global constant and nothing else.
fn db_with_global_constant() -> gd_types::NativeDb {
    gd_types::NativeDb::from_json(
        r#"{
            "header": {"version_major":4,"version_minor":7,"version_patch":2,
                       "version_status":"stable","version_build":"official",
                       "version_full_name":"Godot Engine v4.7.2.stable.official",
                       "precision":"single"},
            "global_constants": [{"name":"INT32_MAX","value":2147483647,"is_bitfield":false}],
            "global_enums": [], "utility_functions": [],
            "builtin_classes": [], "classes": [], "singletons": []
        }"#,
    )
    .expect("inline dump parses")
}

fn errors_with(db: &gd_types::NativeDb, src: &str) -> Vec<String> {
    let tree = gd_syntax::parse(src).tree;
    let result = analyze(&tree, None, "t.gd", db, &NoCrossFile, &policy());
    result
        .diagnostics
        .iter()
        .filter(|d| d.severity() == Severity::Error)
        .map(|d| d.message().to_owned())
        .collect()
}

#[test]
fn dump_global_constant_resolves_and_folds_as_int() {
    let db = db_with_global_constant();
    // No `print` — the inline DB carries no utilities either.
    let src = "func test() -> void:\n\tvar x: int = INT32_MAX\n\tx += 1\n";
    assert_eq!(
        errors_with(&db, src),
        Vec::<String>::new(),
        "a global constant the dump carries must resolve, and as `int` (an `int` annotation is \
         what proves the type, not just that it resolved)"
    );
}

#[test]
fn misspelled_global_constant_is_reported() {
    let db = db_with_global_constant();
    let src = "func test() -> void:\n\tvar x = INT32_MAXX\n\tx = 1\n";
    assert_eq!(
        errors_with(&db, src),
        vec![r#"Identifier "INT32_MAXX" not declared in the current scope."#.to_owned()],
        "the lookup must be a real membership test, not a prefix or shape heuristic"
    );
}
