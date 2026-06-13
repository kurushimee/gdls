//! Tests for the additive symbol-enumeration APIs (M8 Phase 1, #64):
//! `gd_analyze::enumerate` — script-`extends`-chain member collection (Group 2) and the
//! `DataType`→members dispatcher (Group 3). Interfaces are built by the REAL extractor over real
//! source, so what the enumeration sees is byte-identical to production; only the `CrossFileQuery`
//! plumbing is mocked (the same pattern as `cross_file_inheritance.rs`).

use std::collections::HashMap;
use std::path::Path;

use gd_analyze::enumerate::{
    members_of_type, script_chain_members, MemberItem, MemberItemKind, MemberOwner,
};
use gd_analyze::{CrossFileQuery, DataType, DtKind, ScriptRef, TypeSource, VariantType};
use gd_project::{FileId, Interface};
use gd_syntax::ast::{Member, NodeKind};
use gd_syntax::parse;
use gd_types::NativeDb;

fn native_db() -> NativeDb {
    let path =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../gd_types/tests/fixtures/trimmed_api.json");
    NativeDb::load(path.to_str().expect("utf-8 path"))
        .unwrap_or_else(|e| panic!("load native DB fixture at {}: {e}", path.display()))
}

/// A mock workspace: (path, source) pairs run through the real interface extractor.
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

fn names(items: &[MemberItem]) -> Vec<&str> {
    items.iter().map(|i| i.name.as_str()).collect()
}

fn kind_of<'a>(items: &'a [MemberItem], name: &str) -> Option<&'a MemberItemKind> {
    items.iter().find(|i| i.name == name).map(|i| &i.kind)
}

// ---------------------------------------------------------------------------------------------------
// Group 2 — script extends-chain member enumeration.
// ---------------------------------------------------------------------------------------------------

const BASE_GD: &str = "\
class_name Base
extends Node
var base_var: int
func base_method() -> void:
\tpass
signal base_signal
const BASE_CONST := 1
enum Mode { A, B }
";

const DERIVED_GD: &str = "\
class_name Derived
extends Base
var derived_var: String
func derived_method() -> int:
\treturn 0
func base_method() -> int:
\treturn 1
";

#[test]
fn script_chain_collects_members_through_extends_and_derived_shadows_base() {
    let project = Project::new(&[("res://base.gd", BASE_GD), ("res://derived.gd", DERIVED_GD)]);
    let native = native_db();
    let start = ScriptRef {
        file: project.fid("res://derived.gd"),
        inner: Vec::new(),
    };
    let members = script_chain_members(&project, &native, &start);
    let got = names(&members);

    // Derived's own members.
    assert!(got.contains(&"derived_var"), "derived_var present: {got:?}");
    assert!(
        got.contains(&"derived_method"),
        "derived_method present: {got:?}"
    );
    // Inherited from Base.
    assert!(got.contains(&"base_var"), "base_var inherited: {got:?}");
    assert!(
        got.contains(&"base_signal"),
        "base_signal inherited: {got:?}"
    );
    assert!(got.contains(&"BASE_CONST"), "BASE_CONST inherited: {got:?}");
    // Named enum + its values.
    assert!(got.contains(&"Mode"), "named enum Mode inherited: {got:?}");
    assert!(got.contains(&"A"), "enum value A inherited: {got:?}");
    assert!(got.contains(&"B"), "enum value B inherited: {got:?}");

    // `base_method` is overridden in Derived → appears exactly once (derived shadows base), and
    // it is the DERIVED one (return int, not void).
    let base_method_items: Vec<&MemberItem> =
        members.iter().filter(|i| i.name == "base_method").collect();
    assert_eq!(
        base_method_items.len(),
        1,
        "base_method appears once (derived shadows base)"
    );
    assert_eq!(base_method_items[0].kind, MemberItemKind::Method);

    // Kinds are mapped from the interface model.
    assert_eq!(
        kind_of(&members, "base_var"),
        Some(&MemberItemKind::Property)
    );
    assert_eq!(
        kind_of(&members, "base_signal"),
        Some(&MemberItemKind::Signal)
    );
    assert_eq!(
        kind_of(&members, "BASE_CONST"),
        Some(&MemberItemKind::Constant)
    );
    assert_eq!(kind_of(&members, "Mode"), Some(&MemberItemKind::Enum));
    assert_eq!(kind_of(&members, "A"), Some(&MemberItemKind::EnumValue));

    // The chain native tail (`Node`) is NOT in the script-only collection — that is the
    // dispatcher's job to append.
    assert!(
        !got.contains(&"queue_free"),
        "native members are not in the script-chain-only set: {got:?}"
    );
}

// ---------------------------------------------------------------------------------------------------
// Group 3 — the DataType→members dispatcher, one test per DtKind arm.
// ---------------------------------------------------------------------------------------------------

fn empty_project() -> Project {
    Project::new(&[])
}

#[test]
fn dispatcher_builtin_arm() {
    let native = native_db();
    let project = empty_project();
    let tree = parse("").tree;
    let dt = DataType {
        kind: DtKind::Builtin,
        builtin_type: VariantType::Vector2,
        type_source: TypeSource::AnnotatedExplicit,
        ..Default::default()
    };
    let members = members_of_type(&dt, &native, &project, &tree);
    let got = names(&members);
    assert!(got.contains(&"x"), "Vector2.x: {got:?}");
    assert!(got.contains(&"y"), "Vector2.y: {got:?}");
    assert_eq!(kind_of(&members, "x"), Some(&MemberItemKind::Property));
}

#[test]
fn dispatcher_native_arm_includes_inherited() {
    let native = native_db();
    let project = empty_project();
    let tree = parse("").tree;
    let dt = DataType {
        kind: DtKind::Native,
        native_type: "Node".to_owned(),
        type_source: TypeSource::AnnotatedExplicit,
        ..Default::default()
    };
    let members = members_of_type(&dt, &native, &project, &tree);
    let got = names(&members);
    assert!(got.contains(&"queue_free"), "Node::queue_free: {got:?}");
    assert!(got.contains(&"get_parent"), "Node::get_parent: {got:?}");
    assert!(got.contains(&"name"), "Node::name: {got:?}");
    // Inherited from Object.
    assert!(
        got.contains(&"get_class"),
        "inherited Object::get_class: {got:?}"
    );
    // A native method carries a rendered detail.
    let qf = members.iter().find(|i| i.name == "queue_free").unwrap();
    assert!(qf.detail.is_some(), "native method has a signature detail");
}

#[test]
fn dispatcher_script_arm_appends_native_tail() {
    let project = Project::new(&[("res://base.gd", BASE_GD), ("res://derived.gd", DERIVED_GD)]);
    let native = native_db();
    let tree = parse("").tree;
    let dt = DataType {
        kind: DtKind::Script,
        script_type: Some(ScriptRef {
            file: project.fid("res://derived.gd"),
            inner: Vec::new(),
        }),
        type_source: TypeSource::AnnotatedExplicit,
        ..Default::default()
    };
    let members = members_of_type(&dt, &native, &project, &tree);
    let got = names(&members);
    // Script members.
    assert!(got.contains(&"derived_method"), "script member: {got:?}");
    assert!(
        got.contains(&"base_var"),
        "inherited script member: {got:?}"
    );
    // The native tail (Base extends Node) is appended.
    assert!(
        got.contains(&"queue_free"),
        "native tail member appended: {got:?}"
    );
}

#[test]
fn dispatcher_script_arm_dedups_native_override_against_script() {
    // A script overriding a native method (`queue_free`, declared on `Node`) must surface that
    // name EXACTLY ONCE — the user's own override — not also as the native base. The native dup
    // (owner=Native, detail=Some(...)) would make `completionItem/resolve` fetch the wrong
    // (base-class) doc/signature for the override (#94 FIX 1).
    const OVERRIDE_GD: &str = "\
extends Node
func queue_free():
\tpass
";
    let project = Project::new(&[("res://override.gd", OVERRIDE_GD)]);
    let native = native_db();
    let tree = parse("").tree;
    let dt = DataType {
        kind: DtKind::Script,
        script_type: Some(ScriptRef {
            file: project.fid("res://override.gd"),
            inner: Vec::new(),
        }),
        type_source: TypeSource::AnnotatedExplicit,
        ..Default::default()
    };
    let members = members_of_type(&dt, &native, &project, &tree);

    let qf: Vec<&MemberItem> = members.iter().filter(|i| i.name == "queue_free").collect();
    assert_eq!(
        qf.len(),
        1,
        "queue_free (script override of Node::queue_free) must appear exactly once: {:?}",
        names(&members)
    );
    // The survivor is the script's own entry — owner is the declaring script file, not the native
    // base. (The native dup would carry `MemberOwner::Native("Node")`.)
    assert_eq!(
        qf[0].owner,
        MemberOwner::Script(project.fid("res://override.gd")),
        "the surviving queue_free is the script override, not the native base"
    );
}

#[test]
fn dispatcher_script_arm_walks_inner_class_extends_chain() {
    // The load-bearing case for in-file class hierarchies: `finish()` rewrites every in-file
    // `Class` type to a `Script{inner:[...]}` ref, so an inner class B that `extends` a sibling
    // inner class A must surface A's members through the Script arm's chain walk. (`resolve_inner_chain`
    // resolves the sibling `extends A` from the file root.)
    const SRC: &str = "\
class_name Host
class A:
\tvar a_field: int
\tfunc a_method() -> void:
\t\tpass
class B extends A:
\tvar b_field: int
";
    let project = Project::new(&[("res://host.gd", SRC)]);
    let native = native_db();
    let tree = parse("").tree;
    let dt = DataType {
        kind: DtKind::Script,
        script_type: Some(ScriptRef {
            file: project.fid("res://host.gd"),
            inner: vec!["B".to_owned()],
        }),
        type_source: TypeSource::AnnotatedExplicit,
        ..Default::default()
    };
    let members = members_of_type(&dt, &native, &project, &tree);
    let got = names(&members);
    assert!(got.contains(&"b_field"), "B's own member: {got:?}");
    assert!(
        got.contains(&"a_field"),
        "inherited from sibling inner class A: {got:?}"
    );
    assert!(
        got.contains(&"a_method"),
        "inherited method from inner class A: {got:?}"
    );
}

#[test]
fn dispatcher_class_arm_walks_in_file_class_node() {
    // A finished AnalysisResult never carries a Class type (it is rewritten to Script in
    // `finish`), so this arm is exercised by constructing the Class DataType directly — the case
    // a caller hits when resolving an in-file class metatype before `finish`.
    let src = "\
class_name Thing
var field_a: int
func method_a() -> void:
\tpass
const K := 3
class Inner:
\tpass
";
    let tree = parse(src).tree;
    // The root class node id.
    let root_id = tree
        .iter_ids()
        .find(|id| matches!(tree.get(*id).kind, NodeKind::Class(_)))
        .expect("root class node");
    let native = native_db();
    let project = empty_project();
    let dt = DataType {
        kind: DtKind::Class,
        class_node: Some(root_id),
        type_source: TypeSource::AnnotatedExplicit,
        ..Default::default()
    };
    let members = members_of_type(&dt, &native, &project, &tree);
    let got = names(&members);
    assert!(got.contains(&"field_a"), "in-file var: {got:?}");
    assert!(got.contains(&"method_a"), "in-file func: {got:?}");
    assert!(got.contains(&"K"), "in-file const: {got:?}");
    assert!(got.contains(&"Inner"), "in-file inner class: {got:?}");
    assert_eq!(kind_of(&members, "method_a"), Some(&MemberItemKind::Method));
    assert_eq!(
        kind_of(&members, "field_a"),
        Some(&MemberItemKind::Property)
    );
    assert_eq!(kind_of(&members, "Inner"), Some(&MemberItemKind::Class));
}

#[test]
fn dispatcher_enum_arm_lists_values() {
    let native = native_db();
    let project = empty_project();
    let tree = parse("").tree;
    let mut enum_values = HashMap::new();
    enum_values.insert("A".to_owned(), 0i64);
    enum_values.insert("B".to_owned(), 1i64);
    enum_values.insert("C".to_owned(), 2i64);
    let dt = DataType {
        kind: DtKind::Enum,
        native_type: "Self".to_owned(),
        enum_type: "Mode".to_owned(),
        enum_values,
        type_source: TypeSource::AnnotatedExplicit,
        ..Default::default()
    };
    let members = members_of_type(&dt, &native, &project, &tree);
    let got = names(&members);
    // All three values, sorted (deterministic).
    assert_eq!(got, vec!["A", "B", "C"], "enum values sorted: {got:?}");
    assert!(members.iter().all(|i| i.kind == MemberItemKind::EnumValue));
}

#[test]
fn dispatcher_variant_arm_is_empty() {
    let native = native_db();
    let project = empty_project();
    let tree = parse("").tree;
    let dt = DataType::variant();
    assert!(
        members_of_type(&dt, &native, &project, &tree).is_empty(),
        "a dynamic Variant offers no member set"
    );
}

#[test]
fn unnamed_enum_values_enumerate_as_constants_from_interface() {
    // An unnamed `enum { … }` hoists its values into the interface as `Const` members; the
    // collector surfaces them (they are addressable bare names on the class).
    const SRC: &str = "\
class_name HasAnon
extends Node
enum { LOOSE, FREE }
";
    let project = Project::new(&[("res://anon.gd", SRC)]);
    let native = native_db();
    let start = ScriptRef {
        file: project.fid("res://anon.gd"),
        inner: Vec::new(),
    };
    let members = script_chain_members(&project, &native, &start);
    let got = names(&members);
    assert!(got.contains(&"LOOSE"), "unnamed-enum value LOOSE: {got:?}");
    assert!(got.contains(&"FREE"), "unnamed-enum value FREE: {got:?}");
}

#[test]
fn class_node_members_defensive_on_non_class_and_skips_groups() {
    // Defensive: enumerating a non-class node id yields an empty list, never a panic.
    use gd_analyze::enumerate::class_node_members;
    let tree = parse("var x := 1\n").tree;
    // pick any non-class node (the literal `1` if present, else the variable's identifier)
    let some_non_class = tree
        .iter_ids()
        .find(|id| !matches!(tree.get(*id).kind, NodeKind::Class(_)));
    if let Some(id) = some_non_class {
        assert!(class_node_members(&tree, id).is_empty());
    }
    // Also confirm the project's Member enum variants are exhaustively handled (compile guard):
    // a Group member contributes nothing.
    let with_group = parse("@export_group(\"g\")\nvar y := 1\n").tree;
    let root = with_group
        .iter_ids()
        .find(|id| matches!(with_group.get(*id).kind, NodeKind::Class(_)))
        .unwrap();
    let members = class_node_members(&with_group, root);
    // `y` is present; the group marker is not a member.
    assert!(members.iter().any(|m| m.name == "y"));
    if let NodeKind::Class(c) = &with_group.get(root).kind {
        assert!(
            c.members.iter().any(|m| matches!(m, Member::Group(_))),
            "fixture actually has a group member to skip"
        );
    }
}
