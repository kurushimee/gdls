//! #460: the statement-shape warning family must not fire on a `match` pattern guard.
//!
//! Upstream the family is parse-time and lives in one place — `parse_statement`'s
//! expression-statement arm (gdscript_parser.cpp:2144-2190). A guard never goes through that arm:
//! `parse_match_branch` calls `parse_expression(false)` and appends the result straight to
//! `guard_body->statements` (gdscript_parser.cpp:2537), so the check cannot see it.
//!
//! gdls ports the family into the analyzer, where `resolve_suite` walks every suite the same way.
//! What separates a guard from a real suite there is `is_root`: the guard body is the one suite
//! Godot resolves with `p_is_root = false` (gdscript_analyzer.cpp:2443).
//!
//! Every row is pinned against `godot --headless --check-only` at 4.7.2 with the family at level 2.

use gd_analyze::warn_policy::{StrictSettings, WarnPolicy};
use gd_analyze::warnings::WarningCode;
use gd_analyze::{analyze_with_options, AnalyzeOptions, NoCrossFile};
use gd_project::{FileId, WarningConfig};
use gd_syntax::{Dialect, ParseOptions};
use gd_types::NativeDb;

const TAGS: [Dialect; 2] = [Dialect::Godot4_6, Dialect::Godot4_7];

/// The three codes `emit_standalone_statement_warnings` can push.
const FAMILY: [WarningCode; 3] = [
    WarningCode::StandaloneExpression,
    WarningCode::StandaloneTernary,
    WarningCode::ReturnValueDiscarded,
];

fn mini_native() -> NativeDb {
    NativeDb::from_json(
        r#"{
            "header": {"version_major": 4, "version_minor": 7, "version_patch": 2},
            "utility_functions": [
                {"name": "print", "return_type": "void", "category": "general",
                 "is_vararg": true, "hash": 1, "arguments": []}
            ],
            "classes": [
                {"name": "Object"},
                {"name": "Node", "inherits": "Object"}
            ]
        }"#,
    )
    .expect("valid mini dump")
}

/// Only the statement-shape family, as `(code, 1-based line)`. `RETURN_VALUE_DISCARDED` and
/// `STANDALONE_TERNARY` are ignore-by-default, so both are enabled explicitly.
fn family(src: &str, dialect: Dialect) -> Vec<(WarningCode, u32)> {
    let tree = gd_syntax::parse_with_options(
        src,
        &ParseOptions {
            dialect,
            script_path: "",
        },
    )
    .tree;
    let strict = StrictSettings {
        enable_warnings: ["RETURN_VALUE_DISCARDED", "STANDALONE_TERNARY"]
            .iter()
            .map(|s| s.to_string())
            .collect(),
        ..Default::default()
    };
    let policy = WarnPolicy::build(&WarningConfig::default(), &strict, dialect);
    analyze_with_options(
        &tree,
        Some(FileId::new(1)),
        "t.gd",
        &mini_native(),
        &NoCrossFile,
        &policy,
        AnalyzeOptions {
            dialect,
            ..Default::default()
        },
    )
    .diagnostics
    .iter()
    .filter_map(|d| {
        let code = d.warning_code()?;
        if !FAMILY.contains(&code) {
            return None;
        }
        let line = 1 + src.as_bytes()[..d.span().start.min(src.len())]
            .iter()
            .filter(|&&b| b == b'\n')
            .count() as u32;
        Some((code, line))
    })
    .collect()
}

/// `expr` as a guard on a branch that binds `b`.
fn guarded(expr: &str) -> String {
    format!(
        "extends Node\n\nfunc f(n: int, c: bool) -> String:\n\tmatch n:\n\t\tvar b when {expr}:\n\t\t\treturn str(b)\n\t\t_:\n\t\t\treturn \"other\"\n"
    )
}

/// The same `expr` as a plain statement in a function body — the control for every guard row.
fn statement(expr: &str) -> String {
    format!("extends Node\n\nfunc f(n: int, c: bool) -> void:\n\t{expr}\n")
}

/// One expression per arm of `emit_standalone_statement_warnings`, each silent as a guard.
#[test]
fn no_statement_shape_warning_fires_on_a_guard() {
    for d in TAGS {
        for expr in [
            "b > 10",                          // catch-all arm
            "b",                               // bare identifier
            "(1 if c else 2) > 0",             // ternary inside a comparison
            "1 if c else 2",                   // bare ternary
            "preload(\"res://x.gd\") != null", // preload inside a comparison
            "preload(\"res://x.gd\")",         // bare preload
            "1",                               // a non-String literal
            "\"note\"",                        // a String literal (a comment upstream)
        ] {
            assert_eq!(
                family(&guarded(expr), d),
                Vec::<(WarningCode, u32)>::new(),
                "{expr:?} at {d:?}"
            );
        }
    }
}

/// The control: written as ordinary statements, the same expressions still warn. This is what
/// proves the gate suppressed the guard and not the family.
#[test]
fn the_same_expressions_still_warn_as_statements() {
    for d in TAGS {
        for (expr, code) in [
            ("n > 10", WarningCode::StandaloneExpression),
            ("n", WarningCode::StandaloneExpression),
            ("1 if c else 2", WarningCode::StandaloneTernary),
            ("preload(\"res://x.gd\")", WarningCode::ReturnValueDiscarded),
            ("1", WarningCode::StandaloneExpression),
        ] {
            assert_eq!(
                family(&statement(expr), d),
                vec![(code, 4)],
                "{expr:?} at {d:?}"
            );
        }
        // A String literal statement is a multiline comment upstream — silent either way.
        assert_eq!(
            family(&statement("\"note\""), d),
            Vec::<(WarningCode, u32)>::new(),
            "string literal at {d:?}"
        );
    }
}

/// The guarded branch's BLOCK is a root suite (`resolve_suite(p_match_branch->block)` takes the
/// default `true`, gdscript_analyzer.cpp:2446), so a standalone statement inside it still warns.
#[test]
fn the_guarded_branchs_block_still_warns() {
    let src = "extends Node\n\nfunc f(n: int) -> void:\n\tmatch n:\n\t\tvar b when b > 10:\n\t\t\tb\n\t\t_:\n\t\t\tpass\n";
    for d in TAGS {
        assert_eq!(
            family(src, d),
            vec![(WarningCode::StandaloneExpression, 6)],
            "{d:?}"
        );
    }
}

/// An unguarded branch's block is a root suite too — the gate must not reach it.
#[test]
fn an_unguarded_branch_block_still_warns() {
    let src =
        "extends Node\n\nfunc f(n: int) -> void:\n\tmatch n:\n\t\t1:\n\t\t\tn\n\t\t_:\n\t\t\tpass\n";
    for d in TAGS {
        assert_eq!(
            family(src, d),
            vec![(WarningCode::StandaloneExpression, 6)],
            "{d:?}"
        );
    }
}
