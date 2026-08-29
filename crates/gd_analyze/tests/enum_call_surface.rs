//! Calls whose base is an enum or a degraded pseudo-type (#325).
//!
//! Four upstream behaviors converge on `enum_declaration_and_usage.gd`, the last entry the analyze
//! ratchet carried: the `can_be_builtin` gate that separates a legal `Side.CORNER_LEFT` from an
//! illegal bare `Side` (analyzer.cpp:4646-4652); the enum-meta call arm, which branches on the
//! enum's own `builtin_type` so only a script-declared enum borrows Dictionary's methods
//! (:3724-3730); the static-function fall-through on any hard meta base (:3772); and
//! `Native class X used in script doesn't exist or isn't exposed.` (:5944) for a dotted native
//! enum, which only surfaces because the subscript's `!valid` arm sets the KIND alone (:4913) and
//! leaves `native_type` standing.
//!
//! Every expectation below was taken from `godot --headless --check-only` on 4.7.2, message for
//! message and in order.

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

/// Wrap a body in a script that declares a script-level enum, so both enum flavors are in scope.
fn script(body: &str) -> String {
    format!("extends Node\n\nenum Custom {{ A, B }}\n\nfunc f() -> void:\n{body}")
}

/// A global enum name is a pseudo-type: only a subscript base may spell it. Godot passes
/// `can_be_builtin = true` from exactly one site (analyzer.cpp:4799), so a member read is fine and
/// everything else — a bare assignment, a call base — is not.
#[test]
fn a_global_enum_may_name_a_member_but_may_not_stand_alone() {
    for d in TAGS {
        assert_eq!(
            errors(&script("\tprint(Side.SIDE_LEFT)\n"), d),
            Vec::<String>::new(),
            "member read at {d:?}"
        );
        assert_eq!(
            errors(&script("\tvar x = Side\n\tprint(x)\n"), d),
            vec![r#"Global enum "Side" cannot be used on its own."#.to_owned()],
            "bare use at {d:?}"
        );
    }
}

/// A native/global/built-in enum metatype carries `builtin_type = NIL`, not `DICTIONARY`
/// (analyzer.cpp:174-176 and twins), so it has no method table at all — three errors, in Godot's
/// order: the standalone-use gate, the native-enum arm, then the static fall-through.
#[test]
fn calling_a_method_on_a_global_enum_reports_the_native_enum_arm() {
    for d in TAGS {
        assert_eq!(
            errors(&script("\tSide.size()\n"), d),
            vec![
                r#"Global enum "Side" cannot be used on its own."#.to_owned(),
                r#"The native enum "Side" does not behave like Dictionary and does not have methods of its own."#.to_owned(),
                r#"Static function "size()" not found in base "Side"."#.to_owned(),
            ],
            "at {d:?}"
        );
    }
}

/// A script-declared enum DOES borrow Dictionary's methods, so a const one is silent and a
/// non-const one draws the Dictionary error — the distinction gdls used to miss by searching the
/// Dictionary table for every enum base regardless of flavor.
#[test]
fn a_script_enum_borrows_dictionarys_methods_and_only_the_non_const_ones_error() {
    for d in TAGS {
        assert_eq!(
            errors(&script("\tprint(Custom.size())\n"), d),
            Vec::<String>::new(),
            "const method at {d:?}"
        );
        assert_eq!(
            errors(&script("\tCustom.clear()\n"), d),
            vec![
                r#"Cannot call non-const Dictionary function "clear()" on enum "Custom"."#
                    .to_owned()
            ],
            "non-const method at {d:?}"
        );
    }
}

/// A name that is on no Dictionary gets the script-enum wording, plus the static fall-through
/// rendering the enum as `<file>.<Enum>` (`DataType::to_string()`'s ENUM arm keeps the filename).
#[test]
fn an_unknown_method_on_a_script_enum_says_enums_only_have_dictionary_methods() {
    for d in TAGS {
        assert_eq!(
            errors(&script("\tCustom.nope()\n"), d),
            vec![
                r#"Enums only have Dictionary built-in methods. Function "nope()" does not exist for enum "Custom"."#.to_owned(),
                r#"Static function "nope()" not found in base "a.gd.Custom"."#.to_owned(),
            ],
            "at {d:?}"
        );
    }
}

/// A dotted native enum used as a call base. The first error comes from the subscript itself; the
/// other two only fire because that subscript's degraded type keeps its `native_type` and its hard
/// meta flags (analyzer.cpp:4913 sets the KIND alone).
#[test]
fn a_dotted_builtin_enum_call_base_reports_the_unexposed_native_class() {
    for d in TAGS {
        assert_eq!(
            errors(&script("\tVector3.Axis.size()\n"), d),
            vec![
                r#"Type "Axis" in base "Vector3" cannot be used on its own."#.to_owned(),
                "Native class Vector3.Axis used in script doesn't exist or isn't exposed."
                    .to_owned(),
                r#"Static function "size()" not found in base "Variant"."#.to_owned(),
            ],
            "at {d:?}"
        );
    }
}

/// The same three, on a native-class enum rather than a built-in one.
#[test]
fn a_dotted_native_enum_call_base_reports_the_unexposed_native_class() {
    for d in TAGS {
        assert_eq!(
            errors(&script("\tNode.ProcessMode.clear()\n"), d),
            vec![
                r#"Type "ProcessMode" in base "Node" cannot be used on its own."#.to_owned(),
                "Native class Node.ProcessMode used in script doesn't exist or isn't exposed."
                    .to_owned(),
                r#"Static function "clear()" not found in base "Variant"."#.to_owned(),
            ],
            "at {d:?}"
        );
    }
}

/// The one place the enum name is legal as a call base is a member read that resolves — the gate
/// is on standalone use, not on the enum type itself.
#[test]
fn reading_a_native_enum_value_through_its_class_stays_silent() {
    for d in TAGS {
        assert_eq!(
            errors(&script("\tprint(Node.PROCESS_MODE_ALWAYS)\n"), d),
            Vec::<String>::new(),
            "at {d:?}"
        );
    }
}

/// Every negative here bottoms out in the native surface, so a non-`Exact` DB must stay silent —
/// a custom engine build may define the very class or method the stock dump lacks. The
/// standalone-use gate is not such a claim (the enum already resolved), so it still fires.
#[test]
fn a_non_exact_api_surface_withholds_the_native_negatives() {
    let mut db = native_db();
    db.set_provenance(gd_types::ApiProvenance::Generic);
    let src = script("\tVector3.Axis.size()\n\tCustom.nope()\n\tSide.size()\n");
    let tree = gd_syntax::parse_with_options(
        &src,
        &ParseOptions {
            dialect: Dialect::Godot4_7,
            script_path: "",
        },
    )
    .tree;
    let policy = WarnPolicy::build(
        &WarningConfig::default(),
        &StrictSettings::default(),
        Dialect::Godot4_7,
    );
    let msgs: Vec<String> = analyze_with_options(
        &tree,
        Some(FileId::new(1)),
        "a.gd",
        &db,
        &NoCrossFile,
        &policy,
        AnalyzeOptions {
            dialect: Dialect::Godot4_7,
            ..Default::default()
        },
    )
    .diagnostics
    .iter()
    .filter(|d| d.warning_code().is_none())
    .map(|d| d.message().to_string())
    .collect();

    assert!(
        !msgs
            .iter()
            .any(|m| m.contains("used in script doesn't exist")),
        "unexposed-native-class claim survived a Generic DB: {msgs:?}"
    );
    assert!(
        !msgs
            .iter()
            .any(|m| m.contains("Enums only have Dictionary built-in methods")),
        "Dictionary-method-miss claim survived a Generic DB: {msgs:?}"
    );
    assert!(
        msgs.iter()
            .any(|m| m == r#"Global enum "Side" cannot be used on its own."#),
        "the standalone-use gate needs no dump and must still fire: {msgs:?}"
    );
}
