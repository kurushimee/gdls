//! Regression tests for navigation-surface correctness bugs — workspace/symbol URI encoding and
//! call-hierarchy callee attribution — found while hardening the M4 nav features.
//!
//! Each test pins a specific bug so a future regression (re-introduction of the same class of
//! bug) fails the suite rather than silently under-reporting through the LSP.
//!
//! Coverage map:
//!   - **xfile URI encoding**: `lsp_responds_under_space_containing_project_path` exercises
//!     the LSP end-to-end against a project whose root contains a space, asserting that the
//!     `workspace/symbol` URI comes back percent-encoded (matching the cache writer's key shape).
//!     The WP-R2 cross-file cycle this encoding once silently disabled is now pinned end-to-end in
//!     `tests/cache_coherence.rs`; the unit-level key-agreement coverage lives in `xfile.rs`'s and
//!     `uri.rs`'s own `#[cfg(test)] mod tests`. The raw-vs-percent-encoded key drift is now a
//!     compile-time impossibility via the `uri::CanonicalKey` newtype (the old `cache_keys`
//!     dual-probe is gone).
//!   - **inherited-callee classification**: `inherited_bare_call_records_non_script_callee`
//!     parses a file that calls `_ready()` (inherited from `Node`) and asserts the recorded
//!     `Binding::Call` callee never classifies as a Script callee of the calling file.
//!   - **class-entry line** (#33): `workspace_symbol_anchors_class_at_declaration_line` pins
//!     that a `class_name` on line ≥ 2 anchors at its declaration, not file top — the registry
//!     used to store no line and the handler hardcoded line 1.
//!   - **watcher-channel death**: not pinned here (intentional gap — see the
//!     module-level comment block below). Adding a regression test would require refactoring
//!     `gd_server::serve` to accept an injectable watcher receiver; that refactor is M5 scope.

mod common;

use common::{notification, recv, request, shutdown, MINI_API};
use lsp_server::{Connection, Message};
use lsp_types::{
    InitializeParams, InitializedParams, WorkspaceSymbolParams, WorkspaceSymbolResponse,
};

// ============================================================================
// xfile URI percent-encoding seam
// ============================================================================

/// End-to-end LSP smoke under a project rooted at a space-containing path. The original bug: the
/// xfile reader derived a raw, un-encoded path candidate that never matched the percent-encoded
/// keys the LSP wire produces (`%20` for the space), silently disabling
/// `WorkspaceXFileQuery::member_initializer_xrefs` — and with it the WP-R2 cross-file mutual-member
/// cycle check — on every project under a path with a space. The fix routes every cache key
/// through `uri::CanonicalKey` (`for_uri` for the writer, `for_path` for the reader, equal by
/// construction). The unit test `xfile.rs::tests::returns_xrefs_when_project_path_contains_a_space`
/// proves the wrapper-level key agreement; this test wires the full LSP path so a future regression
/// in URI canonicalization surfaces on a real `serve` run.
#[test]
fn lsp_responds_under_space_containing_project_path() {
    let dir = tempfile::Builder::new()
        .prefix("gdls test ")
        .tempdir()
        .expect("create temp dir");
    let root =
        camino::Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("temp dir is UTF-8");
    // Pre-flight: the space must actually be present in the path. Some platforms / temp
    // implementations may strip or substitute it; surface that as a skip rather than a
    // false-positive pass.
    if !root.as_str().contains(' ') {
        eprintln!(
            "skipping: temp dir lacks a space ({root}); platform substitutes the prefix character"
        );
        return;
    }

    std::fs::write(root.join("project.godot"), "config_version=5\n").unwrap();
    std::fs::write(root.join("extension_api.json"), MINI_API).unwrap();
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(
        root.join("src/hero.gd"),
        "class_name Hero\nextends Node2D\n",
    )
    .unwrap();

    let (server, client) = Connection::memory();
    let server_thread = std::thread::spawn(move || gd_server::serve(server));

    let init = InitializeParams {
        initialization_options: Some(serde_json::json!({
            "projectRoot": root.as_str(),
            "autoDumpExtensionApi": false,
            "extensionApiPath": root.join("extension_api.json").as_str(),
        })),
        ..Default::default()
    };
    client.sender.send(request(1, "initialize", init)).unwrap();
    let _ = recv(&client);
    client
        .sender
        .send(notification("initialized", InitializedParams {}))
        .unwrap();

    client
        .sender
        .send(request(
            2,
            "workspace/symbol",
            WorkspaceSymbolParams {
                query: "Hero".to_string(),
                ..Default::default()
            },
        ))
        .unwrap();

    let resp = recv(&client);
    let Message::Response(r) = resp else {
        panic!("expected Response, got {resp:?}");
    };
    let result: WorkspaceSymbolResponse = serde_json::from_value(r.result.expect("ok result"))
        .expect("workspace/symbol returns a WorkspaceSymbolResponse");
    let WorkspaceSymbolResponse::Flat(symbols) = result else {
        panic!("expected Flat shape");
    };
    let hero = symbols
        .iter()
        .find(|s| s.name == "Hero")
        .expect("Hero must be visible in workspace/symbol");
    assert!(
        hero.location.uri.as_str().contains("%20"),
        "Hero URI under a space-containing project root must be percent-encoded; got {:?}",
        hero.location.uri
    );

    shutdown(&client, server_thread);
    drop(dir);
}

// ============================================================================
// workspace/symbol class entries anchor at the class_name declaration (#33)
// ============================================================================

/// The common `extends`-first script shape puts `class_name` on line 2 (or later). The registry
/// used to store only the declaring path, and `workspace_symbol` hardcoded line 1 — every class
/// result anchored at file top, only accidentally correct for line-1 declarations. The fix
/// records the identifier's line on the `ClassEntry` at index time; this pins the rendered
/// 0-based LSP line for a line-3 declaration.
#[test]
fn workspace_symbol_anchors_class_at_declaration_line() {
    let dir = tempfile::tempdir().expect("create temp dir");
    let root =
        camino::Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("temp dir is UTF-8");

    std::fs::write(root.join("project.godot"), "config_version=5\n").unwrap();
    std::fs::write(root.join("extension_api.json"), MINI_API).unwrap();
    std::fs::write(
        root.join("knight.gd"),
        "# leading comment\nextends Node2D\nclass_name Knight\n",
    )
    .unwrap();

    let (server, client) = Connection::memory();
    let server_thread = std::thread::spawn(move || gd_server::serve(server));

    let init = InitializeParams {
        initialization_options: Some(serde_json::json!({
            "projectRoot": root.as_str(),
            "autoDumpExtensionApi": false,
            "extensionApiPath": root.join("extension_api.json").as_str(),
        })),
        ..Default::default()
    };
    client.sender.send(request(1, "initialize", init)).unwrap();
    let _ = recv(&client);
    client
        .sender
        .send(notification("initialized", InitializedParams {}))
        .unwrap();

    client
        .sender
        .send(request(
            2,
            "workspace/symbol",
            WorkspaceSymbolParams {
                query: "Knight".to_string(),
                ..Default::default()
            },
        ))
        .unwrap();

    let resp = recv(&client);
    let Message::Response(r) = resp else {
        panic!("expected Response, got {resp:?}");
    };
    let result: WorkspaceSymbolResponse = serde_json::from_value(r.result.expect("ok result"))
        .expect("workspace/symbol returns a WorkspaceSymbolResponse");
    let WorkspaceSymbolResponse::Flat(symbols) = result else {
        panic!("expected Flat shape");
    };
    let knight = symbols
        .iter()
        .find(|s| s.name == "Knight")
        .expect("Knight must be visible in workspace/symbol");
    assert_eq!(
        knight.location.range.start.line, 2,
        "class_name on source line 3 must render LSP line 2, not the file top"
    );
    // The range covers exactly the `Knight` identifier (`class_name Knight` → cols 11..17),
    // not a zero-width point at column 0.
    assert_eq!(knight.location.range.start.character, 11);
    assert_eq!(knight.location.range.end.character, 17);
    assert_eq!(knight.location.range.end.line, 2);

    shutdown(&client, server_thread);
    drop(dir);
}

/// Every `workspace/symbol` result's range must cover the symbol's NAME token (the spec reads
/// the range to reveal/select the hit; a zero-width point at column 0 lands the caret on
/// leading syntax like `var ` or indentation). One fixture per member kind, each sliced back
/// out of the source line by the returned character extent.
#[test]
fn workspace_symbol_ranges_cover_the_name_token() {
    let dir = tempfile::tempdir().expect("create temp dir");
    let root =
        camino::Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("temp dir is UTF-8");

    let src = "class_name Arsenal\n\
               extends Node2D\n\
               const MAX_AMMO := 30\n\
               var speed: float = 1.0\n\
               signal fired(power: int)\n\
               func reload(clip: int) -> bool:\n\
               \treturn clip > 0\n\
               enum Mode { SAFE, BURST }\n";
    std::fs::write(root.join("project.godot"), "config_version=5\n").unwrap();
    std::fs::write(root.join("extension_api.json"), MINI_API).unwrap();
    std::fs::write(root.join("arsenal.gd"), src).unwrap();

    let (server, client) = Connection::memory();
    let server_thread = std::thread::spawn(move || gd_server::serve(server));

    let init = InitializeParams {
        initialization_options: Some(serde_json::json!({
            "projectRoot": root.as_str(),
            "autoDumpExtensionApi": false,
            "extensionApiPath": root.join("extension_api.json").as_str(),
        })),
        ..Default::default()
    };
    client.sender.send(request(1, "initialize", init)).unwrap();
    let _ = recv(&client);
    client
        .sender
        .send(notification("initialized", InitializedParams {}))
        .unwrap();

    let lines: Vec<&str> = src.lines().collect();
    for (id, name) in ["Arsenal", "MAX_AMMO", "speed", "fired", "reload", "Mode"]
        .iter()
        .enumerate()
    {
        client
            .sender
            .send(request(
                id as i32 + 2,
                "workspace/symbol",
                WorkspaceSymbolParams {
                    query: name.to_string(),
                    ..Default::default()
                },
            ))
            .unwrap();
        let resp = recv(&client);
        let Message::Response(r) = resp else {
            panic!("expected Response, got {resp:?}");
        };
        let result: WorkspaceSymbolResponse =
            serde_json::from_value(r.result.expect("ok result")).expect("response shape");
        let WorkspaceSymbolResponse::Flat(symbols) = result else {
            panic!("expected Flat shape");
        };
        let hit = symbols
            .iter()
            .find(|s| s.name == *name)
            .unwrap_or_else(|| panic!("`{name}` must be visible in workspace/symbol"));
        let range = hit.location.range;
        assert_ne!(
            range.start, range.end,
            "`{name}`: no result may carry a zero-width range"
        );
        assert_eq!(
            range.start.line, range.end.line,
            "`{name}`: single-line token"
        );
        // The fixture is ASCII, so character offsets equal byte offsets within the line.
        let line = lines[range.start.line as usize];
        assert_eq!(
            &line[range.start.character as usize..range.end.character as usize],
            *name,
            "`{name}`: the range must slice exactly the name token out of {line:?}"
        );
    }

    shutdown(&client, server_thread);
    drop(dir);
}

// ============================================================================
// Non-Script callee classification for non-lexically-anchored bare calls
// ============================================================================

/// A bare call to `_ready()` from `extends Node` dispatches to `Node._ready` — a native method
/// on a class declared in `extension_api.json`, not in this file. The original recording bug
/// tagged every reached call with this file as the callee, so `callHierarchy/incomingCalls` of
/// `Node._ready` missed every site and `outgoingCalls` rendered the call as an in-file
/// self-pointer. The consolidated recording site derives the callee target from the resolution
/// the dispatch actually used: under MINI_API's method-less dump the native lookup misses, so
/// the call classifies `Unresolved` (never `Script{this file}`); with a methods-bearing dump it
/// classifies `Native` (companion test below).
#[test]
fn inherited_bare_call_records_non_script_callee() {
    use gd_analyze::{analyze, Binding, StrictSettings, SyntacticQuery, WarnPolicy};
    use gd_project::WarningConfig;

    let source = "extends Node\n\nfunc start() -> void:\n\t_ready()\n";
    let parse = gd_syntax::parse(source);
    assert!(
        parse.diagnostics.is_empty(),
        "test fixture must parse cleanly; got {:?}",
        parse.diagnostics
    );

    let native = gd_types::NativeDb::from_json(MINI_API).expect("mini native db");
    let mut index = gd_project::Index::new(camino::Utf8PathBuf::from("/proj"));
    let iface = gd_project::extract_interface(&parse.tree);
    let file = index.set_interface(camino::Utf8Path::new("/proj/src/foo.gd"), iface);
    index.finish_cold_index();
    let xfile = SyntacticQuery::new(&index, &native);
    let policy = WarnPolicy::build(&WarningConfig::default(), &StrictSettings::default());

    let result = analyze(&parse.tree, Some(file), "foo.gd", &native, &xfile, &policy);

    let ready_call = result.bindings().iter().find_map(|b| match b {
        Binding::Call {
            callee_name,
            callee,
            ..
        } if callee_name == "_ready" => Some(callee.clone()),
        _ => None,
    });
    let callee = ready_call.expect("a Binding::Call must be recorded for the bare `_ready()`");
    assert_eq!(
        callee.script_file(),
        None,
        "an inherited native bare call must never classify as a Script callee of this file; \
         got {callee:?}"
    );
}

/// The same bare `_ready()` under a dump that DOES carry `Node._ready` classifies
/// `CalleeTarget::Native` with the class the lookup ran against — what the stub-anchored
/// outgoingCalls leg consumes.
#[test]
fn bare_native_call_with_methods_dump_records_native_target() {
    use gd_analyze::{analyze, Binding, CalleeTarget, StrictSettings, SyntacticQuery, WarnPolicy};
    use gd_project::WarningConfig;

    const METHODS_API: &str = r#"{
        "header": {"version_major": 4, "version_minor": 6, "version_patch": 3},
        "classes": [
            {"name": "Object", "is_instantiable": true},
            {"name": "Node", "inherits": "Object", "is_instantiable": true,
             "methods": [{"name": "_ready", "is_const": false, "is_static": false,
                          "is_vararg": false, "is_virtual": true, "hash": 1, "arguments": []}]}
        ]
    }"#;
    let source = "extends Node\n\nfunc start() -> void:\n\t_ready()\n";
    let parse = gd_syntax::parse(source);
    let native = gd_types::NativeDb::from_json(METHODS_API).expect("methods-bearing db");
    let mut index = gd_project::Index::new(camino::Utf8PathBuf::from("/proj"));
    let file = index.set_interface(
        camino::Utf8Path::new("/proj/src/foo.gd"),
        gd_project::extract_interface(&parse.tree),
    );
    index.finish_cold_index();
    let xfile = SyntacticQuery::new(&index, &native);
    let policy = WarnPolicy::build(&WarningConfig::default(), &StrictSettings::default());

    let result = analyze(&parse.tree, Some(file), "foo.gd", &native, &xfile, &policy);
    let callee = result
        .bindings()
        .iter()
        .find_map(|b| match b {
            Binding::Call {
                callee_name,
                callee,
                ..
            } if callee_name == "_ready" => Some(callee.clone()),
            _ => None,
        })
        .expect("a Binding::Call must be recorded for the bare `_ready()`");
    assert_eq!(
        callee,
        CalleeTarget::Native {
            class: "Node".to_string()
        },
        "a resolved native bare call classifies Native with the lookup class"
    );
}

// ============================================================================
// Non-Script classification on the DOTTED / SUPER shapes
// ============================================================================
//
// All callee shapes (bare, dotted `self.f()` / `obj.f()` / `C.f()`, and super) now record at
// ONE consolidated site that derives the target from the resolution the dispatch used. The
// Script branch is covered (`self.attack()` resolving in-file, in `watcher_and_nav.rs`, and
// `inherited_bare_call_attributes_to_declaring_base` below); these two pin the non-Script
// branch so the original bug class (stamping this file as the callee, which mis-renders
// `outgoingCalls` as an in-file self-pointer) cannot silently reappear on the dotted/super
// shapes.

/// Analyze a standalone `.gd` `source` and report whether the first recorded `Binding::Call`
/// for `callee_name` classified a Script callee: `Some(true)` = `CalleeTarget::Script`,
/// `Some(false)` = Native/Unresolved, outer `None` = no such call binding recorded.
fn recorded_call_is_in_file(source: &str, callee_name: &str) -> Option<bool> {
    use gd_analyze::{analyze, Binding, StrictSettings, SyntacticQuery, WarnPolicy};
    use gd_project::WarningConfig;

    let parse = gd_syntax::parse(source);
    assert!(
        parse.diagnostics.is_empty(),
        "fixture must parse cleanly; got {:?}",
        parse.diagnostics
    );
    let native = gd_types::NativeDb::from_json(MINI_API).expect("mini native db");
    let mut index = gd_project::Index::new(camino::Utf8PathBuf::from("/proj"));
    let iface = gd_project::extract_interface(&parse.tree);
    let file = index.set_interface(camino::Utf8Path::new("/proj/src/foo.gd"), iface);
    index.finish_cold_index();
    let xfile = SyntacticQuery::new(&index, &native);
    let policy = WarnPolicy::build(&WarningConfig::default(), &StrictSettings::default());
    let result = analyze(&parse.tree, Some(file), "foo.gd", &native, &xfile, &policy);

    result.bindings().iter().find_map(|b| match b {
        Binding::Call {
            callee_name: n,
            callee,
            ..
        } if n == callee_name => Some(callee.script_file().is_some()),
        _ => None,
    })
}

/// A dotted call to a NATIVE method (`self._ready()` → `Node._ready`) is not an in-file
/// function, so it must never classify a Script callee. `Some(true)` here would be the
/// regression.
#[test]
fn dotted_native_call_records_non_script_callee() {
    assert_eq!(
        recorded_call_is_in_file(
            "extends Node\n\nfunc start() -> void:\n\tself._ready()\n",
            "_ready"
        ),
        Some(false),
        "dotted native call `self._ready()` must classify a non-Script callee \
         (Some(true) = wrongly resolved in-file; None = no Binding::Call recorded at all)"
    );
}

/// `super._ready()` dispatches to the PARENT (native `Node._ready` for `extends Node`), never an
/// in-file function. The site handles `call.is_super` explicitly but was unexercised at the
/// binding level; pin the non-Script classification.
#[test]
fn super_native_call_records_non_script_callee() {
    assert_eq!(
        recorded_call_is_in_file(
            "extends Node\n\nfunc start() -> void:\n\tsuper._ready()\n",
            "_ready"
        ),
        Some(false),
        "super native call `super._ready()` must classify a non-Script callee \
         (Some(true) = wrongly resolved in-file; None = no Binding::Call recorded at all)"
    );
}

/// The consolidated recording site walks the in-file lookup from the CURRENT class, so a bare
/// call inside an inner class records the inner class as the owning `class_path` — the
/// disambiguator that stops same-named methods in one file from sharing call-site sets.
#[test]
fn inner_class_bare_call_records_owning_class_path() {
    use gd_analyze::{analyze, Binding, CalleeTarget, StrictSettings, SyntacticQuery, WarnPolicy};
    use gd_project::WarningConfig;

    let source = "extends Node\n\
                  func helper() -> void:\n\tpass\n\
                  func go_root() -> void:\n\thelper()\n\
                  class Inner:\n\
                  \tfunc helper() -> void:\n\t\tpass\n\
                  \tfunc go() -> void:\n\t\thelper()\n";
    let parse = gd_syntax::parse(source);
    assert!(
        parse.diagnostics.is_empty(),
        "fixture must parse cleanly; got {:?}",
        parse.diagnostics
    );
    let native = gd_types::NativeDb::from_json(MINI_API).expect("mini native db");
    let mut index = gd_project::Index::new(camino::Utf8PathBuf::from("/proj"));
    let file = index.set_interface(
        camino::Utf8Path::new("/proj/src/foo.gd"),
        gd_project::extract_interface(&parse.tree),
    );
    index.finish_cold_index();
    let xfile = SyntacticQuery::new(&index, &native);
    let policy = WarnPolicy::build(&WarningConfig::default(), &StrictSettings::default());
    let result = analyze(&parse.tree, Some(file), "foo.gd", &native, &xfile, &policy);

    let callees: Vec<CalleeTarget> = result
        .bindings()
        .iter()
        .filter_map(|b| match b {
            Binding::Call {
                callee_name,
                callee,
                ..
            } if callee_name == "helper" => Some(callee.clone()),
            _ => None,
        })
        .collect();
    assert!(
        callees.contains(&CalleeTarget::Script {
            file,
            class_path: Vec::new()
        }),
        "the root-class bare call records the root class_path; got {callees:?}"
    );
    assert!(
        callees.contains(&CalleeTarget::Script {
            file,
            class_path: vec!["Inner".to_string()]
        }),
        "the inner-class bare call records its owning class path; got {callees:?}"
    );
}

/// A BARE call to an INHERITED method attributes its callee to the base that DECLARES it
/// (dispatch-accurate via the script-chain walk at the consolidated recording site), never
/// `Unresolved` and never the calling file. Build a real 2-file index so the chain
/// `Derived extends Base` is resolvable; `go()`'s bare `shared()` dispatches to `Base.shared`.
#[test]
fn inherited_bare_call_attributes_to_declaring_base() {
    use gd_analyze::{analyze, Binding, StrictSettings, SyntacticQuery, WarnPolicy};
    use gd_project::WarningConfig;

    let base_src = "class_name Base\nextends Node\nfunc shared() -> void:\n\tpass\n";
    let derived_src = "extends Base\nfunc go() -> void:\n\tshared()\n";
    let native = gd_types::NativeDb::from_json(MINI_API).expect("mini native db");
    let mut index = gd_project::Index::new(camino::Utf8PathBuf::from("/proj"));
    index.set_interface(
        camino::Utf8Path::new("/proj/base.gd"),
        gd_project::extract_interface(&gd_syntax::parse(base_src).tree),
    );
    let derived_fid = index.set_interface(
        camino::Utf8Path::new("/proj/derived.gd"),
        gd_project::extract_interface(&gd_syntax::parse(derived_src).tree),
    );
    index.finish_cold_index();
    let base_fid = index
        .file_id(camino::Utf8Path::new("/proj/base.gd"))
        .expect("base.gd interned");
    let xfile = SyntacticQuery::new(&index, &native);
    let policy = WarnPolicy::build(&WarningConfig::default(), &StrictSettings::default());
    let parse = gd_syntax::parse(derived_src);
    let result = analyze(
        &parse.tree,
        Some(derived_fid),
        "derived.gd",
        &native,
        &xfile,
        &policy,
    );

    let shared_call = result.bindings().iter().find_map(|b| match b {
        Binding::Call {
            callee_name,
            callee,
            ..
        } if callee_name == "shared" => Some(callee.clone()),
        _ => None,
    });
    assert_eq!(
        shared_call,
        Some(gd_analyze::CalleeTarget::Script {
            file: base_fid,
            class_path: Vec::new()
        }),
        "an inherited bare call `shared()` must attribute its callee to the declaring Base \
         (dispatch resolution), not Unresolved and not the calling Derived file"
    );
}

// ============================================================================
// watcher-channel death — landed in M5 WP-RD3
// ============================================================================
//
// The fix at `server.rs` turned a `break;` into `watcher_rx = None;` so the LSP session keeps
// serving when the debouncer thread dies. WP-RD3 landed the refactor that makes it directly
// testable: `gd_server::serve_with_injected_watcher` takes the watcher's event receiver as a
// parameter, so `tests/watcher_event_loop.rs::channel_death_disables_watcher_but_session_survives`
// drops the `Sender` and asserts `publishDiagnostics` still flows — exactly the injectable-
// receiver refactor the module-note above deferred to M5.

// ============================================================================
// The FileId(0) leak and the handler silent-continue path: observability via
// log breadcrumb — not behaviorally testable without a log-capture harness. Tracked as
// stderr-visible regressions; the LSP responses themselves degrade correctly through the
// existing code paths.
// ============================================================================
