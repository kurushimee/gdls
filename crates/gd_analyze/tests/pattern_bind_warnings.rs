//! #462: a `match` pattern bind warns unused and shadowed, and is read-only.
//!
//! `resolve_match_pattern`'s `PT_BIND` arm does three things upstream (gdscript_analyzer.cpp:2485-2496):
//! stamp the datatype, `is_shadowing(bind, "pattern bind", true)`, and UNUSED_VARIABLE when
//! `usages == 0` and the name has no `_` prefix. gdls ported only the first. A fourth piece sits in
//! `reduce_identifier`: a `LOCAL_BIND` read is stamped constant (:4454-4458), which is what makes
//! assigning to a bind an error — a `LOCAL_ITERATOR` deliberately is not (:4450-4453).
//!
//! The bind's scope is its branch's guard body and its block, and nothing else: the parser
//! registers it as a local in both (gdscript_parser.cpp:2521-2527 and :2560-2566).
//!
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

fn analyze(src: &str, dialect: Dialect) -> gd_analyze::AnalysisResult {
    let tree = gd_syntax::parse_with_options(
        src,
        &ParseOptions {
            dialect,
            script_path: "",
        },
    )
    .tree;
    let policy = WarnPolicy::build(
        &WarningConfig::default(),
        &StrictSettings::default(),
        dialect,
    );
    analyze_with_options(
        &tree,
        Some(FileId::new(1)),
        "t.gd",
        &native_db(),
        &NoCrossFile,
        &policy,
        AnalyzeOptions {
            dialect,
            ..Default::default()
        },
    )
}

fn line_of(src: &str, start: usize) -> u32 {
    1 + src.as_bytes()[..start.min(src.len())]
        .iter()
        .filter(|&&b| b == b'\n')
        .count() as u32
}

/// Bind-relevant warnings only — `UNREACHABLE_PATTERN` fires all over these fixtures and is not
/// what is under test.
fn warnings(src: &str, dialect: Dialect) -> Vec<(WarningCode, u32)> {
    analyze(src, dialect)
        .diagnostics
        .iter()
        .filter_map(|d| {
            let code = d.warning_code()?;
            matches!(
                code,
                WarningCode::UnusedVariable
                    | WarningCode::ShadowedVariable
                    | WarningCode::ShadowedGlobalIdentifier
            )
            .then(|| (code, line_of(src, d.span().start)))
        })
        .collect()
}

fn errors(src: &str, dialect: Dialect) -> Vec<String> {
    analyze(src, dialect)
        .diagnostics
        .iter()
        .filter(|d| d.severity() == gd_analyze::Severity::Error)
        .map(|d| d.message().to_string())
        .collect()
}

fn messages(src: &str, dialect: Dialect) -> Vec<String> {
    analyze(src, dialect)
        .diagnostics
        .iter()
        .filter(|d| d.warning_code().is_some())
        .map(|d| d.message().to_string())
        .collect()
}

/// The reported case, with both messages rendered exactly as the engine renders them.
#[test]
fn a_bind_warns_unused_and_shadowed() {
    let src = "extends Node\n\nvar field: int = 0\n\n\
               func f(n: int) -> String:\n\tmatch n:\n\t\tvar field:\n\t\t\treturn str(field)\n\
               \t\tvar unused:\n\t\t\treturn \"u\"\n\t\tvar _ok:\n\t\t\treturn \"ok\"\n\
               \t\t_:\n\t\t\treturn \"other\"\n";
    for d in TAGS {
        assert_eq!(
            warnings(src, d),
            vec![
                (WarningCode::ShadowedVariable, 7),
                (WarningCode::UnusedVariable, 9),
            ],
            "{d:?}"
        );
        let msgs = messages(src, d);
        assert!(
            msgs.contains(
                &"The local pattern bind \"field\" is shadowing an already-declared variable at \
                  line 3 in the current class."
                    .to_owned()
            ),
            "{msgs:?} at {d:?}"
        );
        assert!(
            msgs.contains(
                &"The local variable \"unused\" is declared but never used in the block. If this \
                  is intended, prefix it with an underscore: \"_unused\"."
                    .to_owned()
            ),
            "{msgs:?} at {d:?}"
        );
    }
}

/// A bind used ONLY in the guard is used. This is what a block-only sweep window would miss.
#[test]
fn a_bind_used_only_in_the_guard_is_used() {
    let src = "extends Node\n\nfunc f(n: int) -> String:\n\tmatch n:\n\t\t\
               var g when g > 10:\n\t\t\treturn \"big\"\n\t\t_:\n\t\t\treturn \"other\"\n";
    for d in TAGS {
        assert_eq!(warnings(src, d), vec![], "{d:?}");
    }
}

/// Each branch gets its own window, so a same-named bind in a later branch cannot rescue an
/// earlier one — and a `match` nested inside a block restores the outer branch on the way out.
#[test]
fn each_branch_has_its_own_window() {
    let two = "extends Node\n\nfunc f(n: int) -> String:\n\tmatch n:\n\t\t\
               var s:\n\t\t\treturn \"a\"\n\t\tvar s:\n\t\t\treturn str(s)\n";
    let nested = "extends Node\n\nfunc f(n: int, m: int) -> String:\n\tmatch n:\n\t\t\
                  var o:\n\t\t\tmatch m:\n\t\t\t\tvar i:\n\t\t\t\t\treturn str(i)\n\
                  \t\t\t\t_:\n\t\t\t\t\treturn \"x\"\n\t\t_:\n\t\t\treturn \"y\"\n";
    for d in TAGS {
        assert_eq!(
            warnings(two, d),
            vec![(WarningCode::UnusedVariable, 5)],
            "two branches at {d:?}"
        );
        // The outer bind `o` is unused; the inner `i` is used inside the nested branch.
        assert_eq!(
            warnings(nested, d),
            vec![(WarningCode::UnusedVariable, 5)],
            "nested at {d:?}"
        );
    }
}

/// A bind inside an array pattern goes through the same arm.
#[test]
fn a_nested_pattern_bind_warns_too() {
    let src = "extends Node\n\nfunc f(n: int) -> String:\n\tmatch n:\n\t\t\
               [var a, var c]:\n\t\t\treturn str(a)\n\t\t_:\n\t\t\treturn \"x\"\n";
    for d in TAGS {
        assert_eq!(
            warnings(src, d),
            vec![(WarningCode::UnusedVariable, 5)],
            "{d:?}"
        );
    }
}

/// `is_shadowing`'s global branch rides along, naming the bind by its own kind.
#[test]
fn a_bind_shadowing_a_global_names_the_kind() {
    for (name, tail) in [
        ("print", "has the same name as a built-in function."),
        ("Node", "has the same name as a native class."),
    ] {
        let src = format!(
            "extends Node\n\nfunc f(n: int) -> String:\n\tmatch n:\n\t\t\
             var {name}:\n\t\t\treturn str({name})\n\t\t_:\n\t\t\treturn \"x\"\n"
        );
        for d in TAGS {
            assert_eq!(
                warnings(&src, d),
                vec![(WarningCode::ShadowedGlobalIdentifier, 5)],
                "{name} at {d:?}"
            );
            assert!(
                messages(&src, d).contains(&format!("The pattern bind \"{name}\" {tail}")),
                "{name} at {d:?}"
            );
        }
    }
}

/// A bind read is stamped constant, so writing to one is an error — and a `for` iterator, which
/// upstream deliberately leaves unstamped, stays assignable.
#[test]
fn a_bind_is_read_only_but_an_iterator_is_not() {
    let bind = "extends Node\n\nfunc f(n: int) -> void:\n\tmatch n:\n\t\t\
                var b:\n\t\t\tb = 1\n\t\t\tprint(b)\n";
    let iter = "extends Node\n\nfunc f() -> void:\n\tfor i in range(3):\n\t\ti = 1\n\t\tprint(i)\n";
    for d in TAGS {
        assert_eq!(
            errors(bind, d),
            vec!["Cannot assign a new value to a constant.".to_owned()],
            "bind at {d:?}"
        );
        assert_eq!(errors(iter, d), Vec::<String>::new(), "iterator at {d:?}");
    }
}

/// A write is not a use here either (gdscript_parser.cpp:3152 covers `LOCAL_BIND`), so the bind
/// above is unused on top of being written to.
#[test]
fn a_write_only_bind_is_still_unused() {
    let src = "extends Node\n\nfunc f(n: int) -> void:\n\tmatch n:\n\t\tvar b:\n\t\t\tb = 1\n";
    for d in TAGS {
        assert_eq!(
            warnings(src, d),
            vec![(WarningCode::UnusedVariable, 5)],
            "{d:?}"
        );
    }
}
