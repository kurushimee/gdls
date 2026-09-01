//! #539 — an enum value read through its own enum's name.
//!
//! Godot exposes an enum constant two ways: unqualified on the class
//! (`TileSet.TILE_SHAPE_SQUARE`) and qualified through the enum
//! (`TileSet.TileShape.TILE_SHAPE_SQUARE`). `member_type_of`'s native-meta arm answered only the
//! first, so every cross-file shape written the second way lost its type — the argument checks
//! went silent and the labels printed nothing.
//!
//! `make_native_enum_type(.., meta = true)` on the middle segment is what Godot does at
//! `gdscript_analyzer.cpp:4363-4366`; the walk's tail is what keeps the enum ITSELF from being an
//! answer, since `var e := TileSet.TileShape` is "cannot be used on its own" where it is written.
//!
//! Every expected row is verbatim `Godot_v4.7.2-stable --headless --check-only` output.

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
        &StrictSettings {
            enable_warnings: vec!["UNSAFE_CALL_ARGUMENT".to_owned()],
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
class_name QLib
extends Node

enum Grade { WOOD, IRON }

func qualified(a := TileSet.TileShape.TILE_SHAPE_SQUARE) -> void:
\tprint(a)

func unqualified(a := TileSet.TILE_SHAPE_SQUARE) -> void:
\tprint(a)

func qualified_soft(a = TileSet.TileShape.TILE_SHAPE_SQUARE) -> void:
\tprint(a)

func absent_value(a := TileSet.TileShape.NOT_A_VALUE) -> void:
\tprint(a)

func member_off_a_value(a := TileSet.TileShape.TILE_SHAPE_SQUARE.x) -> void:
\tprint(a)

func the_enum_itself(a := TileSet.TileShape) -> void:
\tprint(a)

func own_enum(a := QLib.Grade.IRON) -> void:
\tprint(a)
";

fn warnings_of(stmt: &str) -> Vec<String> {
    let src = format!("extends Node\n\nfunc go(lib: QLib, v: Variant) -> void:\n\t{stmt}\n");
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
    result
        .diagnostics
        .iter()
        .filter(|d| d.code() == "UNSAFE_CALL_ARGUMENT")
        .map(|d| d.message().to_owned())
        .collect()
}

fn unsafe_arg(func: &str, par: &str) -> String {
    format!(
        r#"The argument 1 of the function "{func}()" requires the subtype "{par}" but the supertype "Variant" was provided."#
    )
}

fn errors_of(stmt: &str) -> Vec<String> {
    let src = format!("extends Node\n\nfunc go(lib: QLib, s: String) -> void:\n\t{stmt}\n");
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
    result
        .diagnostics
        .iter()
        .filter(|d| d.severity() == gd_analyze::Severity::Error)
        .map(|d| d.message().to_owned())
        .collect()
}

/// The issue's repro:
///
/// ```text
/// Parse Error: Invalid argument for "qualified()" function: argument 1 should be
/// "TileSet.TileShape" but is "String".
/// ```
#[test]
fn a_qualified_enum_value_resolves() {
    assert_eq!(
        warnings_of("lib.qualified(v)"),
        vec![unsafe_arg("qualified", "TileSet.TileShape")]
    );
}

/// Both spellings are one type. If the two routes ever mint different shapes, a project that mixes
/// them gets two different answers for one enum.
#[test]
fn both_spellings_give_the_same_type() {
    assert_eq!(
        warnings_of("lib.unqualified(v)"),
        warnings_of("lib.qualified(v)")
            .iter()
            .map(|m| m.replace("qualified", "unqualified"))
            .collect::<Vec<_>>()
    );
}

/// Hardness comes from the writing, not from the walk: the `=` twin resolves the same type and
/// still takes an incompatible hard argument without the error arm.
#[test]
fn the_soft_twin_stays_soft() {
    assert_eq!(
        warnings_of("lib.qualified_soft(v)"),
        vec![unsafe_arg("qualified_soft", "TileSet.TileShape")]
    );
    assert_eq!(errors_of("lib.qualified_soft(s)"), Vec::<String>::new());
}

/// The hard twin does arm it.
#[test]
fn the_hard_twin_errors_on_an_incompatible_argument() {
    assert_eq!(
        errors_of("lib.qualified(s)"),
        vec![
            r#"Invalid argument for "qualified()" function: argument 1 should be "TileSet.TileShape" but is "String"."#
                .to_owned()
        ]
    );
}

/// The enum itself is a pseudo-type — "Type ... cannot be used on its own" where it is written —
/// so it still carries nothing. The walk resolves THROUGH it; it is never the answer.
#[test]
fn the_enum_itself_still_carries_nothing() {
    assert_eq!(warnings_of("lib.the_enum_itself(v)"), Vec::<String>::new());
}

/// A name the enum does not declare answers nothing rather than the enum's own type.
#[test]
fn an_absent_value_carries_nothing() {
    assert_eq!(warnings_of("lib.absent_value(v)"), Vec::<String>::new());
}

/// A member read off the resolved VALUE is not something the seam models, so the walk stops rather
/// than guessing.
#[test]
fn a_member_off_the_value_carries_nothing() {
    assert_eq!(
        warnings_of("lib.member_off_a_value(v)"),
        Vec::<String>::new()
    );
}

/// A SCRIPT enum meta is a real dictionary value in Godot, not a pseudo-type, so the new tail
/// refusal must not catch it — `QLib.Grade.IRON` keeps resolving.
#[test]
fn a_script_enums_qualified_value_still_resolves() {
    assert_eq!(
        warnings_of("lib.own_enum(v)"),
        vec![unsafe_arg("own_enum", "QLib.Grade")]
    );
}
