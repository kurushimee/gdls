//! Cross-file member/inheritance typing tests — the v1.0.1 false-positive families.
//!
//! Interfaces are built by the REAL extractor (`gd_project::extract_interface`) over real source
//! strings, so what the analyzer sees here is byte-identical to production; only the lookup
//! plumbing (`CrossFileQuery`) is mocked.

use gd_syntax::Dialect;
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
        Dialect::DEFAULT,
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
/// (`scripts/acceptance/scan_diags.py`), and this is its in-repo distillation.
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

// === #212: inner class accessed via a preloaded-const identifier =================================
//
// `Lib.Box.new()` where `const Lib = preload("res://lib.gd")` and `lib.gd` declares an inner
// `class Box` must resolve: `Lib.Box` is the inner class `Box` of the preloaded script (a Script
// meta type with `inner = ["Box"]`), `Lib.Box.new()` yields a `Box` instance, and `b.field`
// resolves to `Box.field`. Pre-fix the reducer's `lookup_script_chain_member` had no inner-class
// arm, so `Lib.Box` returned `Unresolved` and `b` degraded to Variant — `references`/`rename`/
// hover/completion on `b.field` saw nothing. Godot's analyzer resolves the inner class through the
// preloaded constant's class chain (gdscript_analyzer.cpp constant/subscript member walk).

/// `Lib.Box.new()` types `b` as the inner `Box` instance, and `b.field` records a `Binding::Use`
/// against `lib.gd` with the inner class path `["Box"]`.
#[test]
fn inner_class_via_preload_const_resolves() {
    let lib = "\
extends Node
var field := 1
class Box:
\tvar field := 2
";
    let consumer = "\
extends Node
const Lib = preload(\"res://lib.gd\")
func run() -> void:
\tvar b := Lib.Box.new()
\tb.field = 5
";
    let project = Project::new(&[("res://lib.gd", lib), ("res://use.gd", "")]);
    let result = analyze_file(&project, "res://use.gd", consumer);
    assert_eq!(error_messages(&result), Vec::<String>::new());

    // `b.field` must type as int (the inner `Box.field`, not Variant).
    let tree = parse(consumer).tree;
    let mut field_typed_int = false;
    for id in tree.iter_ids() {
        if let gd_syntax::ast::NodeKind::Identifier(ident) = &tree.get(id).kind {
            if ident.name == "field" {
                let dt = result.types.get(id);
                if dt.is_set() && format!("{dt}") == "int" {
                    field_typed_int = true;
                }
            }
        }
    }
    assert!(
        field_typed_int,
        "b.field must type as int (the inner Box.field) via Lib.Box.new()"
    );

    // A `Binding::Use` for `field` must target lib.gd with the inner class path ["Box"].
    let lib_fid = project.fid("res://lib.gd");
    let field_uses: Vec<&Binding> = result
        .bindings()
        .iter()
        .filter(|b| {
            matches!(
                b,
                Binding::Use {
                    target_file: Some(f),
                    target_name,
                    ..
                } if *f == lib_fid && target_name == "field"
            )
        })
        .collect();
    assert!(
        field_uses.iter().any(|b| matches!(
            b,
            Binding::Use { target_class_path, .. } if target_class_path == &["Box".to_owned()]
        )),
        "b.field must record a Use against lib.gd inner class [\"Box\"]; got {field_uses:?}"
    );
}

// ============================================================================
// #256 — member misses on a SCRIPT base
// ============================================================================

/// Policy with the named ignore-by-default codes turned on.
fn policy_enabling(names: &[&str]) -> WarnPolicy {
    let strict = StrictSettings {
        enable_warnings: names.iter().map(|s| s.to_string()).collect(),
        ..Default::default()
    };
    WarnPolicy::build(
        &gd_project::WarningConfig::default(),
        &strict,
        Dialect::DEFAULT,
    )
}

fn analyze_file_with(
    project: &Project,
    consumer_path: &str,
    src: &str,
    policy: &WarnPolicy,
) -> AnalysisResult {
    let tree = parse(src).tree;
    let native = native_db();
    analyze(
        &tree,
        Some(project.fid(consumer_path)),
        consumer_path,
        &native,
        project,
        policy,
    )
}

fn warning_messages(result: &AnalysisResult) -> Vec<String> {
    result
        .diagnostics
        .iter()
        .filter(|d| d.severity() != Severity::Error)
        .map(|d| d.message().to_owned())
        .collect()
}

/// #256, finishing #123's stated acceptance: a method miss on a `class_name` INSTANCE warns like
/// the same miss on a native base does. It stays a warning, never an error — the interface view of
/// another file is a shallow extract, so a gap in it must never become `Function not found`.
///
/// The base name is the script's `class_name`, not the `<Script #1>` placeholder `Display` renders
/// (Godot's `DataType::to_string()` for SCRIPT, gdscript_parser.cpp:5321-5329).
#[test]
fn script_base_method_miss_warns_unsafe_method_access() {
    let consumer = "\
extends Node
func go() -> void:
\tvar d := BaseThing.new()
\td.bogus()
";
    let project = Project::new(&[("res://base.gd", BASE_GD), ("res://c.gd", "")]);
    let policy = policy_enabling(&["UNSAFE_METHOD_ACCESS"]);
    let result = analyze_file_with(&project, "res://c.gd", consumer, &policy);
    assert_eq!(error_messages(&result), Vec::<String>::new());
    assert_eq!(
        warning_messages(&result),
        vec![r#"The method "bogus()" is not present on the inferred type "BaseThing" (but may be present on a subtype)."#.to_string()]
    );
}

/// The property half of the same arm.
#[test]
fn script_base_property_miss_warns_unsafe_property_access() {
    let consumer = "\
extends Node
func go() -> void:
\tvar d := BaseThing.new()
\tprint(d.bogus_prop)
";
    let project = Project::new(&[("res://base.gd", BASE_GD), ("res://c.gd", "")]);
    let policy = policy_enabling(&["UNSAFE_PROPERTY_ACCESS"]);
    let result = analyze_file_with(&project, "res://c.gd", consumer, &policy);
    assert_eq!(error_messages(&result), Vec::<String>::new());
    assert_eq!(
        warning_messages(&result),
        vec![r#"The property "bogus_prop" is not present on the inferred type "BaseThing" (but may be present on a subtype)."#.to_string()]
    );
}

/// FP guard — the risk class. Every member the chain really carries stays silent: the class's own
/// `var`/`const`/`signal`/`func`, and the members it inherits from its NATIVE root.
#[test]
fn real_script_chain_members_stay_silent() {
    let consumer = "\
extends Node
func go() -> void:
\tvar d := BaseThing.new()
\tprint(d.hp)
\tprint(d.SPEED)
\td.boost(1)
\td.ping.emit(1)
\td.queue_free()
";
    let project = Project::new(&[("res://base.gd", BASE_GD), ("res://c.gd", "")]);
    let policy = policy_enabling(&["UNSAFE_PROPERTY_ACCESS", "UNSAFE_METHOD_ACCESS"]);
    let result = analyze_file_with(&project, "res://c.gd", consumer, &policy);
    assert_eq!(error_messages(&result), Vec::<String>::new());
    assert_eq!(warning_messages(&result), Vec::<String>::new());
}

/// FP guard: a member declared on a script the base itself extends resolves through the chain.
#[test]
fn inherited_script_chain_members_stay_silent() {
    let mid = "class_name MidThing\nextends BaseThing\nvar extra := 1\n";
    let consumer = "\
extends Node
func go() -> void:
\tvar d := MidThing.new()
\tprint(d.extra)
\tprint(d.hp)
\td.boost(1)
";
    let project = Project::new(&[
        ("res://base.gd", BASE_GD),
        ("res://mid.gd", mid),
        ("res://c.gd", ""),
    ]);
    let policy = policy_enabling(&["UNSAFE_PROPERTY_ACCESS", "UNSAFE_METHOD_ACCESS"]);
    let result = analyze_file_with(&project, "res://c.gd", consumer, &policy);
    assert_eq!(error_messages(&result), Vec::<String>::new());
    assert_eq!(warning_messages(&result), Vec::<String>::new());
}

/// FP guard: a chain whose root gdls cannot reach was never fully walked, so a miss in it proves
/// nothing — silent regardless of policy.
#[test]
fn unwalkable_script_chain_stays_silent() {
    let orphan = "class_name Orphan\nextends SomethingNobodyDeclares\n";
    let consumer = "\
extends Node
func go() -> void:
\tvar d := Orphan.new()
\td.bogus()
\tprint(d.bogus_prop)
";
    let project = Project::new(&[("res://orphan.gd", orphan), ("res://c.gd", "")]);
    let policy = policy_enabling(&["UNSAFE_PROPERTY_ACCESS", "UNSAFE_METHOD_ACCESS"]);
    let result = analyze_file_with(&project, "res://c.gd", consumer, &policy);
    assert_eq!(warning_messages(&result), Vec::<String>::new());
}

/// Godot's policy contract: both codes are ignore-by-default, so the DEFAULT policy says nothing
/// about either miss.
#[test]
fn script_base_misses_are_silent_under_the_default_policy() {
    let consumer = "\
extends Node
func go() -> void:
\tvar d := BaseThing.new()
\td.bogus()
\tprint(d.bogus_prop)
";
    let project = Project::new(&[("res://base.gd", BASE_GD), ("res://c.gd", "")]);
    let result = analyze_file(&project, "res://c.gd", consumer);
    assert_eq!(error_messages(&result), Vec::<String>::new());
    assert_eq!(warning_messages(&result), Vec::<String>::new());
}

/// A method miss emits the METHOD warning only — the callee's attribute reduction must not also
/// fire the PROPERTY one for the same name.
#[test]
fn script_base_method_miss_does_not_double_report_as_a_property() {
    let consumer = "\
extends Node
func go() -> void:
\tvar d := BaseThing.new()
\td.bogus()
";
    let project = Project::new(&[("res://base.gd", BASE_GD), ("res://c.gd", "")]);
    let policy = policy_enabling(&["UNSAFE_PROPERTY_ACCESS", "UNSAFE_METHOD_ACCESS"]);
    let result = analyze_file_with(&project, "res://c.gd", consumer, &policy);
    assert_eq!(warning_messages(&result).len(), 1, "exactly one warning");
}

const BASE_WITH_INNER_GD: &str = "\
class_name Holder
extends RefCounted
class Inner:
\tvar x: int = 0
";

/// #284: an inner class inherited from a cross-file base, named bare in a type annotation, must
/// resolve to that INNER class — not to the base script. Resolving it to the base made the
/// declared type read as the base while the initializer reduced to the inner class, so
/// `var v: Inner = Inner.new()` failed its assignment check.
#[test]
fn inherited_inner_class_as_bare_annotation_types_as_the_inner_class() {
    let child = "\
extends Holder
func go() -> void:
\tvar v: Inner = Inner.new()
\tprint(v.x)
";
    let project = Project::new(&[
        ("res://holder.gd", BASE_WITH_INNER_GD),
        ("res://child.gd", ""),
    ]);
    let result = analyze_file(&project, "res://child.gd", child);
    assert_eq!(error_messages(&result), Vec::<String>::new());

    let tree = parse(child).tree;
    let mut found = false;
    for id in tree.iter_ids() {
        if let gd_syntax::ast::NodeKind::Variable(_) = &tree.get(id).kind {
            let dt = result.types.get(id);
            if dt.is_set() {
                assert!(
                    format!("{dt}").ends_with(".Inner"),
                    "`v` must type as the inner class, got `{dt}`"
                );
                found = true;
            }
        }
    }
    assert!(found, "expected a typed `v` variable");
}

const ENUM_HOLDER_GD: &str = "\
extends RefCounted
class_name Holder2
enum EId { A = 0, B }
class Inner:
\tvar identifier: EId
\tfunc _init(_identifier: EId):
\t\tidentifier = _identifier
func go(list: Array[EnumUser]) -> void:
\tfor data: EnumUser in list:
\t\tvar _v: Inner = Inner.new(data.identifier)
";

const ENUM_USER_GD: &str = "\
extends RefCounted
class_name EnumUser
var identifier: Holder2.EId
";

/// #286: one enum reached two ways must carry one identity. The in-file side names it after the
/// declaring class's fqcn, which `class_name` overrides, so the cross-file side has to do the
/// same — deriving it from the file path gave `Holder2.EId` and `holder2.gd.EId` for the same
/// enum, and the argument check rejected the pair.
#[test]
fn cross_file_enum_identity_matches_the_in_file_one() {
    let project = Project::new(&[
        ("res://holder2.gd", ENUM_HOLDER_GD),
        ("res://enum_user.gd", ENUM_USER_GD),
    ]);
    let result = analyze_file(&project, "res://holder2.gd", ENUM_HOLDER_GD);
    assert_eq!(error_messages(&result), Vec::<String>::new());
}

// ---------------------------------------------------------------------------------------------
// #299: a member miss on a META script base (`ClassName.X`) is a provable negative.
// ---------------------------------------------------------------------------------------------

const META_OWNER_GD: &str = "\
class_name MetaOwner
extends Node
enum Slot { WEAPON, ARMOR }
const MAX_SLOTS := 32
class Entry:
\tvar count: int = 0
var hp: int = 3
func boost() -> void:
\tpass
";

/// Godot 4.7.2 on this exact shape:
///   Cannot find member "NoSuchInner" in base "MetaOwner".
///   Cannot find member "NO_SUCH_CONST" in base "MetaOwner".
/// gdls degraded every Script-branch miss to a silent Variant, so neither fired.
#[test]
fn meta_script_base_member_miss_is_an_error() {
    let project = Project::new(&[("res://owner.gd", META_OWNER_GD), ("res://user.gd", "")]);
    let src = "\
extends Node
func a() -> void:
\tprint(MetaOwner.NO_SUCH_CONST)
func b() -> void:
\tvar x = MetaOwner.NoSuchInner.new()
\tprint(x)
";
    let result = analyze_file(&project, "res://user.gd", src);
    let errs = error_messages(&result);
    assert!(
        errs.iter()
            .any(|m| m == r#"Cannot find member "NO_SUCH_CONST" in base "MetaOwner"."#),
        "missing the const miss; got {errs:?}"
    );
    assert!(
        errs.iter()
            .any(|m| m == r#"Cannot find member "NoSuchInner" in base "MetaOwner"."#),
        "missing the inner-class miss; got {errs:?}"
    );
}

/// Every real member of the same class must stay silent — the negative may only fire on a name
/// the chain genuinely does not carry.
#[test]
fn meta_script_base_real_members_stay_clean() {
    let project = Project::new(&[("res://owner.gd", META_OWNER_GD), ("res://user.gd", "")]);
    let src = "\
extends Node
func a() -> void:
\tprint(MetaOwner.MAX_SLOTS)
\tprint(MetaOwner.Slot)
\tprint(MetaOwner.Slot.WEAPON)
\tvar e = MetaOwner.Entry.new()
\tprint(e)
\tvar o = MetaOwner.new()
\tprint(o)
";
    let result = analyze_file(&project, "res://user.gd", src);
    assert!(
        error_messages(&result).is_empty(),
        "real members must not error; got {:?}",
        error_messages(&result)
    );
}

/// A member inherited from the chain's own script base, and one from the chain's NATIVE root,
/// must both count as present.
#[test]
fn meta_script_base_inherited_members_stay_clean() {
    let project = Project::new(&[
        ("res://base.gd", BASE_GD),
        (
            "res://derived.gd",
            "class_name DerivedThing\nextends BaseThing\nconst OWN := 1\n",
        ),
        ("res://user.gd", ""),
    ]);
    let src = "\
extends Node
func a() -> void:
\tprint(DerivedThing.OWN)
\tprint(DerivedThing.SPEED)
";
    let result = analyze_file(&project, "res://user.gd", src);
    assert!(
        error_messages(&result).is_empty(),
        "inherited members must not error; got {:?}",
        error_messages(&result)
    );
}

/// The soundness gate: when the chain cannot be fully walked the miss is gdls's view, not the
/// user's code, so it must stay silent. `Mystery` extends a class no interface is registered for.
#[test]
fn meta_script_base_miss_stays_silent_when_the_chain_is_unresolvable() {
    let project = Project::new(&[
        (
            "res://mystery.gd",
            "class_name Mystery\nextends SomeUnknownForkClass\nconst OWN := 1\n",
        ),
        ("res://user.gd", ""),
    ]);
    let src = "\
extends Node
func a() -> void:
\tprint(Mystery.NO_SUCH_CONST)
";
    let result = analyze_file(&project, "res://user.gd", src);
    assert!(
        error_messages(&result).is_empty(),
        "an unresolvable chain must stay permissive; got {:?}",
        error_messages(&result)
    );
}

/// An INSTANCE base keeps Godot's dynamic-lookup semantics: no error, only the existing
/// UNSAFE_PROPERTY_ACCESS warning (#256). The new arm must not leak onto this path.
#[test]
fn instance_script_base_member_miss_stays_a_warning() {
    let project = Project::new(&[("res://owner.gd", META_OWNER_GD), ("res://user.gd", "")]);
    let src = "\
extends Node
func a() -> void:
\tvar o := MetaOwner.new()
\tprint(o.no_such_property)
";
    let result = analyze_file(&project, "res://user.gd", src);
    assert!(
        error_messages(&result).is_empty(),
        "an instance miss must not be an error; got {:?}",
        error_messages(&result)
    );
}

/// The type-annotation half of #299. Godot 4.7.2:
///   Could not find type "NoSuchType" under base "MetaOwner".
#[test]
fn qualified_type_annotation_miss_is_an_error() {
    let project = Project::new(&[("res://owner.gd", META_OWNER_GD), ("res://user.gd", "")]);
    let src = "\
extends Node
func a() -> void:
\tvar y: MetaOwner.NoSuchType = null
\tprint(y)
";
    let result = analyze_file(&project, "res://user.gd", src);
    let errs = error_messages(&result);
    assert!(
        errs.iter()
            .any(|m| m == r#"Could not find type "NoSuchType" under base "MetaOwner"."#),
        "got {errs:?}"
    );
}

/// Every real nested type must keep resolving — including an enum and an inner class reached
/// through an inner class, and both inherited from a script base.
#[test]
fn qualified_type_annotation_real_nested_types_stay_clean() {
    let project = Project::new(&[
        (
            "res://nest.gd",
            "class_name Nest\nextends Node\nenum Top { A }\nclass Mid:\n\tenum Deep { B }\n\tclass Leaf:\n\t\tvar v: int = 0\n",
        ),
        (
            "res://nestchild.gd",
            "class_name NestChild\nextends Nest\nconst OWN := 1\n",
        ),
        ("res://user.gd", ""),
    ]);
    let src = "\
extends Node
func a() -> void:
\tvar t: Nest.Top = Nest.Top.A
\tvar m: Nest.Mid = null
\tvar d: Nest.Mid.Deep = Nest.Mid.Deep.B
\tvar l: Nest.Mid.Leaf = null
\tvar i: NestChild.Top = NestChild.Top.A
\tprint(t, m, d, l, i)
";
    let result = analyze_file(&project, "res://user.gd", src);
    assert!(
        error_messages(&result).is_empty(),
        "real nested types must not error; got {:?}",
        error_messages(&result)
    );
}

/// The same soundness gate as the member arm: an unresolvable chain stays permissive.
#[test]
fn qualified_type_annotation_miss_stays_silent_when_the_chain_is_unresolvable() {
    let project = Project::new(&[
        (
            "res://mystery.gd",
            "class_name Mystery\nextends SomeUnknownForkClass\nconst OWN := 1\n",
        ),
        ("res://user.gd", ""),
    ]);
    let src = "\
extends Node
func a() -> void:
\tvar y: Mystery.NoSuchType = null
\tprint(y)
";
    let result = analyze_file(&project, "res://user.gd", src);
    assert!(
        error_messages(&result).is_empty(),
        "an unresolvable chain must stay permissive; got {:?}",
        error_messages(&result)
    );
}

/// A companion to `qualified_type_annotation_real_nested_types_stay_clean`, which on its own
/// cannot tell "resolved" from "silently degraded to Variant" — both produce zero errors. Here an
/// int is assigned to each nested type, so the annotation MUST have resolved for the assignment
/// check to reject it. Guards against the miss-is-an-error arm being reached by a real type.
#[test]
fn qualified_type_annotations_actually_resolve_not_just_degrade() {
    let project = Project::new(&[
        (
            "res://nest.gd",
            "class_name Nest\nextends Node\nenum Top { A }\nclass Mid:\n\tenum Deep { B }\n\tclass Leaf:\n\t\tvar v: int = 0\n",
        ),
        (
            "res://nestchild.gd",
            "class_name NestChild\nextends Nest\nconst OWN := 1\n",
        ),
        ("res://user.gd", ""),
    ]);
    for (annotation, label) in [
        ("Nest.Mid", "head inner class"),
        ("Nest.Mid.Leaf", "inner class under an inner class"),
        ("NestChild.Mid", "inner class inherited from a script base"),
    ] {
        let src =
            format!("extends Node\nfunc a() -> void:\n\tvar x: {annotation} = 1\n\tprint(x)\n");
        let result = analyze_file(&project, "res://user.gd", &src);
        let errs = error_messages(&result);
        assert!(
            errs.iter().any(|m| m.contains("Cannot assign")),
            "{label} (`{annotation}`) did not resolve — an int assignment was accepted; got {errs:?}"
        );
        assert!(
            !errs.iter().any(|m| m.contains("Could not find type")),
            "{label} (`{annotation}`) wrongly reported as absent; got {errs:?}"
        );
    }
    // Enum leaves: assigning an int to an enum-typed var is the same discriminator.
    for (annotation, label) in [
        ("Nest.Top", "head enum"),
        ("Nest.Mid.Deep", "enum under an inner class"),
        ("NestChild.Top", "enum inherited from a script base"),
    ] {
        let src =
            format!("extends Node\nfunc a() -> void:\n\tvar x: {annotation} = \"s\"\n\tprint(x)\n");
        let result = analyze_file(&project, "res://user.gd", &src);
        let errs = error_messages(&result);
        assert!(
            errs.iter().any(|m| m.contains("Cannot assign")),
            "{label} (`{annotation}`) did not resolve — a String assignment was accepted; got {errs:?}"
        );
    }
}

// --- #314: the cross-file lexical scope, not just the inheritance chain -------------------------
// Godot's `get_class_node_current_scope_classes` (analyzer.cpp:320-344) walks each class's base
// AND its outer class, transitively. gdls walked only the base chain past the file boundary, so a
// constant declared on a cross-file base's *enclosing* class read as undeclared. Shape lifted from
// upstream's own `analyzer/features/lookup_class.gd`.

const OUTER_SCOPE_GD: &str = "\
class A:
	const TARGET := \"wrong\"

	class B:
		const TARGET := \"wrong\"
		const WAITING := \"godot\"

		class D extends C:
			pass

class C:
	const TARGET := \"right\"

class E extends A.B.D:
	pass
";

#[test]
fn cross_file_base_enclosing_class_constant_resolves() {
    let project = Project::new(&[
        ("res://outer_scope.gd", OUTER_SCOPE_GD),
        ("res://user.gd", ""),
    ]);
    // `WAITING` lives on `A.B`, which is nowhere in `E`'s inheritance (`E` → `A.B.D` → `C`) —
    // only in `A.B.D`'s outer chain.
    let src = "\
const External := preload(\"res://outer_scope.gd\")

class Mine extends External.E:
	func a() -> void:
		print(TARGET)
		print(WAITING)
";
    let errs = error_messages(&analyze_file(&project, "res://user.gd", src));
    assert!(
        errs.is_empty(),
        "a constant on a cross-file base's enclosing class must resolve; got {errs:?}"
    );
}

#[test]
fn cross_file_base_enclosing_class_does_not_leak_into_qualified_access() {
    let project = Project::new(&[
        ("res://outer_scope.gd", OUTER_SCOPE_GD),
        ("res://user.gd", ""),
    ]);
    // The scope walk is for BARE identifiers only. `External.E` exposes what `E` inherits, and
    // `WAITING` is not one of those — Godot's `reduce_identifier_from_base` never consults the
    // outer chain. Widening the qualified lookup too would invent a member.
    let src = "\
const External := preload(\"res://outer_scope.gd\")

func a() -> void:
	print(External.E.WAITING)
";
    let errs = error_messages(&analyze_file(&project, "res://user.gd", src));
    assert!(
        errs.iter()
            .any(|m| m.contains("WAITING") && m.contains("Cannot find member")),
        "a qualified `Base.member` access must NOT reach the base's enclosing class; got {errs:?}"
    );
}

/// Same shape as [`OUTER_SCOPE_GD`], with the three `TARGET` declarations given different types
/// so a diagnostic can tell which one won.
const SHADOW_SCOPE_GD: &str = "\
class A:
	const TARGET := 1.5

	class B:
		const TARGET := \"str\"

		class D extends C:
			pass

class C:
	const TARGET := 7

class E extends A.B.D:
	pass
";

#[test]
fn cross_file_scope_walk_prefers_the_base_over_the_outer() {
    let project = Project::new(&[
        ("res://shadow_scope.gd", SHADOW_SCOPE_GD),
        ("res://user.gd", ""),
    ]);
    // `TARGET` is declared three times: `float` on `A`, `String` on `A.B`, `int` on `C`.
    // `D extends C`, so the base wins over both outers — analyzer.cpp:332-343 recurses into the
    // base BEFORE the outer, and the walk dedups, so whichever it reaches first is the answer.
    let src = "\
const External := preload(\"res://shadow_scope.gd\")

class Mine extends External.E:
	func a() -> void:
		var x: int = TARGET
		print(x)
";
    let errs = error_messages(&analyze_file(&project, "res://user.gd", src));
    assert!(
        errs.is_empty(),
        "base-declared TARGET (int) must shadow both outer-declared ones; got {errs:?}"
    );
}

/// A `class_name` head in a type annotation is upstream's `CLASS` arm reached through gdls's
/// interface walk (#338). It owes the same two-message split the in-file inner-class arm makes:
/// a constant resolves on the meta base and comes back a value (analyzer.cpp:918), while an
/// instance variable or a signal is not on a meta base at all (:915). Enums and inner classes
/// resolve, and an enum *value* under one draws :918 against the enum's own qualified name.
/// Every row is pinned against `godot --headless --check-only` at 4.7.2.
#[test]
fn a_cross_file_type_annotation_chain_splits_its_rejections_by_member_kind() {
    const LIB: &str = "\
class_name Lib1
extends Node

var lib_var = 1
signal lib_sig
const LIB_C = 5
enum LibE { A }
class LibInner:
\tpass
";
    let project = Project::new(&[("res://lib1.gd", LIB), ("res://use.gd", "")]);
    let use_ = |annotation: &str| {
        format!("extends Node\n\nfunc f() -> void:\n\tvar x: {annotation} = null\n\tprint(x)\n")
    };

    for name in ["lib_var", "lib_sig", "Nope"] {
        assert_eq!(
            error_messages(&analyze_file(
                &project,
                "res://use.gd",
                &use_(&format!("Lib1.{name}"))
            )),
            vec![format!(
                r#"Could not find type "{name}" under base "Lib1"."#
            )],
            "{name}"
        );
    }
    assert_eq!(
        error_messages(&analyze_file(&project, "res://use.gd", &use_("Lib1.LIB_C"))),
        vec![r#"Member "LIB_C" under base "Lib1" is not a valid type."#.to_owned()],
    );
    assert_eq!(
        error_messages(&analyze_file(
            &project,
            "res://use.gd",
            &use_("Lib1.LibE.A")
        )),
        vec![r#"Member "A" under base "Lib1.LibE" is not a valid type."#.to_owned()],
    );
    for (annotation, value) in [
        ("Lib1.LibE", "Lib1.LibE.A"),
        ("Lib1.LibInner", "null"),
        ("Lib1", "null"),
    ] {
        let src = format!(
            "extends Node\n\nfunc f() -> void:\n\tvar x: {annotation} = {value}\n\tprint(x)\n"
        );
        assert_eq!(
            error_messages(&analyze_file(&project, "res://use.gd", &src)),
            Vec::<String>::new(),
            "{annotation}"
        );
    }
}
