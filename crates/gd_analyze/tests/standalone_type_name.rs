//! `Builtin type cannot be used as a name on its own.` and the bare-`Variant` fall-through (#327).
//!
//! Both arms of `reduce_identifier` used to expose their metatype unconditionally where Godot
//! gates on `can_be_builtin` — the flag #325 threaded onto the context. Upstream passes it `true`
//! from exactly one site, the base of a subscript (analyzer.cpp:4799), so `int.MAX` and
//! `Variant.Type.TYPE_NIL` resolve while a bare `int` or `Variant` does not.
//!
//! Two call shapes must stay clear of the gate, and they are the reason the whole thing is
//! delicate: a builtin constructor (`Vector2()`) and a builtin static call
//! (`Color.html_is_valid()`) name a builtin type in callee position, and `reduce_call` answers
//! both from the NAME (analyzer.cpp:3279-3283 and :3597-3603) without ever reducing the
//! identifier. Reducing it anyway would put this error on every builtin construction in the
//! language — which is exactly what the first cut of #327 did, dropping analyze conformance to
//! 0.9133.
//!
//! Every expectation is pinned against `godot --headless --check-only`, identical at 4.6.3 and
//! 4.7.2.

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
    .filter(|d| d.warning_code().is_none())
    .map(|d| d.message().to_string())
    .collect()
}

const TAGS: [Dialect; 2] = [Dialect::Godot4_6, Dialect::Godot4_7];

fn script(body: &str) -> String {
    format!("extends Node\n\nfunc f() -> void:\n{body}")
}

const STANDALONE: &str = "Builtin type cannot be used as a name on its own.";

/// Godot pushes the standalone error and then deliberately does NOT return, so the name also
/// reaches the not-declared tail. Two errors per bare builtin name, in that order.
#[test]
fn a_bare_builtin_type_name_draws_both_errors() {
    for d in TAGS {
        assert_eq!(
            errors(&script("\tvar i = int\n\tprint(i)\n"), d),
            vec![
                STANDALONE.to_owned(),
                r#"Identifier "int" not declared in the current scope."#.to_owned(),
            ],
            "assignment at {d:?}"
        );
        assert_eq!(
            errors(&script("\tprint(Vector2)\n"), d),
            vec![
                STANDALONE.to_owned(),
                r#"Identifier "Vector2" not declared in the current scope."#.to_owned(),
            ],
            "argument position at {d:?}"
        );
    }
}

/// `Variant` has no standalone message of its own — its arm is simply skipped, so it lands on the
/// not-declared tail alone.
#[test]
fn a_bare_variant_is_just_undeclared() {
    for d in TAGS {
        assert_eq!(
            errors(&script("\tprint(Variant)\n"), d),
            vec![r#"Identifier "Variant" not declared in the current scope."#.to_owned()],
            "at {d:?}"
        );
    }
}

/// The subscript base is the one position where both names are legal. `Variant` is allowed there
/// precisely so nested enums work (analyzer.cpp:4667, "Allow `Variant` here since it might be used
/// for nested enums").
#[test]
fn a_type_name_is_legal_as_a_subscript_base() {
    for d in TAGS {
        assert_eq!(
            errors(&script("\tprint(Variant.Type.TYPE_NIL)\n"), d),
            Vec::<String>::new(),
            "nested enum at {d:?}"
        );
        assert_eq!(
            errors(&script("\tprint(Vector3.AXIS_X)\n"), d),
            Vec::<String>::new(),
            "builtin constant at {d:?}"
        );
    }
}

/// A member miss on a builtin base is a miss, never the standalone error — the base itself was
/// spelled in a legal position. Whether the miss is *reported* depends on the dump carrying the
/// builtin (the trimmed fixture has no `int` entry, so the negative is unprovable and stays
/// silent); what this pins is that the standalone gate keeps its hands off.
#[test]
fn a_missing_member_on_a_builtin_base_is_never_the_standalone_error() {
    for d in TAGS {
        let msgs = errors(&script("\tprint(int.MAX)\n"), d);
        assert!(
            !msgs.iter().any(|m| m == STANDALONE),
            "a legal subscript base drew the standalone error at {d:?}: {msgs:?}"
        );
        assert!(
            msgs.iter()
                .all(|m| m == r#"Cannot find member "MAX" in base "int"."#),
            "unexpected diagnostics at {d:?}: {msgs:?}"
        );
    }
}

/// A builtin CONSTRUCTOR names a builtin type in callee position and must stay silent. Godot
/// answers it from the name before any identifier reduction (analyzer.cpp:3279-3283).
#[test]
fn a_builtin_constructor_call_is_not_a_standalone_name() {
    for d in TAGS {
        for body in [
            "\tvar v = Vector2()\n\tprint(v)\n",
            "\tvar v = Vector2(1, 2)\n\tprint(v)\n",
            "\tvar a = Array()\n\tprint(a)\n",
            "\tvar s = String()\n\tprint(s)\n",
        ] {
            assert_eq!(
                errors(&script(body), d),
                Vec::<String>::new(),
                "{body:?} at {d:?}"
            );
        }
    }
}

/// A builtin STATIC call does the same through a subscript callee, whose base `reduce_call`
/// builds from the name without reducing it (analyzer.cpp:3597-3603).
#[test]
fn a_builtin_static_call_is_not_a_standalone_name() {
    for d in TAGS {
        assert_eq!(
            errors(&script("\tprint(Color.html_is_valid(\"00ffff\"))\n"), d),
            Vec::<String>::new(),
            "at {d:?}"
        );
    }
}

/// The builtin constructor beats an in-file `func` of the same name — `reduce_call` tests the
/// name against the builtin table before any member lookup, so the declaration below is dead code
/// as far as the call site is concerned. Pinned against the oracle, which reports exactly this
/// pair for `var v: int = Vector2()` under a `func Vector2() -> int`.
#[test]
fn a_same_named_func_does_not_shadow_the_builtin_constructor() {
    for d in TAGS {
        assert_eq!(
            errors(
                "extends Node\n\nfunc Vector2() -> int:\n\treturn 5\n\nfunc f() -> void:\n\tvar v: int = Vector2()\n\tprint(v)\n",
                d
            ),
            vec![
                r#"Cannot assign a value of type "Vector2" as "int"."#.to_owned(),
                r#"Cannot assign a value of type Vector2 to variable "v" with specified type int."#
                    .to_owned(),
            ],
            "at {d:?}"
        );
    }
}
