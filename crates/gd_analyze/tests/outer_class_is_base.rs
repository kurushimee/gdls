//! #488 — an outer class contributes only what a lexical scope legitimately contributes.
//!
//! `reduce_identifier` walks the full scope: the class, its inheritance chain, and its outer chain
//! (`get_class_node_current_scope_classes`, `gdscript_analyzer.cpp:320-344`). Godot then gates the
//! VARIABLE, SIGNAL, and FUNCTION arms on `is_base` (`:4228`, `:4238`, `:4247`), which goes false
//! the moment the walk steps off the inheritance chain (`:4269-4275`) — so an inner class can read
//! an outer class's constants, enums, enum values, and inner classes by bare name, but not its
//! instance members. The constant and enum arms are ungated, which is what the #314 lookup and the
//! enum-shadowing behavior rely on.
//!
//! Every row is pinned against `Godot_v4.7.2-stable --headless --check-only`.

use gd_analyze::warn_policy::{StrictSettings, WarnPolicy};
use gd_analyze::NoCrossFile;
use gd_project::WarningConfig;
use gd_syntax::Dialect;
use gd_types::NativeDb;

fn mini_native() -> NativeDb {
    NativeDb::from_json(
        r#"{
            "header": {"version_major": 4, "version_minor": 7, "version_patch": 2},
            "classes": [
                {"name": "Object"},
                {"name": "RefCounted", "inherits": "Object"},
                {"name": "Node", "inherits": "Object"}
            ]
        }"#,
    )
    .expect("valid mini dump")
}

fn errors(src: &str) -> Vec<String> {
    let tree = gd_syntax::parse(src).tree;
    let policy = WarnPolicy::build(
        &WarningConfig::default(),
        &StrictSettings::default(),
        Dialect::DEFAULT,
    );
    let result = gd_analyze::analyze(&tree, None, "t.gd", &mini_native(), &NoCrossFile, &policy);
    result
        .diagnostics
        .iter()
        .filter(|d| d.warning_code().is_none())
        .map(|d| d.message().to_owned())
        .collect()
}

fn undeclared(name: &str) -> String {
    format!(r#"Identifier "{name}" not declared in the current scope."#)
}

const OUTER: &str = "\
extends Node

static func osf() -> void:
	pass

func oif() -> void:
	pass

var ov := 2
const OC := 3
enum OE { A, B }
signal osig

class OInner:
	pass
";

fn in_inner(body: &str) -> String {
    format!("{OUTER}\nclass Inner:\n\tfunc f() -> void:\n\t\t{body}\n")
}

/// The `is_base`-gated arms. A static function is gated too — `is_base` is about where the walk
/// is, not about the member's own staticness.
#[test]
fn an_outer_instance_member_is_not_in_scope() {
    for name in ["osf", "oif", "ov", "osig"] {
        assert_eq!(
            errors(&in_inner(&format!("var x = {name}"))),
            vec![undeclared(name)],
            "{name}"
        );
    }
}

/// The ungated arms. Reading an outer constant, enum, enum value, or inner class by bare name is
/// what the #314 cross-file lookup and the enum-shadowing behavior are built on.
#[test]
fn an_outer_constant_enum_or_inner_class_stays_in_scope() {
    for body in ["var x = OC", "var x = OE", "var x = OE.A", "var x = OInner"] {
        assert_eq!(errors(&in_inner(body)), Vec::<String>::new(), "{body}");
    }
}

/// `is_base` follows the INHERITANCE chain, so a member inherited from an in-file base is in
/// scope even though that base is declared as a sibling inner class.
#[test]
fn an_inherited_member_stays_in_scope() {
    let src = "\
extends Node

class Base:
	var bv := 1
	func bf() -> void:
		pass

class Inner extends Base:
	func f() -> void:
		var a = bv
		var b = bf
";
    assert_eq!(errors(src), Vec::<String>::new());
}

/// The two halves in one file: the inherited member resolves, the head's own does not.
#[test]
fn inheritance_and_outer_are_told_apart() {
    let src = "\
extends Node

var hv := 1

class Mid:
	var mv := 2

class Inner extends Mid:
	func f() -> void:
		var a = mv
		var b = hv
";
    assert_eq!(errors(src), vec![undeclared("hv")]);
}

/// The head class's own members are unaffected — `is_base` starts true, so the first class in the
/// walk always answers. Getting this wrong would break every ordinary member read in the language.
#[test]
fn a_classs_own_members_still_resolve() {
    let src = "\
extends Node

var v := 1
signal s
func f() -> void:
	pass

func g() -> void:
	var a = v
	var b = s
	var c = f
";
    assert_eq!(errors(src), Vec::<String>::new());
}
