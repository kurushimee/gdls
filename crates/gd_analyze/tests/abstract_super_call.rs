//! #559 — `super.<a>()` where the parent declares `<a>` `@abstract`.
//!
//! `gdscript_analyzer.cpp:3637-3644` runs one check for both halves of the same idea: a `super`
//! call whose resolved method has no body. `METHOD_FLAG_VIRTUAL` gives the "virtual function"
//! wording (a native `_ready`-style hook), `METHOD_FLAG_VIRTUAL_REQUIRED` the "abstract function"
//! one. gdls resolved the abstract case only inside a single file; across a file boundary the
//! call bound to the parent's declaration and said nothing.
//!
//! Every expected row is verbatim output from Godot 4.7.2's editor language server, which — unlike
//! `--check-only` — publishes the whole diagnostic list.

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

/// A mock workspace, built by the REAL interface extractor so the abstract flag crosses the seam
/// exactly as it does in production.
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

fn errors(project: &Project, path: &str, src: &str) -> Vec<String> {
    let tree = parse(src).tree;
    analyze(
        &tree,
        Some(project.by_path[path]),
        path,
        &native_db(),
        project,
        &policy(),
    )
    .diagnostics
    .iter()
    .filter(|d| d.severity() == Severity::Error)
    .map(|d| d.message().to_owned())
    .collect()
}

const BASE: &str = "@abstract\nextends Node\nclass_name WBase\n\n@abstract func step() -> void\n\nfunc done() -> void:\n\tpass\n";

const MSG: &str =
    r#"Cannot call the parent class' abstract function "step()" because it hasn't been defined."#;

/// The reported case: the parent lives in another file.
#[test]
fn a_cross_file_abstract_parent_reports() {
    let child = "extends WBase\n\nfunc step() -> void:\n\tsuper.step()\n";
    let p = Project::new(&[("base.gd", BASE), ("child.gd", child)]);
    assert_eq!(errors(&p, "child.gd", child), vec![MSG.to_owned()]);
}

/// The same through a path `extends`, which resolves the parent by a different road.
#[test]
fn a_path_extends_reaches_the_same_answer() {
    let child = "extends \"base.gd\"\n\nfunc step() -> void:\n\tsuper.step()\n";
    let p = Project::new(&[("base.gd", BASE), ("child.gd", child)]);
    assert_eq!(errors(&p, "child.gd", child), vec![MSG.to_owned()]);
}

/// A parent method with a real body has something to call.
#[test]
fn a_concrete_parent_method_is_silent() {
    let child = "extends WBase\n\nfunc done() -> void:\n\tsuper.done()\n";
    let p = Project::new(&[("base.gd", BASE), ("child.gd", child)]);
    assert_eq!(errors(&p, "child.gd", child), Vec::<String>::new());
}

/// The call has to be a `super` one — this class's own override is a normal call.
#[test]
fn a_plain_call_to_the_override_is_silent() {
    let child = "extends WBase\n\nfunc step() -> void:\n\tpass\n\nfunc go() -> void:\n\tstep()\n";
    let p = Project::new(&[("base.gd", BASE), ("child.gd", child)]);
    assert_eq!(errors(&p, "child.gd", child), Vec::<String>::new());
}

/// Two levels up, with the middle class supplying the body: the walk stops at the first
/// declaration it finds, which is the concrete one.
#[test]
fn a_middle_class_that_defines_it_is_silent() {
    let mid = "extends WBase\nclass_name WMid\n\nfunc step() -> void:\n\tpass\n";
    let leaf = "extends WMid\n\nfunc step() -> void:\n\tsuper.step()\n";
    let p = Project::new(&[("base.gd", BASE), ("mid.gd", mid), ("leaf.gd", leaf)]);
    assert_eq!(errors(&p, "leaf.gd", leaf), Vec::<String>::new());
}

/// The in-file half, which already worked — pinned here so the move to a single check site cannot
/// quietly drop it.
#[test]
fn a_same_file_abstract_parent_still_reports() {
    let src = "extends Node\n\n@abstract class Base:\n\t@abstract func step() -> void\n\nclass Child extends Base:\n\tfunc step() -> void:\n\t\tsuper.step()\n";
    let p = Project::new(&[("t.gd", src)]);
    assert_eq!(errors(&p, "t.gd", src), vec![MSG.to_owned()]);
}

/// A native virtual keeps its own wording — the two arms must not have collapsed into one.
#[test]
fn a_native_virtual_keeps_the_virtual_wording() {
    let src = "extends Node\n\nfunc _ready() -> void:\n\tsuper._ready()\n";
    let p = Project::new(&[("t.gd", src)]);
    assert_eq!(
        errors(&p, "t.gd", src),
        vec![
            r#"Cannot call the parent class' virtual function "_ready()" because it hasn't been defined."#
                .to_owned()
        ]
    );
}
