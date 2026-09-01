//! #546 — a call to a name a BUILTIN metatype does not have.
//!
//! Godot's call gate (`gdscript_analyzer.cpp:3757`) is `is_self || (base_type.is_hard_type() &&
//! base_type.kind == BUILTIN)` — flat, with no metatype exclusion — so `Vector2.bogus_static()`
//! reports exactly like `Vector2(1, 2).bogus()` does. gdls excluded metatypes from that gate and
//! answered nothing at all.
//!
//! Two messages come out per miss, in this order: `Cannot find member "X" in base "Y".` from the
//! attribute walk (`:4103-4113`) and `Function "X()" not found in base Y.` from the call miss
//! (`:3757`, base name unquoted).
//!
//! `Vector2.new()` is the same miss. Godot's `get_function_signature` puts its BUILTIN arm
//! (`:5904-5937`) above the `p_is_constructor` → `_init` rewrite (`:5960`), so `new` is looked up
//! as an ordinary method and misses — builtins have no `new`. gdls took the constructor arm and
//! fabricated an instance for a call the engine refuses.
//!
//! Every expected row is verbatim `Godot_v4.7.2-stable --headless --check-only` output.

use std::path::Path;

use gd_analyze::{analyze, NoCrossFile, Severity, StrictSettings, WarnPolicy};
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
        &gd_project::WarningConfig::default(),
        &StrictSettings {
            enable_warnings: vec!["UNSAFE_METHOD_ACCESS".to_owned()],
            ..Default::default()
        },
        Dialect::DEFAULT,
    )
}

/// Errors then `UNSAFE_METHOD_ACCESS` warnings, in emission order.
fn diagnose_with(db: &NativeDb, stmt: &str) -> (Vec<String>, Vec<String>) {
    let src = format!("extends Node\n\nfunc f(v: Vector2) -> void:\n\tprint(v)\n\t{stmt}\n");
    let tree = parse(&src).tree;
    let result = analyze(&tree, None, "res://main.gd", db, &NoCrossFile, &policy());
    let errors = result
        .diagnostics
        .iter()
        .filter(|d| d.severity() == Severity::Error)
        .map(|d| d.message().to_owned())
        .collect();
    let warnings = result
        .diagnostics
        .iter()
        .filter(|d| d.code() == "UNSAFE_METHOD_ACCESS")
        .map(|d| d.message().to_owned())
        .collect();
    (errors, warnings)
}

fn diagnose(stmt: &str) -> (Vec<String>, Vec<String>) {
    diagnose_with(&native_db(), stmt)
}

fn miss(name: &str, base: &str) -> Vec<String> {
    vec![
        format!(r#"Cannot find member "{name}" in base "{base}"."#),
        format!(r#"Function "{name}()" not found in base {base}."#),
    ]
}

/// The headline row.
#[test]
fn a_builtin_metatype_static_miss_reports_both_messages() {
    assert_eq!(
        diagnose("Vector2.bogus_static()").0,
        miss("bogus_static", "Vector2")
    );
}

/// `Vector2.new()` is not a constructor — builtins have no `new`, and Godot reports it as an
/// ordinary miss with the raw name.
#[test]
fn new_on_a_builtin_is_an_ordinary_miss() {
    assert_eq!(diagnose("Vector2.new()").0, miss("new", "Vector2"));
}

/// A metatype `Dictionary` is not exempt, even though a dictionary INSTANCE is: the instance
/// exemption is upstream's own (any key is a member) and belongs to the attribute walk, not here.
#[test]
fn a_dictionary_metatype_is_not_exempt() {
    assert_eq!(
        diagnose("Dictionary.nope_static()").0,
        miss("nope_static", "Dictionary")
    );
}

/// One more type, so the rows are not a Vector2 special case.
#[test]
fn the_same_holds_for_another_builtin() {
    assert_eq!(
        diagnose("Color.nope_static()").0,
        miss("nope_static", "Color")
    );
}

/// A real static resolves and says nothing.
#[test]
fn a_real_static_stays_silent() {
    assert_eq!(
        diagnose("print(Vector2.from_angle(1.0))").0,
        Vec::<String>::new()
    );
}

/// A constant called as a function keeps its own message — the name IS on the metatype, so this is
/// not a miss.
#[test]
fn a_constant_called_as_a_function_keeps_its_message() {
    assert_eq!(
        diagnose("print(Vector2.ZERO())").0,
        vec![r#"Name "ZERO" called as a function but is a "Vector2"."#.to_owned()]
    );
}

/// A plain READ of a missing static reports the member miss ONCE. Godot prints it twice — the
/// attribute walk and its subscript caller each push the same string — and one row is the honest
/// rendering of one mistake.
#[test]
fn a_plain_read_reports_the_member_miss_once() {
    assert_eq!(
        diagnose("print(Vector2.nope_static)").0,
        vec![r#"Cannot find member "nope_static" in base "Vector2"."#.to_owned()]
    );
}

/// A SOFT builtin base is not a hard type, so the gate does not fire: Godot says nothing on the
/// read and only warns on the call.
#[test]
fn a_soft_builtin_base_only_warns() {
    let src = "extends Node\n\nfunc f() -> void:\n\tvar u = Vector2.ZERO\n\tu.nope_call()\n";
    let tree = parse(src).tree;
    let db = native_db();
    let result = analyze(&tree, None, "res://main.gd", &db, &NoCrossFile, &policy());
    let errors: Vec<String> = result
        .diagnostics
        .iter()
        .filter(|d| d.severity() == Severity::Error)
        .map(|d| d.message().to_owned())
        .collect();
    assert_eq!(errors, Vec::<String>::new());
}

/// A builtin INSTANCE miss is unchanged — it already reported both messages, and the metatype
/// widening must not double it.
#[test]
fn a_builtin_instance_miss_is_unchanged() {
    assert_eq!(diagnose("v.bogus()").0, miss("bogus", "Vector2"));
}

/// Provenance still gates every row: a dump that is not the engine surface cannot disprove a
/// static, so all of it goes silent.
#[test]
fn every_row_is_silent_without_an_exact_dump() {
    let mut db = native_db();
    db.set_provenance(gd_types::ApiProvenance::Generic);
    assert_eq!(
        diagnose_with(&db, "Vector2.bogus_static()").0,
        Vec::<String>::new()
    );
    assert_eq!(diagnose_with(&db, "Vector2.new()").0, Vec::<String>::new());
}
