//! `Class "X" hides a …` — all four arms (#361).
//!
//! `resolve_class_inheritance` reports when a class's own name shadows something already in scope
//! (`gdscript_analyzer.cpp:396-407`). gdls shipped the builtin and native arms; the global-script-
//! class and autoload-singleton arms waited on the self-exclusion and the autoload table, and both
//! now exist.
//!
//! The self-exclusion is where the care goes: the head class of `foo.gd` declaring `class_name Foo`
//! is what put `Foo` in the registry, so it must not report itself, while an INNER class named
//! `Foo` in that same file still does. Every row is pinned against
//! `Godot_v4.7.2-stable --headless --check-only`.

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

/// A mock workspace with a `class_name` registry and an autoload name table.
struct Project {
    ifaces: HashMap<FileId, Interface>,
    by_class_name: HashMap<String, FileId>,
    by_path: HashMap<String, FileId>,
    paths: HashMap<FileId, String>,
    autoloads: Vec<String>,
}

impl Project {
    fn new(files: &[(&str, &str)], autoloads: &[&str]) -> Self {
        let mut p = Project {
            ifaces: HashMap::new(),
            by_class_name: HashMap::new(),
            by_path: HashMap::new(),
            paths: HashMap::new(),
            autoloads: autoloads.iter().map(|s| (*s).to_owned()).collect(),
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
    fn is_autoload(&self, name: &str) -> bool {
        self.autoloads.iter().any(|a| a == name)
    }
}

/// Errors from the file at index `which` of `files`, analyzed against the whole mock project.
fn errors_in(files: &[(&str, &str)], autoloads: &[&str], which: usize) -> Vec<String> {
    let project = Project::new(files, autoloads);
    let (path, src) = files[which];
    analyze(
        &parse(src).tree,
        Some(FileId::new(which as u32 + 1)),
        path,
        &native_db(),
        &project,
        &policy(),
    )
    .diagnostics
    .iter()
    .filter(|d| d.severity() == Severity::Error)
    .map(|d| d.message().to_owned())
    .collect()
}

const OTHER: &str = "class_name Inner\nextends Resource\n";

/// An inner class named after a `class_name` REGISTERED IN ANOTHER FILE reports. This is the shape
/// that makes `extends Outer.Inner` ambiguous in the first place.
#[test]
fn an_inner_class_shadowing_another_files_class_name_reports() {
    let outer = "class_name Outer\nextends Node\n\nclass Inner:\n\textends Node\n";
    let errs = errors_in(
        &[("res://other.gd", OTHER), ("res://outer.gd", outer)],
        &[],
        1,
    );
    assert!(
        errs.iter()
            .any(|e| e == r#"Class "Inner" hides a global script class."#),
        "got {errs:?}"
    );
}

/// The self-exclusion: a file's OWN head `class_name` is what registered the name, so it must not
/// report itself.
#[test]
fn a_files_own_head_class_name_does_not_report_itself() {
    let errs = errors_in(&[("res://other.gd", OTHER)], &[], 0);
    assert!(
        !errs
            .iter()
            .any(|e| e.contains("hides a global script class")),
        "the head class registered the name; got {errs:?}"
    );
}

/// … but an INNER class in that same file, sharing the head's name, still reports — Godot's
/// condition is "a different path OR not the head class", so the second half fires here.
#[test]
fn an_inner_class_sharing_its_own_files_class_name_reports() {
    let src = "class_name SelfName\nextends Node\n\nclass SelfName:\n\textends Node\n";
    let errs = errors_in(&[("res://self_name.gd", src)], &[], 0);
    assert!(
        errs.iter()
            .any(|e| e == r#"Class "SelfName" hides a global script class."#),
        "got {errs:?}"
    );
}

/// The autoload arm.
#[test]
fn a_class_shadowing_an_autoload_singleton_reports() {
    let src = "extends Node\n\nclass SettingsGlobal:\n\textends Node\n";
    let errs = errors_in(&[("res://main.gd", src)], &["SettingsGlobal"], 0);
    assert!(
        errs.iter()
            .any(|e| e == r#"Class "SettingsGlobal" hides an autoload singleton."#),
        "got {errs:?}"
    );
}

/// Godot's chain reports the FIRST matching arm only, so a name that is both a native class and a
/// registered `class_name` still reads as the native one.
#[test]
fn the_first_matching_arm_wins() {
    let shadow = "class_name Node2D\nextends Resource\n";
    let src = "extends Node\n\nclass Node2D:\n\textends Node\n";
    let errs = errors_in(
        &[("res://shadow.gd", shadow), ("res://main.gd", src)],
        &[],
        1,
    );
    assert!(
        errs.iter()
            .any(|e| e == r#"Class "Node2D" hides a native class."#),
        "got {errs:?}"
    );
    assert!(
        !errs
            .iter()
            .any(|e| e.contains("hides a global script class")),
        "only the first arm reports; got {errs:?}"
    );
}

/// A class name that shadows nothing stays silent, autoload table or not.
#[test]
fn an_unshadowed_class_name_is_silent() {
    let src = "extends Node\n\nclass Helper:\n\textends Node\n";
    let errs = errors_in(&[("res://main.gd", src)], &["SettingsGlobal"], 0);
    assert!(!errs.iter().any(|e| e.contains("hides")), "got {errs:?}");
}
