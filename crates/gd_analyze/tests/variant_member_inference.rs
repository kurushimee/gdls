//! A member read off a dynamic base has no type, and `:=` cannot infer from it (#468).
//!
//! Godot's analyzer leaves the result of `v.y`, `v[0]`, and `v.m()` at its default `UNDETECTED`
//! source (`gdscript_analyzer.cpp:4854`, `:5123`, `:3267` with `:3557`), so `var x := v.y` draws
//! `Cannot infer the type of "x" variable because the value doesn't have a set type.` gdls stamped
//! a SET soft Variant at all three sites, and every downstream gate is written to stay quiet on
//! exactly that, so the whole family was silent.
//!
//! What keeps the fix from over-firing is `DataType::is_positively_dynamic()`: gdls has a second
//! source of soft Variants that Godot does not have — its own degrades, which every silent miss
//! falls back to — and an inference failure on a line with nothing wrong with it is the one
//! direction this port never takes. A degrade stays silent; only a base that is dynamic because
//! the CODE is dynamic carries the no-type-ness forward.
//!
//! The subtle half is that a declaration erases the difference. `resolve_assignable` rewrites the
//! initializer's source to `INFERRED` (`gdscript_analyzer.cpp:2163-2167`), which is exactly the
//! value a gdls degrade already has, so `var un = v` and `var lib = preload("res://gone.gd")` end
//! up byte-identical. `DataType::dynamic_origin` is the one bit that separates them, set at that
//! one site and read only by the predicate.
//!
//! Every row is pinned against `godot --headless --check-only` at 4.7.2.

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

/// `UNSAFE_METHOD_ACCESS` is off in Godot's default project settings, and slice 6 deliberately
/// decoupled the warning from the type stamp, so the two rows have to be assertable together.
fn warnings_on() -> WarningConfig {
    let mut cfg = WarningConfig::default();
    cfg.levels.insert(
        "unsafe_method_access".to_owned(),
        gd_project::WarnLevel::Warn,
    );
    cfg
}

fn diagnose(src: &str, dialect: Dialect) -> Vec<gd_analyze::Diagnostic> {
    let tree = gd_syntax::parse_with_options(
        src,
        &ParseOptions {
            dialect,
            script_path: "",
        },
    )
    .tree;
    let db = native_db();
    let policy = WarnPolicy::build(&warnings_on(), &StrictSettings::default(), dialect);
    analyze_with_options(
        &tree,
        Some(FileId::new(1)),
        "dyn.gd",
        &db,
        &NoCrossFile,
        &policy,
        AnalyzeOptions {
            dialect,
            ..Default::default()
        },
    )
    .diagnostics
}

/// Errors only. The `UNTYPED_DECLARATION` / `INFERRED_DECLARATION` noise every one of these
/// scripts also draws is the project's own warning config, not what this file is about.
fn errors(src: &str, dialect: Dialect) -> Vec<String> {
    diagnose(src, dialect)
        .iter()
        .filter(|d| d.severity() == gd_analyze::Severity::Error)
        .map(|d| d.message().to_string())
        .collect()
}

fn unsafe_method_access_rows(src: &str, dialect: Dialect) -> usize {
    diagnose(src, dialect)
        .iter()
        .filter(|d| d.warning_code() == Some(gd_analyze::warnings::WarningCode::UnsafeMethodAccess))
        .count()
}

const TAGS: [Dialect; 2] = [Dialect::Godot4_6, Dialect::Godot4_7];

fn script(body: &str) -> String {
    format!("extends Node\n\nfunc f(v, hv: Variant) -> void:\n{body}\n")
}

fn no_set_type(name: &str) -> String {
    format!(
        r#"Cannot infer the type of "{name}" variable because the value doesn't have a set type."#
    )
}

/// The issue's own repro, whole. Twelve inference failures, one member miss, and two
/// `UNSAFE_METHOD_ACCESS` rows — the exact set `godot --check-only` reports on this file.
#[test]
fn the_reported_repro_reports_exactly_godots_rows() {
    let src = "\
extends Node

func f(v, d: Dictionary, td: Dictionary[String, int], ti: Dictionary[int, int], hv: Variant, n: Node2D) -> void:
\tvar c1 := v.y
\tvar c2 := v.m()
\tvar c3 := v[0]
\tvar c4 := d.k
\tvar c5 := d[\"k\"]
\tvar c6 := d.size
\tvar c7 := hv.y
\tvar c8 := hv[0]
\tvar c9 := td.k
\tvar c10 := td[\"x\"]
\tvar c11 := hv.m()
\tvar dict = {}
\tvar c12 := dict.k
\tvar un = v
\tvar c13 := un.p
\tvar c14 := n.position
\tvar soft = n
\tvar c15 := soft.position
\tprint(c1, c2, c3, c4, c5, c6, c7, c8, c9, c10, c11, c12, c13, c14, c15)
\tprint(ti.k)
";
    for d in TAGS {
        let mut want: Vec<String> = ["c1", "c2", "c3", "c4", "c5", "c6", "c7", "c8"]
            .iter()
            .map(|n| no_set_type(n))
            .collect();
        want.extend(["c11", "c12", "c13", "c15"].iter().map(|n| no_set_type(n)));
        want.push(r#"Cannot find member "k" in base "Dictionary[int, int]"."#.to_owned());
        assert_eq!(errors(src, d), want, "{d:?}");
        assert_eq!(unsafe_method_access_rows(src, d), 2, "{d:?}");
    }
}

/// The base case: an untyped parameter is dynamic because the code is dynamic.
#[test]
fn a_member_read_off_an_untyped_parameter_cannot_infer() {
    for d in TAGS {
        assert_eq!(
            errors(&script("\tvar c := v.y\n\tprint(c)"), d),
            vec![no_set_type("c")],
            "{d:?}"
        );
    }
}

/// A written `: Variant` is hard, and hardness is its own proof that the dynamism is the user's.
#[test]
fn a_member_read_off_a_hard_variant_cannot_infer() {
    for d in TAGS {
        assert_eq!(
            errors(&script("\tvar c := hv.y\n\tprint(c)"), d),
            vec![no_set_type("c")],
            "{d:?}"
        );
    }
}

/// An index read takes the same route through `reduce_subscript`'s Variant-base arm.
#[test]
fn an_index_read_off_a_dynamic_base_cannot_infer() {
    for d in TAGS {
        assert_eq!(
            errors(
                &script("\tvar c := v[0]\n\tvar e := hv[1]\n\tprint(c, e)"),
                d
            ),
            vec![no_set_type("c"), no_set_type("e")],
            "{d:?}"
        );
    }
}

/// The call half. `UNSAFE_METHOD_ACCESS` already fired here; what was missing is that the call
/// itself has no type, so `:=` cannot read one off it.
#[test]
fn a_method_call_off_a_hard_variant_cannot_infer() {
    for d in TAGS {
        let src = script("\tvar c := hv.m()\n\tprint(c)");
        assert_eq!(errors(&src, d), vec![no_set_type("c")], "{d:?}");
        assert_eq!(unsafe_method_access_rows(&src, d), 1, "{d:?}");
    }
}

/// The bit's reason for existing: the declaration hands `un` the same `Variant`/`Inferred` value
/// a degrade has, and only `dynamic_origin` remembers which one it was.
#[test]
fn an_untyped_declaration_carries_its_initializers_dynamism() {
    for d in TAGS {
        assert_eq!(
            errors(&script("\tvar un = v\n\tvar c := un.p\n\tprint(c)"), d),
            vec![no_set_type("c")],
            "{d:?}"
        );
    }
}

/// The bit has to survive an arbitrary number of hops, which it does because each hop reads the
/// predicate on the previous one rather than a fixed origin.
#[test]
fn dynamism_survives_a_chain_of_untyped_declarations() {
    for d in TAGS {
        assert_eq!(
            errors(
                &script("\tvar a = v\n\tvar b = a\n\tvar c := b.p\n\tprint(c)"),
                d
            ),
            vec![no_set_type("c")],
            "{d:?}"
        );
    }
}

/// Declaration propagation and the call miss composed.
#[test]
fn a_method_call_off_an_untyped_declared_local_cannot_infer() {
    for d in TAGS {
        assert_eq!(
            errors(&script("\tvar un = v\n\tvar c := un.m()\n\tprint(c)"), d),
            vec![no_set_type("c")],
            "{d:?}"
        );
    }
}

/// The operator guards read the same predicate, so the bit reaches them too.
#[test]
fn a_binary_op_over_a_dynamic_declared_local_cannot_infer() {
    for d in TAGS {
        assert_eq!(
            errors(&script("\tvar un = v\n\tvar c := un + 1\n\tprint(c)"), d),
            vec![no_set_type("c")],
            "{d:?}"
        );
    }
}

/// `var x = null` drops to Variant at the declaration (`gdscript_analyzer.cpp:2158-2161`), and the
/// bit is read off the pre-drop hard `Nil` — which is trustworthy, so the drop stays dynamic.
#[test]
fn a_null_initialized_local_reads_dynamic() {
    for d in TAGS {
        assert_eq!(
            errors(&script("\tvar x = null\n\tvar c := x.p\n\tprint(c)"), d),
            vec![no_set_type("c")],
            "{d:?}"
        );
    }
}

/// An untyped function return is dynamic for the same reason an untyped parameter is.
#[test]
fn an_untyped_function_return_reads_dynamic() {
    for d in TAGS {
        let src = "extends Node\n\nfunc g():\n\treturn 1\n\nfunc f() -> void:\n\tvar r = g()\n\tvar c := r.p\n\tprint(c)\n";
        assert_eq!(errors(src, d), vec![no_set_type("c")], "{d:?}");
    }
}

/// The fence on the other side. A `Dictionary` with a string-ish key type narrows to its declared
/// value type (`gdscript_analyzer.cpp:4876-4886`), so these two infer and stay silent — the shape
/// the `Dictionary` short-circuit would otherwise have turned into a false inference failure.
#[test]
fn a_typed_string_key_dictionary_still_infers_its_value_type() {
    for d in TAGS {
        assert_eq!(
            errors(
                "extends Node\n\nfunc f(td: Dictionary[String, int]) -> void:\n\tvar a := td.k\n\tvar b := td[\"x\"]\n\tprint(a, b)\n",
                d
            ),
            Vec::<String>::new(),
            "{d:?}"
        );
    }
}

/// An unusable key type is a real miss, and it keeps its error rather than degrading.
#[test]
fn an_int_keyed_dictionary_reports_the_member_miss() {
    for d in TAGS {
        assert_eq!(
            errors(
                "extends Node\n\nfunc f(ti: Dictionary[int, int]) -> void:\n\tprint(ti.k)\n",
                d
            ),
            vec![r#"Cannot find member "k" in base "Dictionary[int, int]"."#.to_owned()],
            "{d:?}"
        );
    }
}

/// The polarity pin. `preload` of a script this harness cannot resolve is gdls not seeing
/// something, not the code being dynamic, so nothing is claimed about it — through a declaration,
/// through a member read, and through a call. Flip `dynamic_origin`'s default and this is the
/// test that fails.
#[test]
fn a_degraded_variant_stays_silent_through_a_declaration_a_read_and_a_call() {
    for d in TAGS {
        assert_eq!(
            errors(
                &script(
                    "\tvar lib = preload(\"res://lib.gd\")\n\tvar x := lib.k\n\tvar y := lib.m()\n\tprint(x, y)"
                ),
                d
            ),
            Vec::<String>::new(),
            "{d:?}"
        );
    }
}

/// A soft NON-Variant base is always genuine — no degrade produces one — so it carries forward
/// unchanged, and a member read off it fails to infer exactly as Godot's does.
#[test]
fn a_soft_native_base_still_cannot_infer_a_member() {
    for d in TAGS {
        assert_eq!(
            errors(
                "extends Node\n\nfunc f(n: Node2D) -> void:\n\tvar soft = n\n\tvar c := soft.position\n\tprint(c)\n",
                d
            ),
            vec![no_set_type("c")],
            "{d:?}"
        );
    }
}

/// And a HARD native base is not dynamic at all: the read has a real type and infers.
#[test]
fn a_hard_native_base_infers_its_member_type() {
    for d in TAGS {
        assert_eq!(
            errors(
                "extends Node\n\nfunc f(n: Node2D) -> void:\n\tvar c := n.position\n\tprint(c)\n",
                d
            ),
            Vec::<String>::new(),
            "{d:?}"
        );
    }
}

/// A `for` variable over a dynamic container is dynamic, and `resolve_for` stamps it the same
/// `UNDETECTED` Godot does (`gdscript_analyzer.cpp:2338`).
#[test]
fn a_for_variable_over_a_dynamic_container_cannot_infer_a_member() {
    for d in TAGS {
        assert_eq!(
            errors(
                &script(
                    "\tfor a in v:\n\t\tvar x := a.p\n\t\tprint(x)\n\tfor c in hv:\n\t\tvar z := c.p\n\t\tprint(z)"
                ),
                d
            ),
            vec![no_set_type("x"), no_set_type("z")],
            "{d:?}"
        );
    }
}

/// The same arm is where a degrade reaches the loop, and this is the shape the Pixelorama sweep
/// caught: an unresolvable `preload` reaches a `for` two declarations later, and stamping the loop
/// variable `UNDETECTED` would turn a whole loop body into false inference failures.
#[test]
fn a_for_variable_over_a_degraded_container_stays_silent() {
    for d in TAGS {
        assert_eq!(
            errors(
                &script(
                    "\tvar lib = preload(\"res://lib.gd\")\n\tvar items = lib.frames\n\tfor it in items:\n\t\tvar x := it.p\n\t\tprint(x)"
                ),
                d
            ),
            Vec::<String>::new(),
            "{d:?}"
        );
    }
}
