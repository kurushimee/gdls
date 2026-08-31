//! #435, second half — callee-position resolution is skipped for the CALLEE, not for everything
//! written inside it.
//!
//! Godot resolves a call target through `reduce_identifier_from_base` and never through the
//! standalone `reduce_identifier` (`gdscript_analyzer.cpp:3556-3559`), so four steps of gdls's
//! `reduce_identifier` — the static-access check, the cross-file scope walk (#314), the utility
//! arm, and the not-declared report — skip the callee. That exemption used to be a bool raised
//! across the whole callee SUBTREE, so an identifier merely nested inside a callee expression lost
//! all four: an argument of the inner call in `f(x)()` got no type, no cross-file member, and no
//! `Identifier "x" not declared in the current scope.`
//!
//! `f(x)()` is itself a parse error in both engines (`Cannot call on an expression. Use ".call()"
//! if it's a Callable.`), so `--check-only` stops before the analyzer ever sees it. gdls analyzes
//! partial trees on purpose — an editor is full of half-written code — and reporting the truth
//! about the arguments there is the point.

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

const LIB_GD: &str = "\
class_name Lib
extends Node

var hp := 1
";

fn errors(consumer: &str) -> Vec<String> {
    let project = Project::new(&[("res://lib.gd", LIB_GD), ("res://main.gd", consumer)]);
    let tree = parse(consumer).tree;
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
        .filter(|d| d.severity() == Severity::Error)
        .map(|d| d.message().to_owned())
        .collect()
}

/// An undeclared name passed to the INNER call of `f(x)()` is reported. It is an argument, not the
/// callee, so nothing about callee position applies to it.
#[test]
fn an_argument_inside_a_callee_expression_is_still_checked() {
    let errs = errors("extends Node\n\nfunc f(_a): pass\n\nfunc g() -> void:\n\tf(nope)()\n");
    assert!(
        errs.iter()
            .any(|m| m == r#"Identifier "nope" not declared in the current scope."#),
        "the inner call's argument must still be resolved; got {errs:?}"
    );
}

/// The same position, for a name that DOES resolve — a member of the cross-file base, which only
/// the scope walk (#314) reaches. It must not be reported undeclared.
#[test]
fn a_cross_file_member_inside_a_callee_expression_resolves() {
    let errs = errors("extends Lib\n\nfunc f(_a): pass\n\nfunc g() -> void:\n\tf(hp)()\n");
    assert!(
        !errs
            .iter()
            .any(|m| m.contains(r#"Identifier "hp" not declared"#)),
        "the base's member is declared; got {errs:?}"
    );
}

/// The subscript-index spelling of the same shape. `a[i]()` puts `i` inside the callee subtree too.
#[test]
fn a_subscript_index_inside_a_callee_expression_is_still_checked() {
    let errs = errors("extends Node\n\nfunc g() -> void:\n\tvar a := []\n\ta[nope]()\n");
    assert!(
        errs.iter()
            .any(|m| m == r#"Identifier "nope" not declared in the current scope."#),
        "the index expression must still be resolved; got {errs:?}"
    );
}

/// The exemption still holds where it belongs: a bare callee naming nothing draws the call-shaped
/// answer, never the identifier-shaped `not declared` one.
#[test]
fn the_callee_itself_keeps_its_exemption() {
    let errs = errors("extends Node\n\nfunc g() -> void:\n\tnope()\n");
    assert!(
        !errs
            .iter()
            .any(|m| m.contains(r#"Identifier "nope" not declared"#)),
        "the callee itself must not draw the identifier report; got {errs:?}"
    );
}
