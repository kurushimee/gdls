//! #445 — what a cross-file member's type is when its initializer is a chain.
//!
//! `InitShape` used to be flat: one optional dotted path and nothing under it, so `OS.get_temp_dir()`
//! was recordable but `OS.get_temp_dir().path_join("x")` was not. Every chain longer than one link
//! read as `Variant` from another file, which silences the access on the member and everything
//! downstream of it — the single largest source of gdls-only `UNSAFE_*` rows on a real project.
//!
//! The shapes here are the ones the shallow pass can decode without an analyzer: a name, a call
//! through a name, and either of those read off the result of the previous link. Anything whose
//! answer depends on an argument or an index is still refused whole, because a wrong cross-file
//! type is worse than none.
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
}

const DEP_GD: &str = "\
class_name Dep445
extends RefCounted

var tag := \"t\"

static func dmake() -> Dep445:
\treturn Dep445.new()
";

const HOLDER_GD: &str = "\
class_name Holder445
extends Node

var joined := OS.get_temp_dir().path_join(\"x\")
var shouted := OS.get_temp_dir().to_upper()
var dir := DirAccess.open(\"res://\")
var exists := DirAccess.open(\"res://\").dir_exists(\"x\")
var chained := Dep445.dmake().tag
var preload_chain := preload(\"res://dep.gd\").new().tag
var instance_off_meta := Node.get_parent()
var through_index := rows[0].compute()
var through_value := tag.nothing().here()
";

fn diagnose(stmt: &str) -> (Vec<String>, Vec<String>) {
    let consumer = format!("extends Node\n\nfunc go(h: Holder445) -> void:\n\t{stmt}\n");
    let project = Project::new(&[
        ("res://dep.gd", DEP_GD),
        ("res://holder.gd", HOLDER_GD),
        ("res://main.gd", &consumer),
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

fn missing_method(name: &str, ty: &str) -> String {
    format!(
        "The method \"{name}()\" is not present on the inferred type \"{ty}\" \
         (but may be present on a subtype)."
    )
}

/// A builtin has no subtypes, so a miss on one is a hard pair, not an `UNSAFE_*` warning.
fn builtin_miss(name: &str, ty: &str) -> Vec<String> {
    vec![
        format!("Cannot find member \"{name}\" in base \"{ty}\"."),
        format!("Function \"{name}()\" not found in base {ty}."),
    ]
}

#[test]
fn a_builtin_method_called_on_a_singletons_result_types_the_member() {
    let (errors, access) = diagnose("print(h.joined.to_upper())");
    assert!(errors.is_empty(), "{errors:?}");
    assert!(access.is_empty(), "{access:?}");
    let (errors, _) = diagnose("print(h.joined.nope())");
    assert_eq!(errors, builtin_miss("nope", "String"));
}

#[test]
fn the_chain_keeps_going_past_the_second_link() {
    let (errors, access) = diagnose("print(h.shouted.length())");
    assert!(errors.is_empty(), "{errors:?}");
    assert!(access.is_empty(), "{access:?}");
    let (errors, _) = diagnose("print(h.shouted.nope())");
    assert_eq!(errors, builtin_miss("nope", "String"));
}

#[test]
fn a_static_call_on_a_native_class_types_the_member() {
    // `DirAccess.open` is static, so the bare class name is a legal base for it.
    let (errors, access) = diagnose("print(h.dir.get_files())");
    assert!(errors.is_empty(), "{errors:?}");
    assert!(access.is_empty(), "{access:?}");
    // An object type could still be a subtype at runtime, so the miss is the warning, not the pair.
    let (errors, access) = diagnose("print(h.dir.nope())");
    assert!(errors.is_empty(), "{errors:?}");
    assert_eq!(access, vec![missing_method("nope", "DirAccess")]);
}

#[test]
fn a_method_called_on_a_static_calls_result_types_the_member() {
    let (errors, _) = diagnose("print(h.exists.nope())");
    assert_eq!(errors, builtin_miss("nope", "bool"));
}

#[test]
fn a_member_read_off_a_script_statics_result_types_the_member() {
    let (errors, access) = diagnose("print(h.chained.to_upper())");
    assert!(errors.is_empty(), "{errors:?}");
    assert!(access.is_empty(), "{access:?}");
    let (errors, _) = diagnose("print(h.chained.nope())");
    assert_eq!(errors, builtin_miss("nope", "String"));
}

#[test]
fn a_member_read_off_a_preloaded_constructor_types_the_member() {
    let (errors, _) = diagnose("print(h.preload_chain.nope())");
    assert_eq!(errors, builtin_miss("nope", "String"));
}

#[test]
fn an_instance_method_off_a_bare_class_name_is_refused() {
    // `Node.get_parent()` needs an instance the bare class name does not carry, so the initializer
    // names nothing the reader may trust. The member stays `Variant`, which is the one type that
    // claims nothing — Godot reports the same shape on it, having refused the initializer itself.
    let (errors, access) = diagnose("print(h.instance_off_meta.nope())");
    assert!(errors.is_empty(), "{errors:?}");
    assert_eq!(access, vec![missing_method("nope", "Variant")]);
}

#[test]
fn a_chain_with_more_than_one_reading_is_still_refused_whole() {
    // An index, and a call through a value rather than a name: neither has a single reading the
    // shallow pass can decode, and nesting does not change that.
    for stmt in [
        "print(h.through_index.nope())",
        "print(h.through_value.nope())",
    ] {
        let (errors, access) = diagnose(stmt);
        assert!(errors.is_empty(), "{stmt}: {errors:?}");
        assert_eq!(access, vec![missing_method("nope", "Variant")], "{stmt}");
    }
}
