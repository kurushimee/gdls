//! #448 — a property miss on a class whose extends chain crosses a file.
//!
//! `UNSAFE_PROPERTY_ACCESS` is a claim about a base's whole member surface, so gdls only makes it
//! where it walked that surface end to end. The old gate read a file hop as a hole in the walk,
//! which suppressed the warning for every project class that does not extend a native class
//! directly — most of them, in a real codebase. The gate is now the walk itself
//! (`base_is_introspectable`) plus `Exact` provenance, the same pairing #433 landed on the method
//! side, so a complete cross-file chain proves a miss exactly as a same-file one does and an
//! incomplete one still says nothing.
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
            enable_warnings: vec!["UNSAFE_PROPERTY_ACCESS".to_owned()],
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

const ROOT_GD: &str = "\
class_name Anim448
extends RefCounted

var animatable_properties := {}
";

/// The consumer is always the third file, so it is the one that gets analyzed.
fn diagnose(files: &[(&str, &str)], consumer: &str) -> (Vec<String>, Vec<String>) {
    let mut all: Vec<(&str, &str)> = files.to_vec();
    all.push(("res://main.gd", consumer));
    let project = Project::new(&all);
    let tree = parse(consumer).tree;
    let result = analyze(
        &tree,
        Some(FileId::new(all.len() as u32)),
        "res://main.gd",
        &native_db(),
        &project,
        &policy(),
    );
    let errors = result
        .diagnostics
        .iter()
        .filter(|d| d.severity() == Severity::Error && d.warning_code().is_none())
        .map(|d| d.message().to_owned())
        .collect();
    let access = result
        .diagnostics
        .iter()
        .filter(|d| d.code() == "UNSAFE_PROPERTY_ACCESS")
        .map(|d| d.message().to_owned())
        .collect();
    (errors, access)
}

fn missing_prop(name: &str, ty: &str) -> String {
    format!(
        "The property \"{name}\" is not present on the inferred type \"{ty}\" \
         (but may be present on a subtype)."
    )
}

#[test]
fn a_miss_on_a_class_extending_a_cross_file_class_warns() {
    let consumer = "\
class_name Base448
extends Anim448

var parent: Base448

func f() -> void:
\tprint(parent.expanded)
";
    let (errors, access) = diagnose(&[("res://anim.gd", ROOT_GD)], consumer);
    assert!(errors.is_empty(), "{errors:?}");
    assert_eq!(access, vec![missing_prop("expanded", "Base448")]);
}

#[test]
fn a_member_the_cross_file_base_declares_stays_silent() {
    // The warning has to come from a walk that actually reached the other file, not from a
    // same-file-only view that would miss the inherited name too.
    let consumer = "\
class_name Base448
extends Anim448

var parent: Base448

func f() -> void:
\tprint(parent.animatable_properties)
";
    let (errors, access) = diagnose(&[("res://anim.gd", ROOT_GD)], consumer);
    assert!(errors.is_empty(), "{errors:?}");
    assert!(access.is_empty(), "{access:?}");
}

#[test]
fn a_three_link_chain_still_proves_the_miss() {
    let mid = "\
class_name Mid448
extends Anim448

var mid_prop := 1
";
    let consumer = "\
class_name Base448
extends Mid448

var parent: Base448

func f() -> void:
\tprint(parent.mid_prop)
\tprint(parent.expanded)
";
    let (errors, access) = diagnose(
        &[("res://anim.gd", ROOT_GD), ("res://mid.gd", mid)],
        consumer,
    );
    assert!(errors.is_empty(), "{errors:?}");
    assert_eq!(access, vec![missing_prop("expanded", "Base448")]);
}

#[test]
fn a_chain_whose_head_is_not_in_the_workspace_still_says_nothing() {
    // `Absent448` is nowhere, so the walk never reached a native root and the member surface is
    // unknown. Silence is the only honest answer, and this is the case the old blanket gate was
    // really written for.
    let consumer = "\
class_name Base448
extends Absent448

var parent: Base448

func f() -> void:
\tprint(parent.expanded)
";
    let (_, access) = diagnose(&[("res://anim.gd", ROOT_GD)], consumer);
    assert!(access.is_empty(), "{access:?}");
}

#[test]
fn a_chain_through_a_file_that_did_not_parse_says_nothing() {
    // A parse error's recovery may simply have dropped the member being read, so an interface
    // that is not `parse_clean` proves nothing — the same rule the self-call miss follows.
    let broken = "\
class_name Anim448
extends RefCounted

func (((
";
    let consumer = "\
class_name Base448
extends Anim448

var parent: Base448

func f() -> void:
\tprint(parent.expanded)
";
    let (_, access) = diagnose(&[("res://anim.gd", broken)], consumer);
    assert!(access.is_empty(), "{access:?}");
}

#[test]
fn an_extends_cycle_says_nothing() {
    let a = "\
class_name CycA448
extends CycB448
";
    let b = "\
class_name CycB448
extends CycA448
";
    let consumer = "\
extends Node

func f(x: CycA448) -> void:
\tprint(x.expanded)
";
    let (_, access) = diagnose(&[("res://a.gd", a), ("res://b.gd", b)], consumer);
    assert!(access.is_empty(), "{access:?}");
}
