//! `Cannot infer the type of "X" … because the value doesn't have a set type.` after a failed
//! lookup, and the typed-container loop variable that has to resolve for it not to over-fire
//! (#323).
//!
//! Godot's not-found identifier path sets a `dummy` of `VARIANT` with the DEFAULT `UNDETECTED`
//! source (analyzer.cpp:4691-4693), and `reduce_call` never assigns `call_type` when
//! `get_function_signature` fails (:3745) — so `var a := NotAClass.new()` reads as "no set type"
//! and draws the inference error alongside the undeclared one. gdls's `reduce_expression`
//! tail-guard promotes an unset type to a SOFT `Variant`, which made that whole half of the check
//! unreachable; the dummy is now carried explicitly through both sites.
//!
//! The literal upstream gate (`!is_hard_type()`) is deliberately NOT what is ported here: gdls's
//! reducer under-hard-types enough valid expressions that it drops analyze conformance to 0.9745.
//! Every case below is pinned against `godot --headless --check-only` on the matching release.

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
        "inference.gd",
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

const UNDECLARED: &str = r#"Identifier "NotAClass" not declared in the current scope."#;
const CANNOT_INFER_A: &str =
    r#"Cannot infer the type of "a" variable because the value doesn't have a set type."#;

/// The pair Godot reports, in Godot's order.
#[test]
fn constructing_an_undeclared_class_reports_the_inference_failure_too() {
    for d in TAGS {
        assert_eq!(
            errors(
                "extends Node\n\nfunc f() -> void:\n\tvar a := NotAClass.new()\n\tprint(a)\n",
                d
            ),
            vec![UNDECLARED.to_owned(), CANNOT_INFER_A.to_owned()],
            "at {d:?}"
        );
    }
}

/// Any call through the undeclared name, not just `new()`.
#[test]
fn calling_a_method_on_an_undeclared_name_reports_the_inference_failure() {
    for d in TAGS {
        assert_eq!(
            errors(
                "extends Node\n\nfunc f() -> void:\n\tvar a := NotAClass.make()\n\tprint(a)\n",
                d
            ),
            vec![UNDECLARED.to_owned(), CANNOT_INFER_A.to_owned()],
            "at {d:?}"
        );
    }
}

/// An explicit annotation is not an inference, so only the undeclared error fires.
#[test]
fn an_annotated_declaration_draws_only_the_undeclared_error() {
    for d in TAGS {
        assert_eq!(
            errors(
                "extends Node\n\nfunc f() -> void:\n\tvar a: Node = NotAClass.new()\n\tprint(a)\n",
                d
            ),
            vec![UNDECLARED.to_owned()],
            "at {d:?}"
        );
    }
}

/// The no-type dummy is stamped ONLY where the undeclared error actually fires. A name gdls
/// deliberately stays silent about — a plausible inherited native member — keeps the permissive
/// soft `Variant`, so it must not start reporting an inference failure gdls has no grounds for.
#[test]
fn a_deliberately_silent_lookup_miss_does_not_report_an_inference_failure() {
    for d in TAGS {
        assert_eq!(
            errors(
                "extends Node2D\n\nfunc f() -> void:\n\tvar a := position\n\tprint(a)\n",
                d
            ),
            Vec::<String>::new(),
            "at {d:?}"
        );
    }
}

/// A resolved call chain infers fine — the dummy must not leak into ordinary code.
#[test]
fn a_resolved_call_chain_still_infers() {
    for d in TAGS {
        assert_eq!(
            errors(
                "extends Node\n\nfunc f() -> void:\n\tvar a := get_class()\n\tprint(a)\n",
                d
            ),
            Vec::<String>::new(),
            "at {d:?}"
        );
    }
}

// ===================================================================================================
// The typed-container loop variable (analyzer.cpp:2307-2309).
// ===================================================================================================

/// Iterating a typed `Dictionary` yields its KEY type — `has_container_element_type(0)` — so the
/// loop variable is a real `Node2D` and its members resolve. Before this the variable had no type
/// at all, which was invisible until the inference check above started reading no-type-ness.
#[test]
fn a_typed_dictionaries_loop_variable_takes_the_key_type() {
    for d in TAGS {
        assert_eq!(
            errors(
                "extends Node\n\nvar d: Dictionary[Node2D, int] = {}\n\nfunc f() -> void:\n\tfor n in d:\n\t\tvar c := n.get_class()\n\t\tprint(c)\n",
                d
            ),
            Vec::<String>::new(),
            "at {d:?}"
        );
    }
}

/// The `Array[T]` half keeps working, and the element type is a real type rather than a Variant.
#[test]
fn a_typed_arrays_loop_variable_takes_the_element_type() {
    for d in TAGS {
        assert_eq!(
            errors(
                "extends Node\n\nvar a: Array[Node2D] = []\n\nfunc f() -> void:\n\tfor n in a:\n\t\tvar c := n.get_class()\n\t\tprint(c)\n",
                d
            ),
            Vec::<String>::new(),
            "at {d:?}"
        );
    }
}

/// An UNtyped dictionary still yields `Variant`, and a chain off it is exactly where Godot does
/// report the inference failure — confirmed against `godot --check-only` on the same source.
#[test]
fn an_untyped_dictionarys_loop_variable_still_yields_variant() {
    for d in TAGS {
        assert_eq!(
            errors(
                "extends Node\n\nvar d: Dictionary = {}\n\nfunc f() -> void:\n\tfor n in d:\n\t\tvar c := n.get_viewport()\n\t\tprint(c)\n",
                d
            ),
            vec![
                r#"Cannot infer the type of "c" variable because the value doesn't have a set type."#
                    .to_owned()
            ],
            "at {d:?}"
        );
    }
}
