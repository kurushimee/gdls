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

/// Family A, part (d): `self` (and any instance of a class extending a cross-file base) is
/// compatible with both the named base class and its native root — Godot's source decomposition
/// walks `base_type.class_type`/`get_base_script()` chains (analyzer.cpp:6210-6296). Both calls
/// used to error `Invalid argument ... but is "<Class>".`.
#[test]
fn self_compat_through_cross_file_chain() {
    let child = "\
extends BaseThing
func take(b: BaseThing) -> void:
\tpass
func need_node(n: Node) -> void:
\tpass
func go() -> void:
\ttake(self)
\tneed_node(self)
";
    let project = Project::new(&[("res://base.gd", BASE_GD), ("res://child.gd", "")]);
    let result = analyze_file(&project, "res://child.gd", child);
    assert_eq!(error_messages(&result), Vec::<String>::new());
}

/// The decomposition must not over-correct: an unrelated class (complete chain, no hit) still
/// fails argument compatibility.
#[test]
fn unrelated_class_argument_still_errors() {
    let child = "\
extends BaseThing
func take(b: BaseThing) -> void:
\tpass
func go(c: Constants) -> void:
\ttake(c)
";
    let project = Project::new(&[
        ("res://base.gd", BASE_GD),
        ("res://constants.gd", CONSTANTS_GD),
        ("res://child.gd", ""),
    ]);
    let result = analyze_file(&project, "res://child.gd", child);
    let errors = error_messages(&result);
    assert!(
        errors
            .iter()
            .any(|m| m.contains(r#"Invalid argument for "take()""#)),
        "expected the arg-compat error for an unrelated class, got {errors:?}"
    );
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

/// The sweep-driven v1.0.1 batch, pinned as one zero-diagnostics fixture: every shape here
/// errored on a real OSS project (Pixelorama @ stock 4.6.3) after the first fix wave. Kitchen
/// sink by design — the gate that proved them is the full-project sweep
/// (`scripts/m6-acceptance/scan_diags.py`), and this is its in-repo distillation.
#[test]
fn sweep_batch_shapes_publish_no_errors() {
    let base = "\
class_name SweepBase
extends Node
enum Modes { PASS = 5, NORM }
var map := SweepMap.new()
var tint := Color.BLUE
var flag := false
func mirror_array(arr: Array[Vector2i], cb := 1) -> Array[Vector2i]:
\treturn arr
";
    let map_gd = "\
class_name SweepMap
extends RefCounted
func get_nearest(pos: Vector2i) -> Vector2i:
\treturn pos
";
    let sub = "class_name SweepSub\nextends SweepBase\n";
    let consumer = "\
extends SweepBase

const Sibling := preload(\"sib.gd\")


func modes(d: Dictionary) -> SweepBase.Modes:
\tvar _t := d[\"type\"] as Variant.Type
\treturn SweepBase.Modes.PASS


func go(pos: Vector2i, others: Array[SweepBase]) -> void:
\tvar left := pos.x
\tvar start_x := left
\tvar near := map.get_nearest(pos)
\tvar _offset := near
\tvar _color := tint if flag else Color.WHITE
\tfor point in mirror_array([pos]):
\t\tvar _draw_point := point
\tfor o in others:
\t\tif is_ancestor_of(o):
\t\t\tpass
\tvar _sib = Sibling
\tmatch pos:
\t\tVector2i(-1, -1):
\t\t\tpass
\t\tVector2i.ZERO:
\t\t\tpass
\tprint(start_x)
";
    let sib = "extends RefCounted\n";
    let project = Project::new(&[
        ("res://base.gd", base),
        ("res://map.gd", map_gd),
        ("res://sub.gd", sub),
        ("res://sib.gd", sib),
        ("res://use.gd", ""),
    ]);
    let result = analyze_file(&project, "res://use.gd", consumer);
    assert_eq!(error_messages(&result), Vec::<String>::new());
}

/// `self is OwnSubclassInAnotherFile` — tested from the BASE file's own analysis (Pixelorama's
/// `Guide.gd: if self is SymmetryGuide`): the subclass's chain passes through THIS file, which
/// the Class-target bridge resolves; the reverse direction (sibling vs sibling) must keep
/// erroring — Godot rejects provably-disjoint `is` tests.
#[test]
fn self_is_own_subclass_through_chain() {
    let base_src = "\
class_name BridgeBase
extends Node
func check() -> void:
\tif self is BridgeSub:
\t\tpass
";
    let sub = "class_name BridgeSub\nextends BridgeBase\n";
    let project = Project::new(&[
        ("res://bridge_base.gd", base_src),
        ("res://bridge_sub.gd", sub),
    ]);
    let result = analyze_file(&project, "res://bridge_base.gd", base_src);
    assert_eq!(error_messages(&result), Vec::<String>::new());
}

/// v1.0.4 (#32 companion): a native method missing from every chain INTERFACE still binds its
/// real signature through the chain's native root — upstream's get_function_signature continues
/// into ClassDB after the script walk. Discriminating assert: `get_class()` (declared on the
/// trimmed fixture's `Object` root, returns String) assigned to an `int` must produce the
/// assignment-mismatch error; the pre-fix silent-Variant degrade produced nothing.
#[test]
fn chain_interface_miss_binds_native_method_signature() {
    let child = "\
extends BaseThing
func f() -> void:
\tvar n: int = get_class()
\tprint_debug(n)
";
    let project = Project::new(&[("res://base.gd", BASE_GD), ("res://child.gd", "")]);
    let result = analyze_file(&project, "res://child.gd", child);
    let errors = error_messages(&result);
    assert!(
        errors.iter().any(|m| m.contains("Cannot assign")),
        "String-returning native method through the chain must type-check the assignment; got {errors:?}"
    );
}

/// The single lambda in `src`, by `NodeId`, for #141 `use_self` assertions. Panics unless exactly
/// one lambda is present (parse is deterministic, so this id matches the analyzed tree).
fn sole_lambda(src: &str) -> gd_syntax::ast::NodeId {
    let tree = parse(src).tree;
    let lambdas: Vec<_> = tree
        .iter_ids()
        .filter(|&id| matches!(tree.get(id).kind, gd_syntax::ast::NodeKind::Lambda(_)))
        .collect();
    assert_eq!(
        lambdas.len(),
        1,
        "expected exactly one lambda; got {}",
        lambdas.len()
    );
    lambdas[0]
}

/// #141 cross-file arm: a lambda reading an INHERITED instance VARIABLE through a cross-file
/// Script base marks `use_self` (Godot's VARIABLE arm, analyzer.cpp:4428 — reached for inherited
/// members via the script_classes walk).
#[test]
fn lambda_reading_cross_file_inherited_var_marks_use_self() {
    let child = "\
extends BaseThing
func f():
\tvar g = func(): return hp
";
    let project = Project::new(&[("res://base.gd", BASE_GD), ("res://child.gd", "")]);
    let result = analyze_file(&project, "res://child.gd", child);
    assert!(
        result.lambda_uses_self(sole_lambda(child)),
        "a lambda reading an inherited instance var (hp) through a cross-file base must mark use_self (#141)"
    );
}

/// #141 cross-file arm — the over-mark guard: a lambda reading an INHERITED CONSTANT through a
/// cross-file Script base must NOT mark `use_self`. Godot's CONSTANT arm (analyzer.cpp:4344-4352)
/// has no mark site, so the implicit-self script-chain resolution must gate the mark on member
/// kind. (Reproduce-first: the unconditional mark on any script-chain hit failed this.)
#[test]
fn lambda_reading_cross_file_inherited_const_is_not_marked() {
    let child = "\
extends BaseThing
func f():
\tvar g = func(): return SPEED
";
    let project = Project::new(&[("res://base.gd", BASE_GD), ("res://child.gd", "")]);
    let result = analyze_file(&project, "res://child.gd", child);
    assert!(
        !result.lambda_uses_self(sole_lambda(child)),
        "a lambda reading an inherited CONSTANT (SPEED) through a cross-file base must NOT mark use_self (#141)"
    );
}

/// #141 cross-file arm: a lambda referencing an INHERITED SIGNAL through a cross-file Script base
/// marks `use_self` (Godot's SIGNAL arm, analyzer.cpp:4425 via the MEMBER_SIGNAL fallthrough).
/// Pins the `Signal` leg of the implicit-self kind-gate, alongside the `Variable` positive above.
#[test]
fn lambda_reading_cross_file_inherited_signal_marks_use_self() {
    let child = "\
extends BaseThing
func f():
\tvar g = func(): return ping
";
    let project = Project::new(&[("res://base.gd", BASE_GD), ("res://child.gd", "")]);
    let result = analyze_file(&project, "res://child.gd", child);
    assert!(
        result.lambda_uses_self(sole_lambda(child)),
        "a lambda referencing an inherited signal (ping) through a cross-file base must mark use_self (#141)"
    );
}

/// #173 cross-file arity: a VARARG script method inherited through a cross-file base must NOT
/// arity-error when over-supplied. The interface extractor now carries the rest-parameter flag
/// (`MemberFlags::is_vararg`), so `script_chain_call` reports `is_vararg = true` and the too-many
/// check is suppressed — Godot stamps `METHOD_FLAG_VARARG` on script functions
/// (gdscript_analyzer.cpp:5866-5868), so the call is valid.
#[test]
fn cross_file_vararg_method_over_supply_is_silent() {
    let base = "\
class_name VarargBase
extends Node
func emit_many(first, ...rest) -> void:
\tpass
";
    let child = "\
extends VarargBase
func _ready() -> void:
\temit_many(1, 2, 3, 4)
";
    let project = Project::new(&[("res://varargbase.gd", base), ("res://child.gd", "")]);
    let result = analyze_file(&project, "res://child.gd", child);
    let errors = error_messages(&result);
    assert!(
        !errors.iter().any(|m| m.contains("Too many arguments")),
        "a cross-file vararg method over-supplied must not arity-error; got {errors:?}"
    );
}

/// #173 cross-file arity, positive control: a NON-vararg script method inherited through a
/// cross-file base IS arity-checked. `required()` takes exactly one param; supplying three is too
/// many — proving the cross-file count check is genuinely live (not vacuously silent).
#[test]
fn cross_file_fixed_arity_method_over_supply_fires() {
    let base = "\
class_name FixedBase
extends Node
func required(only) -> void:
\tpass
";
    let child = "\
extends FixedBase
func _ready() -> void:
\trequired(1, 2, 3)
";
    let project = Project::new(&[("res://fixedbase.gd", base), ("res://child.gd", "")]);
    let result = analyze_file(&project, "res://child.gd", child);
    let errors = error_messages(&result);
    assert!(
        errors.iter().any(|m| m
            == "Too many arguments for \"required()\" call. Expected at most 1 but received 3."),
        "a cross-file fixed-arity method over-supplied must arity-error; got {errors:?}"
    );
}

// === #216: cross-file Script `_init` constructor arity ==========================================
//
// `X.new(...)` where `X` is a cross-file `DtKind::Script` base (a `preload`-constant or a
// `class_name`-resolved script) must arity-check its `_init` exactly as Godot's
// `get_function_signature(p_is_constructor=true)` does (gdscript_analyzer.cpp:5829-5903 →
// validate_call_arg :5944-5950). #208 shipped the in-file Class + native cases and excluded
// Script; these pin the cross-file Script case. Verified against the real Godot 4.6.3 binary:
// a no-`_init` script over-called emits `Expected at most 0`; a parameterized `_init`
// under/over-called emits the real bounds; the function name in the message is `new`.

/// Parameterized cross-file `_init`, under-supplied → "Too few arguments". A `preload`-constant
/// base typed `DtKind::Script`.
#[test]
fn cross_file_init_too_few_arguments_fires() {
    let dep = "\
class_name Init216Few
extends RefCounted
func _init(a, b):
\tpass
";
    let consumer = "\
extends RefCounted
const Dep = preload(\"res://dep.gd\")
func go() -> void:
\tvar _x = Dep.new(1)
";
    let project = Project::new(&[("res://dep.gd", dep), ("res://use.gd", "")]);
    let result = analyze_file(&project, "res://use.gd", consumer);
    let errors = error_messages(&result);
    assert!(
        errors
            .iter()
            .any(|m| m
                == "Too few arguments for \"new()\" call. Expected at least 2 but received 1."),
        "cross-file parameterized _init under-call must arity-error; got {errors:?}"
    );
}

/// Parameterized cross-file `_init`, over-supplied → "Too many arguments".
#[test]
fn cross_file_init_too_many_arguments_fires() {
    let dep = "\
class_name Init216Many
extends RefCounted
func _init(a, b):
\tpass
";
    let consumer = "\
extends RefCounted
const Dep = preload(\"res://dep.gd\")
func go() -> void:
\tvar _x = Dep.new(1, 2, 3)
";
    let project = Project::new(&[("res://dep.gd", dep), ("res://use.gd", "")]);
    let result = analyze_file(&project, "res://use.gd", consumer);
    let errors = error_messages(&result);
    assert!(
        errors
            .iter()
            .any(|m| m
                == "Too many arguments for \"new()\" call. Expected at most 2 but received 3."),
        "cross-file parameterized _init over-call must arity-error; got {errors:?}"
    );
}

/// Parameterized cross-file `_init` called with the right arity → SILENT.
#[test]
fn cross_file_init_correct_arity_is_silent() {
    let dep = "\
class_name Init216Ok
extends RefCounted
func _init(a, b):
\tpass
";
    let consumer = "\
extends RefCounted
const Dep = preload(\"res://dep.gd\")
func go() -> void:
\tvar _x = Dep.new(1, 2)
";
    let project = Project::new(&[("res://dep.gd", dep), ("res://use.gd", "")]);
    let result = analyze_file(&project, "res://use.gd", consumer);
    let errors = error_messages(&result);
    assert!(
        !errors.iter().any(|m| m.contains("arguments for")),
        "cross-file correct _init arity must be silent; got {errors:?}"
    );
}

/// A cross-file script with NO `_init` over-called → "Expected at most 0" (Godot's empty-par_types
/// constructor fallback, gdscript_analyzer.cpp:5897-5903). Verified against the real 4.6.3 binary.
#[test]
fn cross_file_no_init_over_call_fires_expected_at_most_zero() {
    let dep = "\
class_name Init216None
extends RefCounted
func hello() -> void:
\tpass
";
    let consumer = "\
extends RefCounted
const Dep = preload(\"res://dep.gd\")
func go() -> void:
\tvar _x = Dep.new(1, 2, 3)
";
    let project = Project::new(&[("res://dep.gd", dep), ("res://use.gd", "")]);
    let result = analyze_file(&project, "res://use.gd", consumer);
    let errors = error_messages(&result);
    assert!(
        errors
            .iter()
            .any(|m| m
                == "Too many arguments for \"new()\" call. Expected at most 0 but received 3."),
        "cross-file no-_init over-call must emit Expected at most 0; got {errors:?}"
    );
}

/// A cross-file script with NO `_init` constructed with zero args → SILENT (the common valid case).
#[test]
fn cross_file_no_init_zero_args_is_silent() {
    let dep = "\
class_name Init216NoneOk
extends RefCounted
func hello() -> void:
\tpass
";
    let consumer = "\
extends RefCounted
const Dep = preload(\"res://dep.gd\")
func go() -> void:
\tvar _x = Dep.new()
";
    let project = Project::new(&[("res://dep.gd", dep), ("res://use.gd", "")]);
    let result = analyze_file(&project, "res://use.gd", consumer);
    let errors = error_messages(&result);
    assert!(
        !errors.iter().any(|m| m.contains("arguments for")),
        "cross-file no-_init zero-arg construction must be silent; got {errors:?}"
    );
}

/// A cross-file `_init` with a defaulted param, called with only the required args → SILENT.
#[test]
fn cross_file_init_defaulted_params_required_only_is_silent() {
    let dep = "\
class_name Init216Default
extends RefCounted
func _init(a, b = 2):
\tpass
";
    let consumer = "\
extends RefCounted
const Dep = preload(\"res://dep.gd\")
func go() -> void:
\tvar _x = Dep.new(1)
";
    let project = Project::new(&[("res://dep.gd", dep), ("res://use.gd", "")]);
    let result = analyze_file(&project, "res://use.gd", consumer);
    let errors = error_messages(&result);
    assert!(
        !errors.iter().any(|m| m.contains("arguments for")),
        "cross-file defaulted _init called with required args must be silent; got {errors:?}"
    );
}

/// A cross-file `class_name`-resolved Script base (not a preload-constant) is arity-checked too.
#[test]
fn cross_file_init_via_class_name_fires() {
    let dep = "\
class_name Init216Named
extends RefCounted
func _init(a, b):
\tpass
";
    let consumer = "\
extends RefCounted
func go() -> void:
\tvar _x = Init216Named.new(1, 2, 3)
";
    let project = Project::new(&[("res://dep.gd", dep), ("res://use.gd", "")]);
    let result = analyze_file(&project, "res://use.gd", consumer);
    let errors = error_messages(&result);
    assert!(
        errors
            .iter()
            .any(|m| m
                == "Too many arguments for \"new()\" call. Expected at most 2 but received 3."),
        "class_name-resolved Script _init over-call must arity-error; got {errors:?}"
    );
}

/// A leaf script that declares NO `_init` but extends a cross-file base that DOES: the chain walk
/// must resolve the inherited `_init` arity and fire on a mis-arity `Leaf.new(...)`. This is the
/// transitive case `script_chain_call` exists for (a flat single-interface lookup of the leaf would
/// miss it). Godot walks the same ClassNode base chain (gdscript_analyzer.cpp:5837-5851); verified
/// against the real 4.6.3 binary (leaf with inherited-only `_init`, under-called → "Too few...").
#[test]
fn cross_file_inherited_init_from_base_fires() {
    let base = "\
class_name Init216Base
extends RefCounted
func _init(a, b):
\tpass
";
    let leaf = "\
class_name Init216Leaf
extends Init216Base
func leaf_only() -> void:
\tpass
";
    let consumer = "\
extends RefCounted
const Leaf = preload(\"res://leaf.gd\")
func go() -> void:
\tvar _x = Leaf.new(1)
";
    let project = Project::new(&[
        ("res://base.gd", base),
        ("res://leaf.gd", leaf),
        ("res://use.gd", ""),
    ]);
    let result = analyze_file(&project, "res://use.gd", consumer);
    let errors = error_messages(&result);
    assert!(
        errors
            .iter()
            .any(|m| m
                == "Too few arguments for \"new()\" call. Expected at least 2 but received 1."),
        "inherited cross-file _init under-call must arity-error through the chain; got {errors:?}"
    );
}

/// FAIL-CLOSED: an UNRESOLVED base (the dependency file is not in the project index) must stay
/// SILENT — gdls must never manufacture "Expected at most 0" for a `_init` it cannot prove absent.
#[test]
fn cross_file_unresolved_base_stays_silent() {
    // `Missing216` is referenced but NOT registered in the project, so its interface never
    // resolves. The base degrades to Variant; the constructor arity check must not fire.
    let consumer = "\
extends RefCounted
const Dep = preload(\"res://missing.gd\")
func go() -> void:
\tvar _x = Dep.new(1, 2, 3)
";
    let project = Project::new(&[("res://use.gd", "")]);
    let result = analyze_file(&project, "res://use.gd", consumer);
    let errors = error_messages(&result);
    assert!(
        !errors.iter().any(|m| m.contains("arguments for")),
        "unresolved cross-file base must not arity-error (fail-closed); got {errors:?}"
    );
}

/// FAIL-CLOSED: a base that declares `_init` as a NON-Func member (a `const`/`var`, degenerate)
/// must leave the constructor count check OFF — gdls never arity-checks against a non-callable.
#[test]
fn cross_file_init_non_func_member_stays_silent() {
    let dep = "\
class_name Init216NonFunc
extends RefCounted
const _init = 1
";
    let consumer = "\
extends RefCounted
const Dep = preload(\"res://dep.gd\")
func go() -> void:
\tvar _x = Dep.new(1, 2, 3)
";
    let project = Project::new(&[("res://dep.gd", dep), ("res://use.gd", "")]);
    let result = analyze_file(&project, "res://use.gd", consumer);
    let errors = error_messages(&result);
    assert!(
        !errors.iter().any(|m| m.contains("arguments for")),
        "a non-Func _init member must not trigger the constructor arity check; got {errors:?}"
    );
}
