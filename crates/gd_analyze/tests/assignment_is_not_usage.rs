//! #464: writing to a name is not using it.
//!
//! `parse_assignment` decrements the declaration's `usages` the moment it sees a bare identifier
//! on the left (gdscript_parser.cpp:3135-3157, "Also remove one usage since assignment isn't
//! usage"), cancelling the increment `parse_identifier` just made. gdls counts uses by sweeping
//! identifier nodes instead of keeping a counter, so it had no equivalent and every write-only
//! local and parameter read as used.
//!
//! The decrement covers locals, local constants, parameters, iterators, and pattern binds. It
//! never covers members, so a private member written but never read stays silent — pinned below.
//! Every row is pinned against `godot --headless --check-only` at 4.7.2.

use std::path::Path;

use gd_analyze::warnings::WarningCode;
use gd_analyze::{analyze_with_options, AnalyzeOptions, NoCrossFile, StrictSettings, WarnPolicy};
use gd_project::{FileId, WarningConfig};
use gd_syntax::{Dialect, ParseOptions};
use gd_types::NativeDb;

const TAGS: [Dialect; 2] = [Dialect::Godot4_6, Dialect::Godot4_7];

fn native_db() -> NativeDb {
    let path =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../gd_types/tests/fixtures/trimmed_api.json");
    NativeDb::load(path.to_str().expect("utf-8 path"))
        .unwrap_or_else(|e| panic!("load native DB fixture at {}: {e}", path.display()))
}

/// Every warning as `(code, 1-based line)`.
fn warnings(src: &str, dialect: Dialect) -> Vec<(WarningCode, u32)> {
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
        "t.gd",
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
    .filter_map(|d| {
        let code = d.warning_code()?;
        let line = 1 + src.as_bytes()[..d.span().start.min(src.len())]
            .iter()
            .filter(|&&b| b == b'\n')
            .count() as u32;
        Some((code, line))
    })
    .collect()
}

fn unused(src: &str, dialect: Dialect) -> Vec<(WarningCode, u32)> {
    warnings(src, dialect)
        .into_iter()
        .filter(|(c, _)| {
            matches!(
                c,
                WarningCode::UnusedVariable
                    | WarningCode::UnusedParameter
                    | WarningCode::UnusedLocalConstant
                    | WarningCode::UnusedPrivateClassVariable
                    | WarningCode::UnusedSignal
            )
        })
        .collect()
}

fn func(body: &str) -> String {
    format!("extends Node\n\nfunc f() -> void:\n{body}\n")
}

/// The two reported shapes: a plain write and a compound write, neither of which rescues the
/// local. The read on the right of `x += 1` is part of the same assignment and does not count.
#[test]
fn a_write_only_local_is_unused() {
    for d in TAGS {
        assert_eq!(
            unused(&func("\tvar x := 1\n\tx = 2"), d),
            vec![(WarningCode::UnusedVariable, 4)],
            "plain at {d:?}"
        );
        assert_eq!(
            unused(&func("\tvar x := 1\n\tx += 1"), d),
            vec![(WarningCode::UnusedVariable, 4)],
            "compound at {d:?}"
        );
    }
}

/// A read anywhere else still counts, on either side of the write, and the right-hand side of the
/// write is a read like any other.
#[test]
fn any_real_read_still_counts() {
    for d in TAGS {
        for body in [
            "\tvar x := 1\n\tx = x + 1",
            "\tvar x := 1\n\tx += 1\n\tprint(x)",
            "\tvar x := 1\n\tprint(x)\n\tx = 2",
        ] {
            assert_eq!(unused(&func(body), d), vec![], "{body:?} at {d:?}");
        }
    }
}

/// Only a bare identifier assignee is exempt. `case Node::SUBSCRIPT: // Okay.` covers both
/// subscript and attribute targets, so the base stays used.
#[test]
fn a_subscript_or_attribute_target_is_still_a_use() {
    for d in TAGS {
        for body in [
            "\tvar arr := [1]\n\tarr[0] = 2",
            "\tvar v := Vector2()\n\tv.x = 1",
            "\tvar dict := {}\n\tdict[\"k\"] = 1",
        ] {
            assert_eq!(unused(&func(body), d), vec![], "{body:?} at {d:?}");
        }
    }
}

/// A capture written inside a lambda is a write like any other — the enclosing local is unused.
#[test]
fn a_write_inside_a_lambda_is_not_a_use() {
    for d in TAGS {
        assert_eq!(
            unused(
                &func("\tvar x := 1\n\tvar lam := func(): x = 2\n\tlam.call()"),
                d
            ),
            vec![(WarningCode::UnusedVariable, 4)],
            "write at {d:?}"
        );
        assert_eq!(
            unused(
                &func("\tvar x := 1\n\tvar lam := func(): print(x)\n\tlam.call()"),
                d
            ),
            vec![],
            "read at {d:?}"
        );
    }
}

/// The same decrement runs for `FUNCTION_PARAMETER`, so a write-only parameter is unused.
#[test]
fn a_write_only_parameter_is_unused() {
    for d in TAGS {
        for body in ["\tp = 1", "\tp += 1"] {
            assert_eq!(
                unused(
                    &format!("extends Node\n\nfunc f(p: int) -> void:\n{body}\n"),
                    d
                ),
                vec![(WarningCode::UnusedParameter, 3)],
                "{body:?} at {d:?}"
            );
        }
        assert_eq!(
            unused(
                "extends Node\n\nfunc f(p: int) -> int:\n\tp = 1\n\treturn p\n",
                d
            ),
            vec![],
            "read after write at {d:?}"
        );
    }
}

/// The decrement is for locals only. A member written but never read keeps its use, so the
/// private-member and signal sweeps are untouched.
#[test]
fn a_write_to_a_member_is_still_a_use() {
    let src = "extends Node\n\nvar _written_only: int = 0\nvar _never: int = 0\n\n\
               func f() -> void:\n\t_written_only = 1\n";
    for d in TAGS {
        assert_eq!(
            unused(src, d),
            vec![(WarningCode::UnusedPrivateClassVariable, 4)],
            "{d:?}"
        );
    }
}
