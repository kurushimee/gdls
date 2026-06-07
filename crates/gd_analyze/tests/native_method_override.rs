//! Regression net for `NATIVE_METHOD_OVERRIDE` (error-by-default) emission on an override *chain*.
//!
//! Godot's `get_function_signature` returns at its `found_function != nullptr` branch
//! (`gdscript_analyzer.cpp:5853-5873`) — BEFORE it ever consults `ClassDB` and sets `native_base`
//! (`:5906-5916`) — whenever a script ancestor already declares the function. So a derived class
//! that overrides a native method *already overridden by an in-file parent* does NOT re-emit the
//! warning: only the class whose resolution actually reaches the native method warns.
//!
//! gdls previously walked the in-file base chain straight to the native root
//! (`resolver::enclosing_native_base`) without that short-circuit, double-firing the warning down
//! the whole chain — a false error-by-default on a valid override chain. These tests pin the
//! corrected behaviour; the single-file corpus fixture `overriding_native_method` has no
//! script-ancestor companion, so this is the only coverage for the chain case.

use std::path::Path;

use gd_analyze::{analyze, NoCrossFile, StrictSettings, WarnPolicy, WarningCode};
use gd_project::{FileId, WarningConfig};
use gd_syntax::parse;
use gd_types::NativeDb;

/// The committed native-DB fixture (same one the conformance harness loads). `Object.get` is a
/// bound (non-virtual) method there, so overriding it is the canonical NMO trigger.
fn native_db() -> NativeDb {
    let path =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../gd_types/tests/fixtures/trimmed_api.json");
    NativeDb::load(path.to_str().expect("utf-8 path"))
        .unwrap_or_else(|e| panic!("load native DB fixture at {}: {e}", path.display()))
}

fn policy() -> WarnPolicy {
    WarnPolicy::build(&WarningConfig::default(), &StrictSettings::default())
}

/// Count how many `NATIVE_METHOD_OVERRIDE` diagnostics `src` produces. The code is error-by-default,
/// but [`Diagnostic::warning_code`] still reports it for the promoted-error case, so the count is
/// severity-independent.
fn nmo_count(src: &str) -> usize {
    let tree = parse(src).tree;
    let db = native_db();
    let result = analyze(
        &tree,
        Some(FileId::new(1)),
        "nmo.gd",
        &db,
        &NoCrossFile,
        &policy(),
    );
    result
        .diagnostics
        .iter()
        .filter(|d| d.warning_code() == Some(WarningCode::NativeMethodOverride))
        .count()
}

#[test]
fn native_override_fires_once_on_a_direct_override() {
    // Baseline guard against the chain fix over-suppressing: a single class overriding `Object.get`
    // must still warn exactly once (mirrors the `overriding_native_method` golden fixture).
    let src = "\
extends RefCounted

func get(_property: StringName) -> Variant:
\treturn null
";
    assert_eq!(
        nmo_count(src),
        1,
        "a direct native override must warn exactly once"
    );
}

#[test]
fn native_override_is_not_re_emitted_down_a_script_chain() {
    // `A` overrides the native `Object.get`; `B extends A` overrides it again. Godot warns on
    // `A.get` ONLY — `B.get` resolves to its script parent `A` first, so `native_base` stays empty
    // and the warning is suppressed (`gdscript_analyzer.cpp:5853-5873`). gdls must match.
    let src = "\
extends RefCounted

class A extends RefCounted:
\tfunc get(_property: StringName) -> Variant:
\t\treturn null

class B extends A:
\tfunc get(_property: StringName) -> Variant:
\t\treturn null
";
    assert_eq!(
        nmo_count(src),
        1,
        "NMO must fire on A.get only, not re-fire on B.get down the override chain",
    );
}
