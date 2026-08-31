//! #467 — an INSTANCE member reached through the class rather than an instance.
//!
//! Godot's static ladder in `reduce_call` has three arms (`gdscript_analyzer.cpp:3655-3677`) and
//! gdls carried only the outer two. The missing middle one fires when the base is not `self`, the
//! base is a metatype, and the resolved callee is not static:
//! `Cannot call non-static function "X()" on the class "Y" directly. Make an instance instead.`
//! The member half is the same behavior at a different site: `reduce_identifier_from_base`'s CLASS
//! arms (`analyzer.cpp:4228-4257`) refuse to bind a non-static variable, a signal, or a non-static
//! function through a metatype, and the subscript caller then reports `Cannot find member`.
//!
//! Three shapes decide whether this over-fires, and each has a row below. An enum metatype is
//! treated as a Dictionary VALUE for a call (`analyzer.cpp:3664-3667`), so `E.has("A")` binds
//! `Dictionary::has` and must stay silent. Every method of an engine SINGLETON is force-flagged
//! static (`analyzer.cpp:6036-6039`), which is the only reason `OS.get_name()` is legal. And a
//! constructor is force-flagged static too (`analyzer.cpp:5963-5966`), so `X.new()` never fires.
//!
//! Every row is pinned against `Godot_v4.7.2-stable --headless --check-only` inside an imported
//! project. Both tags agree — the ladder, the enum clear, the singleton and constructor forcing,
//! and the member-arm gates are byte-identical between 4.6.3 and 4.7.2, so no dialect guard is
//! owed.

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

/// `UNSAFE_*` default to Ignore; the negative rows assert on them, so turn them on.
fn policy() -> WarnPolicy {
    WarnPolicy::build(
        &gd_project::WarningConfig::default(),
        &StrictSettings {
            enable_warnings: vec![
                "UNSAFE_METHOD_ACCESS".to_owned(),
                "UNSAFE_PROPERTY_ACCESS".to_owned(),
            ],
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

signal ping

var hp := 1

static var counter := 0

const K := 5

func i_m() -> int:
\treturn 1

static func s_m() -> int:
\treturn 2
";

/// A `class_name` whose file does not parse cleanly: its interface cannot testify to a signature,
/// so the #406 `sig_resolved` gate must keep the new arm quiet.
const BROKEN_GD: &str = "\
class_name BrokenLib
extends Node

func i_m() -> int:
\treturn 1

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
    // Only the UNSAFE_* family: the `Inner` fixture's `signal sig` draws UNUSED_SIGNAL in every
    // row, and these assertions are about whether a miss was claimed, not about hygiene.
    let warnings = result
        .diagnostics
        .iter()
        .filter(|d| d.code().starts_with("UNSAFE_"))
        .map(|d| d.message().to_owned())
        .collect();
    (errors, warnings)
}

/// A file carrying the inner class every in-file row reaches through.
fn with_inner(stmt: &str) -> String {
    format!(
        "\
extends Node

class Inner:
\tsignal sig
\tconst K := 5
\tstatic var sv := 0
\tvar i_v: int = 0
\tfunc i_m() -> int:
\t\treturn 2
\tstatic func s_m() -> int:
\t\treturn 3

enum E {{ A, B }}

func g() -> int:
\treturn 1

func f() -> void:
\t{stmt}
"
    )
}

fn cannot_call(func: &str, class: &str) -> String {
    format!(
        r#"Cannot call non-static function "{func}()" on the class "{class}" directly. Make an instance instead."#
    )
}

fn cannot_find(member: &str, base: &str) -> String {
    format!(r#"Cannot find member "{member}" in base "{base}"."#)
}

// ===================================================================================================
// The call arm.
// ===================================================================================================

#[test]
fn an_instance_method_through_an_inner_class_name_errors() {
    let (errors, _) = diagnose(&with_inner("print(Inner.i_m())"));
    assert_eq!(errors, vec![cannot_call("i_m", "Inner")], "{errors:?}");
}

#[test]
fn an_instance_method_through_the_own_class_name_errors() {
    let src = "\
extends Node

class_name Main467

func g() -> int:
\treturn 1

func f() -> void:
\tprint(Main467.g())
";
    let (errors, _) = diagnose(src);
    assert_eq!(errors, vec![cannot_call("g", "Main467")], "{errors:?}");
}

#[test]
fn a_native_instance_method_through_the_native_class_name_errors() {
    let (errors, _) = diagnose(&with_inner("print(Node.get_child(0))"));
    assert_eq!(errors, vec![cannot_call("get_child", "Node")], "{errors:?}");
}

#[test]
fn an_instance_method_through_a_cross_file_class_name_errors() {
    let (errors, _) = diagnose(&with_inner("print(Lib.i_m())"));
    assert_eq!(errors, vec![cannot_call("i_m", "Lib")], "{errors:?}");
}

#[test]
fn an_instance_method_through_a_preload_const_errors() {
    let src = "\
extends Node

const S = preload(\"res://lib.gd\")

func f() -> void:
\tprint(S.i_m())
";
    let (errors, _) = diagnose(src);
    assert_eq!(errors, vec![cannot_call("i_m", "Lib")], "{errors:?}");
}

#[test]
fn a_builtin_instance_method_through_the_type_name_errors() {
    let (errors, _) = diagnose(&with_inner("print(String.length())"));
    assert_eq!(errors, vec![cannot_call("length", "String")], "{errors:?}");
}

/// `to_string` is not on the class's own chain — it comes from the `GDScript` surface
/// `get_function_signature` also consults for a metatype (analyzer.cpp:6017-6025).
#[test]
fn a_gdscript_surface_method_through_a_class_name_errors() {
    let (errors, _) = diagnose(&with_inner("print(Inner.to_string())"));
    assert_eq!(
        errors,
        vec![cannot_call("to_string", "Inner")],
        "{errors:?}"
    );
}

/// The oracle's paired row: a void instance method through the class name draws BOTH the new error
/// and the pre-existing void-return one, in that order.
#[test]
fn a_void_instance_method_still_draws_the_return_value_pair() {
    let src = "\
extends Node

class_name Void467

func v() -> void:
\tpass

func f() -> void:
\tprint(Void467.v())
";
    let (errors, _) = diagnose(src);
    assert_eq!(
        errors,
        vec![
            cannot_call("v", "Void467"),
            r#"Cannot get return value of call to "v()" because it returns "void"."#.to_owned(),
        ],
        "{errors:?}"
    );
}

// ===================================================================================================
// The member half.
// ===================================================================================================

#[test]
fn an_instance_variable_through_a_class_name_cannot_be_found() {
    let (errors, _) = diagnose(&with_inner("print(Inner.i_v)"));
    assert_eq!(errors, vec![cannot_find("i_v", "Inner")], "{errors:?}");
}

#[test]
fn a_signal_through_a_class_name_cannot_be_found() {
    let (errors, _) = diagnose(&with_inner("print(Inner.sig)"));
    assert_eq!(errors, vec![cannot_find("sig", "Inner")], "{errors:?}");
}

#[test]
fn an_uncalled_instance_method_through_a_class_name_cannot_be_found() {
    let (errors, _) = diagnose(&with_inner("var m = Inner.i_m\n\tprint(m)"));
    assert_eq!(errors, vec![cannot_find("i_m", "Inner")], "{errors:?}");
}

#[test]
fn a_cross_file_instance_variable_through_a_class_name_cannot_be_found() {
    let (errors, _) = diagnose(&with_inner("print(Lib.hp)"));
    assert_eq!(errors, vec![cannot_find("hp", "Lib")], "{errors:?}");
}

// ===================================================================================================
// The firewall. Every row here is a shape the new arm must NOT fire on.
// ===================================================================================================

#[test]
fn a_static_method_through_a_class_name_stays_silent() {
    for stmt in ["print(Inner.s_m())", "print(Lib.s_m())"] {
        let (errors, warnings) = diagnose(&with_inner(stmt));
        assert_eq!(errors, Vec::<String>::new(), "{stmt}: {errors:?}");
        assert_eq!(warnings, Vec::<String>::new(), "{stmt}: {warnings:?}");
    }
}

#[test]
fn a_constructor_through_a_class_name_stays_silent() {
    for stmt in [
        "print(Inner.new())",
        "print(Lib.new())",
        "print(Node.new())",
    ] {
        let (errors, warnings) = diagnose(&with_inner(stmt));
        assert_eq!(errors, Vec::<String>::new(), "{stmt}: {errors:?}");
        assert_eq!(warnings, Vec::<String>::new(), "{stmt}: {warnings:?}");
    }
}

#[test]
fn an_instance_receiver_stays_silent() {
    for stmt in [
        "print(Inner.new().i_m())",
        "var l := Lib.new()\n\tprint(l.i_m())",
        "print(\"x\".length())",
    ] {
        let (errors, warnings) = diagnose(&with_inner(stmt));
        assert_eq!(errors, Vec::<String>::new(), "{stmt}: {errors:?}");
        assert_eq!(warnings, Vec::<String>::new(), "{stmt}: {warnings:?}");
    }
}

/// `Engine::has_singleton` force-flags every singleton method static (analyzer.cpp:6036-6039).
/// Without that forcing, every `OS.`/`Engine.`/`Time.` call in a real project would fire.
#[test]
fn an_engine_singleton_call_through_its_class_name_stays_silent() {
    for stmt in [
        // `Engine` is a fixture singleton whose CLASS the trimmed dump does not carry, so it
        // cannot resolve here; `OS` and `Time` carry both halves.
        "print(OS.get_name())",
        "print(Time.get_ticks_msec())",
    ] {
        let (errors, warnings) = diagnose(&with_inner(stmt));
        assert_eq!(errors, Vec::<String>::new(), "{stmt}: {errors:?}");
        assert_eq!(warnings, Vec::<String>::new(), "{stmt}: {warnings:?}");
    }
}

/// An enum metatype is a Dictionary VALUE for the purpose of a call (analyzer.cpp:3664-3667), so
/// `Dictionary`'s own non-static methods bind and must not read as "called on the class".
#[test]
fn an_enum_metatype_dictionary_call_stays_silent() {
    for stmt in ["print(E.has(\"A\"))", "print(E.keys())"] {
        let (errors, warnings) = diagnose(&with_inner(stmt));
        assert_eq!(errors, Vec::<String>::new(), "{stmt}: {errors:?}");
        assert_eq!(warnings, Vec::<String>::new(), "{stmt}: {warnings:?}");
    }
}

/// #406's gate on the new arm: an unclean or unresolvable base yields no signature, and a claim
/// about staticness is a claim about a signature — so it must not be made.
#[test]
fn a_base_gdls_could_not_fully_walk_stays_silent() {
    let (errors, _) = diagnose(&with_inner("print(BrokenLib.i_m())"));
    assert_eq!(errors, Vec::<String>::new(), "{errors:?}");

    let src = "\
extends Node

class Orphan extends NoSuchThing:
\tfunc i_m() -> int:
\t\treturn 1

func f() -> void:
\tprint(Orphan.i_m())
";
    let (errors, _) = diagnose(src);
    assert!(
        !errors
            .iter()
            .any(|e| e.contains("Make an instance instead")),
        "{errors:?}"
    );
}

/// Constants, enums, and static members carry no metatype gate upstream and still resolve — the
/// `CLI.print_version` shape a real project is full of.
#[test]
fn a_static_member_and_const_through_a_class_name_still_resolve() {
    for stmt in [
        "print(Inner.sv)",
        "print(Inner.K)",
        "print(Lib.counter)",
        "print(Lib.K)",
        "var c = Inner.s_m\n\tprint(c)",
    ] {
        let (errors, warnings) = diagnose(&with_inner(stmt));
        assert_eq!(errors, Vec::<String>::new(), "{stmt}: {errors:?}");
        assert_eq!(warnings, Vec::<String>::new(), "{stmt}: {warnings:?}");
    }
}

/// The native tail of `reduce_identifier_from_base` has no metatype gate at all
/// (analyzer.cpp:4333-4386), so a native property still reads through a class name.
#[test]
fn a_native_property_through_a_class_name_still_resolves() {
    let (errors, warnings) = diagnose(&with_inner("print(Lib.process_mode)"));
    assert_eq!(errors, Vec::<String>::new(), "{errors:?}");
    assert_eq!(warnings, Vec::<String>::new(), "{warnings:?}");
}
