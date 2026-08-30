//! #355 — what a `Script`/`Class` `DataType` renders as inside a diagnostic message.
//!
//! Godot's `DataType::to_string()` is total on the value: a CLASS kind carries the parser class it
//! names, so it prints `class_type->identifier->name`, else `class_type->fqcn`
//! (`gdscript_parser.cpp:5354-5358`). gdls substitutes opaque ids for those pointers, so the name
//! rides along on the value instead. These rows are pinned against
//! `godot --headless --check-only` on the equivalent project.

use gd_syntax::Dialect;
use std::collections::HashMap;
use std::path::Path;

use gd_analyze::{analyze, AnalysisResult, CrossFileQuery, Severity, StrictSettings, WarnPolicy};
use gd_project::{FileId, Interface};
use gd_syntax::parse;
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

const LIB1: &str = "class_name Lib1\nextends Node\n";
const SELF_PATH: &str = "res://src/probe.gd";

fn analyze_probe(src: &str) -> AnalysisResult {
    let project = Project::new(&[("res://src/lib1.gd", LIB1), (SELF_PATH, src)]);
    let tree = parse(src).tree;
    analyze(
        &tree,
        Some(project.by_path[SELF_PATH]),
        SELF_PATH,
        &native_db(),
        &project,
        &policy(),
    )
}

fn errors(src: &str) -> Vec<String> {
    analyze_probe(src)
        .diagnostics
        .iter()
        .filter(|d| d.severity() == Severity::Error)
        .map(|d| d.message().to_owned())
        .collect()
}

/// A native class used as a value is `GDScriptNativeClass`, never the class's own name — the one
/// arm where Godot does *not* print `native_type` (`gdscript_parser.cpp:5348-5350`).
#[test]
fn a_native_metatype_renders_as_gdscriptnativeclass() {
    assert_eq!(
        errors("extends Node\nfunc f() -> void:\n\tprint(-Node)\n"),
        vec![
            r#"Invalid operand of type "GDScriptNativeClass" for unary operator "unary-"."#
                .to_owned()
        ]
    );
}

/// A global script class prints the `class_name` it was registered under.
#[test]
fn a_global_class_metatype_renders_as_its_class_name() {
    assert_eq!(
        errors("extends Node\nfunc f() -> void:\n\tprint(-Lib1)\n"),
        vec![r#"Invalid operand of type "Lib1" for unary operator "unary-"."#.to_owned()]
    );
}

/// An inner class prints its declared identifier, not the file that holds it.
#[test]
fn an_inner_class_metatype_renders_as_its_identifier() {
    assert_eq!(
        errors("extends Node\nclass In:\n\tvar x := 1\nfunc f() -> void:\n\tprint(-In)\n"),
        vec![r#"Invalid operand of type "In" for unary operator "unary-"."#.to_owned()]
    );
}

/// A head class with no `class_name` has no identifier, so Godot falls back to the fqcn — the
/// script's `res://` path.
#[test]
fn an_anonymous_head_class_renders_as_its_res_path() {
    assert_eq!(
        errors("extends Node\nfunc f() -> void:\n\tprint(-self)\n"),
        vec![
            r#"Invalid operand of type "res://src/probe.gd" for unary operator "unary-"."#
                .to_owned()
        ]
    );
}

/// The same names inside the unquoted assignment message. The `In` row draws two lines, not one:
/// an inner-class identifier is a *constant* expression (`gdscript_analyzer.cpp:4046`), so the
/// const-narrowing companion fires first. `Node` and `Lib1` are not marked constant, so they draw
/// one line each — see the boundary rows in `is_constant_marking.rs`.
#[test]
fn assignment_messages_carry_the_same_names() {
    assert_eq!(
        errors(concat!(
            "extends Node\n",
            "class In:\n\tvar x := 1\n",
            "func f() -> void:\n",
            "\tvar e: int = Node\n",
            "\tvar g: int = In\n",
            "\tvar h: int = Lib1\n",
            "\tprint(e, g, h)\n",
        )),
        vec![
            r#"Cannot assign a value of type GDScriptNativeClass to variable "e" with specified type int."#.to_owned(),
            r#"Cannot assign a value of type "In" as "int"."#.to_owned(),
            r#"Cannot assign a value of type In to variable "g" with specified type int."#.to_owned(),
            r#"Cannot assign a value of type Lib1 to variable "h" with specified type int."#.to_owned(),
        ]
    );
}

/// A script class nested in a container renders through the same path — the element type is a
/// `DataType` like any other, and the message prints the whole `Array[Lib1]`.
#[test]
fn a_container_element_renders_its_script_name() {
    assert_eq!(
        errors("extends Node\nvar arr: Array[Lib1] = [2]\n"),
        vec![
            r#"Cannot have an element of type "int" in an array of type "Array[Lib1]"."#.to_owned()
        ]
    );
}
