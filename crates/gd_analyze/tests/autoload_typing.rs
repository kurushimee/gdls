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
            name_span: Default::default(),
            line: 1,
            doc: None,
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

    fn autoload_native_type(&self, name: &str) -> Option<String> {
        // A scriptless SCENE autoload — both a PascalCase and a lowercase one, to prove the
        // bare-Node floor fires regardless of casing (the lowercase case is the false-positive the
        // floor closes: without it, `scriptless_thing` falls through to "Identifier not declared").
        if name == "SceneNoScript" || name == "scriptless_thing" {
            Some("Node".to_owned())
        } else {
            None
        }
    }

    fn is_autoload(&self, name: &str) -> bool {
        // Every autoload this mock knows — INCLUDING `unresolved_auto`, which resolves to NO script
        // and NO native type (its scene/uid is unresolvable). That name exercises the pure
        // "not declared" suppression gate (step 10), the path the resolved cases don't reach.
        matches!(
            name,
            "Global" | "SceneNoScript" | "scriptless_thing" | "unresolved_auto"
        )
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

/// Like [`resolve_type_of_identifier`] but returns the whole result so a test can inspect
/// diagnostics (the false-positive checks for the scriptless-Node arm).
fn analyze_src(src: &str, fid: FileId) -> gd_analyze::AnalysisResult {
    let parsed = parse(src);
    let db = native_db();
    let query = AutoloadQuery::new(fid);
    analyze(
        &parsed.tree,
        Some(FileId::new(99)),
        "caller.gd",
        &db,
        &query,
        &policy(),
    )
}

/// Scriptless SCENE autoload → bare NATIVE `Node` (Godot's hard-coded floor). The `SceneNoScript`
/// identifier resolves to a Native type whose `native_type` is `"Node"`, NOT a Script and NOT a
/// degraded/unset type.
#[test]
fn scriptless_scene_autoload_resolves_to_native_node() {
    let fid = FileId::new(42);
    let src = "extends Node\n\nfunc test():\n\tSceneNoScript.add_child(self)\n";
    let dt = resolve_type_of_identifier(src, "SceneNoScript", fid);
    assert_eq!(
        dt.kind,
        DtKind::Native,
        "scriptless scene autoload must resolve to a NATIVE type, got {:?}",
        dt.kind
    );
    assert_eq!(
        dt.native_type, "Node",
        "Godot types a scriptless scene autoload as the hard-coded bare `Node`"
    );
    assert!(
        !dt.is_meta_type,
        "the singleton IS the instance — not a meta type"
    );
}

/// The false positive the native floor closes: a *lowercase*-named scriptless scene autoload must
/// NOT emit `Identifier "…" not declared in the current scope.` (`name.starts_with(uppercase)` does
/// not save a lowercase name from step 10 — the native-floor arm at step 9a must catch it first).
#[test]
fn lowercase_scriptless_autoload_no_not_declared_false_positive() {
    let fid = FileId::new(42);
    let src = "extends Node\n\nfunc test():\n\tscriptless_thing.add_child(self)\n";
    let result = analyze_src(src, fid);
    let offending: Vec<&str> = result
        .diagnostics
        .iter()
        .map(gd_analyze::Diagnostic::message)
        .filter(|m| m.contains("scriptless_thing") && m.contains("not declared"))
        .collect();
    assert!(
        offending.is_empty(),
        "a lowercase scriptless autoload must not be flagged 'not declared'; got: {offending:?}"
    );
    // And it's typed as the bare Node floor.
    let dt = resolve_type_of_identifier(src, "scriptless_thing", fid);
    assert_eq!(dt.kind, DtKind::Native);
    assert_eq!(dt.native_type, "Node");
}

/// The pure `is_autoload` suppression gate (step 10): a registered autoload whose typing could NOT
/// be resolved this pass (unresolvable uid / missing scene — no FileId, no native type) is STILL
/// "declared" in Godot's eyes. A *lowercase*-named one (`unresolved_auto`) must NOT be flagged
/// `Identifier "…" not declared` — the `is_global_like` uppercase gate doesn't save it, only
/// `is_autoload` does. This is the path the resolved (script / native-floor) tests never reach.
#[test]
fn unresolvable_autoload_no_not_declared_false_positive() {
    let fid = FileId::new(42);
    let src = "extends Node\n\nfunc test():\n\tunresolved_auto.foo()\n";
    let result = analyze_src(src, fid);
    let offending: Vec<&str> = result
        .diagnostics
        .iter()
        .map(gd_analyze::Diagnostic::message)
        .filter(|m| m.contains("unresolved_auto") && m.contains("not declared"))
        .collect();
    assert!(
        offending.is_empty(),
        "an unresolvable registered autoload must not be flagged 'not declared'; got: {offending:?}"
    );
}

/// Control: a name NOT registered as any autoload AND lowercase AND not a native member still gets
/// the "not declared" error — proving the `is_autoload` gate is narrow (it suppresses ONLY registered
/// autoloads, not every unresolved lowercase identifier).
#[test]
fn unregistered_lowercase_identifier_still_not_declared() {
    let fid = FileId::new(42);
    let src = "extends Node\n\nfunc test():\n\ttotally_unknown_thing.foo()\n";
    let result = analyze_src(src, fid);
    let flagged = result.diagnostics.iter().any(|d| {
        d.message().contains("totally_unknown_thing") && d.message().contains("not declared")
    });
    assert!(
        flagged,
        "an unregistered lowercase identifier must still be flagged 'not declared' (the gate is \
         autoload-only); diagnostics: {:?}",
        result
            .diagnostics
            .iter()
            .map(gd_analyze::Diagnostic::message)
            .collect::<Vec<_>>()
    );
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

// --- #129: scriptless scene autoload used as a TYPE annotation ---

/// #129: a scriptless SCENE autoload used as a TYPE annotation (`var x: SceneNoScript`) must NOT emit
/// `Could not find type "…"`. Godot's type-position arm early-returns `bad_type` SILENTLY for a
/// registered singleton autoload with no backing script (`gdscript_analyzer.cpp:822-823`) — it does
/// NOT fall through to the "Could not find type" error at `:902`. Verified against the 4.6.3 binary:
/// `var x: <scriptless-scene-autoload>` compiles with exit 0 and no error, while a genuinely-unknown
/// type still reports "Could not find type". Reproduce-first: gdls fell through to the error.
#[test]
fn scriptless_scene_autoload_as_type_no_could_not_find_type() {
    let fid = FileId::new(42);
    let src = "extends Node\n\nfunc test():\n\tvar x: SceneNoScript = null\n\tprint(x)\n";
    let offending: Vec<String> = analyze_src(src, fid)
        .diagnostics
        .iter()
        .map(|d| d.message().to_owned())
        .filter(|m| m.contains("SceneNoScript") && m.contains("Could not find type"))
        .collect();
    assert!(
        offending.is_empty(),
        "a scriptless scene autoload as a type must not emit 'Could not find type'; got: {offending:?}"
    );
}

/// Regression guard (the #78 behavior must be preserved): a SCRIPT-BACKED autoload used as a type
/// annotation (`var x: Global`) still resolves through the script path — no "Could not find type".
#[test]
fn script_backed_autoload_as_type_still_resolves() {
    let fid = FileId::new(42);
    let src = "extends Node\n\nfunc test():\n\tvar x: Global = null\n\tprint(x)\n";
    let offending: Vec<String> = analyze_src(src, fid)
        .diagnostics
        .iter()
        .map(|d| d.message().to_owned())
        .filter(|m| m.contains("Global") && m.contains("Could not find type"))
        .collect();
    assert!(
        offending.is_empty(),
        "a script-backed autoload as a type must resolve (no 'Could not find type'); got: {offending:?}"
    );
}

/// Control: the suppression is autoload-ONLY. A genuinely-unknown type name (not a registered
/// autoload) must STILL emit `Could not find type "…"` (mirroring the 4.6.3 binary positive control).
#[test]
fn unregistered_type_annotation_still_could_not_find_type() {
    let fid = FileId::new(42);
    let src = "extends Node\n\nfunc test():\n\tvar x: TotallyUnknownType999 = null\n\tprint(x)\n";
    let flagged = analyze_src(src, fid).diagnostics.iter().any(|d| {
        d.message().contains("TotallyUnknownType999") && d.message().contains("Could not find type")
    });
    assert!(
        flagged,
        "an unregistered unknown type must still emit 'Could not find type' (the gate is autoload-only)"
    );
}
