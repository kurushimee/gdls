//! #433 — a method miss on a base with no surface to walk.
//!
//! `UNSAFE_METHOD_ACCESS` is one sentence that claims two different things. On a `Class`,
//! `Script`, or non-meta `Native` base it asserts that a surface gdls walked lacks the name, so
//! the ancestry has to be introspectable end to end before gdls will say it. On a `Variant` base
//! there is no surface — upstream has none either — and the miss holds by construction, so the
//! same row needs no chain firewall. gdls used to run one gate for both and lost every Variant
//! row: an untyped local, an untyped parameter, a `Variant`-annotated member, the dummy an
//! undeclared identifier leaves behind. Upstream's own hardness test
//! (`gdscript_analyzer.cpp:3749-3753`) only ever excludes a HARD `BUILTIN`, so a soft `int` miss
//! warns and a hard one does not.
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
            enable_warnings: vec!["UNSAFE_METHOD_ACCESS".to_owned()],
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
class_name MissLib
extends Node

var hp := 1

func known() -> void:
\tpass
";

fn diagnose(src: &str) -> (Vec<String>, Vec<String>) {
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
    let errors = result
        .diagnostics
        .iter()
        .filter(|d| d.severity() == Severity::Error)
        .map(|d| d.message().to_owned())
        .collect();
    let warnings = result
        .diagnostics
        .iter()
        .filter(|d| d.code() == "UNSAFE_METHOD_ACCESS")
        .map(|d| d.message().to_owned())
        .collect();
    (errors, warnings)
}

fn miss(name: &str, ty: &str) -> String {
    format!(
        r#"The method "{name}()" is not present on the inferred type "{ty}" (but may be present on a subtype)."#
    )
}

fn warnings_of(src: &str) -> Vec<String> {
    diagnose(src).1
}

// ===================================================================================================
// Variant: every shape upstream warns on.
// ===================================================================================================

/// A HARD `Variant` — an annotated parameter, an annotated member, an annotated local. Upstream's
/// hardness test excludes only builtins, so all three warn.
#[test]
fn a_hard_variant_base_warns_on_a_method_miss() {
    let src = "\
extends Node

var member_v: Variant

func take(p: Variant) -> void:
\tp.nope_param()

func go() -> void:
\tmember_v.nope_member()
\tvar local_v: Variant = 1
\tlocal_v.nope_local()
";
    assert_eq!(
        warnings_of(src),
        vec![
            miss("nope_param", "Variant"),
            miss("nope_member", "Variant"),
            miss("nope_local", "Variant"),
        ]
    );
}

/// A SOFT `Variant` — an untyped parameter and a local inferred from an untyped return. This is
/// the family the old gate lost outright: it required `is_hard_type()`, which neither has.
#[test]
fn a_soft_variant_base_warns_on_a_method_miss() {
    let src = "\
extends Node

func take(p) -> void:
\tp.nope_param()

func go() -> void:
\tvar soft = untyped_source()
\tsoft.nope_soft()

func untyped_source():
\treturn 1
";
    assert_eq!(
        warnings_of(src),
        vec![miss("nope_param", "Variant"), miss("nope_soft", "Variant")]
    );
}

/// The dummy an undeclared identifier leaves behind is a Variant with no type, and upstream warns
/// through it alongside the not-declared error.
#[test]
fn an_undeclared_identifier_base_warns_beside_its_own_error() {
    let (errors, warnings) = diagnose("extends Node\n\nfunc go() -> void:\n\tnowhere.nope()\n");
    assert_eq!(
        errors,
        vec![r#"Identifier "nowhere" not declared in the current scope."#.to_owned()]
    );
    assert_eq!(warnings, vec![miss("nope", "Variant")]);
}

/// `self` is upstream's one unconditional exclusion (`!is_self`), and `super.` never reaches the
/// probe at all.
#[test]
fn self_and_super_stay_silent() {
    for stmt in ["self.nope_self()", "nope_bare()", "super.nope_super()"] {
        let src = format!("extends Node\n\nfunc go() -> void:\n\t{stmt}\n");
        assert_eq!(warnings_of(&src), Vec::<String>::new(), "{stmt}");
    }
}

// ===================================================================================================
// Builtins: upstream's exclusion is `hard && BUILTIN`, so hardness decides.
// ===================================================================================================

/// A hard builtin base is the one thing upstream's gate excludes — the two errors carry the miss.
#[test]
fn a_hard_builtin_base_reports_only_the_errors() {
    let (errors, warnings) = diagnose(
        "extends Node\n\nfunc go() -> void:\n\tvar i: int = 1\n\ti.nope_hard()\n\tvar a: Array = []\n\ta.nope_array()\n",
    );
    assert_eq!(
        errors,
        vec![
            r#"Cannot find member "nope_hard" in base "int"."#.to_owned(),
            r#"Function "nope_hard()" not found in base int."#.to_owned(),
            r#"Cannot find member "nope_array" in base "Array"."#.to_owned(),
            r#"Function "nope_array()" not found in base Array."#.to_owned(),
        ]
    );
    assert_eq!(warnings, Vec::<String>::new());
}

/// A SOFT builtin base warns, and it renders the builtin's own name, not `"Variant"`.
#[test]
fn a_soft_builtin_base_warns_under_its_own_name() {
    let src = "\
extends Node

var flag: Variant

func go() -> void:
\tvar si = 1 if flag else 2
\tsi.nope_soft()
";
    assert_eq!(warnings_of(src), vec![miss("nope_soft", "int")]);
}

// ===================================================================================================
// The walked kinds keep their firewall.
// ===================================================================================================

/// An inner-class METAtype base warns — upstream interpolates the metatype raw, so the row reads
/// under the identifier — and the static-miss error still fires beside it.
#[test]
fn an_inner_class_metatype_base_warns_beside_its_static_miss() {
    let (errors, warnings) = diagnose(
        "extends Node\n\nclass Inner:\n\tstatic func known() -> void:\n\t\tpass\n\nfunc go() -> void:\n\tInner.nope_inner()\n",
    );
    assert_eq!(
        errors,
        vec![r#"Static function "nope_inner()" not found in base "Inner"."#.to_owned()]
    );
    assert_eq!(warnings, vec![miss("nope_inner", "Inner")]);
}

/// A cross-file script base warns on a miss and stays silent on a hit.
#[test]
fn a_cross_file_script_base_warns_only_on_the_miss() {
    let src = "\
extends Node

func go() -> void:
\tvar lib := MissLib.new()
\tlib.known()
\tlib.nope_cross()
";
    assert_eq!(warnings_of(src), vec![miss("nope_cross", "MissLib")]);
}

/// A real native method resolves before the branch; a native miss warns under the class name.
#[test]
fn a_native_instance_base_warns_only_on_the_miss() {
    let src = "\
extends Node

func go() -> void:
\tvar n := Node.new()
\tn.queue_free()
\tn.nope_native()
";
    assert_eq!(warnings_of(src), vec![miss("nope_native", "Node")]);
}

/// A native METAtype miss stays silent. Upstream prints `"GDScriptNativeClass"` there, a rendering
/// gdls does not produce, so the row is left out deliberately as an under-report.
#[test]
fn a_native_metatype_base_stays_silent() {
    assert_eq!(
        warnings_of("extends Node\n\nfunc go() -> void:\n\tNode.nope_static()\n"),
        Vec::<String>::new()
    );
}

/// An enum VALUE base warns beside its own pair of errors; the enum METAtype is the arm upstream's
/// warning is an `else if` of, so it never reaches the probe.
#[test]
fn an_enum_value_base_warns_but_the_metatype_does_not() {
    let src = "\
extends Node

enum Mode { A, B }

func go() -> void:
\tvar e := Mode.A
\te.nope_value()
\tMode.nope_meta()
";
    let (errors, warnings) = diagnose(src);
    assert!(
        errors.contains(&"Cannot call function on enum value.".to_owned()),
        "{errors:?}"
    );
    assert!(
        errors
            .iter()
            .any(|e| e.starts_with(r#"Static function "nope_meta()""#)),
        "{errors:?}"
    );
    assert_eq!(warnings, vec![miss("nope_value", "main.gd.Mode")]);
}

/// The issue's own repro, verbatim: a property miss on a native base leaves a Variant, and the
/// call one link later is the row gdls used to drop — through the attribute directly and through
/// a local that holds it.
#[test]
fn a_property_miss_leaves_a_variant_that_warns_one_call_later() {
    let src = "\
extends Node

func f(d: AcceptDialog) -> void:
\td.image_exports.append(1)

func g(d: AcceptDialog) -> void:
\tvar x = d.image_exports
\tx.append(1)
";
    assert_eq!(
        warnings_of(src),
        vec![miss("append", "Variant"), miss("append", "Variant")]
    );
}
