//! Unit tests for autoload-singleton typing.
//!
//! Verifies that when a `CrossFileQuery` reports an autoload name → FileId, and that file's
//! `Interface` declares members, identifier resolution of the autoload name yields a Script
//! INSTANCE type (not dynamic `Variant`) — so `Global.popup_error("x")` resolves `popup_error`
//! through the instance type rather than degrading to Variant.

use std::path::Path;

use gd_analyze::{
    analyze, CrossFileQuery, DataType, DtKind, MemberXref, StrictSettings, WarnPolicy,
};
use gd_project::{FileId, Interface, MemberDecl, MemberKind, TypeExpr};
use gd_syntax::parse;
use gd_types::NativeDb;

/// The committed native-DB fixture, loaded once.
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
    )
}

/// A mock CrossFileQuery that knows one autoload name "Global" → a FileId, and provides its
/// Interface with a `func popup_error(msg: String) -> void` member.
struct AutoloadQuery {
    autoload_fid: FileId,
    autoload_iface: Interface,
}

impl AutoloadQuery {
    fn new(fid: FileId) -> Self {
        // Build a minimal interface with one func member: popup_error(msg: String) -> void
        let popup_error = MemberDecl {
            name: "popup_error".to_string(),
            kind: MemberKind::Func,
            ty: TypeExpr::None, // return void
            params: vec![TypeExpr::Named {
                path: vec!["String".to_string()],
                args: vec![],
            }],
            param_names: vec!["msg".to_string()],
            required_params: 1,
            flags: Default::default(),
            span: Default::default(),
            line: 1,
        };
        let iface = Interface {
            class_name: None,
            members: vec![popup_error],
            ..Default::default()
        };
        AutoloadQuery {
            autoload_fid: fid,
            autoload_iface: iface,
        }
    }
}

impl CrossFileQuery for AutoloadQuery {
    fn global_class_file(&self, _name: &str) -> Option<FileId> {
        None
    }

    fn interface(&self, file: FileId) -> Option<&Interface> {
        if file == self.autoload_fid {
            Some(&self.autoload_iface)
        } else {
            None
        }
    }

    fn resolve_res_path(&self, _path: &str) -> Option<FileId> {
        None
    }

    fn autoload_file(&self, name: &str) -> Option<FileId> {
        if name == "Global" {
            Some(self.autoload_fid)
        } else {
            None
        }
    }

    fn member_initializer_xrefs(&self, _file: FileId, _member: &str) -> Vec<MemberXref> {
        Vec::new()
    }
}

/// Helper: analyze with AutoloadQuery and find the type of the first identifier named `target_name`
/// in the source that has a set type (by scanning NodeKind::Identifier, last-set wins).
fn resolve_type_of_identifier(src: &str, target_name: &str, fid: FileId) -> DataType {
    let parsed = parse(src);
    let tree = parsed.tree;
    let db = native_db();
    let query = AutoloadQuery::new(fid);
    let result = analyze(
        &tree,
        Some(FileId::new(99)),
        "caller.gd",
        &db,
        &query,
        &policy(),
    );

    // Walk the parse tree to find an Identifier node with the target name.
    use gd_syntax::ast::NodeKind;
    let mut found = DataType::default();
    for node_id in tree.iter_ids() {
        if let NodeKind::Identifier(ident) = &tree.get(node_id).kind {
            if ident.name == target_name {
                let dt = result.types.get(node_id);
                if dt.is_set() {
                    found = dt.clone();
                }
            }
        }
    }
    found
}

/// Primary test: `Global.popup_error("x")` — the `Global` identifier must resolve to a Script
/// instance type (DtKind::Script, is_meta_type=false), not a degraded Variant.
#[test]
fn autoload_identifier_resolves_to_script_instance_type() {
    let global_fid = FileId::new(42);
    // A caller that references `Global.popup_error("x")`.
    let src = "extends Node\n\nfunc test():\n\tGlobal.popup_error(\"x\")\n";
    let dt = resolve_type_of_identifier(src, "Global", global_fid);
    assert_eq!(
        dt.kind,
        DtKind::Script,
        "autoload `Global` must resolve to DtKind::Script, got {:?}",
        dt.kind
    );
    assert!(
        !dt.is_meta_type,
        "autoload `Global` must be an INSTANCE type (is_meta_type=false), got is_meta_type=true"
    );
    let script_ref = dt
        .script_type
        .as_ref()
        .expect("Script type must have a ScriptRef");
    assert_eq!(
        script_ref.file, global_fid,
        "ScriptRef must point to the autoload's FileId"
    );
}

/// Shadowing test: a local `var Global = 1` inside a function must shadow the autoload.
/// The local identifier resolves as an integer (DtKind::Builtin), not the autoload script.
#[test]
fn local_var_shadows_autoload() {
    let global_fid = FileId::new(42);
    // A function with a local variable named `Global`.
    let src = "extends Node\n\nfunc test():\n\tvar Global = 1\n\tprint(Global)\n";
    // After shadowing, `Global` in `print(Global)` should be the int-typed local, not the autoload.
    let dt = resolve_type_of_identifier(src, "Global", global_fid);
    // The type of the local `var Global = 1` should be an integer (DtKind::Builtin) or Variant
    // (if inferred from literal `1` without explicit type), but NOT DtKind::Script.
    assert_ne!(
        dt.kind,
        DtKind::Script,
        "local `var Global` must shadow the autoload; the type of `Global` in print() must NOT be Script, got {:?}",
        dt.kind
    );
}

/// Class member shadowing: a class-level `var Global := 1` must shadow the autoload.
/// The reference in a method body resolves to the member type, not the autoload script.
#[test]
fn class_member_shadows_autoload() {
    let global_fid = FileId::new(42);
    let src = "extends Node\nvar Global := 1\n\nfunc test():\n\tprint(Global)\n";
    let dt = resolve_type_of_identifier(src, "Global", global_fid);
    assert_ne!(
        dt.kind,
        DtKind::Script,
        "class member `var Global` must shadow the autoload; type must NOT be Script, got {:?}",
        dt.kind
    );
}

/// Autoload name that doesn't match any declared autoload does not affect identifier resolution.
/// (NoCrossFile-equivalent behavior: unknown names degrade normally.)
#[test]
fn unknown_name_not_typed_as_autoload() {
    let global_fid = FileId::new(42);
    // Use a name not registered as an autoload.
    let src = "extends Node\n\nfunc test():\n\tOtherSingleton.do_something()\n";
    let dt = resolve_type_of_identifier(src, "OtherSingleton", global_fid);
    // Should NOT be DtKind::Script — it's not an autoload, so it should stay unresolved.
    assert_ne!(
        dt.kind,
        DtKind::Script,
        "non-autoload identifier must not be resolved as Script, got {:?}",
        dt.kind
    );
}
