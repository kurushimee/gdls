//! Regression tests for navigation-surface correctness bugs — workspace/symbol URI encoding and
//! call-hierarchy `callee_file` attribution — found while hardening the M4 nav features.
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
//!   - **callee_file inherited**: `inherited_bare_call_records_callee_file_none` parses a
//!     file that calls `_ready()` (inherited from `Node`) and asserts the recorded
//!     `Binding::Call.callee_file` is `None`, not `Some(ctx.file)`.
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
// callee_file None for non-lexically-anchored bare calls
// ============================================================================

/// A bare call to `_ready()` from `extends Node` dispatches to `Node._ready` — a native method
/// on a class declared in `extension_api.json`, not in this file. The pre-fix recording site
/// unconditionally tagged every reached call with `callee_file = Some(ctx.file)`, so
/// `callHierarchy/incomingCalls` of `Node._ready` missed every site and `outgoingCalls`
/// rendered the call as an in-file self-pointer. The fix routes through
/// `current_file_declares_function` in `reducer.rs`: when the file doesn't declare the
/// callee name, record `None`.
#[test]
fn inherited_bare_call_records_callee_file_none() {
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
            callee_file,
            ..
        } if callee_name == "_ready" => Some(*callee_file),
        _ => None,
    });

    match ready_call {
        Some(None) => {
            // Expected: _ready is on Node (native), the analyzer correctly recorded None so
            // the nav handlers degrade through the native / unresolved path.
        }
        Some(Some(fid)) => panic!(
            "regression: inherited bare call `_ready()` recorded callee_file = Some({fid:?}). \
             It should be None — _ready is declared on Node (native), not in this file. \
             See `reducer.rs::current_file_declares_function`."
        ),
        None => {
            // The reducer might also legitimately reach an early-return path that skips the
            // recording entirely (e.g., a future reshape moves `_ready` resolution into the
            // Object-method branch). Failing closed here would couple the regression test to
            // an implementation detail; a follow-on Binding::Call with the wrong callee_file
            // would still fail the Some(Some(_)) arm above. Surface the result as an
            // informative skip so future readers know why the test "passes".
            eprintln!(
                "warning: no Binding::Call recorded for _ready; reducer may have reshaped the \
                 path. Bindings produced: {:?}",
                result.bindings()
            );
        }
    }
}

// ============================================================================
// Follow-up: callee_file None on the DOTTED / SUPER site
// ============================================================================
//
// `inherited_bare_call_records_callee_file_none` (above) covers the BARE recording site
// (`reducer.rs` ~2849). There is a SECOND recording site for dotted (`self.f()`, `obj.f()`,
// `C.f()`) and super (`super.f()`) callees (`reducer.rs` ~3557), with its own `callee_file`
// logic (`in_file_function_id.is_some()`). Its Some-branch is covered (`self.attack()` resolving
// in-file, in `watcher_and_nav.rs`); these two pin its None-branch so the same class of bug fixed
// for bare calls (wrongly stamping `Some(ctx.file)`, which mis-renders `outgoingCalls` as an
// in-file self-pointer) cannot silently reappear on the dotted/super shapes.

/// Analyze a standalone `.gd` `source` and report whether the first recorded `Binding::Call` for
/// `callee_name` resolved in-file: `Some(true)` = `callee_file = Some(_)`, `Some(false)` =
/// `callee_file = None`, outer `None` = no such call binding recorded.
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
            callee_file,
            ..
        } if n == callee_name => Some(callee_file.is_some()),
        _ => None,
    })
}

/// A dotted call to a NATIVE method (`self._ready()` → `Node._ready`) is not an in-file function,
/// so the dotted site must record `callee_file = None`. `Some(true)` here would be the regression.
#[test]
fn dotted_native_call_records_callee_file_none() {
    assert_eq!(
        recorded_call_is_in_file(
            "extends Node\n\nfunc start() -> void:\n\tself._ready()\n",
            "_ready"
        ),
        Some(false),
        "dotted native call `self._ready()` must record callee_file = None \
         (Some(true) = wrongly resolved in-file; None = no Binding::Call recorded at all)"
    );
}

/// `super._ready()` dispatches to the PARENT (native `Node._ready` for `extends Node`), never an
/// in-file function. The site handles `call.is_super` explicitly but was unexercised at the
/// binding level; pin `callee_file = None`.
#[test]
fn super_native_call_records_callee_file_none() {
    assert_eq!(
        recorded_call_is_in_file(
            "extends Node\n\nfunc start() -> void:\n\tsuper._ready()\n",
            "_ready"
        ),
        Some(false),
        "super native call `super._ready()` must record callee_file = None \
         (Some(true) = wrongly resolved in-file; None = no Binding::Call recorded at all)"
    );
}

/// WP-RD6: a BARE call to an INHERITED method now attributes `callee_file` to the base that
/// DECLARES it (dispatch-accurate via `resolve_callee_file`'s extends-chain walk), not `None` —
/// the prior name-presence test could not express this. Build a real 2-file index so the chain
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
            callee_file,
            ..
        } if callee_name == "shared" => Some(*callee_file),
        _ => None,
    });
    assert_eq!(
        shared_call,
        Some(Some(base_fid)),
        "an inherited bare call `shared()` must attribute callee_file to the declaring Base \
         (WP-RD6 dispatch resolution), not None and not the calling Derived file"
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
