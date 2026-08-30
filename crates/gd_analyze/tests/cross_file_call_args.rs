//! Argument-type checking on a cross-file call (#336).
//!
//! `script_chain_call` synthesized its `CallSig` with `par_types: vec![DataType::variant(); n]` —
//! every parameter a soft Variant regardless of what the declaring file wrote. Soft Variant is the
//! silence value: every check in the argument loop is gated on `par_type.is_hard_type()`, so the
//! whole per-argument half of `validate_call_arg` was dead across file boundaries. Passing a
//! `String` to another script's `func take_int(x: int)` said nothing.
//!
//! The fix projects each parameter through `resolve_interface_type_expr`, the same function the
//! return type already went through. What makes that safe without a signature-wide gate is that
//! the degrade is per-slot and lands on exactly the value the gates read: an annotation the
//! interface cannot see comes back SOFT Variant and stays silent, while a real `x: Variant` comes
//! back HARD and is silent for a different reason (every gate accepts Variant). Upstream degrades
//! per property too (`type_from_property`, analyzer.cpp:6127-6129), never per method.
//!
//! Every row is pinned against `godot --headless --check-only` at 4.7.2.

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

/// A mock workspace over the REAL interface extractor, so what the analyzer sees is what
/// production builds. (Same shape as `cross_file_inheritance.rs`'s.)
struct Project {
    ifaces: HashMap<FileId, Interface>,
    by_class_name: HashMap<String, FileId>,
    by_path: HashMap<String, FileId>,
}

impl Project {
    fn new(files: &[(&str, &str)]) -> Self {
        let mut ifaces = HashMap::new();
        let mut by_class_name = HashMap::new();
        let mut by_path = HashMap::new();
        for (i, (path, src)) in files.iter().enumerate() {
            let fid = FileId::new((i + 1) as u32);
            let iface = gd_project::extract_interface(&parse(src).tree);
            if let Some(name) = iface.class_name.clone() {
                by_class_name.insert(name, fid);
            }
            by_path.insert((*path).to_string(), fid);
            ifaces.insert(fid, iface);
        }
        Project {
            ifaces,
            by_class_name,
            by_path,
        }
    }

    fn fid(&self, path: &str) -> FileId {
        *self.by_path.get(path).expect("known path")
    }
}

impl CrossFileQuery for Project {
    fn interface(&self, file: FileId) -> Option<&Interface> {
        self.ifaces.get(&file)
    }
    fn global_class_file(&self, name: &str) -> Option<FileId> {
        self.by_class_name.get(name).copied()
    }
    fn resolve_res_path(&self, res: &str) -> Option<FileId> {
        self.by_path.get(res).copied()
    }
    fn autoload_file(&self, _name: &str) -> Option<FileId> {
        None
    }
    fn file_path(&self, file: FileId) -> Option<&str> {
        self.by_path
            .iter()
            .find(|(_, f)| **f == file)
            .map(|(p, _)| p.as_str())
    }
}

fn analyze_use(project: &Project, src: &str) -> AnalysisResult {
    let tree = parse(src).tree;
    let native = native_db();
    analyze(
        &tree,
        Some(project.fid("res://use.gd")),
        "res://use.gd",
        &native,
        project,
        &policy(),
    )
}

fn errors(project: &Project, src: &str) -> Vec<String> {
    analyze_use(project, src)
        .diagnostics
        .iter()
        .filter(|d| d.severity() == Severity::Error)
        .map(|d| d.message().to_owned())
        .collect()
}

const LIB: &str = "\
class_name ArgLib
extends Node

func take_int(x: int) -> void:
\tprint(x)

func take_variant(x: Variant) -> void:
\tprint(x)

func untyped(x) -> void:
\tprint(x)

func take_arr(a: Array[int]) -> void:
\tprint(a)

func take_node(n: Node) -> void:
\tprint(n)

static func s_take_int(x: int) -> void:
\tprint(x)

func _init(seed_value: int = 0) -> void:
\tprint(seed_value)
";

fn project() -> Project {
    Project::new(&[("res://lib.gd", LIB), ("res://use.gd", "")])
}

fn use_body(body: &str) -> String {
    format!("extends Node\n\nfunc f() -> void:\n{body}\n")
}

/// The two messages a wrong-typed argument draws, in Godot's order.
fn wrong_arg(from: &str, to: &str, func: &str, index: usize) -> Vec<String> {
    vec![
        format!(r#"Cannot pass a value of type "{from}" as "{to}"."#),
        format!(
            r#"Invalid argument for "{func}()" function: argument {index} should be "{to}" but is "{from}"."#
        ),
    ]
}

#[test]
fn a_wrong_typed_argument_to_another_scripts_method_is_reported() {
    let p = project();
    assert_eq!(
        errors(
            &p,
            &use_body("\tvar l := ArgLib.new()\n\tl.take_int(\"nope\")")
        ),
        wrong_arg("String", "int", "take_int", 1),
    );
    // A static call through the class name routes through the same block.
    assert_eq!(
        errors(&p, &use_body("\tArgLib.s_take_int(\"nope\")")),
        wrong_arg("String", "int", "s_take_int", 1),
    );
    // And so does a constructor, against the cross-file `_init` — which Godot names `new()` in
    // the message, not `_init()`.
    assert_eq!(
        errors(&p, &use_body("\tvar l := ArgLib.new(\"nope\")")),
        wrong_arg("String", "int", "new", 1),
    );
}

/// A native-typed parameter takes the object path: no `Cannot pass` pair, one argument error.
#[test]
fn a_wrong_typed_object_argument_is_reported() {
    let p = project();
    assert_eq!(
        errors(&p, &use_body("\tvar l := ArgLib.new()\n\tl.take_node(5)")),
        vec![
            r#"Invalid argument for "take_node()" function: argument 1 should be "Node" but is "int"."#
                .to_owned()
        ],
    );
}

/// Fail-open, per slot. A parameter with no evidence behind it stays silent — that is what makes
/// the check safe without a gate over the whole signature.
#[test]
fn a_parameter_the_interface_cannot_type_stays_silent() {
    let p = project();
    for body in [
        // An explicit `: Variant` — HARD Variant, which every gate accepts.
        "\tvar l := ArgLib.new()\n\tl.take_variant(\"anything\")",
        // No annotation at all — SOFT Variant, silent by construction.
        "\tvar l := ArgLib.new()\n\tl.untyped(\"anything\")",
    ] {
        assert_eq!(errors(&p, &use_body(body)), Vec::<String>::new(), "{body}");
    }
}

/// A soft ARGUMENT against a hard parameter is upstream's `UNSAFE_CALL_ARGUMENT` territory, not an
/// error — it must not become one now that the parameter is typed.
#[test]
fn a_soft_argument_against_a_typed_parameter_is_not_an_error() {
    let p = project();
    assert_eq!(
        errors(
            &p,
            &use_body("\tvar l := ArgLib.new()\n\tvar d = {}\n\tl.take_int(d.k)")
        ),
        Vec::<String>::new(),
    );
    // A supertype argument is the same family — the runtime check is Godot's, not ours.
    assert_eq!(
        errors(
            &p,
            &use_body("\tvar l := ArgLib.new()\n\tvar n: Node = self\n\tl.take_node(n)")
        ),
        Vec::<String>::new(),
    );
}

/// A typed-container parameter narrows the literal, so its elements get checked too — a new error
/// family cross-file, and the correct one (analyzer.cpp:3622-3636).
#[test]
fn a_typed_container_parameter_narrows_the_argument_literal() {
    let p = project();
    assert_eq!(
        errors(
            &p,
            &use_body("\tvar l := ArgLib.new()\n\tl.take_arr([1, \"a\"])")
        ),
        vec![
            r#"Cannot include a value of type "String" as "int"."#.to_owned(),
            r#"Cannot have an element of type "String" in an array of type "Array[int]"."#
                .to_owned(),
        ],
    );
}

/// The silence contract: every correctly-typed call stays clean.
#[test]
fn correctly_typed_calls_stay_silent() {
    let p = project();
    for body in [
        "\tvar l := ArgLib.new()\n\tl.take_int(1)",
        "\tvar l := ArgLib.new()\n\tl.take_node(self)",
        "\tvar l := ArgLib.new()\n\tl.take_arr([1, 2])",
        "\tvar l := ArgLib.new(3)",
        "\tArgLib.s_take_int(7)",
    ] {
        assert_eq!(errors(&p, &use_body(body)), Vec::<String>::new(), "{body}");
    }
}
