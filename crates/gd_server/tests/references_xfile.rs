//! M6-E gate: `textDocument/references` finds callers through body-local typed vars.
//!
//! When `lib.gd` defines `func helper()` and callers reach it through a body-local typed var
//! (`var l: Lib = Lib.new(); l.helper()`), find-references on `helper`'s declaration must
//! include all cross-file call sites — even files whose interface does not mention `Lib`.

mod common;

use common::{file_uri, notification, recv, request, shutdown, TempProject};
use lsp_server::Connection;
use lsp_types::{
    DidOpenTextDocumentParams, InitializeParams, InitializedParams, Location, Position,
    ReferenceContext, ReferenceParams, TextDocumentIdentifier, TextDocumentItem,
    TextDocumentPositionParams, WorkDoneProgressParams,
};

fn init_and_open(project: &TempProject, client: &Connection, files: &[&str]) {
    let init = InitializeParams {
        initialization_options: Some(serde_json::json!({
            "projectRoot": project.root.as_str(),
            "autoDumpExtensionApi": false,
            "extensionApiPath": project.root.join("extension_api.json").as_str(),
        })),
        ..Default::default()
    };
    client.sender.send(request(1, "initialize", init)).unwrap();
    let _ = recv(client);
    client
        .sender
        .send(notification("initialized", InitializedParams {}))
        .unwrap();

    for (i, rel) in files.iter().enumerate() {
        let abs = project.root.join(rel);
        let text = std::fs::read_to_string(abs.as_std_path()).expect("read file");
        let uri = file_uri(&abs);
        client
            .sender
            .send(notification(
                "textDocument/didOpen",
                DidOpenTextDocumentParams {
                    text_document: TextDocumentItem {
                        uri,
                        language_id: "gdscript".to_string(),
                        version: (i + 2) as i32,
                        text,
                    },
                },
            ))
            .unwrap();
    }
    // Drain all publishDiagnostics pushes.
    while common::try_recv(client, std::time::Duration::from_millis(300)).is_some() {}
}

/// M6-E: references on `helper` in `lib.gd` must return the call sites in `a.gd` and `b.gd`,
/// including when one caller reaches the method through a body-local typed var — not a typed
/// parameter — so `Lib` does NOT appear in that caller's interface. This exercises the project-
/// wide text-scan path that supersedes `name_referencers` for method/signal targets.
#[test]
fn references_finds_cross_file_method_calls() {
    let p = TempProject::new();
    p.write("project.godot", "config_version=5\n");
    p.write("extension_api.json", common::MINI_API);

    // lib.gd defines `class_name Lib` and `func helper()`.
    // Line 0: `class_name Lib`
    // Line 1: `extends Node`
    // Line 3: `func helper():`
    // Line 4: `\tpass`
    // `helper` identifier at line 3, col 5..11.
    p.write(
        "lib.gd",
        "class_name Lib\nextends Node\n\nfunc helper():\n\tpass\n",
    );

    // a.gd calls helper() via a typed parameter — `Lib` appears in its interface.
    // Line 0: `extends Node`
    // Line 2: `func test(l: Lib):`
    // Line 3: `\tl.helper()`  — `helper` at col 3..9
    p.write("a.gd", "extends Node\n\nfunc test(l: Lib):\n\tl.helper()\n");

    // b.gd calls helper() through a BODY-LOCAL typed var — `Lib` does NOT appear in b.gd's
    // interface (the interface pass only records types from parameters/return/annotations, not
    // local variable declarations). This is the seam the M6-E fix must close: b.gd is NOT in
    // `name_referencers("Lib")` or `name_referencers("helper")`, so it was previously missed.
    // Line 0: `extends Node`
    // Line 2: `func run():`
    // Line 3: `\tvar l: Lib = Lib.new()`
    // Line 4: `\tl.helper()`  — `helper` at col 3..9
    p.write(
        "b.gd",
        "extends Node\n\nfunc run():\n\tvar l: Lib = Lib.new()\n\tl.helper()\n",
    );

    let (server, client) = Connection::memory();
    let server_thread = std::thread::spawn(move || gd_server::serve(server));
    init_and_open(&p, &client, &["lib.gd", "a.gd", "b.gd"]);

    let lib_uri = file_uri(&p.root.join("lib.gd"));
    let params = ReferenceParams {
        text_document_position: TextDocumentPositionParams {
            text_document: TextDocumentIdentifier {
                uri: lib_uri.clone(),
            },
            // Click on `helper` at line 3, col 7.
            position: Position {
                line: 3,
                character: 7,
            },
        },
        context: ReferenceContext {
            include_declaration: false,
        },
        work_done_progress_params: WorkDoneProgressParams::default(),
        partial_result_params: Default::default(),
    };
    client
        .sender
        .send(request(10, "textDocument/references", params))
        .unwrap();
    let resp = common::recv_response(&client);
    assert!(resp.error.is_none(), "references errored: {:?}", resp.error);
    let locs: Vec<Location> =
        serde_json::from_value(resp.result.expect("references result")).unwrap();

    // Must include call sites from BOTH a.gd (typed-parameter caller) AND b.gd (body-local
    // typed-var caller). The b.gd assertion is the critical M6-E seam.
    let a_uri = file_uri(&p.root.join("a.gd"));
    let b_uri = file_uri(&p.root.join("b.gd"));
    let has_a = locs.iter().any(|l| l.uri == a_uri);
    let has_b = locs.iter().any(|l| l.uri == b_uri);
    assert!(
        has_a,
        "references must include call site in a.gd (typed-param caller); got: {locs:?}"
    );
    assert!(
        has_b,
        "references must include call site in b.gd (body-local var caller — M6-E seam); \
         got: {locs:?}"
    );

    // The call sites must be the `helper` identifier range (narrow), not the whole call expression.
    for loc in locs.iter().filter(|l| l.uri == a_uri || l.uri == b_uri) {
        assert_eq!(
            loc.range.start.character, 3,
            "call site range should start at `helper` identifier col 3, got {loc:?}"
        );
    }

    shutdown(&client, server_thread);
}

/// M6-E false-positive gate: `textDocument/references` for `Lib::helper` must NOT include
/// occurrences of an unrelated same-named method in `other.gd` (`class_name Other` with its own
/// `func helper()`). The callee_file-filtered mechanism must distinguish between the two.
#[test]
fn references_excludes_unrelated_same_named_method() {
    let p = TempProject::new();
    p.write("project.godot", "config_version=5\n");
    p.write("extension_api.json", common::MINI_API);

    // lib.gd defines `class_name Lib` and `func helper()`.
    p.write(
        "lib.gd",
        "class_name Lib\nextends Node\n\nfunc helper():\n\tpass\n",
    );

    // other.gd is an unrelated class with its own `func helper()`.
    // It does NOT extend Lib, does NOT call Lib.helper — it just happens to have the same name.
    // Line 0: `class_name Other`
    // Line 1: `extends Node`
    // Line 3: `func helper():`  — `helper` at col 5..11
    // Line 4: `\tpass`
    p.write(
        "other.gd",
        "class_name Other\nextends Node\n\nfunc helper():\n\tpass\n",
    );

    // caller.gd calls Lib.helper() — it's a genuine reference.
    // Line 0: `extends Node`
    // Line 2: `func run():`
    // Line 3: `\tvar l: Lib = Lib.new()`
    // Line 4: `\tl.helper()`  — `helper` at col 3..9
    p.write(
        "caller.gd",
        "extends Node\n\nfunc run():\n\tvar l: Lib = Lib.new()\n\tl.helper()\n",
    );

    let (server, client) = Connection::memory();
    let server_thread = std::thread::spawn(move || gd_server::serve(server));
    init_and_open(&p, &client, &["lib.gd", "other.gd", "caller.gd"]);

    let lib_uri = file_uri(&p.root.join("lib.gd"));
    let params = ReferenceParams {
        text_document_position: TextDocumentPositionParams {
            text_document: TextDocumentIdentifier { uri: lib_uri },
            // Click on `helper` at line 3, col 7 (declaration site in lib.gd).
            position: Position {
                line: 3,
                character: 7,
            },
        },
        context: ReferenceContext {
            include_declaration: false,
        },
        work_done_progress_params: WorkDoneProgressParams::default(),
        partial_result_params: Default::default(),
    };
    client
        .sender
        .send(request(20, "textDocument/references", params))
        .unwrap();
    let resp = common::recv_response(&client);
    assert!(resp.error.is_none(), "references errored: {:?}", resp.error);
    let locs: Vec<Location> =
        serde_json::from_value(resp.result.expect("references result")).unwrap();

    let caller_uri = file_uri(&p.root.join("caller.gd"));
    let other_uri = file_uri(&p.root.join("other.gd"));

    // caller.gd's `l.helper()` call IS a genuine reference — must be included.
    assert!(
        locs.iter().any(|l| l.uri == caller_uri),
        "references must include genuine call site in caller.gd; got: {locs:?}"
    );

    // other.gd's `func helper():` declaration is unrelated — must NOT appear.
    assert!(
        !locs.iter().any(|l| l.uri == other_uri),
        "references must NOT include other.gd's unrelated helper (false positive); got: {locs:?}"
    );

    shutdown(&client, server_thread);
}

/// Fix 3: `include_declaration: true` for a cross-file method must include the declaration site
/// in the declaring file. The cursor is on a CALL SITE in caller.gd; with include_declaration=true,
/// the response must include a Location in lib.gd at the `helper` declaration.
#[test]
fn references_include_declaration_returns_cross_file_decl() {
    let p = TempProject::new();
    p.write("project.godot", "config_version=5\n");
    p.write("extension_api.json", common::MINI_API);

    // lib.gd: class_name Lib, func helper() at line 3, col 5..11.
    p.write(
        "lib.gd",
        "class_name Lib\nextends Node\n\nfunc helper():\n\tpass\n",
    );

    // caller.gd: calls l.helper() at line 3, col 3..9.
    p.write(
        "caller.gd",
        "extends Node\n\nfunc test(l: Lib):\n\tl.helper()\n",
    );

    let (server, client) = Connection::memory();
    let server_thread = std::thread::spawn(move || gd_server::serve(server));
    init_and_open(&p, &client, &["lib.gd", "caller.gd"]);

    let caller_uri = file_uri(&p.root.join("caller.gd"));
    let params = ReferenceParams {
        text_document_position: TextDocumentPositionParams {
            text_document: TextDocumentIdentifier {
                uri: caller_uri.clone(),
            },
            // Cursor on `helper` at line 3, col 5 (inside `l.helper()`).
            position: Position {
                line: 3,
                character: 5,
            },
        },
        context: ReferenceContext {
            include_declaration: true,
        },
        work_done_progress_params: WorkDoneProgressParams::default(),
        partial_result_params: Default::default(),
    };
    client
        .sender
        .send(request(40, "textDocument/references", params))
        .unwrap();
    let resp = common::recv_response(&client);
    assert!(resp.error.is_none(), "references errored: {:?}", resp.error);
    let locs: Vec<Location> =
        serde_json::from_value(resp.result.expect("references result")).unwrap();

    let lib_uri = file_uri(&p.root.join("lib.gd"));

    // The declaration in lib.gd at line 3, col 5..11 must be included.
    let has_decl = locs
        .iter()
        .any(|l| l.uri == lib_uri && l.range.start.line == 3 && l.range.start.character == 5);
    assert!(
        has_decl,
        "include_declaration:true must include the method declaration in lib.gd at line 3, col 5; \
         got: {locs:?}"
    );

    shutdown(&client, server_thread);
}

/// Bare same-file call: `func a(): helper()` calling a sibling `func helper()`.
/// Find-references on `helper`'s declaration must include the same-file bare call site.
///
/// Regression guard for the callee_ident_span bare-call path: bare calls have `CallNode::callee =
/// None` so the SubscriptAccess arm can't produce the span; must fall back to
/// `call_site.start..call_site.start + function_name.len()` for the narrow identifier span.
#[test]
fn references_finds_bare_same_file_call() {
    let p = TempProject::new();
    p.write("project.godot", "config_version=5\n");
    p.write("extension_api.json", common::MINI_API);

    // lib.gd: `func a()` calls sibling `func helper()` with a bare (unqualified) call.
    // Line 0: `extends Node`
    // Line 2: `func a():`
    // Line 3: `\thelper()`  — `helper` identifier at col 1..7 (after the tab)
    // Line 5: `func helper():`
    // Line 6: `\tpass`
    // Click declaration at line 5, col 7 — must return call site at line 3, col 1.
    p.write(
        "lib.gd",
        "extends Node\n\nfunc a():\n\thelper()\n\nfunc helper():\n\tpass\n",
    );

    let (server, client) = Connection::memory();
    let server_thread = std::thread::spawn(move || gd_server::serve(server));
    init_and_open(&p, &client, &["lib.gd"]);

    let lib_uri = file_uri(&p.root.join("lib.gd"));
    let params = ReferenceParams {
        text_document_position: TextDocumentPositionParams {
            text_document: TextDocumentIdentifier {
                uri: lib_uri.clone(),
            },
            // Click on `helper` at line 5, col 7 (declaration).
            position: Position {
                line: 5,
                character: 7,
            },
        },
        context: ReferenceContext {
            include_declaration: false,
        },
        work_done_progress_params: WorkDoneProgressParams::default(),
        partial_result_params: Default::default(),
    };
    client
        .sender
        .send(request(30, "textDocument/references", params))
        .unwrap();
    let resp = common::recv_response(&client);
    assert!(resp.error.is_none(), "references errored: {:?}", resp.error);
    let locs: Vec<Location> =
        serde_json::from_value(resp.result.expect("references result")).unwrap();

    // The bare call site at line 3 (func a body) must be in results.
    let has_call = locs
        .iter()
        .any(|l| l.uri == lib_uri && l.range.start.line == 3);
    assert!(
        has_call,
        "references must include bare same-file call site at line 3; got: {locs:?}"
    );

    // The narrow span: `helper` starts at col 1 (after the tab).
    for loc in locs
        .iter()
        .filter(|l| l.uri == lib_uri && l.range.start.line == 3)
    {
        assert_eq!(
            loc.range.start.character, 1,
            "bare call site range should start at `helper` col 1 (after tab); got {loc:?}"
        );
    }

    shutdown(&client, server_thread);
}

/// Property-read recall guard: find-references on a field clicked at an **attribute read-site**
/// (`self.hp`) must still report the field's other read occurrences. A property attribute is not a
/// method call, so it must take the raw-identifier scan — not the callee-filtered method path,
/// which emits `Binding::Call` records only and would drop every property read (the analyzer
/// records no binding for an attribute read).
///
/// Discriminating: with the cursor on `self.hp` in `func a`, the response must include the *other*
/// read (`self.hp` in `func b`). Routing the click through the method path (the pre-fix behavior)
/// returns neither read — `assert has_b` fails. The raw-scan path finds both.
#[test]
fn references_on_property_at_attribute_site_finds_other_reads() {
    let p = TempProject::new();
    p.write("project.godot", "config_version=5\n");
    p.write("extension_api.json", common::MINI_API);

    // One file: field `hp` declared, then read through `self.hp` in two methods.
    // Line 0: `extends Node`
    // Line 1: `var hp: int = 0`
    // Line 2: `func a() -> void:`
    // Line 3: `\tself.hp = 1`   — `hp` attribute at col 6..8 (`\tself.` = 6 bytes)
    // Line 4: `func b() -> void:`
    // Line 5: `\tself.hp = 2`   — `hp` attribute at col 6..8
    p.write(
        "obj.gd",
        "extends Node\nvar hp: int = 0\nfunc a() -> void:\n\tself.hp = 1\nfunc b() -> void:\n\tself.hp = 2\n",
    );

    let (server, client) = Connection::memory();
    let server_thread = std::thread::spawn(move || gd_server::serve(server));
    init_and_open(&p, &client, &["obj.gd"]);

    let obj_uri = file_uri(&p.root.join("obj.gd"));
    let params = ReferenceParams {
        text_document_position: TextDocumentPositionParams {
            text_document: TextDocumentIdentifier {
                uri: obj_uri.clone(),
            },
            // Click `hp` in `self.hp` at the func-a read site (line 3, col 6 — on `h`).
            position: Position {
                line: 3,
                character: 6,
            },
        },
        context: ReferenceContext {
            include_declaration: false,
        },
        work_done_progress_params: WorkDoneProgressParams::default(),
        partial_result_params: Default::default(),
    };
    client
        .sender
        .send(request(60, "textDocument/references", params))
        .unwrap();
    let resp = common::recv_response(&client);
    assert!(resp.error.is_none(), "references errored: {:?}", resp.error);
    let locs: Vec<Location> =
        serde_json::from_value(resp.result.expect("references result")).unwrap();

    // The other read site (`self.hp` in func b, line 5) must be reported — proving the raw scan ran
    // instead of the callee-only method path (which drops property reads entirely).
    assert!(
        locs.iter()
            .any(|l| l.uri == obj_uri && l.range.start.line == 5),
        "references on a property at a read-site must include the other read in func b (line 5); \
         got: {locs:?}"
    );

    shutdown(&client, server_thread);
}

/// Signal-reference recall: find-references on a `signal hit` declaration must include the signal's
/// use sites (`hit.emit()`, `hit.connect(...)`). These reach the signal through the **base** of a
/// subscript call, not its callee — the `Binding::Call` for `hit.emit()` records `callee_name =
/// "emit"`, never `"hit"`, so the callee-filtered method projection (`push_callee_ident_locations`)
/// can never match. Recall instead rides the base identifier `hit`, which the dispatcher pre-reduces
/// as an identifier (`reducer.rs` Call arm) into a `Binding::Use{target_name: "hit"}` that
/// `push_binding_locations` reports. This is a distinct path from the bare-call case above, so it
/// gets its own guard.
#[test]
fn references_finds_signal_emit_and_connect_sites() {
    let p = TempProject::new();
    p.write("project.godot", "config_version=5\n");
    p.write("extension_api.json", common::MINI_API);

    // Line 0: `extends Node`
    // Line 2: `signal hit`        — `hit` declaration at col 7..10
    // Line 4: `func a():`
    // Line 5: `\thit.emit()`      — `hit` base at col 1..4 (after the tab)
    // Line 7: `func b():`
    // Line 8: `\thit.connect(a)`  — `hit` base at col 1..4
    p.write(
        "sig.gd",
        "extends Node\n\nsignal hit\n\nfunc a():\n\thit.emit()\n\nfunc b():\n\thit.connect(a)\n",
    );

    let (server, client) = Connection::memory();
    let server_thread = std::thread::spawn(move || gd_server::serve(server));
    init_and_open(&p, &client, &["sig.gd"]);

    let sig_uri = file_uri(&p.root.join("sig.gd"));
    let params = ReferenceParams {
        text_document_position: TextDocumentPositionParams {
            text_document: TextDocumentIdentifier {
                uri: sig_uri.clone(),
            },
            // Click on `hit` at line 2, col 7 (the `signal hit` declaration).
            position: Position {
                line: 2,
                character: 7,
            },
        },
        context: ReferenceContext {
            include_declaration: false,
        },
        work_done_progress_params: WorkDoneProgressParams::default(),
        partial_result_params: Default::default(),
    };
    client
        .sender
        .send(request(90, "textDocument/references", params))
        .unwrap();
    let resp = common::recv_response(&client);
    assert!(resp.error.is_none(), "references errored: {:?}", resp.error);
    let locs: Vec<Location> =
        serde_json::from_value(resp.result.expect("references result")).unwrap();

    // Both signal-use sites must be reported: the `emit` site (line 5) and the `connect` site
    // (line 8), each on the `hit` base identifier at col 1 (after the tab).
    for line in [5, 8] {
        assert!(
            locs.iter()
                .any(|l| l.uri == sig_uri && l.range.start.line == line),
            "references on `signal hit` must include the use site at line {line}; got: {locs:?}"
        );
        for loc in locs
            .iter()
            .filter(|l| l.uri == sig_uri && l.range.start.line == line)
        {
            assert_eq!(
                loc.range.start.character, 1,
                "signal use site range should start at `hit` col 1 (after tab); got {loc:?}"
            );
        }
    }

    shutdown(&client, server_thread);
}

/// Native subscript-call recall: a cursor on `_ready` in `self._ready()` (a NATIVE `Node` method,
/// so the reducer records `Binding::Call { callee_file: None }`) must still report the cross-file
/// `self._ready()` occurrence in another file. Regression guard for the `target_file` conflation
/// where `find_map(..).flatten().or(current_fid)` collapsed the native `Some(None)` case into the
/// current file: `push_callee_ident_locations` then filtered on `callee_file == Some(current_fid)`
/// — which no native call carries — and silently dropped every cross-file reference. The fix keeps
/// `target_file = None` for native callees so the scan falls back to `push_identifier_locations`
/// (raw text scan), the pre-M6 behaviour.
#[test]
fn references_on_native_subscript_call_finds_cross_file_uses() {
    let p = TempProject::new();
    p.write("project.godot", "config_version=5\n");
    p.write("extension_api.json", common::MINI_API);

    // a.gd calls the native `_ready` through `self`. `_ready` identifier at line 3:
    // `\tself._ready()` → tab=col0, `self`=1..5, `.`=col5, `_ready`=col6..12.
    p.write("a.gd", "extends Node\n\nfunc setup():\n\tself._ready()\n");
    // b.gd makes the same native call — the cross-file occurrence the bug dropped.
    p.write("b.gd", "extends Node\n\nfunc run():\n\tself._ready()\n");

    let (server, client) = Connection::memory();
    let server_thread = std::thread::spawn(move || gd_server::serve(server));
    init_and_open(&p, &client, &["a.gd", "b.gd"]);

    let a_uri = file_uri(&p.root.join("a.gd"));
    let b_uri = file_uri(&p.root.join("b.gd"));
    let params = ReferenceParams {
        text_document_position: TextDocumentPositionParams {
            text_document: TextDocumentIdentifier { uri: a_uri.clone() },
            // Click inside `_ready` at line 3, col 8.
            position: Position {
                line: 3,
                character: 8,
            },
        },
        context: ReferenceContext {
            include_declaration: false,
        },
        work_done_progress_params: WorkDoneProgressParams::default(),
        partial_result_params: Default::default(),
    };
    client
        .sender
        .send(request(11, "textDocument/references", params))
        .unwrap();
    let resp = common::recv_response(&client);
    assert!(resp.error.is_none(), "references errored: {:?}", resp.error);
    let locs: Vec<Location> =
        serde_json::from_value(resp.result.expect("references result")).unwrap();

    assert!(
        locs.iter().any(|l| l.uri == a_uri),
        "references on native `_ready` must include the current-file call site in a.gd; \
         got: {locs:?}"
    );
    assert!(
        locs.iter().any(|l| l.uri == b_uri),
        "references on native `_ready` must include the cross-file call site in b.gd \
         (native callee → raw text scan, not callee_file-filtered); got: {locs:?}"
    );
    // The reported occurrences must be the narrow `_ready` identifier (col 6), not the whole call.
    for loc in locs.iter().filter(|l| l.uri == a_uri || l.uri == b_uri) {
        assert_eq!(
            loc.range.start.character, 6,
            "native call site range should start at `_ready` identifier col 6; got {loc:?}"
        );
    }

    shutdown(&client, server_thread);
}

/// includeDeclaration:false on a NON-method target (the raw-identifier-scan path where the
/// declaration token used to leak through unconditionally): a class-level `var` declaration
/// click returns only the reads; with `true` the declaration appears exactly once.
#[test]
fn references_include_declaration_false_excludes_member_var_decl() {
    let p = TempProject::new();
    p.write("project.godot", "config_version=5\n");
    p.write("extension_api.json", common::MINI_API);
    // Line 1: `var hp: int = 0` — declaration `hp` at cols 4..6.
    // Lines 3/5: `\tself.hp = …` — reads at cols 6..8.
    p.write(
        "obj.gd",
        "extends Node\nvar hp: int = 0\nfunc a() -> void:\n\tself.hp = 1\nfunc b() -> void:\n\tself.hp = 2\n",
    );

    let (server, client) = Connection::memory();
    let server_thread = std::thread::spawn(move || gd_server::serve(server));
    init_and_open(&p, &client, &["obj.gd"]);

    let uri = file_uri(&p.root.join("obj.gd"));
    let send = |id: i32, include: bool| -> Vec<Location> {
        let params = ReferenceParams {
            text_document_position: TextDocumentPositionParams {
                text_document: TextDocumentIdentifier { uri: uri.clone() },
                position: Position {
                    line: 1,
                    character: 4,
                },
            },
            work_done_progress_params: WorkDoneProgressParams::default(),
            partial_result_params: Default::default(),
            context: ReferenceContext {
                include_declaration: include,
            },
        };
        client
            .sender
            .send(request(id, "textDocument/references", params))
            .unwrap();
        let resp = common::recv_response(&client);
        assert!(resp.error.is_none(), "references errored: {:?}", resp.error);
        serde_json::from_value::<Option<Vec<Location>>>(resp.result.unwrap())
            .unwrap()
            .unwrap_or_default()
    };

    let decl_start = Position {
        line: 1,
        character: 4,
    };
    let without = send(20, false);
    assert!(
        !without.iter().any(|l| l.range.start == decl_start),
        "the declaration's own name token must be filtered with includeDeclaration:false; \
         got {without:?}"
    );
    for line in [3u32, 5] {
        assert!(
            without
                .iter()
                .any(|l| l.range.start == Position { line, character: 6 }),
            "the line-{line} read must stay; got {without:?}"
        );
    }

    let with = send(21, true);
    assert_eq!(
        with.iter().filter(|l| l.range.start == decl_start).count(),
        1,
        "with includeDeclaration:true the declaration appears exactly once; got {with:?}"
    );

    shutdown(&client, server_thread);
}
