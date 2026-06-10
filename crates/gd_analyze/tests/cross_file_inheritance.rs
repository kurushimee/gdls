//! Cross-file member/inheritance typing tests — the v1.0.1 false-positive families.
//!
//! Interfaces are built by the REAL extractor (`gd_project::extract_interface`) over real source
//! strings, so what the analyzer sees here is byte-identical to production; only the lookup
//! plumbing (`CrossFileQuery`) is mocked.

use std::collections::HashMap;
use std::path::Path;

use gd_analyze::{
    analyze, AnalysisResult, Binding, BindingTargetKind, CrossFileQuery, FoldedValue, Severity,
    StrictSettings, WarnPolicy,
};
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
    )
}

/// A mock workspace: (path, source) pairs run through the real interface extractor; class_names
/// registered from the extracted interfaces themselves.
struct Project {
    ifaces: HashMap<FileId, Interface>,
    by_class_name: HashMap<String, FileId>,
    by_path: HashMap<String, FileId>,
    paths: HashMap<FileId, String>,
}

impl Project {
    fn new(files: &[(&str, &str)]) -> Self {
        let mut ifaces = HashMap::new();
        let mut by_class_name = HashMap::new();
        let mut by_path = HashMap::new();
        let mut paths = HashMap::new();
        for (i, (path, src)) in files.iter().enumerate() {
            let fid = FileId::new(i as u32 + 1);
            let iface = gd_project::extract_interface(&parse(src).tree);
            if let Some(name) = &iface.class_name {
                by_class_name.insert(name.clone(), fid);
            }
            by_path.insert((*path).to_owned(), fid);
            paths.insert(fid, (*path).to_owned());
            ifaces.insert(fid, iface);
        }
        Project {
            ifaces,
            by_class_name,
            by_path,
            paths,
        }
    }

    fn fid(&self, path: &str) -> FileId {
        self.by_path[path]
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

/// Analyze `consumer_path` (which must be one of the project's files) against the project.
fn analyze_file(project: &Project, consumer_path: &str, src: &str) -> AnalysisResult {
    let tree = parse(src).tree;
    let native = native_db();
    analyze(
        &tree,
        Some(project.fid(consumer_path)),
        consumer_path,
        &native,
        project,
        &policy(),
    )
}

fn error_messages(result: &AnalysisResult) -> Vec<String> {
    result
        .diagnostics
        .iter()
        .filter(|d| d.severity() == Severity::Error)
        .map(|d| d.message().to_owned())
        .collect()
}

const CONSTANTS_GD: &str = "\
class_name Constants
const COLORS := {\"red\": 1}
const SETTINGS: Dictionary = {}
enum Mode { A = 5, B }
enum { LOOSE, FREE }
";

const BASE_GD: &str = "\
class_name BaseThing
extends Node
signal ping(x: int)
var hp: int = 10
const SPEED := 10
func boost(amount: int) -> void:
\tpass
";

/// Family A, parts (a)/(b)/(c): a class whose `extends` resolves through another file to a
/// Node root must pass the `$`/`@onready` node-ness gates and must not report inherited members
/// as undeclared. Every line here used to error.
#[test]
fn extends_classname_node_root_clears_dollar_onready_and_inherited_members() {
    let child = "\
extends BaseThing
@onready var lbl = $Label
func _ready() -> void:
\tping.emit(1)
\tboost(SPEED)
\thp += 1
";
    let project = Project::new(&[("res://base.gd", BASE_GD), ("res://child.gd", "")]);
    let result = analyze_file(&project, "res://child.gd", child);
    assert_eq!(error_messages(&result), Vec::<String>::new());
}

/// Two script-to-script hops still reach the native root (Godot propagates native_type through
/// arbitrary chains, analyzer.cpp:617-619).
#[test]
fn two_hop_chain_reaches_node_root() {
    let mid = "class_name MidThing\nextends BaseThing\n";
    let child = "extends MidThing\n@onready var lbl = $Label\n";
    let project = Project::new(&[
        ("res://base.gd", BASE_GD),
        ("res://mid.gd", mid),
        ("res://child.gd", ""),
    ]);
    let result = analyze_file(&project, "res://child.gd", child);
    assert_eq!(error_messages(&result), Vec::<String>::new());
}

/// The fix must not over-correct: a chain that genuinely bottoms out in RefCounted keeps both
/// node-ness errors (faithful — Godot rejects `$` outside Node-derived classes).
#[test]
fn refcounted_root_chain_still_rejects_dollar_and_onready() {
    let plain = "class_name PlainThing\n"; // no extends — implicit RefCounted
    let child = "extends PlainThing\n@onready var lbl = $Label\n";
    let project = Project::new(&[("res://plain.gd", plain), ("res://child.gd", "")]);
    let result = analyze_file(&project, "res://child.gd", child);
    let errors = error_messages(&result);
    assert!(
        errors.iter().any(|m| m.contains("get_node()")),
        "expected the $ error to stay for a RefCounted-rooted chain, got {errors:?}"
    );
    assert!(
        errors
            .iter()
            .any(|m| m.contains("\"@onready\" can only be used")),
        "expected the @onready error to stay for a RefCounted-rooted chain, got {errors:?}"
    );
}

/// An `extends` cycle between two files terminates as an UNKNOWN chain: analysis completes, and
/// node-ness checks stay silent rather than guessing.
#[test]
fn extends_cycle_terminates_permissively() {
    let a = "class_name CycA\nextends CycB\n";
    let b = "class_name CycB\nextends CycA\n";
    let child = "extends CycA\n@onready var lbl = $Label\n";
    let project = Project::new(&[("res://a.gd", a), ("res://b.gd", b), ("res://child.gd", "")]);
    let result = analyze_file(&project, "res://child.gd", child);
    assert_eq!(error_messages(&result), Vec::<String>::new());
}

/// #13: member access through a typed cross-file base records `Binding::Use` against the
/// DECLARING file — what references/definition project for member-access sites.
#[test]
fn member_access_records_use_bindings_against_declaring_file() {
    let consumer = "\
extends RefCounted
func go(b: BaseThing) -> void:
\tb.ping.emit(1)
\tvar _h = b.hp
\tvar _f = b.boost
";
    let project = Project::new(&[("res://base.gd", BASE_GD), ("res://use.gd", "")]);
    let result = analyze_file(&project, "res://use.gd", consumer);
    assert_eq!(error_messages(&result), Vec::<String>::new());

    let base_fid = project.fid("res://base.gd");
    let uses: Vec<(String, BindingTargetKind)> = result
        .bindings()
        .iter()
        .filter_map(|b| match b {
            Binding::Use {
                target_file: Some(f),
                target_kind,
                target_name,
                ..
            } if *f == base_fid => Some((target_name.clone(), *target_kind)),
            _ => None,
        })
        .collect();
    for expected in [
        ("ping", BindingTargetKind::Signal),
        ("hp", BindingTargetKind::Variable),
        ("boost", BindingTargetKind::Function),
    ] {
        assert!(
            uses.iter()
                .any(|(n, k)| n == expected.0 && *k == expected.1),
            "missing Use binding {expected:?}; got {uses:?}"
        );
    }
}

/// Inherited members carry their declared types, not Variant: `hp: int` through the chain.
#[test]
fn inherited_member_types_are_precise() {
    let child = "\
extends BaseThing
func go() -> void:
\tvar _total: int = hp
";
    let project = Project::new(&[("res://base.gd", BASE_GD), ("res://child.gd", "")]);
    let result = analyze_file(&project, "res://child.gd", child);
    assert_eq!(error_messages(&result), Vec::<String>::new());

    let tree = parse(child).tree;
    let mut found = false;
    for id in tree.iter_ids() {
        if let gd_syntax::ast::NodeKind::Identifier(ident) = &tree.get(id).kind {
            if ident.name == "hp" {
                let dt = result.types.get(id);
                if dt.is_set() {
                    assert_eq!(format!("{dt}"), "int", "hp must type as int via the chain");
                    found = true;
                }
            }
        }
    }
    assert!(found, "expected a typed `hp` identifier");
}

/// The `Cannot get property from enum value.` false-positive family: a regular cross-file const
/// must NOT type as an anonymous-enum value, so attribute chains through it stay clean.
#[test]
fn const_member_chains_do_not_error_as_enum_values() {
    let consumer = "\
extends RefCounted
func go() -> void:
\tvar _n = Constants.COLORS.size()
\tvar _r = Constants.COLORS.red
\tvar _s = Constants.SETTINGS.has(\"x\")
";
    let project = Project::new(&[("res://constants.gd", CONSTANTS_GD), ("res://use.gd", "")]);
    let result = analyze_file(&project, "res://use.gd", consumer);
    assert_eq!(error_messages(&result), Vec::<String>::new());
}

/// The gate must not over-correct: a genuine unnamed-enum hoist still types as an enum VALUE, so
/// attribute access on it keeps Godot's legitimate error (analyzer.cpp:4066).
#[test]
fn unnamed_enum_hoist_still_types_as_enum_value() {
    let consumer = "\
extends RefCounted
func go() -> void:
\tvar _ok = Constants.LOOSE
\tvar _bad = Constants.LOOSE.x
";
    let project = Project::new(&[("res://constants.gd", CONSTANTS_GD), ("res://use.gd", "")]);
    let result = analyze_file(&project, "res://use.gd", consumer);
    assert_eq!(
        error_messages(&result),
        vec!["Cannot get property from enum value.".to_owned()]
    );
}

/// Cross-file named-enum values carry their DECLARED integers now (`Mode.A = 5`), not sequential
/// placeholders — pinned through the constant fold of `Constants.Mode.A`.
#[test]
fn cross_file_enum_value_folds_declared_integer() {
    let consumer = "\
extends RefCounted
func go() -> void:
\tvar _a = Constants.Mode.A
\tvar _b = Constants.Mode.B
";
    let project = Project::new(&[("res://constants.gd", CONSTANTS_GD), ("res://use.gd", "")]);
    let result = analyze_file(&project, "res://use.gd", consumer);
    assert_eq!(error_messages(&result), Vec::<String>::new());

    let tree = parse(consumer).tree;
    let mut folds = Vec::new();
    for id in tree.iter_ids() {
        if let gd_syntax::ast::NodeKind::Identifier(ident) = &tree.get(id).kind {
            if ident.name == "A" || ident.name == "B" {
                if let Some(f) = result.folds.get(id) {
                    folds.push((ident.name.clone(), f.clone()));
                }
            }
        }
    }
    assert!(
        folds.contains(&("A".to_owned(), FoldedValue::Int(5))),
        "Mode.A must fold to its declared 5, got {folds:?}"
    );
    assert!(
        folds.contains(&("B".to_owned(), FoldedValue::Int(6))),
        "Mode.B must follow the previous+1 chain to 6, got {folds:?}"
    );
}
