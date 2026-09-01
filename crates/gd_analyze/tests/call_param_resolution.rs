//! #537 — `AnalysisResult::call_param_resolutions`: the parameter slots the analyzer resolved from
//! a cross-file default's shape, recorded under the call's own span.
//!
//! The seam already resolves such a slot for the argument checks (#528). What was missing is a way
//! for a label surface to read the SAME answer back. Deriving it a second time is what made hover
//! and signatureHelp disagree with the diagnostic sitting on the very argument they describe, so
//! the answer is published once, by the pass that computed it.
//!
//! Only HARD slots are recorded. A soft `=` default prints `Variant` in Godot's own arguments hint
//! (`gdscript_editor.cpp:819-824`), so carrying its resolved type to a label would print a name
//! upstream never shows.

use std::collections::HashMap;
use std::path::Path;

use gd_analyze::{analyze, CrossFileQuery, StrictSettings, WarnPolicy};
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

const LIB_GD: &str = "\
class_name ResLib
extends Node

func hard_enum(a := TileSet.TILE_SHAPE_SQUARE) -> void:
\tprint(a)

func soft_enum(a = TileSet.TILE_SHAPE_SQUARE) -> void:
\tprint(a)

func annotated(a: int = 1) -> void:
\tprint(a)

func unreadable(a := no_such_name) -> void:
\tprint(a)

func _init(a := TileSet.TILE_SHAPE_SQUARE) -> void:
\tprint(a)
";

/// Every recorded slot in `src`, flattened to `(index, type name)` and sorted, so a test states the
/// answer without pinning byte spans.
fn slots(src: &str) -> Vec<(usize, String)> {
    let project = Project::new(&[("res://lib.gd", LIB_GD), ("res://main.gd", src)]);
    let tree = parse(src).tree;
    let result = analyze(
        &tree,
        Some(FileId::new(2)),
        "res://main.gd",
        &native_db(),
        &project,
        &policy(),
    );
    let mut out: Vec<(usize, String)> = result
        .call_param_resolutions()
        .values()
        .flatten()
        .map(|(i, dt)| (*i, dt.to_string()))
        .collect();
    out.sort();
    out
}

fn call(stmt: &str) -> String {
    format!("extends Node\n\nfunc go(lib: ResLib) -> void:\n\t{stmt}\n")
}

/// The repro. A hard `:=` default the shallow pass could not decode resolves at the seam, and the
/// resolved type is published under the call.
#[test]
fn a_hard_undecodable_default_is_published_for_the_call() {
    assert_eq!(
        slots(&call("lib.hard_enum()")),
        vec![(0, "TileSet.TileShape".to_owned())]
    );
}

/// A soft `=` default resolves for the argument checks but is NOT published: Godot's arguments hint
/// prints `Variant` for it, so a label that read this back would over-name the slot.
#[test]
fn a_soft_default_is_not_published() {
    assert_eq!(slots(&call("lib.soft_enum()")), Vec::new());
}

/// A written annotation needs no resolution — the declaring interface already carries the type, and
/// the label reads it from there.
#[test]
fn an_annotated_parameter_is_not_published() {
    assert_eq!(slots(&call("lib.annotated()")), Vec::new());
}

/// A default nothing can resolve publishes nothing, so the label surface renders the slot with no
/// type rather than claiming one.
#[test]
fn an_unresolvable_default_publishes_nothing() {
    assert_eq!(slots(&call("lib.unreadable()")), Vec::new());
}

/// `X.new(` runs the `_init` signature through the constructor arm; the slots have to survive that
/// hop, or every constructor popup loses the answer the method popup has.
#[test]
fn a_constructor_call_publishes_its_init_slots() {
    assert_eq!(
        slots("extends Node\n\nfunc go() -> void:\n\tvar r := ResLib.new()\n\tprint(r)\n"),
        vec![(0, "TileSet.TileShape".to_owned())]
    );
}

/// Two calls in one file are two records, keyed by their own spans — a consumer that located one
/// call never reads the other's answer.
#[test]
fn each_call_is_recorded_under_its_own_span() {
    let src = call("lib.hard_enum()\n\tlib.hard_enum()");
    let project = Project::new(&[("res://lib.gd", LIB_GD), ("res://main.gd", &src)]);
    let tree = parse(&src).tree;
    let result = analyze(
        &tree,
        Some(FileId::new(2)),
        "res://main.gd",
        &native_db(),
        &project,
        &policy(),
    );
    assert_eq!(result.call_param_resolutions().len(), 2);
}

const CONST_LIB_GD: &str = "\
class_name ConstLib
extends Node

const HOME := TileSet.TILE_SHAPE_SQUARE

func _init(a := HOME) -> void:
\tprint(a)

func park(a := HOME) -> void:
\tprint(a)
";

fn const_slots(src: &str) -> Vec<(usize, String)> {
    let project = Project::new(&[("res://clib.gd", CONST_LIB_GD), ("res://main.gd", src)]);
    let tree = parse(src).tree;
    let result = analyze(
        &tree,
        Some(FileId::new(2)),
        "res://main.gd",
        &native_db(),
        &project,
        &policy(),
    );
    let mut out: Vec<(usize, String)> = result
        .call_param_resolutions()
        .values()
        .flatten()
        .map(|(i, dt)| (*i, dt.to_string()))
        .collect();
    out.sort();
    out
}

/// The default names a const in the DECLARING file rather than a literal, so the answer only
/// exists once the seam has walked one more hop. This is the shape a real project writes.
#[test]
fn a_default_naming_a_declaring_file_const_resolves() {
    assert_eq!(
        const_slots("extends Node\n\nfunc go(l: ConstLib) -> void:\n\tl.park()\n"),
        vec![(0, "TileSet.TileShape".to_owned())]
    );
}

/// Same const, reached through `_init`.
#[test]
fn a_constructor_resolves_a_declaring_file_const_default() {
    assert_eq!(
        const_slots("extends Node\n\nfunc go() -> void:\n\tvar d := ConstLib.new()\n\tprint(d)\n"),
        vec![(0, "TileSet.TileShape".to_owned())]
    );
}
