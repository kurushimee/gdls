//! Regression tests for #88 — utility functions referenced as first-class Callables
//! (the `reduce_identifier` arm at analyzer.cpp:4641-4652).
//!
//! A bare (non-callee) reference to a Variant utility (`print`, `abs`, `floor`, …) or a
//! GDScript-only utility (`len`, `range`, …) must reduce to a constant Callable instead of
//! `Identifier "X" not declared in the current scope.`, and `const PRINTER = print` must stay
//! a constant initializer. Every positive/negative expectation here is oracle-pinned against
//! godot 4.6.3-stable `--headless --check-only` (see #88).

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
/// assertion keeps the test discriminating: the trimmed fixture DB lacks `call_deferred`, so
/// an unknown method on ANY base would also be silently permissive — zero errors alone would
/// hold even if `print` mis-resolved.
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
    let src =
        "extends Node\n\n\nfunc test() -> void:\n\tprint(\"x\")\n\tvar n := abs(-1)\n\tprint(n)\n";
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
