//! `Assigned value for constant "X" isn't a constant expression.` (#344).
//!
//! Godot decides this from `ExpressionNode::is_constant`, but only after trying to force the value
//! through `make_expression_reduced_value` (`gdscript_analyzer.cpp:2124-2133`), which folds arrays,
//! dictionaries, and constant calls. gdls has no such family, so gating on the bit alone would
//! reject every `const A = [1, 2]`. `const_init_nonconstant_ref` instead walks the initializer for
//! a subexpression that can NEVER fold and reports only on a positive identification.
//!
//! The rule is narrower than "anything non-literal", and the boundary is the whole point: an inner
//! class, a named enum, a preload, and an alias of a declared constant ARE constant expressions,
//! while a native class name, a project `class_name`, a builtin type name, a global enum's name,
//! `self`, and any attribute-callee call are not. Every row below is pinned against
//! `Godot_v4.7.2-stable --headless --check-only` inside an imported project.
//!
//! #400 closed the largest hole in that walk. An identifier-callee call used to be skipped whole,
//! so `const X = randi()` and `const X = my_func()` passed in silence. The walk now reads the fold
//! table for the call itself and falls back to a three-way classification of the name: a builtin
//! constructor and the math subset of the utility registry fold, `Array` and `Dictionary` are
//! walked into instead of blamed because gdls cannot fold them but Godot can, and everything else
//! is never constant.

use std::collections::HashMap;
use std::path::Path;

use gd_analyze::{analyze, CrossFileQuery, Severity, StrictSettings, WarnPolicy};
use gd_project::{FileId, Interface};
use gd_syntax::{parse, Dialect};
use gd_types::NativeDb;

const MSG: &str = r#"Assigned value for constant "X" isn't a constant expression."#;

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

/// A mock workspace carrying one `class_name Lib` file, so the global-class arm has something real
/// to resolve. Interfaces come from the real extractor.
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

const LIB_GD: &str = "class_name Lib\nextends Node\nconst LIMIT := 10\n";

/// Every error message the consumer source produces, with `lib.gd` in the project.
fn errors(src: &str) -> Vec<String> {
    let project = Project::new(&[("res://lib.gd", LIB_GD), ("res://main.gd", src)]);
    let tree = parse(src).tree;
    analyze(
        &tree,
        Some(FileId::new(2)),
        "res://main.gd",
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

/// `errors`, at an explicit dialect.
fn errors_at(dialect: Dialect, src: &str) -> Vec<String> {
    let project = Project::new(&[("res://lib.gd", LIB_GD), ("res://main.gd", src)]);
    let tree = gd_syntax::parse_with_options(
        src,
        &gd_syntax::ParseOptions {
            dialect,
            script_path: "",
        },
    )
    .tree;
    let policy = WarnPolicy::build(
        &gd_project::WarningConfig::default(),
        &StrictSettings::default(),
        dialect,
    );
    gd_analyze::analyze_with_options(
        &tree,
        Some(FileId::new(2)),
        "res://main.gd",
        &native_db(),
        &project,
        &policy,
        gd_analyze::AnalyzeOptions {
            dialect,
            ..Default::default()
        },
    )
    .diagnostics
    .iter()
    .filter(|d| d.severity() == Severity::Error)
    .map(|d| d.message().to_owned())
    .collect()
}

/// A class-level `const X = <init>` under a preamble that declares the names the rows reference.
const PREAMBLE: &str = "\
extends Node

enum Kind { ONE, TWO }

class In:
	var y := 1

var member_var := 1
const ALIAS = 7

func g() -> int:
	return 1
";

fn class_const(init: &str) -> Vec<String> {
    errors(&format!("{PREAMBLE}const X = {init}\n"))
}

fn local_const(init: &str) -> Vec<String> {
    errors(&format!(
        "{PREAMBLE}func f() -> void:\n\tconst X = {init}\n\tprint(X)\n"
    ))
}

fn assert_reported(init: &str) {
    let errs = class_const(init);
    assert!(
        errs.iter().any(|e| e == MSG),
        "`const X = {init}` must be reported; got {errs:?}"
    );
}

fn assert_silent(init: &str) {
    let errs = class_const(init);
    assert!(
        !errs.iter().any(|e| e == MSG),
        "`const X = {init}` is a constant expression in Godot; got {errs:?}"
    );
}

// ===================================================================================================
// Reported — the shapes Godot rejects.
// ===================================================================================================

/// A bare class name — native, a project `class_name`, or a builtin type — never folds. Before
/// this, a class-level `const` had no constant-expression check at all, so all three were accepted
/// and then flowed on as type aliases.
#[test]
fn a_bare_class_name_is_not_a_constant_expression() {
    assert_reported("Node");
    assert_reported("Lib");
    assert_reported("Vector2");
}

/// `Vector2` also carries the two errors `reduce_identifier` already emitted; this one lands after
/// them, exactly as Godot's post-reduce check does.
#[test]
fn a_builtin_type_name_keeps_its_own_errors_and_adds_this_one() {
    let errs = class_const("Vector2");
    assert_eq!(
        errs,
        vec![
            "Builtin type cannot be used as a name on its own.".to_owned(),
            r#"Identifier "Vector2" not declared in the current scope."#.to_owned(),
            MSG.to_owned(),
        ],
        "got {errs:?}"
    );
}

/// `reduce_self` sets `is_constant = false` unconditionally (analyzer.cpp:4789).
#[test]
fn self_is_not_a_constant_expression() {
    assert_reported("self");
}

/// A global ENUM's own name does not fold, while one of its VALUES does — the split Godot draws at
/// analyzer.cpp:4620 versus :4646.
#[test]
fn a_global_enum_name_is_not_a_constant_expression_but_its_value_is() {
    assert_reported("ClockDirection");
    assert_silent("CLOCKWISE");
}

/// The disqualifying reference is found however deep it sits: inside an array or dictionary
/// literal, an operand, or a ternary arm.
#[test]
fn a_nested_class_name_disqualifies_the_whole_initializer() {
    assert_reported("[Node]");
    assert_reported("{\"k\": Node}");
    assert_reported("Node if true else 1");
    assert_reported("-Node");
}

/// An ATTRIBUTE-callee call can never fold, whatever it resolves to — the three sites that set
/// `is_constant` on a call all require an identifier callee.
#[test]
fn an_attribute_callee_call_is_not_a_constant_expression() {
    assert_reported("In.new()");
    assert_reported("Node.new()");
    assert_reported("Lib.new()");
}

/// A non-constant local or member reached from the initializer, the case that already worked.
#[test]
fn a_non_constant_local_or_member_disqualifies_the_initializer() {
    assert_reported("member_var");
    let errs = errors(&format!(
        "{PREAMBLE}func f(p: int) -> void:\n\tvar v := 1\n\tconst X = v + p\n\tprint(X)\n"
    ));
    assert!(errs.iter().any(|e| e == MSG), "got {errs:?}");
}

// ===================================================================================================
// Silent — the shapes Godot accepts. A false positive here is worse than the missing diagnostic
// this issue started from, so each row is one Godot verified silent.
// ===================================================================================================

/// An inner class, a named enum, and an alias of a declared constant all fold.
#[test]
fn in_file_constants_classes_and_enums_are_constant_expressions() {
    assert_silent("In");
    assert_silent("Kind");
    assert_silent("ALIAS");
    assert_silent("Kind.ONE");
}

/// The subscript trap: a meta base becomes constant THROUGH the attribute lookup, so the base name
/// alone must not be classified. `Node.PROCESS_MODE_INHERIT` is silent even though bare `Node` is
/// not, and the same holds for a builtin's constant and a project class's constant.
#[test]
fn an_attribute_on_a_class_name_is_a_constant_expression() {
    assert_silent("Node.NOTIFICATION_READY");
    assert_silent("Vector2.ZERO");
    assert_silent("Lib.LIMIT");
}

/// Literals and folded operations, including inside containers.
#[test]
fn literals_and_folded_operations_are_constant_expressions() {
    assert_silent("[1, 2]");
    assert_silent("\"a\" + \"b\"");
    assert_silent("[In]");
    assert_silent("In if true else 1");
}

/// A builtin constructor over constant arguments folds, so it is a constant expression. Since
/// #400 the walk reads the fold line for an identifier callee instead of skipping every one of
/// them, and this row is what the fold line has to keep silent.
#[test]
fn a_folded_builtin_constructor_is_a_constant_expression() {
    assert_silent("Vector2(1, 2)");
    assert_silent("Color(1, 0, 0)");
    assert_silent("Vector2(1, 2) + Vector2(3, 4)");
}

/// A preload folds, and so does a preload nested in a literal.
#[test]
fn a_preload_is_a_constant_expression() {
    assert_silent("preload(\"res://lib.gd\")");
    assert_silent("[preload(\"res://lib.gd\")]");
}

/// A utility function name, `PI`, and anything unresolved are left alone — the first two fold in
/// Godot, and an undeclared name already carries its own error.
#[test]
fn utility_names_and_unresolved_names_are_left_alone() {
    assert_silent("sin");
    assert_silent("PI");
    assert_silent("who_knows");
}

// ===================================================================================================
// #400 — the identifier-callee call, read off the fold line instead of skipped.
// ===================================================================================================

/// A project function, a non-math engine utility, and a GDScript utility that reaches the
/// filesystem all fail to fold, and Godot reports each. Before #400 the walk stepped over every
/// one of them.
#[test]
fn an_unfoldable_identifier_callee_call_is_reported() {
    assert_reported("g()");
    assert_reported("str(1)");
    assert_reported("randi()");
    assert_reported("range(3)");
    assert_reported("load(\"res://lib.gd\")");
}

/// The math subset of the engine's utility registry folds, and so do the eight GDScript utilities
/// Godot marks constant. All of them over constant arguments only.
#[test]
fn a_folded_utility_call_is_a_constant_expression() {
    assert_silent("absi(-1)");
    assert_silent("maxi(1, 2)");
    assert_silent("lerp(1.0, 2.0, 0.5)");
    assert_silent("len(\"ab\")");
    assert_silent("char(65)");
    assert_silent("ord(\"a\")");
    assert_silent("convert(1, TYPE_FLOAT)");
    assert_silent("type_exists(\"Node\")");
    assert_silent("Color8(255, 0, 0)");
    assert_silent("is_instance_of(1, TYPE_INT)");
}

/// A foldable utility over a non-constant argument is still not constant, and the blame lands on
/// the argument, not on the call.
#[test]
fn a_foldable_utility_over_a_nonconstant_argument_is_reported() {
    let errs = errors(&format!(
        "{PREAMBLE}func f(x: int) -> void:\n\tconst X = absi(x)\n\tprint(X)\n"
    ));
    assert!(
        errs.iter().any(|e| e == MSG),
        "a non-constant argument disqualifies the fold; got {errs:?}"
    );
}

/// A folded utility feeding a binary operation stays constant — the fold has to carry a usable
/// value, not a `Nil` placeholder, or the operand check false-positives on the way out.
#[test]
fn a_folded_utility_composes_with_an_operator() {
    assert_silent("min(1, 2) + 1");
    assert_silent("absi(-10) + 1");
}

/// `Array` and `Dictionary` are the two shapes gdls cannot fold but Godot can, so the walk
/// descends into their arguments instead of blaming the call. `Array([1], TYPE_INT, &"", null)` is
/// the typed-array form, and every argument of it is constant.
#[test]
fn the_array_and_dictionary_constructors_are_walked_not_blamed() {
    assert_silent("Array([])");
    assert_silent("Array([1], TYPE_INT, &\"\", null)");
    assert_silent("Dictionary({1: 1})");
}

/// The packed arrays and the reference types are shared, so Godot refuses to fold them even over
/// no arguments at all. Pinned at both dialects: 4.6 hardcodes the same 13 types in a switch, and
/// 4.7 reads `Variant::is_type_shared`, which lists exactly those 13.
#[test]
fn a_shared_type_constructor_never_folds() {
    assert_reported("PackedByteArray()");
    assert_reported("PackedInt32Array([1, 2])");
    for dialect in [Dialect::Godot4_6, Dialect::Godot4_7] {
        assert!(
            errors_at(dialect, &format!("{PREAMBLE}const X = PackedByteArray()\n"))
                .iter()
                .any(|e| e == MSG),
            "{dialect:?}: a packed array is shared at both tags"
        );
    }
}

/// The shadowing guard: a base class's constant named like a native class must resolve as that
/// constant, not fall through to the native-class arm. Without the base-chain step this row is the
/// false positive the whole design is built to avoid.
#[test]
fn a_base_class_constant_shadowing_a_class_name_is_a_constant_expression() {
    let base = "class_name Base\nextends Node\nconst Node2D := 5\n";
    let src = "extends Base\n\nconst X = Node2D\n";
    let project = Project::new(&[("res://base.gd", base), ("res://main.gd", src)]);
    let tree = parse(src).tree;
    let errs: Vec<String> = analyze(
        &tree,
        Some(FileId::new(2)),
        "res://main.gd",
        &native_db(),
        &project,
        &policy(),
    )
    .diagnostics
    .iter()
    .filter(|d| d.severity() == Severity::Error)
    .map(|d| d.message().to_owned())
    .collect();
    assert!(
        !errs.iter().any(|e| e == MSG),
        "a base-class constant shadows the native name; got {errs:?}"
    );
}

// ===================================================================================================
// Both scopes.
// ===================================================================================================

/// The check runs at class level AND inside a function — Godot's is one shared site in
/// `resolve_assignable`, reached from both arms.
#[test]
fn the_check_runs_at_both_scopes() {
    assert!(local_const("Node").iter().any(|e| e == MSG));
    assert!(local_const("self").iter().any(|e| e == MSG));
    assert!(!local_const("In").iter().any(|e| e == MSG));
    assert!(!local_const("Kind.ONE").iter().any(|e| e == MSG));
}
