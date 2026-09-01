//! #449 / #451 — what `UNSAFE_CALL_ARGUMENT` says about a parameter that is not hard-annotated.
//!
//! Godot's gate is `!(par_type.is_hard_type() && par_type.is_variant())`
//! (`gdscript_analyzer.cpp:6096-6100`): the ONE parameter shape a Variant argument passes into
//! silently is a **hard** `Variant`, which is what every dump-declared `Variant` parameter is
//! (`type_from_property(.., p_is_arg=true)` at `:5845-5849`). gdls read the gate as
//! `is_hard && !is_variant`, which agrees on an annotated parameter and disagrees on every other
//! one, so a call into an untyped parameter reported nothing.
//!
//! Getting the gate right needs the parameter's real type to survive the interface seam, which is
//! the #451 half: `func f(a := "")` is `String` and hard, `func f(a = "")` is `String` and soft,
//! `func f(a)` is `Variant` and soft, and a default the shallow pass cannot decode is none of
//! those — it is a type gdls has not read, and naming it `"Variant"` would be wrong rather than
//! merely missing.
//!
//! Every row is verbatim `Godot_v4.7.2-stable --headless --check-only` output.

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
class_name ParLib
extends Node

func takes_annotated(a: String) -> void:
\tprint(a)

func takes_inferred(a := \"\", b := -1) -> void:
\tprint(a, b)

func takes_eq(a = \"\") -> void:
\tprint(a)

func takes_unknown(a := TileSet.TILE_SHAPE_SQUARE) -> void:
\tprint(a)

func takes_untyped(a) -> void:
\tprint(a)

func takes_null(a = null) -> void:
\tprint(a)

func takes_variant(a: Variant) -> void:
\tprint(a)

// #528: one row per shape the seam resolves, and one per shape it must keep refusing.
func takes_preload(a := preload(\"res://thing.tres\")) -> void:
\tprint(a)

func takes_global_enum(a := SIDE_LEFT) -> void:
\tprint(a)

func takes_float_const(a := PI) -> void:
\tprint(a)

func takes_soft_native_enum(a = TileSet.TILE_SHAPE_SQUARE) -> void:
\tprint(a)

func takes_bare_class(a := TileSet) -> void:
\tprint(a)

func takes_pseudo(a := TileSet.TileShape) -> void:
\tprint(a)

func takes_absent(a := TileSet.NOT_A_THING) -> void:
\tprint(a)
";

fn warnings_of(src: &str) -> Vec<String> {
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
    result
        .diagnostics
        .iter()
        .filter(|d| d.code() == "UNSAFE_CALL_ARGUMENT")
        .map(|d| d.message().to_owned())
        .collect()
}

fn unsafe_arg(n: usize, func: &str, par: &str) -> String {
    format!(
        r#"The argument {n} of the function "{func}()" requires the subtype "{par}" but the supertype "Variant" was provided."#
    )
}

fn call(stmt: &str) -> String {
    format!("extends Node\n\nfunc go(lib: ParLib, v: Variant) -> void:\n\t{stmt}\n")
}

// ===================================================================================================
// The gate: which parameter shapes warn.
// ===================================================================================================

/// The issue's repro. An untyped parameter is a SOFT Variant, and Godot warns on it — both halves
/// of the message render `"Variant"`. This is the row gdls dropped entirely.
#[test]
fn an_untyped_parameter_warns_naming_variant_on_both_sides() {
    assert_eq!(
        warnings_of(&call("lib.takes_untyped(v)")),
        vec![unsafe_arg(1, "takes_untyped", "Variant")]
    );
}

/// A hard `Variant` parameter is the one shape that accepts anything silently.
#[test]
fn a_hard_variant_parameter_stays_silent() {
    assert_eq!(
        warnings_of(&call("lib.takes_variant(v)")),
        Vec::<String>::new()
    );
}

/// Every dump-declared `Variant` parameter is hard, per `type_from_property(.., p_is_arg=true)`.
/// Without that, `Array.append`, `Dictionary.has`, and `Object.set` would each draw a row on every
/// call in a project.
#[test]
fn a_native_variant_parameter_stays_silent() {
    let src = "\
extends Node

func go(v: Variant, arr: Array, d: Dictionary, o: Object) -> void:
\tarr.append(v)
\tprint(d.has(v))
\to.set(&\"x\", v)
";
    assert_eq!(warnings_of(src), Vec::<String>::new());
}

/// An annotated parameter warned before this change and still does — the two gates agree there.
#[test]
fn an_annotated_parameter_still_warns() {
    assert_eq!(
        warnings_of(&call("lib.takes_annotated(v)")),
        vec![unsafe_arg(1, "takes_annotated", "String")]
    );
}

// ===================================================================================================
// The seam: what a parameter's default says about its type.
// ===================================================================================================

/// `:=` on a parameter is `ANNOTATED_INFERRED`, so the type is real and hard — and it has to cross
/// the interface, which is what it did not do before.
#[test]
fn a_cross_file_inferred_parameter_names_its_real_type() {
    assert_eq!(
        warnings_of(&call("lib.takes_inferred(v, v)")),
        vec![
            unsafe_arg(1, "takes_inferred", "String"),
            unsafe_arg(2, "takes_inferred", "int"),
        ]
    );
}

/// A plain `=` default is `INFERRED` — soft. The first arm's gate excludes only a hard `Variant`,
/// so it warns just the same, naming the type the default gave it.
#[test]
fn a_cross_file_soft_default_parameter_names_its_real_type() {
    assert_eq!(
        warnings_of(&call("lib.takes_eq(v)")),
        vec![unsafe_arg(1, "takes_eq", "String")]
    );
}

/// `= null` is not an unread type. Godot resolves it to a plain soft `Variant`, the same answer a
/// bare `f(a)` gets, so the row fires and names `"Variant"`.
#[test]
fn a_null_default_is_a_plain_soft_variant() {
    assert_eq!(
        warnings_of(&call("lib.takes_null(v)")),
        vec![unsafe_arg(1, "takes_null", "Variant")]
    );
}

/// A default the SHALLOW pass cannot decode still has its shape recorded, and the seam resolves it
/// (#528). The row Godot emits — naming `TileSet.TileShape` — is emitted here too. Before that, the
/// slot had no type at all and this call was silent, which was an under-report, never a wrong name.
#[test]
fn a_cross_file_undecodable_default_resolves_at_the_seam() {
    assert_eq!(
        warnings_of(&call("lib.takes_unknown(v)")),
        vec![unsafe_arg(1, "takes_unknown", "TileSet.TileShape")]
    );
}

/// Hardness still gates the error arm: a soft parameter takes an incompatible hard argument
/// without complaint, an `:=` one does not.
#[test]
fn hardness_still_decides_the_error_arm() {
    let src = "extends Node\n\nfunc go(lib: ParLib, n: int) -> void:\n\tlib.takes_eq(n)\n\tlib.takes_inferred(n)\n";
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
    let errors: Vec<String> = result
        .diagnostics
        .iter()
        .filter(|d| d.severity() == gd_analyze::Severity::Error)
        .map(|d| d.message().to_owned())
        .collect();
    assert_eq!(
        errors,
        vec![
            r#"Invalid argument for "takes_inferred()" function: argument 1 should be "String" but is "int"."#
                .to_owned()
        ]
    );
}

// ===================================================================================================
// #528: every shape the member seam resolves, reached through a parameter default.
// ===================================================================================================

/// A preload, a global enum value, and a global float constant all cross into a parameter slot and
/// name their real type in the row Godot emits.
#[test]
fn the_resolvable_default_shapes_all_reach_a_parameter() {
    for (stmt, func, ty) in [
        ("lib.takes_preload(v)", "takes_preload", "Resource"),
        ("lib.takes_global_enum(v)", "takes_global_enum", "Side"),
        ("lib.takes_float_const(v)", "takes_float_const", "float"),
    ] {
        assert_eq!(
            warnings_of(&call(stmt)),
            vec![unsafe_arg(1, func, ty)],
            "{stmt}"
        );
    }
}

/// The three shapes that must keep answering nothing. Each is an error in the DECLARING file — a
/// bare native class name is Godot's `GDScriptNativeClass`, an enum used as a value cannot stand on
/// its own, and an absent name has no type at all — so the slot has nothing to carry and the call
/// stays silent rather than claiming `Variant`.
#[test]
fn the_refused_default_shapes_stay_refused() {
    for stmt in [
        "lib.takes_bare_class(v)",
        "lib.takes_pseudo(v)",
        "lib.takes_absent(v)",
    ] {
        assert_eq!(warnings_of(&call(stmt)), Vec::<String>::new(), "{stmt}");
    }
}

/// Softness survives the resolution. A plain `=` default is `INFERRED` in Godot, so an incompatible
/// hard argument passes without an error even though the slot now has a real type; the `:=` twin
/// beside it does not.
#[test]
fn a_resolved_default_keeps_the_hardness_its_writing_gave_it() {
    let src = "extends Node\n\nfunc go(lib: ParLib, n: String) -> void:\n\tlib.takes_soft_native_enum(n)\n\tlib.takes_unknown(n)\n";
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
    let errors: Vec<String> = result
        .diagnostics
        .iter()
        .filter(|d| d.severity() == gd_analyze::Severity::Error)
        .map(|d| d.message().to_owned())
        .collect();
    assert_eq!(
        errors,
        vec![
            r#"Invalid argument for "takes_unknown()" function: argument 1 should be "TileSet.TileShape" but is "String"."#
                .to_owned()
        ]
    );
}
