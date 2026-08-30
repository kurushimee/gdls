//! Binary, unary, and ternary reduction — the three places gdls stamped a permissive `Inferred`
//! Variant where Godot stamps something a later gate can actually read (#337).
//!
//! The shape of the bug was always the same: an operation whose result gdls could not type came
//! out *soft Variant*, and every downstream gate (`has_no_type()`, `is_hard_type()`) is written to
//! stay quiet on exactly that, so `var x := untyped + 5` typed as Variant in silence. Three
//! separate causes sat behind it — a resolver gate that only trusted bare identifiers, a ternary
//! that never stamped its source, and a `reduce_unary_op` that never consulted the operator table
//! at all, so `-"hi"` was simply not an error.
//!
//! What keeps the fix from over-firing is one property of gdls's degrades: every one of them
//! yields a Variant-*kinded* type, so a soft NON-Variant kind is always genuine. The gate widens
//! on that, and a soft Variant operand still buys silence — a documented false negative, the same
//! trade `inference_failure.rs` makes.
//!
//! Every row is pinned against `godot --headless --check-only` at both 4.6.3 and 4.7.2.

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
    let db = native_db();
    let policy = WarnPolicy::build(
        &WarningConfig::default(),
        &StrictSettings::default(),
        dialect,
    );
    analyze_with_options(
        &tree,
        Some(FileId::new(1)),
        "ops.gd",
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
    // Severity, not the code: `INFERENCE_ON_VARIANT` is one of the four error-by-default
    // warnings, and it is the whole point of the ternary row below.
    .filter(|d| d.severity() == gd_analyze::Severity::Error)
    .map(|d| d.message().to_string())
    .collect()
}

const TAGS: [Dialect; 2] = [Dialect::Godot4_6, Dialect::Godot4_7];

fn script(decl: &str, body: &str) -> String {
    format!("extends Node\n\n{decl}\n\nfunc f() -> void:\n{body}\n")
}

fn no_set_type(name: &str) -> String {
    format!(
        r#"Cannot infer the type of "{name}" variable because the value doesn't have a set type."#
    )
}

/// The three cases the issue opened on. A soft operand, a mixed-hardness ternary, and a soft
/// unary operand all reach `:=` without a type it can infer.
#[test]
fn the_three_reported_inference_holes_are_closed() {
    for d in TAGS {
        assert_eq!(
            errors(
                &script("var untyped = 3", "\tvar x := untyped + 5\n\tprint(x)"),
                d
            ),
            vec![no_set_type("x")],
            "binary at {d:?}"
        );
        assert_eq!(
            errors(
                &script("", "\tvar c := true\n\tvar x := 1 if c else 1.5\n\tprint(x)"),
                d
            ),
            vec![
                "The variable type is being inferred from a Variant value, so it will be typed as Variant."
                    .to_owned()
            ],
            "ternary at {d:?}"
        );
        assert_eq!(
            errors(&script("", "\tvar p = 1\n\tvar x := -p\n\tprint(x)"), d),
            vec![no_set_type("x")],
            "unary at {d:?}"
        );
    }
}

/// An untyped parameter is a genuine `UNDETECTED` Variant, so every operator over it is
/// untrustworthy — including `and`, because the variant arm precedes the always-bool arm upstream.
#[test]
fn every_operator_over_an_untyped_parameter_blocks_inference() {
    for d in TAGS {
        for body in ["-p", "p + 5", "p + p", "not p", "p and p"] {
            let src =
                format!("extends Node\n\nfunc f(p) -> void:\n\tvar x := {body}\n\tprint(x)\n");
            assert_eq!(errors(&src, d), vec![no_set_type("x")], "{body} at {d:?}");
        }
        // Without `:=` there is nothing to infer, so nothing to say.
        let src = "extends Node\n\nfunc f(p) -> void:\n\tvar x = p + 5\n\tprint(x)\n";
        assert_eq!(errors(src, d), Vec::<String>::new(), "{d:?}");
    }
}

/// `reduce_unary_op` now consults the operator table (variant_op.cpp:456-485, :882-920), so an
/// operand the operator does not accept is an error in its own right.
#[test]
fn an_unaccepted_unary_operand_is_an_error() {
    for d in TAGS {
        assert_eq!(
            errors(&script("", "\tvar x := -\"hi\"\n\tprint(x)"), d),
            vec![
                r#"Invalid operand of type "String" for unary operator "unary-"."#.to_owned(),
                no_set_type("x"),
            ],
            "{d:?}"
        );
        assert_eq!(
            errors(
                &script("", "\tvar n: Node = self\n\tvar x := -n\n\tprint(x)"),
                d
            ),
            vec![
                r#"Invalid operand of type "Node" for unary operator "unary-"."#.to_owned(),
                no_set_type("x"),
            ],
            "{d:?}"
        );
        // A constant initializer draws the operand error alone — there is no inference to fail.
        assert_eq!(
            errors(&script("const N = -\"hi\"", "\tprint(N)"), d),
            vec![r#"Invalid operand of type "String" for unary operator "unary-"."#.to_owned()],
            "{d:?}"
        );
    }
}

/// A binary fold that fails is a *constant* upstream (analyzer.cpp:3144-3162), so the node types
/// from the reduced value: hard Nil for invalid operands, and a hard String carrying the
/// evaluator's own message for the two by-zero cases.
#[test]
fn a_failed_binary_fold_types_from_its_reduced_value() {
    for d in TAGS {
        assert_eq!(
            errors(&script("", "\tvar x := print + 1\n\tprint(x)"), d),
            vec![
                "Invalid operands to operator +, Callable and int.".to_owned(),
                r#"Cannot infer the type of "x" variable because the value is "null"."#.to_owned(),
            ],
            "{d:?}"
        );
        assert_eq!(
            errors(&script("", "\tvar x := 1 / 0\n\tprint(x)"), d),
            vec!["Division by zero error in operator /.".to_owned()],
            "{d:?}"
        );
        assert_eq!(
            errors(&script("", "\tvar x := 1 % 0\n\tprint(x)"), d),
            vec!["Modulo by zero error in operator %.".to_owned()],
            "{d:?}"
        );
    }
}

/// A soft *non-Variant* operand is genuine — it can only come from `var y = <expr>` — so the
/// widened gate fires on it without any invalid-operand complaint of its own.
#[test]
fn a_soft_non_variant_operand_still_blocks_inference() {
    for d in TAGS {
        for body in ["y << 2", "y % 5"] {
            assert_eq!(
                errors(
                    &script("var y = 3", &format!("\tvar x := {body}\n\tprint(x)")),
                    d
                ),
                vec![no_set_type("x")],
                "{body} at {d:?}"
            );
        }
        // The assignment path is unchanged — `-=` on a soft local says nothing new.
        assert_eq!(
            errors(
                &script("var y = 3", "\tvar s := y\n\ts -= 1\n\tprint(s)"),
                d
            ),
            vec![no_set_type("s")],
            "{d:?}"
        );
    }
}

/// An explicit `: Variant` is a HARD Variant, which Godot knows about and gdls never produces by
/// degrading — so it blocks inference exactly as upstream does.
#[test]
fn a_hard_variant_operand_blocks_inference() {
    for d in TAGS {
        for body in ["v + 5", "-v"] {
            assert_eq!(
                errors(
                    &script(
                        "",
                        &format!("\tvar v: Variant = 1\n\tvar x := {body}\n\tprint(x)")
                    ),
                    d
                ),
                vec![no_set_type("x")],
                "{body} at {d:?}"
            );
        }
    }
}

/// The silence contract: operations that DO type must stay clean, or the widened gate would land
/// on every well-typed expression in a project.
#[test]
fn well_typed_operations_stay_silent() {
    let cases = [
        "\tvar x := Vector2(1, 1) + Vector2(2, 2)\n\tprint(x)",
        "\tvar x := -Vector2(1, 1)\n\tprint(x)",
        "\tvar x := not \"hi\"\n\tprint(x)",
        "\tvar a: Array[int] = [1]\n\tvar x := a + a\n\tprint(x)",
        // No `:=`, so the mixed ternary has nothing to infer.
        "\tvar c := true\n\tvar x = 1 if c else 1.5\n\tprint(x)",
    ];
    for d in TAGS {
        for body in cases {
            assert_eq!(
                errors(&script("", body), d),
                Vec::<String>::new(),
                "{body} at {d:?}"
            );
        }
        // A real inherited property, typed through the native DB, is the shape a whole project
        // is made of — it must not pick up an operand error or an inference failure.
        assert_eq!(
            errors(
                "extends Node2D\n\nfunc f() -> void:\n\tvar x := position + Vector2(1, 1)\n\tprint(x)\n",
                d
            ),
            Vec::<String>::new(),
            "{d:?}"
        );
    }
}

/// The trust guard's other half: a Variant that gdls DEGRADED to (an unresolvable chain, a
/// cross-file member the shallow interface cannot type) must stay silent, and it must stay silent
/// through a `:=` declaration too.
///
/// That second part is the subtle one. Upstream hardens every `:=` local unconditionally
/// (analyzer.cpp:2150-2154), but it only gets there after erroring on any non-hard initializer, so
/// upstream a hard `AnnotatedInferred` local always has a hard initializer behind it. gdls holds
/// that clause back, so hardening here would launder a degrade into a hard Variant that the
/// operator reducers read as a genuine dynamic — and the error would land one use LATER, on a line
/// with nothing wrong with it. Swept against Pixelorama, that laundering was 2 false positives.
#[test]
fn a_degraded_variant_stays_silent_through_a_declaration() {
    for d in TAGS {
        // `{}` is an untyped Dictionary, so `d.k` is a degrade, not a real dynamic.
        assert_eq!(
            errors(
                &script(
                    "",
                    "\tvar dict = {}\n\tvar x := dict.k\n\tvar y := x + 1\n\tprint(y)"
                ),
                d
            ),
            Vec::<String>::new(),
            "{d:?}"
        );
    }
}
