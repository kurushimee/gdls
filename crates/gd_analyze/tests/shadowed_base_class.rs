//! #563 — `SHADOWED_VARIABLE_BASE_CLASS`, the base-class half of `is_shadowing`.
//!
//! `GDScriptAnalyzer::is_shadowing` (`gdscript_analyzer.cpp:6124-6197`) is one walk with three
//! stops, first hit wins: the global identifiers, then — in a local scope — the current class and
//! every script base above it, then the native ancestry. gdls ran the first two and probed only
//! native METHODS on the third, so `func greet(name: String)` under `extends Node` said nothing.
//!
//! The order is the correctness property. Skip a middle stop and a name declared on a script base
//! gets blamed on `Node` — the wrong kind, the wrong class, and no line number.
//!
//! Every expected message is verbatim output from Godot 4.7.2's editor language server.

use gd_analyze::{analyze, NoCrossFile, Severity, StrictSettings, WarnPolicy};
use gd_project::WarningConfig;
use gd_syntax::Dialect;
use gd_types::NativeDb;

/// `Node` carrying one member of each ClassDB kind, plus one name that is BOTH a method and a
/// property so the probe order can be pinned.
fn mini_native() -> NativeDb {
    NativeDb::from_json(
        r#"{
            "header": {"version_major": 4, "version_minor": 7, "version_patch": 2},
            "utility_functions": [
                {"name": "print", "return_type": "void", "category": "general",
                 "is_vararg": true, "hash": 1, "arguments": []}
            ],
            "classes": [
                {"name": "Object"},
                {"name": "RefCounted", "inherits": "Object"},
                {"name": "Node", "inherits": "Object",
                 "methods": [
                    {"name": "duplicate", "is_const": false, "is_static": false,
                     "is_vararg": false, "is_virtual": false, "hash": 1},
                    {"name": "both", "is_const": false, "is_static": false,
                     "is_vararg": false, "is_virtual": false, "hash": 2}
                 ],
                 "signals": [{"name": "tree_entered", "arguments": []}],
                 "properties": [
                    {"name": "name", "type": "StringName", "getter": "get_name",
                     "setter": "set_name"},
                    {"name": "both", "type": "int", "getter": "g", "setter": "s"}
                 ],
                 "constants": [{"name": "NOTIFICATION_READY", "value": 13}],
                 "enums": [{"name": "ProcessMode", "is_bitfield": false,
                            "values": [{"name": "PROCESS_MODE_INHERIT", "value": 0}]}]
                }
            ]
        }"#,
    )
    .expect("valid mini dump")
}

fn policy() -> WarnPolicy {
    WarnPolicy::build(
        &WarningConfig::default(),
        &StrictSettings::default(),
        Dialect::DEFAULT,
    )
}

/// Every warning message the analysis produced, in emit order.
fn warnings(src: &str) -> Vec<String> {
    let tree = gd_syntax::parse(src).tree;
    analyze(&tree, None, "t.gd", &mini_native(), &NoCrossFile, &policy())
        .diagnostics
        .iter()
        .filter(|d| d.severity() == Severity::Warning)
        .map(|d| d.message().to_owned())
        .collect()
}

/// Only the shadowing rows, so an unrelated UNUSED_* row cannot make a test brittle.
fn shadow_rows(src: &str) -> Vec<String> {
    warnings(src)
        .into_iter()
        .filter(|m| m.contains("shadowing"))
        .collect()
}

/// The reported case: a `name` parameter under `extends Node`.
#[test]
fn a_parameter_shadowing_a_native_property_reports() {
    let src = "extends Node\n\nfunc greet(name: String) -> String:\n\treturn name\n";
    assert_eq!(
        shadow_rows(src),
        vec![r#"The local function parameter "name" is shadowing an already-declared property in the base class "Node"."#.to_owned()]
    );
}

/// Each of ClassDB's five kinds gets its own noun, and an enum VALUE reads as "constant" because
/// that is how ClassDB registers it.
#[test]
fn every_native_member_kind_gets_its_own_noun() {
    let src = "extends Node\n\nfunc f() -> void:\n\tvar tree_entered := 1\n\tvar name := 2\n\tvar NOTIFICATION_READY := 3\n\tvar PROCESS_MODE_INHERIT := 4\n\tvar ProcessMode := 5\n\tvar duplicate := 6\n\tprint(tree_entered, name, NOTIFICATION_READY, PROCESS_MODE_INHERIT, ProcessMode, duplicate)\n";
    assert_eq!(
        shadow_rows(src),
        vec![
            r#"The local variable "tree_entered" is shadowing an already-declared signal in the base class "Node"."#.to_owned(),
            r#"The local variable "name" is shadowing an already-declared property in the base class "Node"."#.to_owned(),
            r#"The local variable "NOTIFICATION_READY" is shadowing an already-declared constant in the base class "Node"."#.to_owned(),
            r#"The local variable "PROCESS_MODE_INHERIT" is shadowing an already-declared constant in the base class "Node"."#.to_owned(),
            r#"The local variable "ProcessMode" is shadowing an already-declared enum in the base class "Node"."#.to_owned(),
            r#"The local variable "duplicate" is shadowing an already-declared method in the base class "Node"."#.to_owned(),
        ]
    );
}

/// ClassDB probes methods before properties, so a name that is both reads as "method". A generic
/// member lookup would answer "property" here.
#[test]
fn the_probe_order_is_classdbs_own() {
    let src = "extends Node\n\nfunc f() -> void:\n\tvar both := 1\n\tprint(both)\n";
    assert_eq!(
        shadow_rows(src),
        vec![
            r#"The local variable "both" is shadowing an already-declared method in the base class "Node"."#
                .to_owned()
        ]
    );
}

/// The rest parameter takes the same check every named one does.
#[test]
fn a_rest_parameter_reports() {
    let src = "extends Node\n\nfunc f(...name) -> void:\n\tprint(name)\n";
    assert_eq!(
        shadow_rows(src),
        vec![r#"The local function parameter "name" is shadowing an already-declared property in the base class "Node"."#.to_owned()]
    );
}

/// A `for` iterator variable, with Godot's quoted context noun.
#[test]
fn a_for_iterator_variable_reports() {
    let src = "extends Node\n\nfunc f() -> void:\n\tfor name in [1, 2]:\n\t\tprint(name)\n";
    assert_eq!(
        shadow_rows(src),
        vec![r#"The local "for" iterator variable "name" is shadowing an already-declared property in the base class "Node"."#.to_owned()]
    );
}

/// A class MEMBER runs the walk with `in_local_scope = false`: it skips the current-class stop —
/// a member does not shadow its own class — and still reaches the native one. Godot's wording
/// says "The local variable" either way; the template is shared and upstream does not vary it.
#[test]
fn a_class_member_reaches_the_native_stop() {
    let src = "extends Node\n\nvar duplicate = 1\n";
    assert_eq!(
        shadow_rows(src),
        vec![
            r#"The local variable "duplicate" is shadowing an already-declared method in the base class "Node"."#
                .to_owned()
        ]
    );
}

/// The in-file `class B extends A` chain, with the base's fqcn and the member's line.
#[test]
fn an_in_file_base_class_reports_with_its_line() {
    let src = "extends Node\n\nclass A:\n\tvar foo := 1\n\nclass B extends A:\n\tfunc f() -> void:\n\t\tvar foo := 2\n\t\tprint(foo)\n";
    assert_eq!(
        shadow_rows(src),
        vec![
            r#"The local variable "foo" is shadowing an already-declared variable at line 4 in the base class "t.gd::A"."#
                .to_owned()
        ]
    );
}

/// First hit wins: the LOCAL declaration stops at the current class and never reaches the base
/// stops. The member above it runs its own walk with `in_local_scope = false`, skips the
/// current-class stop, and lands on `Node` — two rows, which is what Godot emits here.
#[test]
fn the_current_class_stop_wins_for_the_local() {
    let src =
        "extends Node\n\nvar name := 1\n\nfunc f() -> void:\n\tvar name := 2\n\tprint(name)\n";
    assert_eq!(
        shadow_rows(src),
        vec![
            r#"The local variable "name" is shadowing an already-declared property in the base class "Node"."#
                .to_owned(),
            r#"The local variable "name" is shadowing an already-declared variable at line 3 in the current class."#
                .to_owned(),
        ]
    );
}

/// And the globals stop wins over all of them — `name` is not a global, so use one that is.
#[test]
fn the_globals_stop_wins_over_the_base_stops() {
    let src = "extends Node\n\nfunc f() -> void:\n\tvar Node := 1\n\tprint(Node)\n";
    assert_eq!(
        shadow_rows(src),
        Vec::<String>::new(),
        "a global collision is SHADOWED_GLOBAL_IDENTIFIER, which words itself differently"
    );
    assert!(
        warnings(src)
            .iter()
            .any(|m| m == r#"The variable "Node" has the same name as a native class."#),
        "{:?}",
        warnings(src)
    );
}

/// A base with no such member has nothing to report.
#[test]
fn a_base_without_the_name_is_silent() {
    let src = "extends RefCounted\n\nfunc greet(name: String) -> String:\n\treturn name\n";
    assert_eq!(shadow_rows(src), Vec::<String>::new());
}

/// An underscore-prefixed parameter is the idiom for "I know, and I mean it" — but only for the
/// UNUSED_* family. Shadowing is about the NAME, so `_name` simply does not collide.
#[test]
fn an_underscore_prefixed_name_does_not_collide() {
    let src = "extends Node\n\nfunc greet(_name: String) -> String:\n\treturn _name\n";
    assert_eq!(shadow_rows(src), Vec::<String>::new());
}

/// The warning is suppressible by name, like every other one.
#[test]
fn the_warning_is_suppressible() {
    let src = "extends Node\n\n@warning_ignore(\"shadowed_variable_base_class\")\nfunc greet(name: String) -> String:\n\treturn name\n";
    assert_eq!(shadow_rows(src), Vec::<String>::new());
}
