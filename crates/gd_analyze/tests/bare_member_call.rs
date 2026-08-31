//! #429 — a bare call naming a member of the chain draws the value-callable pair.
//!
//! Godot never reduces a bare identifier callee before its miss branch
//! (`gdscript_analyzer.cpp:3556-3559`), so the branch's own
//! `reduce_identifier_from_base` is the first thing to type it and the answer is
//! `Name "%s" called as a function but is a "%s".` (`gdscript_analyzer.cpp:3747`). gdls
//! pre-reduces that callee, and the dispatcher's tail-guard used to stamp `Variant` over the
//! typeless state the probe's don't-re-resolve guard (`analyzer.cpp:4025-4027`) needs — so the
//! probe was a no-op and a bare `name()` or `hp()` reported nothing at all.
//!
//! Every row is verbatim `Godot_v4.7.2-stable --headless --check-only` output.

use std::collections::HashMap;
use std::path::Path;

use gd_analyze::{analyze, CrossFileQuery, Severity, StrictSettings, WarnPolicy};
use gd_project::{FileId, Interface};
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

struct Project {
    ifaces: HashMap<FileId, Interface>,
    by_class_name: HashMap<String, FileId>,
    by_path: HashMap<String, FileId>,
    paths: HashMap<FileId, String>,
}

impl Project {
    fn new(files: &[(&str, &str)]) -> Self {
        let mut p = Project {
            ifaces: HashMap::new(),
            by_class_name: HashMap::new(),
            by_path: HashMap::new(),
            paths: HashMap::new(),
        };
        for (i, (path, src)) in files.iter().enumerate() {
            let fid = FileId::new(i as u32 + 1);
            let iface = gd_project::extract_interface(&parse(src).tree);
            if let Some(name) = &iface.class_name {
                p.by_class_name.insert(name.clone(), fid);
            }
            p.by_path.insert((*path).to_owned(), fid);
            p.paths.insert(fid, (*path).to_owned());
            p.ifaces.insert(fid, iface);
        }
        p
    }
}

impl CrossFileQuery for Project {
    fn global_class_file(&self, name: &str) -> Option<FileId> {
        self.by_class_name.get(name).copied()
    }
    fn interface(&self, file: FileId) -> Option<&Interface> {
        self.ifaces.get(&file)
    }
    fn resolve_res_path(&self, path: &str) -> Option<FileId> {
        self.by_path.get(path).copied()
    }
    fn file_path(&self, file: FileId) -> Option<&str> {
        self.paths.get(&file).map(String::as_str)
    }
}

const LIB_GD: &str = "\
class_name Lib
extends Node

signal some_signal(a: int)

const SOME_CONST := 5

enum SomeEnum { A }

static var static_count := 0

var hp := 1

static func compare(a: int, b: int) -> int:
\treturn a - b
";

/// A `class_name` whose file does not parse cleanly, so its interface cannot testify to anything.
const BROKEN_GD: &str = "\
class_name BrokenLib
extends Node

var held := 1

func (( -> :
";

fn diagnose(consumer: &str) -> (Vec<String>, Vec<String>) {
    let project = Project::new(&[
        ("res://lib.gd", LIB_GD),
        ("res://broken.gd", BROKEN_GD),
        ("res://main.gd", consumer),
    ]);
    let tree = parse(consumer).tree;
    let result = analyze(
        &tree,
        Some(FileId::new(3)),
        "res://main.gd",
        &native_db(),
        &project,
        &policy(),
    );
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

fn body(extends: &str, stmt: &str) -> String {
    format!("extends {extends}\n\nfunc go() -> void:\n\t{stmt}\n")
}

fn value_msg(name: &str, ty: &str) -> String {
    format!(r#"Name "{name}" called as a function but is a "{ty}"."#)
}

fn not_a_function(name: &str) -> String {
    format!(r#"Member "{name}" is not a function."#)
}

// ===================================================================================================
// The native surface, reached through the implicit `self` base.
// ===================================================================================================

/// Every member kind `reduce_identifier_from_base`'s native tail can answer
/// (`gdscript_analyzer.cpp:4341-4384`): a property, a signal, a constant, an enum name, and a
/// property whose real type is its getter's enum.
#[test]
fn a_bare_native_member_call_names_its_type() {
    for (name, ty) in [
        ("name", "StringName"),
        ("process_mode", "Node.ProcessMode"),
        ("renamed", "Signal"),
        ("NOTIFICATION_READY", "int"),
        ("ProcessMode", "Node.ProcessMode"),
    ] {
        let (errors, warnings) = diagnose(&body("Node", &format!("{name}()")));
        assert_eq!(errors, vec![value_msg(name, ty)], "{name}");
        assert_eq!(warnings, Vec::<String>::new(), "{name}");
    }
}

/// The issue's repro: the same member bare and through `self.`, in one body. The qualified half
/// always worked — an attribute callee is never pre-reduced — so this pins the two against
/// each other.
#[test]
fn the_bare_and_the_qualified_call_report_the_same_thing() {
    let (errors, _) = diagnose("extends Node\n\nfunc go() -> void:\n\tname()\n\tself.name()\n");
    assert_eq!(
        errors,
        vec![
            value_msg("name", "StringName"),
            value_msg("name", "StringName")
        ]
    );
}

/// A name that is on nothing still reports not-found — the probe must not eat it.
#[test]
fn a_bare_call_on_a_name_that_is_nowhere_still_reports_not_found() {
    let (errors, warnings) = diagnose(&body("Node", "nope_bare()"));
    assert_eq!(
        errors,
        vec![r#"Function "nope_bare()" not found in base self."#.to_owned()]
    );
    assert_eq!(warnings, Vec::<String>::new());
}

/// A native METHOD that exists resolves before the miss branch and reports nothing. This is the
/// false positive the old tail-guard was protecting against — typing `queue_free` as a value
/// would have drawn `Name "queue_free" is a Callable. …` on every such call.
#[test]
fn a_real_native_method_call_stays_silent() {
    for stmt in ["queue_free()", "get_parent()", "add_to_group(\"x\")"] {
        let (errors, warnings) = diagnose(&body("Node", stmt));
        assert_eq!(errors, Vec::<String>::new(), "{stmt}");
        assert_eq!(warnings, Vec::<String>::new(), "{stmt}");
    }
}

// ===================================================================================================
// The cross-file chain.
// ===================================================================================================

/// Every member kind of a cross-file base, bare-called. Godot emits BOTH halves; the
/// `Member "X" is not a function.` half already fired (#406), the value half is what was missing.
#[test]
fn a_bare_call_on_a_cross_file_member_reports_both_halves() {
    for (name, ty) in [
        ("hp", "int"),
        ("some_signal", "Signal"),
        ("SOME_CONST", "int"),
        ("static_count", "int"),
        ("SomeEnum", "Lib.SomeEnum"),
    ] {
        let (errors, _) = diagnose(&body("Lib", &format!("{name}()")));
        assert!(errors.contains(&not_a_function(name)), "{name}: {errors:?}");
        assert!(errors.contains(&value_msg(name, ty)), "{name}: {errors:?}");
        assert_eq!(errors.len(), 2, "{name}: {errors:?}");
    }
}

/// A `super.` call skips the value resolution entirely (every site is gated `!p_call->is_super`),
/// so it gets not-found alongside the `Member` half instead of the value message.
#[test]
fn a_super_call_on_a_cross_file_member_gets_not_found_not_the_value() {
    let (errors, _) = diagnose(&body("Lib", "super.hp()"));
    assert!(errors.contains(&not_a_function("hp")), "{errors:?}");
    assert!(
        errors.contains(&r#"Function "hp()" not found in base Lib."#.to_owned()),
        "{errors:?}"
    );
    assert!(
        !errors.iter().any(|e| e.starts_with(r#"Name "hp""#)),
        "{errors:?}"
    );
}

/// An inherited static function resolves and reports nothing.
#[test]
fn a_bare_call_on_an_inherited_static_stays_silent() {
    let (errors, warnings) = diagnose(&body("Lib", "compare(1, 2)"));
    assert_eq!(errors, Vec::<String>::new());
    assert_eq!(warnings, Vec::<String>::new());
}

/// The base did not parse cleanly, so neither half of the pair fires. Godot never reaches the
/// call at all — it refuses the inheritance with
/// `Could not resolve super class inheritance from "BrokenLib".` — and half a pair invented from
/// a partial interface would be worse than the silence.
#[test]
fn a_bare_call_on_an_unparseable_bases_member_stays_silent() {
    let (errors, warnings) = diagnose(&body("BrokenLib", "held()"));
    assert_eq!(errors, Vec::<String>::new());
    assert_eq!(warnings, Vec::<String>::new());
}

// ===================================================================================================
// #435 — the callee walk stops at the outer boundary.
// ===================================================================================================

/// The issue's repro. A bare call naming an OUTER class's member from an inner class is a miss,
/// not a value. Godot's callee resolution walks the base chain only: with an explicit base its
/// self-name arm is gated off (`gdscript_analyzer.cpp:4188`) and its ancestry loop breaks the
/// moment it leaves that chain (`:4270-4275`), so the outer class is gathered and never reached.
#[test]
fn an_outer_classs_member_called_bare_is_a_miss() {
    let src = "extends Node\n\nconst OC := 1\n\nstatic func osf() -> void:\n\tpass\n\n\
               class Inner:\n\tfunc f() -> void:\n\t\tOC()\n\t\tosf()\n";
    let (errors, _) = diagnose(src);
    assert_eq!(
        errors,
        vec![
            r#"Function "OC()" not found in base self."#,
            r#"Function "osf()" not found in base self."#,
        ]
    );
}

/// A local, a local typed `Callable`, and a parameter, each called bare. None of them is on the
/// chain, so all three are misses — the `Name "x" is a Callable` hint is reserved for a member
/// holding a callable, which the probe DOES reach.
#[test]
fn a_local_or_parameter_called_bare_is_a_miss() {
    let src = "extends Node\n\nfunc f() -> void:\n\tvar x: int = 1\n\tx()\n\n\
               func h() -> void:\n\tvar c: Callable = Callable()\n\tc()\n\n\
               func i(p: int) -> void:\n\tp()\n";
    let (errors, _) = diagnose(src);
    assert_eq!(
        errors,
        vec![
            r#"Function "x()" not found in base self."#,
            r#"Function "c()" not found in base self."#,
            r#"Function "p()" not found in base self."#,
        ]
    );
}

/// A native class name called bare is a miss too: `Node` is a global, not a member of the chain.
#[test]
fn a_native_class_name_called_bare_is_a_miss() {
    let (errors, _) = diagnose(&body("Node", "Node()"));
    assert_eq!(errors, vec![r#"Function "Node()" not found in base self."#]);
}

/// The preservation half. A member of the class ITSELF, and one inherited through an in-file base,
/// both still draw the full value-callable pair — that is what the chain walk exists for.
#[test]
fn an_own_or_inherited_member_still_draws_the_pair() {
    let own = "extends Node\n\nconst C := 1\n\nfunc f() -> void:\n\tC()\n";
    let (errors, _) = diagnose(own);
    assert_eq!(
        errors,
        vec![not_a_function("C"), value_msg("C", "int")],
        "an own member keeps the pair"
    );

    let inherited = "extends Node\n\nclass Mid:\n\tconst MC := 1\n\n\
                     class Leaf extends Mid:\n\tfunc f() -> void:\n\t\tMC()\n";
    let (errors, _) = diagnose(inherited);
    assert_eq!(
        errors,
        vec![not_a_function("MC"), value_msg("MC", "int")],
        "an in-file inherited member keeps the pair"
    );
}

/// A member holding a `Callable` keeps the `.call()` hint — the one message that arm is for.
#[test]
fn a_member_holding_a_callable_keeps_the_call_hint() {
    let src = "extends Node\n\nvar cv: Callable = Callable()\n\nfunc f() -> void:\n\tcv()\n";
    let (errors, _) = diagnose(src);
    assert!(
        errors.contains(
            &r#"Name "cv" is a Callable. You can call it with "cv.call()" instead."#.to_owned()
        ),
        "{errors:?}"
    );
}
