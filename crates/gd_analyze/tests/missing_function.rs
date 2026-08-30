//! #256 — the hard `Function "x()" not found in base …` errors, and the member-miss companions.
//!
//! Three shapes Godot treats as parse errors were entirely silent in gdls: a bare `miss()` from
//! inside a method, `self.miss()`, and a method miss on a hard builtin base. Each was silenced on
//! the grounds that a trimmed dump can't prove absence — but `ApiProvenance::Exact` is exactly the
//! claim that the dump IS the engine surface, and every neighbouring arm in the same function
//! already gates on it. Each expectation below was cross-checked against the real
//! `Godot_v4.6.3-stable` binary (`--headless --check-only --quit --script`).

use gd_syntax::Dialect;
use std::path::Path;

use gd_analyze::{analyze, NoCrossFile, Severity, StrictSettings, WarnPolicy};
use gd_types::{ApiProvenance, NativeDb};

/// The committed real-dump fixture: the canonical native chains, the complete Variant utility
/// table, and whole builtin surfaces for the types it carries.
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

fn errors_with(src: &str, native: &NativeDb) -> Vec<String> {
    let tree = gd_syntax::parse(src).tree;
    let result = analyze(&tree, None, "t.gd", native, &NoCrossFile, &policy());
    result
        .diagnostics
        .iter()
        .filter(|d| d.severity() == Severity::Error)
        .map(|d| d.message().to_owned())
        .collect()
}

fn errors(src: &str) -> Vec<String> {
    errors_with(src, &native_db())
}

/// Oracle: `SCRIPT ERROR: Parse Error: Function "miss_self()" not found in base self.`
#[test]
fn bare_call_to_a_nonexistent_function_errors() {
    let src = "extends Node\n\nfunc go() -> void:\n\tmiss_self()\n";
    assert_eq!(
        errors(src),
        vec![r#"Function "miss_self()" not found in base self."#.to_string()]
    );
}

/// Same error, same base name — Godot's `is_self && !is_super ? "self" : …` renders both shapes
/// as `self`, and the anchor is the callee in both.
#[test]
fn dotted_self_call_to_a_nonexistent_function_errors() {
    let src = "extends Node\n\nfunc go() -> void:\n\tself.miss_self()\n";
    assert_eq!(
        errors(src),
        vec![r#"Function "miss_self()" not found in base self."#.to_string()]
    );
}

/// FP guard, the whole reason the arm was silent: a Variant UTILITY reached by a bare call
/// (`typeof`, `prints`, `abs`) is resolved and early-returned long before the not-found branch.
#[test]
fn bare_utility_calls_stay_silent() {
    let src = "extends Node\n\nfunc go() -> void:\n\tprints(typeof(abs(-1)))\n\tprint_debug(1)\n";
    assert_eq!(errors(src), Vec::<String>::new());
}

/// FP guard: a method the file declares itself, and one inherited from the native base, both
/// resolve — the walk has to miss everywhere before the error fires.
#[test]
fn own_and_inherited_calls_stay_silent() {
    let src = "extends Node\n\nfunc real() -> void:\n\tpass\n\nfunc go() -> void:\n\treal()\n\tself.real()\n\tqueue_free()\n";
    assert_eq!(errors(src), Vec::<String>::new());
}

/// FP guard: absence proves nothing under a non-`Exact` dump — a custom engine build may define
/// the name the stock surface lacks. The identical source is silent.
#[test]
fn non_exact_provenance_stays_silent() {
    let mut db = native_db();
    db.set_provenance(ApiProvenance::Generic);
    let src = "extends Node\n\nfunc go() -> void:\n\tmiss_self()\n";
    assert_eq!(errors_with(src, &db), Vec::<String>::new());
}

/// FP guard: a class whose base does not resolve has no walkable ancestry, so gdls cannot claim
/// a name is absent from it — no error, under any provenance.
#[test]
fn unresolvable_base_stays_silent() {
    let src = "extends NotAThingAnywhere\n\nfunc go() -> void:\n\tmiss_self()\n";
    let errs = errors(src);
    assert!(
        !errs
            .iter()
            .any(|e| e.contains(r#"Function "miss_self()" not found"#)),
        "an unwalkable base must not produce a not-found claim; got {errs:?}"
    );
}

/// Oracle, both lines and in this order:
/// `Cannot find member "bogus" in base "Vector2".` then
/// `Function "bogus()" not found in base Vector2.`
#[test]
fn method_miss_on_a_hard_builtin_base_errors_twice() {
    let src = "extends Node\n\nfunc go() -> void:\n\tvar v := Vector2(1, 2)\n\tv.bogus()\n";
    assert_eq!(
        errors(src),
        vec![
            r#"Cannot find member "bogus" in base "Vector2"."#.to_string(),
            r#"Function "bogus()" not found in base Vector2."#.to_string(),
        ]
    );
}

/// Oracle: a PROPERTY miss on the same base is the first line alone.
#[test]
fn property_miss_on_a_hard_builtin_base_errors_once() {
    let src =
        "extends Node\n\nfunc go() -> void:\n\tvar v := Vector2(1, 2)\n\tprint(v.bogus_prop)\n";
    assert_eq!(
        errors(src),
        vec![r#"Cannot find member "bogus_prop" in base "Vector2"."#.to_string()]
    );
}

/// FP guard: real builtin members and methods resolve out of the dump's `builtin_classes` tables —
/// the same tables `signatureHelp` reads.
#[test]
fn real_builtin_members_and_methods_stay_silent() {
    let src =
        "extends Node\n\nfunc go() -> void:\n\tvar v := Vector2(1, 2)\n\tprint(v.x)\n\tprint(v.length())\n\tprint(v.lerp(v, 0.5))\n";
    assert_eq!(errors(src), Vec::<String>::new());
}

/// FP guard, Godot's own exclusion: a `Dictionary`'s keys ARE its members, so upstream answers
/// any name with a Variant instead of erroring (analyzer.cpp:4124-4128). The exclusion is the
/// member arm's alone — see the #416 rows below for the call arm, which has no such thing.
#[test]
fn dictionary_member_access_stays_silent() {
    let src = "extends Node\n\nfunc go() -> void:\n\tvar d := {}\n\tprint(d.anything)\n";
    assert_eq!(errors(src), Vec::<String>::new());
}

/// FP guard: an UNTYPED (soft) base is the gradual-typing path — Godot's gate is
/// `base.is_hard_type()`, so a dynamically-typed value keeps its silence.
#[test]
fn soft_typed_base_stays_silent() {
    let src = "extends Node\n\nfunc go(anything) -> void:\n\tanything.bogus()\n\tprint(anything.bogus_prop)\n";
    assert_eq!(errors(src), Vec::<String>::new());
}

// ===================================================================================================
// #416 — a Dictionary base, where the member arm and the call arm disagree.
// ===================================================================================================

/// Oracle: `Function "nope_m()" not found in base Dictionary.`, once per call site, and nothing at
/// all for the property.
///
/// The member arm exempts `Dictionary` because upstream does (analyzer.cpp:4126-4128 hands back a
/// bare Variant for any name — a dictionary's keys are its members). The exemption had been carried
/// into the call arm, where upstream's gate is `is_self || (hard && BUILTIN)` flat with nothing of
/// the kind, so every dictionary method typo was silent.
#[test]
fn method_miss_on_a_dictionary_base_errors_and_the_property_miss_does_not() {
    let src = "\
extends Node

func go() -> void:
\tvar inferred := {\"a\": 1}
\tvar annotated: Dictionary = {}
\tinferred.nope_m()
\tannotated.nope_m()
\tprint(inferred.nope_p)
";
    assert_eq!(
        errors(src),
        vec![
            r#"Function "nope_m()" not found in base Dictionary."#.to_string(),
            r#"Function "nope_m()" not found in base Dictionary."#.to_string(),
        ]
    );
}

/// FP guard: the real `Dictionary` methods resolve out of the dump, and an UNTYPED dictionary base
/// is not a hard type, so it stays outside the gate exactly as it does for every other builtin.
#[test]
fn real_dictionary_methods_and_an_untyped_base_stay_silent() {
    let src = "\
extends Node

func go() -> void:
\tvar d := {\"a\": 1}
\tprint(d.size())
\tprint(d.has(\"a\"))
\td.clear()
\tvar loose = {}
\tloose.nope_m()
";
    assert_eq!(errors(src), Vec::<String>::new());
}
