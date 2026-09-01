//! #550 — a `:=` whose initializer is a call that missed fails to infer.
//!
//! Godot's `call_type` is default-constructed (`gdscript_analyzer.cpp:3283`) and never assigned
//! anywhere in the miss branch (`:3722-3776`), so every reported miss stamps UNRESOLVED /
//! UNDETECTED. The `:=` check reads exactly that (`:2141`) and adds `Cannot infer the type of "x"
//! variable because the value doesn't have a set type.` under the miss row. gdls stamped a set
//! `Variant`, which satisfies the check, so the second half of the engine's answer never fired.
//!
//! The no-type dummy only rides behind an error the branch actually pushed. Every quiet degrade
//! beside it — a trimmed dump entry, an unwalkable chain, a partial cross-file interface — claims
//! nothing and keeps its soft `Variant`, which is the under-report it has always been.
//!
//! Every expected row is verbatim `Godot_v4.7.2-stable --headless --check-only` output.

use std::path::Path;

use gd_analyze::{analyze, NoCrossFile, Severity, StrictSettings, WarnPolicy};
use gd_syntax::Dialect;
use gd_types::{ApiProvenance, NativeDb};

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

fn cannot_infer(name: &str) -> String {
    format!(
        r#"Cannot infer the type of "{name}" variable because the value doesn't have a set type."#
    )
}

/// The repro's first half: a bare self-call that missed.
#[test]
fn a_self_call_miss_fails_inference() {
    let src = "extends Node\n\nfunc go() -> void:\n\tvar a := miss_me()\n\tprint(a)\n";
    assert_eq!(
        errors(src),
        vec![
            r#"Function "miss_me()" not found in base self."#.to_owned(),
            cannot_infer("a"),
        ]
    );
}

/// The repro's second half: a method miss on a builtin instance, whose miss carries two rows of
/// its own. The inference row comes last, as upstream orders it.
#[test]
fn a_builtin_instance_miss_fails_inference() {
    let src = "extends Node\n\nfunc go(v: Vector2) -> void:\n\tvar b := v.bogus()\n\tprint(b)\n";
    assert_eq!(
        errors(src),
        vec![
            r#"Cannot find member "bogus" in base "Vector2"."#.to_owned(),
            r#"Function "bogus()" not found in base Vector2."#.to_owned(),
            cannot_infer("b"),
        ]
    );
}

/// The metatype miss (#546) fails inference too.
#[test]
fn a_builtin_metatype_miss_fails_inference() {
    let src = "extends Node\n\nfunc go() -> void:\n\tvar c := Vector2.bogus_static()\n\tprint(c)\n";
    assert_eq!(
        errors(src),
        vec![
            r#"Cannot find member "bogus_static" in base "Vector2"."#.to_owned(),
            r#"Function "bogus_static()" not found in base Vector2."#.to_owned(),
            cannot_infer("c"),
        ]
    );
}

/// The value-callable pair leaves the call unset upstream as well, so `var g := name()` on a Node
/// base gets the inference row under its own message.
#[test]
fn a_value_called_as_a_function_fails_inference() {
    let src = "extends Node\n\nfunc go() -> void:\n\tvar g := name()\n\tprint(g)\n";
    assert_eq!(
        errors(src),
        vec![
            r#"Name "name" called as a function but is a "StringName"."#.to_owned(),
            cannot_infer("g"),
        ]
    );
}

/// An ANNOTATED declaration has nothing to infer, so the miss row stands alone. The dummy is
/// Variant-kinded, which takes the conversion-assign path rather than the hard mismatch error —
/// upstream's UNRESOLVED is `is_variant()` too.
#[test]
fn an_annotated_declaration_gets_no_inference_row() {
    let src = "extends Node\n\nfunc go() -> void:\n\tvar d: int = miss_me()\n\tprint(d)\n";
    assert_eq!(
        errors(src),
        vec![r#"Function "miss_me()" not found in base self."#.to_owned()]
    );
}

/// Both halves are silent under a dump that is not the engine surface: the miss branch pushes
/// nothing there, so nothing rides behind it either.
#[test]
fn neither_row_fires_without_an_exact_dump() {
    let mut db = native_db();
    db.set_provenance(ApiProvenance::Generic);
    let src = "extends Node\n\nfunc go() -> void:\n\tvar a := miss_me()\n\tprint(a)\n";
    assert_eq!(errors_with(src, &db), Vec::<String>::new());
}

/// A class whose base does not resolve has no walkable ancestry, so the miss is a gdls gap, not a
/// user error — the soft `Variant` and its silence both survive.
#[test]
fn an_unwalkable_base_keeps_its_silence() {
    let src = "extends NotAThingAnywhere\n\nfunc go() -> void:\n\tvar a := miss_me()\n\tprint(a)\n";
    assert!(
        !errors(src).iter().any(|e| e.contains("Cannot infer")),
        "a degrade must not gain the inference row: {:?}",
        errors(src)
    );
}

/// A call that RESOLVES still infers precisely — the dummy must not leak onto the healthy path.
#[test]
fn a_resolved_call_still_infers() {
    let src = "extends Node\n\nfunc real() -> int:\n\treturn 1\n\nfunc go() -> void:\n\tvar a := real()\n\tvar b := Vector2(1, 2)\n\tprint(a, b)\n";
    assert_eq!(errors(src), Vec::<String>::new());
}
