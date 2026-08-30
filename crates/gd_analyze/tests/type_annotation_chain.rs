//! `resolve_datatype`'s rejection arms (analyzer.cpp:718-940) — the whole message set a bad type
//! annotation can draw (#338).
//!
//! Before this, a name that resolved to a *non-type* member (a variable, a function, a signal, a
//! value constant) fell out of the in-scope lookup as if it had not been found at all, so the
//! annotation silently degraded to Variant and the declaration typed as `null`. The nested arms
//! (`Variant.X`, `int.X`, `Node.X`) degraded silently too. Godot rejects every one of them, and
//! names *what kind* of thing it found, which is the half that makes the message actionable.
//!
//! Every expectation below is pinned against `godot --headless --check-only` at 4.7.2.

use std::path::Path;

use gd_analyze::{analyze_with_options, AnalyzeOptions, NoCrossFile, StrictSettings, WarnPolicy};
use gd_project::{FileId, WarningConfig};
use gd_syntax::{Dialect, ParseOptions};
use gd_types::{ApiProvenance, NativeDb};

fn native_db(provenance: ApiProvenance) -> NativeDb {
    let path =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../gd_types/tests/fixtures/trimmed_api.json");
    let mut db = NativeDb::load(path.to_str().expect("utf-8 path"))
        .unwrap_or_else(|e| panic!("load native DB fixture at {}: {e}", path.display()));
    db.set_provenance(provenance);
    db
}

fn errors_with(src: &str, dialect: Dialect, provenance: ApiProvenance) -> Vec<String> {
    let tree = gd_syntax::parse_with_options(
        src,
        &ParseOptions {
            dialect,
            script_path: "",
        },
    )
    .tree;
    let db = native_db(provenance);
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

fn errors(src: &str, dialect: Dialect) -> Vec<String> {
    errors_with(src, dialect, ApiProvenance::Exact)
}

const TAGS: [Dialect; 2] = [Dialect::Godot4_6, Dialect::Godot4_7];

/// `decl` goes at class scope, `use` inside a function body.
fn script(decl: &str, use_: &str) -> String {
    format!("extends Node\n\n{decl}\n\nfunc f() -> void:\n\t{use_}\n\tprint(x)\n")
}

/// analyzer.cpp:895 — the in-scope lookup found the name, and it is a member, but not a type. The
/// kind word comes from `Member::get_type_name()` (gdscript_parser.h:618-640).
#[test]
fn a_non_type_class_member_names_its_own_kind() {
    let cases = [
        ("var member_var = 1", "member_var", "variable"),
        ("func member_fn() -> void:\n\tpass", "member_fn", "function"),
        ("signal member_sig", "member_sig", "signal"),
        ("const C = 5", "C", "constant"),
        ("const S := \"s\"", "S", "constant"),
    ];
    for d in TAGS {
        for (decl, name, kind) in cases {
            assert_eq!(
                errors(&script(decl, &format!("var x: {name} = null")), d),
                vec![format!(
                    r#""{name}" is a {kind} but does not contain a type."#
                )],
                "{name} at {d:?}"
            );
        }
    }
}

/// A constant that *does* hold a type still resolves — the kind word alone must not be the test.
#[test]
fn a_constant_holding_a_type_is_still_a_valid_annotation() {
    for d in TAGS {
        assert_eq!(
            errors(
                &script(
                    "class Inner:\n\tpass\nconst Alias = Inner",
                    "var x: Alias = null"
                ),
                d
            ),
            Vec::<String>::new(),
            "{d:?}"
        );
    }
}

/// analyzer.cpp:938's `else` — the head resolved, but to something with no nested types at all.
/// An enum value in type position (`E.A`) is the case that shows up in real source.
#[test]
fn an_enum_value_in_type_position_is_a_nested_type_miss() {
    for d in TAGS {
        assert_eq!(
            errors(&script("enum E { A }", "var x: E.A = null"), d),
            vec![r#"Could not find nested type "A" under base "a.gd.E"."#.to_owned()],
            "{d:?}"
        );
    }
}

/// analyzer.cpp:930-931 — a native head followed by a name that is not one of its enums.
#[test]
fn a_native_base_with_an_unknown_nested_name_is_an_error() {
    for d in TAGS {
        assert_eq!(
            errors(&script("", "var x: Node.NotAnEnum = null"), d),
            vec![r#"Could not find type "NotAnEnum" in "Node"."#.to_owned()],
            "{d:?}"
        );
    }
}

/// The same negative under a `Generic` or `Absent` dump is not provable — a custom engine build's
/// enum is indistinguishable from a typo — so it degrades silently (docs/00 "unknown stays
/// dynamic"). The structural arms below are NOT gated this way.
#[test]
fn the_native_nested_miss_is_gated_to_an_exact_dump() {
    for p in [ApiProvenance::Generic, ApiProvenance::Absent] {
        assert_eq!(
            errors_with(
                &script("", "var x: Node.NotAnEnum = null"),
                Dialect::DEFAULT,
                p
            ),
            Vec::<String>::new(),
            "{p:?}"
        );
    }
}

/// analyzer.cpp:924-926 / :739-740 / :757-758 — an enum has no nested types, and neither `Variant`
/// nor a builtin carries anything but enums. All three are structural, so no provenance gate.
#[test]
fn a_segment_under_an_enum_or_builtin_is_always_rejected() {
    let cases = [
        (
            "var x: Node.ProcessMode.X = null",
            "Enums cannot contain nested types.",
        ),
        (
            "var x: Variant.Type.X = null",
            "Variant only contains enum types, which do not have nested types.",
        ),
        (
            "var x: int.A.B = null",
            "Built-in types only contain enum types, which do not have nested types.",
        ),
    ];
    for d in TAGS {
        for (use_, msg) in cases {
            for p in [
                ApiProvenance::Exact,
                ApiProvenance::Generic,
                ApiProvenance::Absent,
            ] {
                assert_eq!(
                    errors_with(&script("", use_), d, p),
                    vec![msg.to_owned()],
                    "{use_} at {d:?}/{p:?}"
                );
            }
        }
    }
}

/// analyzer.cpp:735 / :754 — a `Variant.` or builtin head followed by a name that is not one of
/// its enums. Dump-derived, so gated like the native arm.
#[test]
fn a_variant_or_builtin_base_with_an_unknown_nested_name_is_an_error() {
    for d in TAGS {
        assert_eq!(
            errors(&script("", "var x: Variant.Nope = null"), d),
            vec![r#"Name "Nope" is not a nested type of "Variant"."#.to_owned()],
            "{d:?}"
        );
        assert_eq!(
            errors(&script("", "var x: int.Foo = null"), d),
            vec![r#"Name "Foo" is not a nested type of "int"."#.to_owned()],
            "{d:?}"
        );
    }
    for p in [ApiProvenance::Generic, ApiProvenance::Absent] {
        assert_eq!(
            errors_with(&script("", "var x: int.Foo = null"), Dialect::DEFAULT, p),
            Vec::<String>::new(),
            "{p:?}"
        );
    }
}

/// The valid shapes the new arms sit next to: an inner class, a named enum, a native enum, and a
/// builtin enum all still resolve.
#[test]
fn the_valid_annotation_shapes_stay_silent() {
    let cases = [
        ("class Inner:\n\tpass", "var x: Inner = null"),
        ("enum E2 { A }", "var x: E2 = E2.A"),
        ("", "var x: Node.ProcessMode = Node.PROCESS_MODE_INHERIT"),
        ("", "var x: Variant.Type = TYPE_NIL"),
        ("", "var x: Vector3.Axis = Vector3.AXIS_X"),
    ];
    for d in TAGS {
        for (decl, use_) in cases {
            assert_eq!(
                errors(&script(decl, use_), d),
                Vec::<String>::new(),
                "{use_} at {d:?}"
            );
        }
    }
}

/// Under a **meta** base, `reduce_identifier_from_base` sees a constant or a static function (they
/// resolve, to a value — analyzer.cpp:918) but not an instance variable, a signal, or an instance
/// function (they are not there at all — :915). The two messages are not interchangeable, and
/// which one a kind draws is pinned here against the oracle.
#[test]
fn an_inner_class_base_splits_its_two_rejections_by_member_kind() {
    const INNER: &str = "class Inner:\n\tvar iv = 1\n\tsignal isig\n\tconst IC = 5\n\tconst IAlias = IInner\n\tenum IE { A }\n\tclass IInner:\n\t\tpass\n\tfunc ifn() -> void:\n\t\tpass\n\tstatic func isfn() -> void:\n\t\tpass";
    let not_found = ["iv", "isig", "ifn", "Nope"];
    let not_a_type = ["IC", "isfn"];
    for d in TAGS {
        for name in not_found {
            assert_eq!(
                errors(&script(INNER, &format!("var x: Inner.{name} = null")), d),
                vec![format!(
                    r#"Could not find type "{name}" under base "Inner"."#
                )],
                "{name} at {d:?}"
            );
        }
        for name in not_a_type {
            assert_eq!(
                errors(&script(INNER, &format!("var x: Inner.{name} = null")), d),
                vec![format!(
                    r#"Member "{name}" under base "Inner" is not a valid type."#
                )],
                "{name} at {d:?}"
            );
        }
        // The shapes that do resolve: an inner class, an enum, and a constant that holds a type.
        for use_ in [
            "var x: Inner.IInner = null",
            "var x: Inner.IE = Inner.IE.A",
            "var x: Inner.IAlias = null",
        ] {
            assert_eq!(
                errors(&script(INNER, use_), d),
                Vec::<String>::new(),
                "{use_} at {d:?}"
            );
        }
        // A segment under an enum keeps walking upstream, so an enum *value* under an inner
        // enum draws :918 and renders the enum's own qualified name as the base.
        assert_eq!(
            errors(&script(INNER, "var x: Inner.IE.A = null"), d),
            vec![r#"Member "A" under base "a.gd::Inner.IE" is not a valid type."#.to_owned()],
            "{d:?}"
        );
    }
}
