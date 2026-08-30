//! #364 — which expressions carry Godot's `ExpressionNode::is_constant` bit.
//!
//! The bit gates `update_const_expression_builtin_type`, so it decides whether a bad assignment
//! draws Godot's *companion* line (`Cannot assign a value of type "X" as "Y".`) on top of the
//! `… with specified type …` line. gdls used to gate on its own fold table instead, which is the
//! narrower set: a class object and a preloaded resource have no `FoldedValue` to hold, so both
//! lost their companion.
//!
//! The boundary is not "every class-typed expression". Godot marks an inner-class identifier
//! (`gdscript_analyzer.cpp:4046`, reached from the scope walk at :4187 and the CLASS member arm at
//! :4259) and a preload (:4778). It does NOT mark a global class resolved from another file
//! (:4572), a native class (:4566), or `self` (:4789). Every row here is pinned against
//! `godot --headless --check-only` on the equivalent project.

use gd_syntax::Dialect;
use std::collections::HashMap;
use std::path::Path;

use gd_analyze::{analyze, CrossFileQuery, Severity, StrictSettings, WarnPolicy};
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
const LIB1_PATH: &str = "res://src/lib1.gd";
const HOLDER: &str =
    "class_name Holder2\nextends Node\n\nclass In:\n\tvar x := 1\n\nconst KONST := 7\n";
const HOLDER_PATH: &str = "res://src/holder2.gd";

/// Analyze `src` as `path`, against a project that also holds `Lib1` and `Holder2`.
fn errors_in(path: &str, src: &str) -> Vec<String> {
    let mut files = vec![(LIB1_PATH, LIB1), (HOLDER_PATH, HOLDER)];
    if path != LIB1_PATH {
        files.push((path, src));
    }
    let project = Project::new(&files);
    // Re-analyze the file under test with the source given here, so a caller can hand a body to a
    // path the project already registered (the in-file `Lib1` row).
    let tree = parse(src).tree;
    let result = analyze(
        &tree,
        Some(project.by_path[path]),
        path,
        &native_db(),
        &project,
        &policy(),
    );
    // Godot's order for two diagnostics anchored at the same initializer is companion-first, which
    // is the order the analyzer already pushes them in; sort only by span so the rows stay stable.
    let mut rows: Vec<_> = result
        .diagnostics
        .iter()
        .filter(|d| d.severity() == Severity::Error)
        .map(|d| (d.span().start, d.message().to_owned()))
        .collect();
    rows.sort_by_key(|(start, _)| *start);
    rows.into_iter().map(|(_, m)| m).collect()
}

/// The whole boundary in one file, in Godot's order. Seven errors: the `In` and `preload` rows draw
/// a companion, the `Lib1`, `Node` and `self` rows do not, and the `int` constant draws nothing.
#[test]
fn the_companion_line_follows_godots_constancy_boundary() {
    let src = concat!(
        "extends Node\n",
        "\n",
        "class In:\n\tvar x := 1\n",
        "\n",
        "const C := 5\n",
        "\n",
        "func f() -> void:\n",
        "\tvar a: int = In\n",
        "\tvar b: int = Lib1\n",
        "\tvar c: int = Node\n",
        "\tvar d: int = C\n",
        "\tvar e: int = preload(\"res://src/lib1.gd\")\n",
        "\tvar g: int = self\n",
        "\tprint(a, b, c, d, e, g)\n",
    );
    assert_eq!(
        errors_in("res://src/probe8.gd", src),
        vec![
            r#"Cannot assign a value of type "In" as "int"."#.to_owned(),
            r#"Cannot assign a value of type In to variable "a" with specified type int."#
                .to_owned(),
            r#"Cannot assign a value of type Lib1 to variable "b" with specified type int."#
                .to_owned(),
            r#"Cannot assign a value of type GDScriptNativeClass to variable "c" with specified type int."#.to_owned(),
            r#"Cannot assign a value of type "Lib1" as "int"."#.to_owned(),
            r#"Cannot assign a value of type Lib1 to variable "e" with specified type int."#
                .to_owned(),
            r#"Cannot assign a value of type res://src/probe8.gd to variable "g" with specified type int."#.to_owned(),
        ]
    );
}

/// The same `Lib1` reference is marked or not depending on WHERE it is written: inside `lib1.gd`
/// the scope walk resolves it as the class naming itself (:4187 → :4046, constant), while from
/// another file it resolves through the global-class registry (:4572), which sets no bit.
#[test]
fn a_class_naming_itself_is_constant_but_a_cross_file_reference_is_not() {
    let in_file = "class_name Lib1\nextends Node\n\nfunc selfref() -> void:\n\tvar a: int = Lib1\n\tprint(a)\n";
    assert_eq!(
        errors_in(LIB1_PATH, in_file),
        vec![
            r#"Cannot assign a value of type "Lib1" as "int"."#.to_owned(),
            r#"Cannot assign a value of type Lib1 to variable "a" with specified type int."#
                .to_owned(),
        ]
    );

    let cross_file = "extends Node\n\nfunc g() -> void:\n\tvar a: int = Lib1\n\tprint(a)\n";
    assert_eq!(
        errors_in("res://src/probe10.gd", cross_file),
        vec![
            r#"Cannot assign a value of type Lib1 to variable "a" with specified type int."#
                .to_owned(),
        ]
    );
}

/// An inner class reached through an attribute (`Holder2.In`) carries the bit across the subscript
/// wrapper. Its sibling `const` does not draw a companion, because an int constant narrows into an
/// int slot cleanly.
#[test]
fn constancy_propagates_through_an_attribute_access() {
    let src = "extends Node\n\nfunc f() -> void:\n\tvar a: int = Holder2.In\n\tvar b: int = Holder2.KONST\n\tprint(a, b)\n";
    assert_eq!(
        errors_in("res://src/probe11.gd", src),
        vec![
            r#"Cannot assign a value of type "In" as "int"."#.to_owned(),
            r#"Cannot assign a value of type In to variable "a" with specified type int."#
                .to_owned(),
        ]
    );
}

/// The cast site takes the same gate (`gdscript_analyzer.cpp:3800`), which is a second message
/// entirely: `Cannot cast …`, drawn before the validity check's `Invalid cast …`.
#[test]
fn a_constant_cast_operand_draws_the_cast_companion() {
    let src = "extends Node\n\nclass In:\n\tvar x := 1\n\nfunc f() -> void:\n\tvar a: int = In as int\n\tprint(a)\n";
    assert_eq!(
        errors_in("res://src/probe9.gd", src),
        vec![
            r#"Cannot cast a value of type "In" as "int"."#.to_owned(),
            r#"Invalid cast. Cannot convert from "In" to "int"."#.to_owned(),
        ]
    );
}

/// A `match` pattern is legal when the expression is constant. A preload is, so the
/// "must be a constant expression" error must not fire — it did while the gate read the fold table,
/// which holds no value for a preloaded resource.
#[test]
fn a_preload_is_a_legal_match_pattern() {
    let src = concat!(
        "extends Node\n",
        "\n",
        "func f(v: Variant) -> void:\n",
        "\tmatch v:\n",
        "\t\tpreload(\"res://src/lib1.gd\"):\n",
        "\t\t\tpass\n",
    );
    assert_eq!(errors_in("res://src/probe12.gd", src), Vec::<String>::new());
}
