//! #380 — a builtin constructor's arguments, checked against its overload list.
//!
//! `reduce_call` typed the RESULT of `Vector2(1, 2, 3, 4, 5)` and never looked at what was inside
//! the parentheses, so every arity and type mismatch passed in silence.
//!
//! Godot forks here: an all-constant call to a non-shared type runs the real `Variant::construct`
//! and reads a `Callable::CallError` back, while everything else walks the constructor list against
//! the arguments' static types. gdls has no `Variant::construct`, so the constant fork is
//! reproduced from the same static data the dispatch itself reads — exact arity plus
//! `can_convert_strict` per argument over the dump's overloads.
//!
//! Every expectation below is pinned against `godot --headless --check-only` at 4.7.2, warnings
//! included (promoted to errors through `project.godot` so `--check-only` prints them).

use std::path::Path;

use gd_analyze::{
    analyze_with_options, AnalyzeOptions, NoCrossFile, Severity, StrictSettings, WarnPolicy,
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

/// Every diagnostic `src` produces, as `(is_error, message)`, so a row can pin the warnings a
/// constructor call draws as precisely as it pins the errors.
fn diagnose(src: &str) -> Vec<(bool, String)> {
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
    // UNSAFE_CALL_ARGUMENT and NARROWING_CONVERSION both default to Ignore, so the warning rows
    // need every code turned on — the same demotion the conformance harness applies.
    let mut config = WarningConfig::default();
    for &name in gd_analyze::warnings::WARN_NAMES.iter() {
        config
            .levels
            .insert(name.to_ascii_lowercase(), gd_project::WarnLevel::Warn);
    }
    let policy = WarnPolicy::build(&config, &StrictSettings::default(), dialect);
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
    .map(|d| (d.severity() == Severity::Error, d.message().to_string()))
    .collect()
}

/// `body` inside a function whose parameters cover the typed shapes the rows need.
fn errors(body: &str) -> Vec<String> {
    diagnose(&format!(
        "extends Node\nfunc f(n: Node, v: Variant, s: String, fl: float) -> void:\n{body}"
    ))
    .into_iter()
    .filter(|(is_err, _)| *is_err)
    .map(|(_, m)| m)
    .collect()
}

/// The warnings a constructor call draws. The unused-parameter noise from the shared signature is
/// dropped so a row reads as what it is about.
fn warnings(body: &str) -> Vec<String> {
    diagnose(&format!(
        "extends Node\nfunc f(n: Node, v: Variant, s: String, fl: float) -> void:\n{body}"
    ))
    .into_iter()
    .filter(|(is_err, m)| !*is_err && !m.starts_with("The parameter "))
    .map(|(_, m)| m)
    .collect()
}

fn no_match(sig: &str) -> Vec<String> {
    let ty = sig.split('(').next().expect("a signature names its type");
    vec![format!(
        r#"No constructor of "{ty}" matches the signature "{sig}"."#
    )]
}

// ===================================================================================================
// The constant fork — Variant::construct's dispatch, reproduced from the overload list.
// ===================================================================================================

/// The issue's own case: five constant arguments, no five-argument overload.
#[test]
fn too_many_constant_arguments_report_the_signature_not_a_count() {
    assert_eq!(
        errors("\tprint(Vector2(1, 2, 3, 4, 5))\n"),
        no_match("Vector2(int, int, int, int, int)")
    );
}

/// Right arity, wrong types. `can_convert_strict` rejects `String → float`, so no overload survives
/// and the message is the same shape as an arity miss — Godot's dispatch loop cannot tell them
/// apart, and neither of its two count messages is ever reachable for a builtin constructor.
#[test]
fn constant_arguments_of_the_wrong_type_report_the_signature() {
    assert_eq!(
        errors("\tprint(Vector2(\"a\", \"b\"))\n"),
        no_match("Vector2(String, String)")
    );
}

/// `null` renders as `null`, not `Nil` — Godot's `DataType::to_string` for a builtin NIL
/// (`gdscript_parser.cpp:5341-5343`).
#[test]
fn a_null_argument_renders_as_null() {
    assert_eq!(
        errors("\tprint(Vector2(null))\n"),
        no_match("Vector2(null)")
    );
}

/// A class used as a value is `GDScriptNativeClass`, and its VALUE type is `Object`, which converts
/// strictly to nothing `Vector2` takes.
#[test]
fn a_metatype_argument_renders_as_gdscriptnativeclass() {
    assert_eq!(
        errors("\tprint(Vector2(Node))\n"),
        no_match("Vector2(GDScriptNativeClass)")
    );
}

/// A failed construct is not constant, so the fold is dropped and the constant it initializes is
/// left unfoldable. Godot draws a second error there, and since #400 so does gdls: the
/// never-constant walk now reads the fold line for an identifier callee instead of skipping it.
/// Both halves, in this order.
#[test]
fn a_failed_constant_constructor_reports_both_halves() {
    let msgs: Vec<String> = diagnose("extends Node\nconst X = Vector2(1, 2, 3)\n")
        .into_iter()
        .filter(|(is_err, _)| *is_err)
        .map(|(_, m)| m)
        .collect();
    assert_eq!(
        msgs,
        vec![
            r#"No constructor of "Vector2" matches the signature "Vector2(int, int, int)"."#
                .to_owned(),
            r#"Assigned value for constant "X" isn't a constant expression."#.to_owned(),
        ]
    );
}

/// The one in-body rejection the dispatch does not already filter out: `int`/`float` built from the
/// single-`String` overload, given a `NodePath`. `can_convert_strict` admits `NodePath` where a
/// `String` is wanted, and `Variant::is_string()` then rejects it
/// (`variant_construct.h:225-244`).
#[test]
fn a_nodepath_into_the_from_string_constructor_names_the_argument() {
    assert_eq!(
        errors("\tprint(int(NodePath(\"x\")))\n"),
        vec![
            r#"Invalid argument for "int()" constructor: argument 1 should be "String" but is "NodePath"."#
                .to_owned()
        ]
    );
}

/// Silence rows for the constant fork. An implicit conversion the dispatch accepts is not an error,
/// however odd it reads: `can_convert_strict(Int, Color)` is true, and `Vector2i(1.5, 2)` converts
/// its float without a narrowing warning, because the warnings live on the other fork entirely.
#[test]
fn constant_arguments_the_dispatch_accepts_stay_silent() {
    for body in [
        "\tprint(Color(1, 2))\n",
        "\tprint(Vector2i(1.5, 2))\n",
        "\tprint(Callable(null, \"x\"))\n",
        "\tprint(Vector2(SIDE_LEFT, 2))\n",
    ] {
        assert_eq!(errors(body), Vec::<String>::new(), "{body:?}");
    }
}

/// The fold survives every non-error exit, so a constructor call stays usable where a constant is
/// required — including through a shared type, which never takes the constant fork at all.
#[test]
fn a_successful_constructor_stays_a_constant_expression() {
    assert_eq!(
        diagnose(concat!(
            "extends Node\n",
            "const A = Array()\n",
            "const B = Array(A)\n",
            "const C = Vector2(Vector2(1, 2))\n",
        ))
        .into_iter()
        .filter(|(is_err, _)| *is_err)
        .map(|(_, m)| m)
        .collect::<Vec<String>>(),
        Vec::<String>::new()
    );
}

// ===================================================================================================
// The general fork — the overload walk over static types.
// ===================================================================================================

/// Hard-typed arguments that match no overload. The walk reads static types, so the message names
/// the annotation rather than a value.
#[test]
fn hard_typed_arguments_matching_no_overload_report_the_signature() {
    assert_eq!(
        errors("\tprint(Vector2(s, s))\n"),
        no_match("Vector2(String, String)")
    );
    assert_eq!(errors("\tprint(Vector2(n))\n"), no_match("Vector2(Node)"));
}

/// A soft builtin is checked exactly like a hard one — `var s = "a"` is still a `String`.
#[test]
fn soft_builtin_arguments_are_checked_too() {
    assert_eq!(
        errors("\tvar t = \"a\"\n\tprint(Vector2(t, t))\n"),
        no_match("Vector2(String, String)")
    );
}

/// A `Variant` argument is compatible with every parameter, so only arity can fail — and Godot does
/// fail it.
#[test]
fn variant_arguments_can_still_miss_on_arity() {
    assert_eq!(
        errors("\tprint(Vector2(v, v, v))\n"),
        no_match("Vector2(Variant, Variant, Variant)")
    );
}

/// A shared type never takes the constant fork, so even an all-constant call walks the overloads.
#[test]
fn a_shared_type_walks_the_overloads_even_when_every_argument_is_constant() {
    assert_eq!(errors("\tprint(Array(1))\n"), no_match("Array(int)"));
}

/// A single `Variant` argument is never an error, only unsafe. Godot builds the acceptable-subtype
/// union by walking every Variant type in enum order and keeping the ones that convert strictly to
/// the target, with its own quoting for one versus several.
#[test]
fn a_single_variant_argument_warns_with_the_acceptable_subtype_union() {
    assert_eq!(
        warnings("\tprint(Vector2(v))\n"),
        vec![r#"The argument 1 of the constructor "Vector2()" requires the subtype "Vector2" or "Vector2i" but the supertype "Variant" was provided."#.to_owned()]
    );
    assert_eq!(
        warnings("\tprint(int(v))\n"),
        vec![r#"The argument 1 of the constructor "int()" requires the subtype "int", "bool", or "float" but the supertype "Variant" was provided."#.to_owned()]
    );
}

/// Past one argument the union shape does not apply, and each Variant argument warns against the
/// parameter it landed on.
#[test]
fn several_variant_arguments_warn_per_parameter() {
    assert_eq!(
        warnings("\tprint(Vector2(v, v))\n"),
        vec![
            r#"The argument 1 of the constructor "Vector2()" requires the subtype "float" but the supertype "Variant" was provided."#.to_owned(),
            r#"The argument 2 of the constructor "Vector2()" requires the subtype "float" but the supertype "Variant" was provided."#.to_owned(),
        ]
    );
}

/// A float reaching an int parameter narrows, unless the target IS `int`.
#[test]
fn a_float_argument_into_an_int_parameter_narrows() {
    assert_eq!(
        warnings("\tprint(Vector2i(fl, 2))\n"),
        vec!["Narrowing conversion (float is converted to int and loses precision).".to_owned()]
    );
    assert_eq!(
        warnings("\tprint(Vector2(fl, 2.0))\n"),
        Vec::<String>::new()
    );
}

// ===================================================================================================
// The degrade guards — gdls states Godot has no equivalent for.
// ===================================================================================================

/// An argument gdls could not resolve already carries its own error. Checking it would manufacture
/// a "no constructor matches" over a signature rendered from a type that does not exist, which is
/// the failure shape a diagnostic slice has to be built against.
#[test]
fn an_unresolved_argument_produces_no_constructor_error() {
    let msgs = errors("\tprint(Vector2(undeclared_x, 1))\n");
    assert!(
        msgs.iter().all(|m| !m.contains("No constructor")),
        "the undeclared identifier is the only complaint, got {msgs:?}"
    );
    assert!(
        !msgs.is_empty(),
        "the undeclared identifier is still reported"
    );
}

/// A builtin the dump does not carry has no overload list to check against, so nothing is checked.
/// `String` is absent from the trimmed fixture; against a real dump this call resolves and reports
/// exactly as Godot does.
#[test]
fn a_builtin_missing_from_the_dump_is_never_checked() {
    assert_eq!(errors("\tprint(String(1, 2, 3))\n"), Vec::<String>::new());
}
