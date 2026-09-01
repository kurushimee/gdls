//! Emission net for utility-call argument validation (#440).
//!
//! Godot's `reduce_call` ends BOTH utility branches in `validate_call_arg` when the call does not
//! fold (`gdscript_analyzer.cpp:3498` and `:3549`), so arity, `UNSAFE_CALL_ARGUMENT`,
//! `NARROWING_CONVERSION` and the `Invalid argument` error all apply to `absi`, `deg_to_rad`,
//! `str_to_var` and the rest exactly as they apply to a method call. gdls's utility arm ran none
//! of it, so every one of those rows was missing.
//!
//! The `MethodInfo` Godot validates against is built by `info_from_utility_func`
//! (`gdscript_analyzer.cpp:55-81`), and two of its properties are what most of these tests are
//! really pinning: a vararg utility is given NO declared arguments, and a `Variant` parameter is
//! HARD, the one shape the ladder stays silent for.
//!
//! Every row is verbatim `Godot_v4.7.2-stable --headless --check-only` output.

use std::path::Path;

use gd_analyze::{analyze, NoCrossFile, Severity, StrictSettings, WarnPolicy};
use gd_project::{FileId, WarningConfig};
use gd_syntax::{parse, Dialect};
use gd_types::NativeDb;

fn native_db() -> NativeDb {
    let path =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../gd_types/tests/fixtures/trimmed_api.json");
    NativeDb::load(path.to_str().expect("utf-8 path"))
        .unwrap_or_else(|e| panic!("load native DB fixture at {}: {e}", path.display()))
}

fn policy() -> WarnPolicy {
    WarnPolicy::build(
        &WarningConfig::default(),
        &StrictSettings {
            enable_warnings: vec!["UNSAFE_CALL_ARGUMENT".to_owned()],
            ..Default::default()
        },
        Dialect::DEFAULT,
    )
}

/// Run `body` as the suite of a `func go()` and return every diagnostic message, warnings and
/// errors alike, in emission order.
fn all(body: &str) -> Vec<String> {
    let src = format!("extends Node\n\nfunc go() -> void:\n{body}");
    let tree = parse(&src).tree;
    let result = analyze(
        &tree,
        Some(FileId::new(1)),
        "utility.gd",
        &native_db(),
        &NoCrossFile,
        &policy(),
    );
    result
        .diagnostics
        .iter()
        .map(|d| d.message().to_owned())
        .collect()
}

/// The same, keeping only the hard errors — the shape most of these cases assert.
fn errors(body: &str) -> Vec<String> {
    let src = format!("extends Node\n\nfunc go() -> void:\n{body}");
    let tree = parse(&src).tree;
    let result = analyze(
        &tree,
        Some(FileId::new(1)),
        "utility.gd",
        &native_db(),
        &NoCrossFile,
        &policy(),
    );
    result
        .diagnostics
        .iter()
        .filter(|d| d.severity() == Severity::Error && d.warning_code().is_none())
        .map(|d| d.message().to_owned())
        .collect()
}

const NARROWING: &str = "Narrowing conversion (float is converted to int and loses precision).";

fn unsafe_arg(idx: usize, name: &str, want: &str, got: &str) -> String {
    format!(
        "The argument {idx} of the function \"{name}()\" requires the subtype \"{want}\" but the \
         supertype \"{got}\" was provided."
    )
}

#[test]
fn a_hard_float_into_an_int_parameter_narrows() {
    assert_eq!(
        all("\tvar f: float = 1.5\n\tprint(absi(f))\n"),
        vec![NARROWING.to_owned()]
    );
}

#[test]
fn an_untyped_argument_is_unsafe_per_parameter() {
    // `v` is untyped, so it reads as the supertype `Variant` against `absi`'s `int` and
    // `str_to_var`'s `String`.
    let rows = all("\tvar v = get_parent()\n\tprint(absi(v))\n\tprint(str_to_var(v))\n");
    assert!(
        rows.contains(&unsafe_arg(1, "absi", "int", "Variant")),
        "{rows:?}"
    );
    assert!(
        rows.contains(&unsafe_arg(1, "str_to_var", "String", "Variant")),
        "{rows:?}"
    );
}

#[test]
fn a_variant_parameter_accepts_anything_in_silence() {
    // `floor` and `snapped` declare `Variant` parameters, and a HARD `Variant` parameter is the
    // one shape `validate_call_arg` says nothing about (analyzer.cpp:6097). Getting the hardness
    // wrong here would put a warning on every one of these calls in a real project.
    assert!(
        all("\tvar v = get_parent()\n\tprint(floor(v))\n\tprint(snapped(v, v))\n").is_empty(),
        "{:?}",
        all("\tvar v = get_parent()\n\tprint(floor(v))\n\tprint(snapped(v, v))\n")
    );
}

#[test]
fn an_int_widening_into_a_float_parameter_stays_silent() {
    assert!(all("\tvar i: int = 2\n\tprint(deg_to_rad(i))\n").is_empty());
}

#[test]
fn an_incompatible_hard_argument_is_an_error() {
    assert_eq!(
        errors("\tprint(deg_to_rad(get_parent()))\n"),
        vec![
            "Invalid argument for \"deg_to_rad()\" function: argument 1 should be \"float\" but \
             is \"Node\"."
                .to_owned()
        ]
    );
}

#[test]
fn a_vararg_utility_declares_no_parameters_at_all() {
    // `info_from_utility_func` gives a vararg utility `METHOD_FLAG_VARARG` and no arguments
    // (analyzer.cpp:66-67), so neither the count checks nor the per-argument loop has anything
    // to say. The dump's nominal `arg1`/`arg2` on `max` and `print` must not leak through as an
    // arity bound.
    for body in [
        "\tprint(max(1, 2, 3))\n",
        "\tvar f: float = 1.5\n\tprint(max(f))\n",
        "\tvar v = get_parent()\n\tprint(max(v, v, v))\n",
    ] {
        assert!(all(body).is_empty(), "{body}: {:?}", all(body));
    }
}

#[test]
fn a_wrong_count_on_a_folding_call_still_errors() {
    // `absi()` and `absi(1, 2)` are vacuously all-constant, so Godot takes the fold path — and
    // execution rejects the count before CALL_OK (variant_utility.cpp:1798-1808), rendering the
    // same two templates through analyzer.cpp:3540-3545.
    assert_eq!(
        errors("\tprint(absi())\n"),
        vec![
            "Too few arguments for \"absi()\" call. Expected at least 1 but received 0.".to_owned()
        ]
    );
    assert_eq!(
        errors("\tprint(absi(1, 2))\n"),
        vec![
            "Too many arguments for \"absi()\" call. Expected at most 1 but received 2.".to_owned()
        ]
    );
}

#[test]
fn a_wrong_count_on_a_validating_call_errors_and_still_checks_the_arguments() {
    // Non-constant arguments take the validate path, where the count error and the per-argument
    // ladder both run — the ladder iterates `min(args, par_types)` and so still reaches argument 1.
    assert_eq!(
        all("\tvar f: float = 1.5\n\tprint(absi(f, f))\n"),
        vec![
            "Too many arguments for \"absi()\" call. Expected at most 1 but received 2.".to_owned(),
            NARROWING.to_owned(),
        ]
    );
}

#[test]
fn a_folding_call_with_the_right_count_stays_silent_and_constant() {
    assert!(all("\tprint(absi(1.5))\n").is_empty());
    // The fold still happens, so the value is usable in a constant expression.
    assert!(errors("\tconst K = absi(-10)\n\tprint(K)\n").is_empty());
}

#[test]
fn a_failed_count_on_a_folding_call_reports_the_count() {
    // CALL_OK is never reached upstream, so nothing is folded and the count error is what stands.
    let src = "extends Node\n\nconst K = absi()\n\nfunc go() -> void:\n\tprint(K)\n";
    let tree = parse(src).tree;
    let result = analyze(
        &tree,
        Some(FileId::new(1)),
        "utility.gd",
        &native_db(),
        &NoCrossFile,
        &policy(),
    );
    let rows: Vec<String> = result
        .diagnostics
        .iter()
        .filter(|d| d.severity() == Severity::Error && d.warning_code().is_none())
        .map(|d| d.message().to_owned())
        .collect();
    assert_eq!(
        rows,
        vec![
            "Too few arguments for \"absi()\" call. Expected at least 1 but received 0.".to_owned()
        ]
    );
    // Godot adds `Assigned value for constant "K" isn't a constant expression.` here, because the
    // call never reached CALL_OK and so never became one. gdls's const walk blames a foldable
    // utility's ARGUMENTS rather than the call, and this one has none to blame — a standing
    // under-report, and the safe direction: blaming the call outright would fire wherever gdls
    // fails to fold something Godot folds.
}

// ===================================================================================================
// The GDScript-only half (gdscript_utility_functions.cpp:570-589).
// ===================================================================================================

#[test]
fn the_one_default_argument_in_the_table_is_color8s() {
    // `varray(255)` at :585-586 makes `Color8` 3..4. Missing it would put "Expected at least 4" on
    // the three-argument form, which is the one everybody writes.
    assert!(all("\tprint(Color8(1, 2, 3))\n").is_empty());
    assert!(all("\tprint(Color8(1, 2, 3, 4))\n").is_empty());
    assert_eq!(
        errors("\tprint(Color8(1, 2))\n"),
        vec![
            "Too few arguments for \"Color8()\" call. Expected at least 3 but received 2."
                .to_owned()
        ]
    );
    assert_eq!(
        errors("\tprint(Color8(1, 2, 3, 4, 5))\n"),
        vec![
            "Too many arguments for \"Color8()\" call. Expected at most 4 but received 5."
                .to_owned()
        ]
    );
    assert_eq!(
        all("\tvar f: float = 1.5\n\tprint(Color8(f, 2, 3))\n"),
        vec![NARROWING.to_owned()]
    );
}

#[test]
fn a_noargs_registration_that_is_not_vararg_bounds_at_zero() {
    // `print_stack` and `get_stack` are `NOARGS` with the vararg column false, so an argument is
    // one too many — unlike `range` and `print_debug`, which are `NOARGS` and vararg.
    assert_eq!(
        errors("\tprint_stack(1)\n"),
        vec![
            "Too many arguments for \"print_stack()\" call. Expected at most 0 but received 1."
                .to_owned()
        ]
    );
    assert!(all("\tprint_stack()\n\tprint(get_stack())\n").is_empty());
    assert!(all("\tprint(range(1, 2, 3))\n\tprint_debug(1, 2, 3)\n").is_empty());
}

#[test]
fn a_gdscript_utilitys_typed_parameter_is_unsafe_for_a_variant_argument() {
    let rows = all(
        "\tvar v = get_parent()\n\tprint(char(v))\n\tprint(ord(v))\n\tprint(load(v))\n\
         \tprint(dict_to_inst(v))\n",
    );
    for (name, want) in [
        ("char", "int"),
        ("ord", "String"),
        ("load", "String"),
        ("dict_to_inst", "Dictionary"),
    ] {
        assert!(
            rows.contains(&unsafe_arg(1, name, want, "Variant")),
            "{rows:?}"
        );
    }
}

#[test]
fn an_argvar_parameter_stays_silent_and_still_bounds_the_count() {
    // `len` and `is_instance_of` take `ARGVAR`, a hard `Variant`, so no argument is ever unsafe —
    // but the count still is.
    assert!(
        all("\tvar v = get_parent()\n\tprint(len(v))\n\tprint(is_instance_of(v, v))\n").is_empty()
    );
    assert_eq!(
        errors("\tprint(len(\"ab\", \"cd\"))\n"),
        vec![
            "Too many arguments for \"len()\" call. Expected at most 1 but received 2.".to_owned()
        ]
    );
}

#[test]
fn the_argtype_parameter_is_the_variant_type_enum() {
    // `ARGTYPE` names the `Variant.Type` global enum, so an `int` constant passes and a `String`
    // draws both the const-narrowing message and the argument error.
    assert!(all("\tvar v = get_parent()\n\tprint(convert(v, TYPE_INT))\n").is_empty());
    assert_eq!(
        errors("\tvar v = get_parent()\n\tprint(convert(v, \"x\"))\n"),
        vec![
            "Cannot pass a value of type \"String\" as \"Variant.Type\".".to_owned(),
            "Invalid argument for \"convert()\" function: argument 2 should be \"Variant.Type\" \
             but is \"String\"."
                .to_owned(),
        ]
    );
}

#[test]
fn the_signature_table_and_the_return_table_answer_for_the_same_names() {
    // Three tables key off the same registration (`gd_utility_return_type`,
    // `is_gd_utility_constant`, and the signature table). A name in one and not another is a
    // silent gap, so pin the set.
    for name in [
        "convert",
        "type_exists",
        "char",
        "ord",
        "range",
        "load",
        "inst_to_dict",
        "dict_to_inst",
        "Color8",
        "print_debug",
        "print_stack",
        "get_stack",
        "len",
        "is_instance_of",
    ] {
        // A call with no arguments reaches the arm for every one of these names; a name the
        // signature table did not answer for would skip the count check and stay silent, and a
        // name the return table did not answer for would fall through to method dispatch and
        // report a missing function instead.
        let rows = errors(&format!("\tvar _x = {name}()\n"));
        assert!(
            rows.iter().all(|m| !m.contains("not found in base")),
            "{name} fell through to method dispatch: {rows:?}"
        );
    }
}

#[test]
fn the_display_table_answers_for_the_same_names_as_the_analyzer_tables() {
    // #584 added a fourth table off the same registration, `gd_types::GDSCRIPT_UTILITY_FUNCTIONS`,
    // which renders the `@GDScript.gd` stub page and the hover for a bare call. It lives in
    // `gd_types` (below `gd_analyze`), so this is the only crate that can see both halves and
    // catch a name in one and not the other.
    let display: Vec<&str> = gd_types::GDSCRIPT_UTILITY_FUNCTIONS
        .iter()
        .map(|u| u.name)
        .collect();
    assert_eq!(
        display,
        [
            "convert",
            "type_exists",
            "char",
            "ord",
            "range",
            "load",
            "inst_to_dict",
            "dict_to_inst",
            "Color8",
            "print_debug",
            "print_stack",
            "get_stack",
            "len",
            "is_instance_of",
        ],
        "the display table must hold the REGISTER_FUNC set, in registration order"
    );
    for name in &display {
        assert!(
            gd_analyze::is_gdscript_utility(name),
            "{name} renders but the analyzer does not resolve it"
        );
        assert!(
            !gd_types::is_variant_utility(name),
            "{name} is GDScript-only; a Variant utility of the same name would render twice"
        );
    }
}
