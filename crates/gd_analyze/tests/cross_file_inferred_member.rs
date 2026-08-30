//! #431 — what a cross-file member's type is when nobody annotated it.
//!
//! A member with no `: T` is read off its initializer, and the shallow interface only decodes the
//! shapes that need no evaluation. Every shape it cannot decode reads as `Variant` from another
//! file while Godot has a real type there, which silences the access on that member and then
//! everything downstream of it.
//!
//! The mirror-image failure matters just as much: an inferred type is SOFT in Godot
//! (`resolve_assignable`'s `!has_specified_type` arm hands `var x = e` `INFERRED` and only `:=`
//! or a `const` gets `ANNOTATED_INFERRED`), and a soft type is excused from the checks a hard one
//! must pass. Handing a reader a hard type it never had reports things Godot does not.
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
            enable_warnings: vec![
                "UNSAFE_PROPERTY_ACCESS".to_owned(),
                "UNSAFE_METHOD_ACCESS".to_owned(),
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
    fn autoload_file(&self, name: &str) -> Option<FileId> {
        (name == "Glob431").then(|| FileId::new(4))
    }
    fn is_autoload(&self, name: &str) -> bool {
        name == "Glob431"
    }
}

const HELPER_GD: &str = "\
class_name Helper431
extends RefCounted

var tag := \"h\"

func greet() -> String:
\treturn tag
";

const HOLDER_GD: &str = "\
class_name Holder431
extends Node

class Inner:
\tvar depth := 7

var made := Helper431.new()
var nested := Holder431.Inner.new()
var hard_cast := get_parent() as CanvasItem
var soft_cast = get_parent() as CanvasItem
@onready var hard_node := $Timer
@onready var soft_node = $Timer
var soft_int = 3
var opaque := whatever()
var single: Glob431
";

const AUTOLOAD_GD: &str = "\
extends Node

var level := 3

func ping() -> int:
\treturn level
";

fn diagnose(stmt: &str) -> (Vec<String>, Vec<String>) {
    let consumer = format!("extends Node\n\nfunc go(h: Holder431) -> void:\n\t{stmt}\n");
    let project = Project::new(&[
        ("res://helper.gd", HELPER_GD),
        ("res://holder.gd", HOLDER_GD),
        ("res://main.gd", &consumer),
        ("res://autoload.gd", AUTOLOAD_GD),
    ]);
    let tree = parse(&consumer).tree;
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
    let unsafe_access = result
        .diagnostics
        .iter()
        .filter(|d| d.code().starts_with("UNSAFE_"))
        .map(|d| d.message().to_owned())
        .collect();
    (errors, unsafe_access)
}

fn missing_prop(name: &str, ty: &str) -> String {
    format!(
        "The property \"{name}\" is not present on the inferred type \"{ty}\" \
         (but may be present on a subtype)."
    )
}

fn missing_method(name: &str, ty: &str) -> String {
    format!(
        "The method \"{name}()\" is not present on the inferred type \"{ty}\" \
         (but may be present on a subtype)."
    )
}

#[test]
fn a_constructed_member_carries_the_class_it_constructs() {
    let (errors, _) = diagnose("print(h.made.greet())");
    assert!(errors.is_empty(), "{errors:?}");
    let (_, access) = diagnose("print(h.made.nope())");
    assert_eq!(access, vec![missing_method("nope", "Helper431")]);
}

#[test]
fn a_dotted_constructor_reaches_the_inner_class() {
    // `Holder431.Inner.new()` used to stop at the outer class, so `depth` read as a member of
    // `Holder431` — a miss that then reported nothing because the type was already wrong.
    let (errors, access) = diagnose("print(h.nested.depth)");
    assert!(errors.is_empty(), "{errors:?}");
    assert!(access.is_empty(), "{access:?}");
    let (_, access) = diagnose("print(h.nested.nope)");
    assert_eq!(access, vec![missing_prop("nope", "Inner")]);
}

#[test]
fn a_cast_types_the_member_it_initializes() {
    let (_, access) = diagnose("print(h.hard_cast.nope)");
    assert_eq!(access, vec![missing_prop("nope", "CanvasItem")]);
}

#[test]
fn a_node_lookup_types_the_member_as_a_bare_node() {
    // `$`/`%` are a hard bare `Node`, the analyzer's own floor for them (docs/02 §11).
    let (_, access) = diagnose("print(h.hard_node.nope)");
    assert_eq!(access, vec![missing_prop("nope", "Node")]);
}

#[test]
fn an_initializer_that_needs_evaluating_still_has_no_answer() {
    // Under-reporting is the safe direction: `whatever()` names nothing decodable, so the member
    // stays Variant and nothing is claimed about it.
    let (errors, access) = diagnose("print(h.opaque.anything)");
    assert!(errors.is_empty(), "{errors:?}");
    assert!(access.is_empty(), "{access:?}");
}

#[test]
fn an_autoload_named_as_a_type_resolves_to_its_script() {
    // `var single: Glob431` — the cross-file twin of the analyzer's own autoload arm
    // (analyzer.cpp:830-845). Without it the annotation resolved to nothing and every access
    // through the member went quiet.
    let (errors, access) = diagnose("print(h.single.ping())");
    assert!(errors.is_empty(), "{errors:?}");
    assert!(access.is_empty(), "{access:?}");
    let (_, access) = diagnose("print(h.single.nope())");
    assert_eq!(access, vec![missing_method("nope", "res://autoload.gd")]);
}

#[test]
fn an_inferred_member_is_soft_and_draws_nothing() {
    // Godot types all three of these, and warns about none of them: `var x = e` is `INFERRED`,
    // and every check below is gated on a hard type.
    for stmt in [
        "print(h.soft_cast.nope)",
        "print(h.soft_node.nope)",
        "var s: String = h.soft_int\n\tprint(s)",
    ] {
        let (errors, access) = diagnose(stmt);
        assert!(errors.is_empty(), "{stmt}: {errors:?}");
        assert!(access.is_empty(), "{stmt}: {access:?}");
    }
}
