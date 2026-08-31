//! `@export_node_path`'s class arguments (`gdscript_parser.cpp:4720-4731`) — #371.
//!
//! Each argument names a class a path is allowed to point at. Godot checks that the class exists,
//! is exposed, and inherits `Node`, resolving a `class_name` through the global-class registry to
//! its native base first.
//!
//! Every row is pinned against `Godot_v4.7.2-stable --headless --check-only`, run in a project
//! whose `.godot/global_script_class_cache.cfg` was generated first — without it `ScriptServer`
//! knows no global classes and every `class_name` argument reports as missing.
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
        &StrictSettings::default(),
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

const NODE_CLS: &str = "\
class_name MyNodeCls
extends Node2D
";

const RES_CLS: &str = "\
class_name MyResCls
extends Resource
";

/// A `class_name` whose own base chain leads nowhere gdls can follow — the fail-open case.
const ORPHAN_CLS: &str = "\
class_name MyOrphanCls
extends SomeClassNobodyIndexed
";

fn errors_with(db: NativeDb, consumer: &str) -> Vec<String> {
    let project = Project::new(&[
        ("res://node_cls.gd", NODE_CLS),
        ("res://res_cls.gd", RES_CLS),
        ("res://orphan_cls.gd", ORPHAN_CLS),
        ("res://main.gd", consumer),
    ]);
    let tree = parse(consumer).tree;
    let result = analyze(
        &tree,
        Some(FileId::new(4)),
        "res://main.gd",
        &db,
        &project,
        &policy(),
    );
    result
        .diagnostics
        .iter()
        .filter(|d| d.severity() == Severity::Error)
        .map(|d| d.message().to_owned())
        .collect()
}

fn errors(consumer: &str) -> Vec<String> {
    errors_with(native_db(), consumer)
}

fn not_found(n: usize, cls: &str) -> String {
    format!(
        r#"Invalid argument {n} of annotation "@export_node_path": The class "{cls}" was not found in the global scope."#
    )
}

fn not_a_node(n: usize, cls: &str) -> String {
    format!(
        r#"Invalid argument {n} of annotation "@export_node_path": The class "{cls}" does not inherit "Node"."#
    )
}

/// A name that is neither a native class nor a project `class_name`. The second row is the
/// argument index: upstream reports the position, and returns on the first bad one.
#[test]
fn an_unknown_class_is_reported() {
    assert_eq!(
        errors("extends Node\n@export_node_path(\"Nope\") var p: NodePath\n"),
        vec![not_found(1, "Nope")]
    );
    assert_eq!(
        errors("extends Node\n@export_node_path(\"Node2D\", \"Nope\") var p: NodePath\n"),
        vec![not_found(2, "Nope")]
    );
}

/// A class that exists but is not a `Node`, reached both directly and through a `class_name`.
#[test]
fn a_non_node_class_is_reported() {
    assert_eq!(
        errors("extends Node\n@export_node_path(\"Resource\") var p: NodePath\n"),
        vec![not_a_node(1, "Resource")]
    );
    assert_eq!(
        errors("extends Node\n@export_node_path(\"MyResCls\") var p: NodePath\n"),
        vec![not_a_node(1, "MyResCls")]
    );
}

/// A native `Node` and a project `class_name` extending one both pass.
#[test]
fn a_node_class_is_accepted() {
    for src in [
        "extends Node\n@export_node_path(\"Node2D\") var p: NodePath\n",
        "extends Node\n@export_node_path(\"MyNodeCls\") var p: NodePath\n",
        "extends Node\n@export_node_path(\"Node\", \"Node2D\") var p: NodePath\n",
        "extends Node\n@export_node_path var p: NodePath\n",
    ] {
        assert_eq!(errors(src), Vec::<String>::new(), "{src}");
    }
}

/// fail-open: a `class_name` gdls could not walk to a native root is unknown, not absent. Reading
/// it as absent would report every project class whose base is a file the index has not reached.
#[test]
fn an_unwalkable_class_name_is_not_reported() {
    assert_eq!(
        errors("extends Node\n@export_node_path(\"MyOrphanCls\") var p: NodePath\n"),
        Vec::<String>::new()
    );
}

/// fail-open: "was not found" is a negative claim, so it needs an API dump that came from the
/// project's own engine. A stock dump proves what exists, never what does not.
#[test]
fn a_non_exact_dump_makes_no_absence_claim() {
    let src = "extends Node\n@export_node_path(\"Nope\") var p: NodePath\n";
    let mut generic = native_db();
    generic.set_provenance(gd_types::ApiProvenance::Generic);
    assert_eq!(errors_with(generic, src), Vec::<String>::new());
    assert_eq!(errors_with(NativeDb::empty(), src), Vec::<String>::new());
    // The claim IS made under an exact dump — otherwise the two rows above prove nothing.
    assert_eq!(errors(src), vec![not_found(1, "Nope")]);
}
