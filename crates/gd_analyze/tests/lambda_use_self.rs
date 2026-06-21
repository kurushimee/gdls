//! Reproduce-first + regression net for #141: the analyzer must track lambda `self`-capture,
//! mirroring Godot's `GDScriptAnalyzer::mark_lambda_use_self()` (`gdscript_analyzer.cpp:6364`).
//!
//! Godot stores the fact on `LambdaNode::use_self` and sets it from six sites where a lambda body
//! implicitly uses `self`: a self-method call (`:3676`), the `$`/`%` get_node shorthand (`:3880`),
//! an inherited/member variable or member signal (`:4425`/`:4428`), an instance member resolved
//! inside a lambda (`:4506`), and `self` itself (`:4778`). gdls keeps the parse tree immutable
//! during analysis, so the fact is recorded in a side table queryable via
//! [`AnalysisResult::lambda_uses_self`].
//!
//! The conformance ratchet is BLIND to this side table (nothing in a `.out` consumes it), so these
//! direct flag assertions are the only coverage — per the reproduce-first / direct-emission-test
//! discipline.

use std::path::Path;

use gd_analyze::{analyze, NoCrossFile, StrictSettings, WarnPolicy};
use gd_project::{FileId, WarningConfig};
use gd_syntax::ast::NodeKind;
use gd_syntax::parse;
use gd_types::NativeDb;

fn native_db() -> NativeDb {
    let path =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../gd_types/tests/fixtures/trimmed_api.json");
    NativeDb::load(path.to_str().expect("utf-8 path"))
        .unwrap_or_else(|e| panic!("load native DB fixture at {}: {e}", path.display()))
}

fn policy() -> WarnPolicy {
    WarnPolicy::build(&WarningConfig::default(), &StrictSettings::default())
}

/// Analyze `src` and report whether its (single) lambda was marked as using `self`.
///
/// Panics if `src` does not contain exactly one lambda, so each test stays unambiguous.
fn lambda_uses_self(src: &str) -> bool {
    let tree = parse(src).tree;
    let db = native_db();
    let result = analyze(
        &tree,
        Some(FileId::new(1)),
        "lambda.gd",
        &db,
        &NoCrossFile,
        &policy(),
    );
    let lambdas: Vec<_> = tree
        .iter_ids()
        .filter(|&id| matches!(tree.get(id).kind, NodeKind::Lambda(_)))
        .collect();
    assert_eq!(
        lambdas.len(),
        1,
        "test source must contain exactly one lambda; found {}",
        lambdas.len()
    );
    result.lambda_uses_self(lambdas[0])
}

#[test]
fn lambda_using_self_keyword_is_marked() {
    // `self` in a lambda body → mark_lambda_use_self (analyzer.cpp:4778).
    let src = "\
extends Node

func f():
\tvar g = func(): return self
";
    assert!(
        lambda_uses_self(src),
        "a lambda that uses `self` must be marked use_self (#141)"
    );
}

#[test]
fn lambda_using_member_variable_is_marked() {
    // A member variable referenced bare inside a lambda → mark_lambda_use_self (analyzer.cpp:4428).
    let src = "\
extends Node

var hp := 10

func f():
\tvar g = func(): return hp
";
    assert!(
        lambda_uses_self(src),
        "a lambda that reads a member variable must be marked use_self (#141)"
    );
}

#[test]
fn lambda_calling_self_method_is_marked() {
    // A bare call to a self method inside a lambda → mark_lambda_use_self (analyzer.cpp:3676).
    let src = "\
extends Node

func helper() -> int:
\treturn 1

func f():
\tvar g = func(): return helper()
";
    assert!(
        lambda_uses_self(src),
        "a lambda that calls a self method must be marked use_self (#141)"
    );
}

#[test]
fn lambda_using_get_node_shorthand_is_marked() {
    // `$Node` inside a lambda → mark_lambda_use_self (analyzer.cpp:3880).
    let src = "\
extends Node

func f():
\tvar g = func(): return $Child
";
    assert!(
        lambda_uses_self(src),
        "a lambda that uses the `$` get_node shorthand must be marked use_self (#141)"
    );
}

#[test]
fn lambda_using_only_locals_is_not_marked() {
    // Negative control: a lambda touching only its own params/locals captures nothing of `self`
    // and must NOT be marked. Over-marking here would be the inverse fidelity bug.
    let src = "\
extends Node

func f():
\tvar g = func(x): return x + 1
";
    assert!(
        !lambda_uses_self(src),
        "a lambda using only its own params/locals must NOT be marked use_self (#141)"
    );
}
