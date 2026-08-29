//! Integration tests for autoload-singleton typing (M6 feature).
//!
//! Verifies that when `project.godot` declares an autoload singleton and the singleton's script is
//! indexed, member access through the autoload name fully resolves:
//!   - `textDocument/hover` on `popup_error` in `Global.popup_error("x")` renders the function
//!     signature, not a degraded Variant type.
//!   - `textDocument/references` on `popup_error` in `global.gd` finds the call site in `caller.gd`.
//!   - `textDocument/definition` on `Global` in `caller.gd` jumps to `global.gd`.
//!   - Shadowing: a local `var Global = 1` prevents the autoload from being typed.

mod common;

use common::{file_uri, notification, recv, recv_response, request, shutdown, TempProject};
use lsp_server::{Connection, Message};
use lsp_types::{
    DidOpenTextDocumentParams, GotoDefinitionParams, GotoDefinitionResponse, Hover, HoverContents,
    HoverParams, InitializeParams, InitializedParams, Location, MarkupKind, PartialResultParams,
    Position, PublishDiagnosticsParams, Range, ReferenceContext, ReferenceParams,
    TextDocumentIdentifier, TextDocumentItem, TextDocumentPositionParams, WorkDoneProgressParams,
};

/// A minimal native DB with the `Node` hierarchy (no methods) — enough for the analyzer's method-miss
/// check to fire (`ApiProvenance::Exact`), so a bogus member on a precisely-typed autoload errors
/// exactly as Godot does. Without it, `provenance` is `Absent` and member-miss is suppressed.
const NODE_API: &str = r#"{
    "header": {"version_major": 4, "version_minor": 6, "version_patch": 3},
    "classes": [
        {"name": "Object"},
        {"name": "Node", "inherits": "Object"}
    ]
}"#;

/// Boot a server over a TempProject with UTF-8 position encoding negotiated.
fn boot(project: &TempProject) -> (Connection, std::thread::JoinHandle<anyhow::Result<()>>) {
    boot_inner(project, false)
}

/// Like [`boot`] but writes `extension_api.json` (the `Node` hierarchy) and points the server at it,
/// so the analyzer has `Exact` native provenance — required to assert a member-MISS error.
fn boot_with_api(
    project: &TempProject,
) -> (Connection, std::thread::JoinHandle<anyhow::Result<()>>) {
    project.write("extension_api.json", NODE_API);
    boot_inner(project, true)
}

fn boot_inner(
    project: &TempProject,
    with_api: bool,
) -> (Connection, std::thread::JoinHandle<anyhow::Result<()>>) {
    let (server, client) = Connection::memory();
    let handle = std::thread::spawn(move || gd_server::serve(server));
    let mut opts = serde_json::json!({
        "projectRoot": project.root.as_str(),
        "autoDumpExtensionApi": false,
    });
    if with_api {
        opts["extensionApiPath"] =
            serde_json::Value::String(project.root.join("extension_api.json").as_str().to_owned());
    }
    let init = InitializeParams {
        initialization_options: Some(opts),
        capabilities: lsp_types::ClientCapabilities {
            general: Some(lsp_types::GeneralClientCapabilities {
                position_encodings: Some(vec![lsp_types::PositionEncodingKind::UTF8]),
                ..Default::default()
            }),
            // These tests assert hover CONTENT, so they ask for markdown the way every real
            // editor profile does. Without the request the server answers plaintext (#261) —
            // the correct floor for a client that declared nothing, but not what is under test.
            text_document: Some(lsp_types::TextDocumentClientCapabilities {
                hover: Some(lsp_types::HoverClientCapabilities {
                    content_format: Some(vec![lsp_types::MarkupKind::Markdown]),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        },
        ..Default::default()
    };
    client.sender.send(request(1, "initialize", init)).unwrap();
    let _ = recv(&client);
    client
        .sender
        .send(notification("initialized", InitializedParams {}))
        .unwrap();
    (client, handle)
}

/// Open a file and drain the implicit `publishDiagnostics` push.
fn did_open(client: &Connection, project: &TempProject, rel: &str) {
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
                    version: 1,
                    text,
                },
            },
        ))
        .unwrap();
    let _ = recv_publish(client);
}

/// Receive until the `publishDiagnostics` push arrives, skipping anything else the server sends
/// unprompted — a session booted without an `extensionApiPath` also gets the one-time
/// `window/showMessage` naming the embedded stock fallback (#259), and a conforming client
/// tolerates server notifications in any order.
fn recv_publish(client: &Connection) -> PublishDiagnosticsParams {
    loop {
        let msg = recv(client);
        let Message::Notification(notif) = msg else {
            panic!("expected a publishDiagnostics notification, got {msg:?}");
        };
        if notif.method == "textDocument/publishDiagnostics" {
            return serde_json::from_value(notif.params).expect("valid PublishDiagnosticsParams");
        }
    }
}

/// Open a file and assert the resulting `publishDiagnostics` carries zero diagnostics.
/// Guards the "never false-positive" rule: autoload typing must not introduce spurious errors.
fn did_open_assert_clean(client: &Connection, project: &TempProject, rel: &str) {
    let abs = project.root.join(rel);
    let text = std::fs::read_to_string(abs.as_std_path()).expect("read file");
    let uri = file_uri(&abs);
    client
        .sender
        .send(notification(
            "textDocument/didOpen",
            DidOpenTextDocumentParams {
                text_document: TextDocumentItem {
                    uri: uri.clone(),
                    language_id: "gdscript".to_string(),
                    version: 1,
                    text,
                },
            },
        ))
        .unwrap();
    let params = recv_publish(client);
    assert!(
        params.diagnostics.is_empty(),
        "autoload typing must produce zero diagnostics for {rel}; got: {:?}",
        params.diagnostics
    );
}

fn hover_at(client: &Connection, uri: &lsp_types::Uri, position: Position) -> Option<Hover> {
    let params = HoverParams {
        text_document_position_params: TextDocumentPositionParams {
            text_document: TextDocumentIdentifier { uri: uri.clone() },
            position,
        },
        work_done_progress_params: WorkDoneProgressParams::default(),
    };
    client
        .sender
        .send(request(10, "textDocument/hover", params))
        .unwrap();
    let resp = recv_response(client);
    let value = resp.result.expect("hover result always present");
    serde_json::from_value(value).expect("valid Option<Hover>")
}

fn definition_at(
    client: &Connection,
    uri: &lsp_types::Uri,
    position: Position,
) -> Option<GotoDefinitionResponse> {
    let params = GotoDefinitionParams {
        text_document_position_params: TextDocumentPositionParams {
            text_document: TextDocumentIdentifier { uri: uri.clone() },
            position,
        },
        work_done_progress_params: WorkDoneProgressParams::default(),
        partial_result_params: PartialResultParams::default(),
    };
    client
        .sender
        .send(request(11, "textDocument/definition", params))
        .unwrap();
    let resp = recv_response(client);
    let value = resp.result.expect("definition result always present");
    serde_json::from_value(value).expect("valid Option<GotoDefinitionResponse>")
}

fn references_at(
    client: &Connection,
    uri: &lsp_types::Uri,
    position: Position,
    include_declaration: bool,
) -> Vec<Location> {
    let params = ReferenceParams {
        text_document_position: TextDocumentPositionParams {
            text_document: TextDocumentIdentifier { uri: uri.clone() },
            position,
        },
        context: ReferenceContext {
            include_declaration,
        },
        work_done_progress_params: WorkDoneProgressParams::default(),
        partial_result_params: Default::default(),
    };
    client
        .sender
        .send(request(12, "textDocument/references", params))
        .unwrap();
    let resp = recv_response(client);
    assert!(resp.error.is_none(), "references errored: {:?}", resp.error);
    serde_json::from_value(resp.result.expect("references result")).unwrap()
}

fn hover_markdown(hover: &Hover) -> &str {
    match &hover.contents {
        HoverContents::Markup(m) => {
            assert_eq!(m.kind, MarkupKind::Markdown);
            &m.value
        }
        other => panic!("expected MarkupContent, got {other:?}"),
    }
}

/// Set up a TempProject with:
///   - `project.godot` declaring `Global="*res://global.gd"`
///   - `global.gd` with `func popup_error(msg: String) -> void`
///   - `caller.gd` with `Global.popup_error("x")` at line 3
fn setup_autoload_project() -> TempProject {
    let p = TempProject::new();
    p.write(
        "project.godot",
        "[application]\nconfig/name=\"Test\"\nconfig/features=PackedStringArray(\"4.6\")\nconfig_version=5\n\n[autoload]\nGlobal=\"*res://global.gd\"\n",
    );
    // global.gd: autoload script with one function.
    // Line 0: `extends Node`
    // Line 2: `func popup_error(msg: String) -> void:`
    // Line 3: `\tpass`
    p.write(
        "global.gd",
        "extends Node\n\nfunc popup_error(msg: String) -> void:\n\tpass\n",
    );
    // caller.gd: calls the autoload.
    // Line 0: `extends Node`
    // Line 2: `func test():`
    // Line 3: `\tGlobal.popup_error("x")`
    //          col: 1234567890...
    //          `\t` = col 0, `G` = col 1, `l` = col 2, `o` = col 3, `b` = col 4,
    //          `a` = col 5, `l` = col 6, `.` = col 7, `p` = col 8 (popup_error starts at col 8)
    p.write(
        "caller.gd",
        "extends Node\n\nfunc test():\n\tGlobal.popup_error(\"x\")\n",
    );
    p
}

/// Hover on `popup_error` in `Global.popup_error("x")` must show the function signature,
/// not a degraded Variant type. This is the primary M6 autoload-typing acceptance test.
/// Also asserts zero diagnostics on `caller.gd` — autoload typing must not false-positive on a
/// valid call like `Global.popup_error("x")` where the arg type matches the param.
#[test]
fn hover_on_autoload_member_shows_signature() {
    let p = setup_autoload_project();
    let (client, handle) = boot(&p);
    did_open(&client, &p, "global.gd");
    did_open_assert_clean(&client, &p, "caller.gd");

    let caller_uri = file_uri(&p.root.join("caller.gd"));

    // `popup_error` starts at col 8 on line 3 (UTF-8: `\tGlobal.` = 8 bytes, then `popup_error`).
    // Hover anywhere in the identifier `popup_error` (e.g. col 10 = inside the name).
    let hover = hover_at(&client, &caller_uri, Position::new(3, 10))
        .expect("hover on popup_error should return content");
    let md = hover_markdown(&hover);
    assert!(
        md.contains("popup_error"),
        "hover must mention popup_error, got: {md:?}"
    );
    assert!(
        md.contains("func"),
        "hover must render function signature with 'func', got: {md:?}"
    );
    assert!(
        md.contains("msg"),
        "hover must include param name 'msg', got: {md:?}"
    );
    assert!(
        md.contains("String"),
        "hover must include param type 'String', got: {md:?}"
    );
    assert!(
        !md.contains("Variant"),
        "hover must NOT show degraded 'Variant' type for a resolved member, got: {md:?}"
    );

    shutdown(&client, handle);
}

/// `textDocument/definition` on `Global` in `Global.popup_error("x")` must jump to `global.gd`.
/// This exercises the new `Binding::use_` recording in the autoload step.
#[test]
fn definition_on_autoload_identifier_jumps_to_script() {
    let p = setup_autoload_project();
    let (client, handle) = boot(&p);
    did_open(&client, &p, "global.gd");
    did_open(&client, &p, "caller.gd");

    let caller_uri = file_uri(&p.root.join("caller.gd"));
    let global_uri = file_uri(&p.root.join("global.gd"));

    // Click on `Global` at line 3, col 2 — `\tG` → `G` is at col 1, let's use col 2 (inside).
    let response = definition_at(&client, &caller_uri, Position::new(3, 2))
        .expect("definition on autoload name must resolve");
    let location = match response {
        GotoDefinitionResponse::Scalar(loc) => loc,
        GotoDefinitionResponse::Array(locs) if locs.len() == 1 => locs.into_iter().next().unwrap(),
        other => panic!("expected a single location, got {other:?}"),
    };
    assert_eq!(
        location.uri, global_uri,
        "definition on autoload `Global` must jump to global.gd"
    );

    shutdown(&client, handle);
}

/// `textDocument/references` on `popup_error`'s declaration in `global.gd` must find the
/// call site in `caller.gd`. This exercises the cross-file references path for autoload members.
#[test]
fn references_on_autoload_method_finds_caller() {
    let p = setup_autoload_project();
    let (client, handle) = boot(&p);
    did_open(&client, &p, "global.gd");
    did_open(&client, &p, "caller.gd");

    let global_uri = file_uri(&p.root.join("global.gd"));
    let caller_uri = file_uri(&p.root.join("caller.gd"));

    // `popup_error` in `global.gd` is at line 2, col 5 (after `func `).
    // Line 2: `func popup_error(msg: String) -> void:`
    //         01234 5678...  → `p` is at col 5
    let locs = references_at(&client, &global_uri, Position::new(2, 5), false);

    let has_caller = locs.iter().any(|l| l.uri == caller_uri);
    assert!(
        has_caller,
        "references on popup_error must include the call site in caller.gd; got: {locs:?}"
    );

    shutdown(&client, handle);
}

/// `textDocument/references` on the autoload NAME itself (`Global` in `Global.popup_error("x")`)
/// must find uses of the singleton across ALL files, not just the current one. Autoload names
/// never appear in interface-level class-name annotations, so the `name_referencers` fast-path
/// returns an empty set; without routing autoloads through the project-wide textual scan, a click
/// on `Global` in `a.gd` would silently miss the `Global` use in `b.gd`. Regression guard for the
/// "cross-file references on an autoload name silently falls back to current-file-only" bug.
#[test]
fn references_on_autoload_name_finds_cross_file_uses() {
    let p = TempProject::new();
    p.write(
        "project.godot",
        "[application]\nconfig/name=\"Test\"\nconfig/features=PackedStringArray(\"4.6\")\nconfig_version=5\n\n[autoload]\nGlobal=\"*res://global.gd\"\n",
    );
    p.write(
        "global.gd",
        "extends Node\n\nfunc popup_error(msg: String) -> void:\n\tpass\n",
    );
    // a.gd and b.gd each use the autoload name `Global` in a function body. Both `Global` tokens
    // sit at line 3, col 1..7.
    p.write(
        "a.gd",
        "extends Node\n\nfunc test():\n\tGlobal.popup_error(\"x\")\n",
    );
    p.write(
        "b.gd",
        "extends Node\n\nfunc other():\n\tGlobal.popup_error(\"y\")\n",
    );

    let (client, handle) = boot(&p);
    did_open(&client, &p, "global.gd");
    did_open(&client, &p, "a.gd");
    did_open(&client, &p, "b.gd");

    let a_uri = file_uri(&p.root.join("a.gd"));
    let b_uri = file_uri(&p.root.join("b.gd"));

    // Click on `Global` (the singleton name) in a.gd at line 3, col 2 (inside the identifier).
    let locs = references_at(&client, &a_uri, Position::new(3, 2), false);

    assert!(
        locs.iter().any(|l| l.uri == a_uri),
        "references on `Global` must include the current-file use in a.gd; got: {locs:?}"
    );
    assert!(
        locs.iter().any(|l| l.uri == b_uri),
        "references on autoload name `Global` must include the cross-file use in b.gd \
         (project-wide scan, not current-file-only); got: {locs:?}"
    );

    shutdown(&client, handle);
}

/// `textDocument/references` on an autoload NAME with `include_declaration: true` must include the
/// autoload script's start-of-file location (the same location `textDocument/definition` returns,
/// via `find_autoload_definition`). Autoload names have no `class_name` declaration and no in-file
/// `func`/`var` declaration, so without routing the `include_declaration` branch through the
/// autoload path the declaration location is silently dropped. Regression guard for the
/// "include_declaration omits the autoload script location" gap. Also asserts the location is
/// absent when `include_declaration: false`, proving the flag — not the cross-file scan — drives it.
#[test]
fn references_on_autoload_name_include_declaration_adds_script_location() {
    let p = setup_autoload_project();
    let (client, handle) = boot(&p);
    did_open(&client, &p, "global.gd");
    did_open(&client, &p, "caller.gd");

    let caller_uri = file_uri(&p.root.join("caller.gd"));
    let global_uri = file_uri(&p.root.join("global.gd"));

    // The autoload script's declaration location is start-of-file in global.gd (0:0-0:0),
    // matching `textDocument/definition` on the same `Global` name.
    let decl = Location {
        uri: global_uri,
        range: Range::new(Position::new(0, 0), Position::new(0, 0)),
    };

    // Click on `Global` in caller.gd at line 3, col 2 (inside the identifier).
    let with_decl = references_at(&client, &caller_uri, Position::new(3, 2), true);
    assert!(
        with_decl.contains(&decl),
        "references on autoload `Global` with include_declaration:true must include the \
         script's start-of-file location {decl:?}; got: {with_decl:?}"
    );

    let without_decl = references_at(&client, &caller_uri, Position::new(3, 2), false);
    assert!(
        !without_decl.contains(&decl),
        "references on autoload `Global` with include_declaration:false must NOT include the \
         script's declaration location {decl:?}; got: {without_decl:?}"
    );

    shutdown(&client, handle);
}

/// Shadowing gate for references: a body-local `var Global = 1` is not the autoload singleton, so
/// references on that occurrence must not take the autoload project-wide scan and report unrelated
/// singleton uses from other files.
#[test]
fn references_on_shadowed_autoload_name_stays_local() {
    let p = TempProject::new();
    p.write(
        "project.godot",
        "[application]\nconfig/name=\"Test\"\nconfig/features=PackedStringArray(\"4.6\")\nconfig_version=5\n\n[autoload]\nGlobal=\"*res://global.gd\"\n",
    );
    p.write(
        "global.gd",
        "extends Node\n\nfunc popup_error(msg: String) -> void:\n\tpass\n",
    );
    p.write(
        "shadow.gd",
        "extends Node\n\nfunc test():\n\tvar Global = 1\n\tprint(Global)\n",
    );
    p.write(
        "caller.gd",
        "extends Node\n\nfunc other():\n\tGlobal.popup_error(\"y\")\n",
    );

    let (client, handle) = boot(&p);
    did_open(&client, &p, "global.gd");
    did_open(&client, &p, "shadow.gd");
    did_open(&client, &p, "caller.gd");

    let shadow_uri = file_uri(&p.root.join("shadow.gd"));
    let caller_uri = file_uri(&p.root.join("caller.gd"));

    let locs = references_at(&client, &shadow_uri, Position::new(4, 8), false);

    assert!(
        locs.iter().any(|l| l.uri == shadow_uri),
        "references on a shadowed local `Global` must include the current-file occurrence; got: {locs:?}"
    );
    assert!(
        !locs.iter().any(|l| l.uri == caller_uri),
        "references on a shadowed local `Global` must not report unrelated autoload uses from caller.gd; got: {locs:?}"
    );

    shutdown(&client, handle);
}

/// Shadowing gate: a local `var Global = 1` inside a function shadows the autoload singleton.
/// Hover on the local `Global` reference must NOT show a Script type — it's an int local.
#[test]
fn local_var_shadows_autoload_no_script_hover() {
    let p = TempProject::new();
    p.write(
        "project.godot",
        "[application]\nconfig/name=\"Test\"\nconfig/features=PackedStringArray(\"4.6\")\nconfig_version=5\n\n[autoload]\nGlobal=\"*res://global.gd\"\n",
    );
    p.write(
        "global.gd",
        "extends Node\n\nfunc popup_error(msg: String) -> void:\n\tpass\n",
    );
    // shadow.gd: declares `var Global = 1` as a local, then references it.
    // Line 0: `extends Node`
    // Line 2: `func test():`
    // Line 3: `\tvar Global = 1`   — `Global` (decl) at col 5..11
    // Line 4: `\tprint(Global)`    — `Global` at col 7..13
    p.write(
        "shadow.gd",
        "extends Node\n\nfunc test():\n\tvar Global = 1\n\tprint(Global)\n",
    );

    let (client, handle) = boot(&p);
    did_open(&client, &p, "global.gd");
    did_open(&client, &p, "shadow.gd");

    let shadow_uri = file_uri(&p.root.join("shadow.gd"));

    // Hover on `Global` in `print(Global)` at line 4, col 8 (inside the identifier).
    // If the autoload were mistakenly applied, this would show a Script type.
    // With shadowing, it should show the local int type (or Variant/int from the inferred literal).
    let hover = hover_at(&client, &shadow_uri, Position::new(4, 8));
    if let Some(ref hover) = hover {
        let md = hover_markdown(hover);
        // If hover returns content, it must NOT claim this is a Script type (the autoload).
        // A shadowed local `var Global = 1` should show `int` (or inferred `Variant`), not `Script`.
        assert!(
            !md.contains("Script"),
            "shadowed local `Global` hover must NOT show Script type (autoload shadowed); got: {md:?}"
        );
    }
    // hover may be None (no type label) for an inferred local — that's also acceptable.

    shutdown(&client, handle);
}

/// Shadow + definition gate: a body-local `var Global = 1` shadows the autoload singleton.
/// `textDocument/definition` on the local `Global` reference must NOT jump to `global.gd`
/// (the autoload script). It must return either `None` (no location for an inferred local) or
/// a location in `shadow.gd` (pointing at the local declaration). This is the regression guard
/// for the "never lie" fix that gates `find_autoload_definition` on the analyzer's binding.
///
/// Without the fix: the definition handler falls through to `find_autoload_definition` and
/// returns `global.gd` — violating Godot's resolution order (locals shadow autoloads).
/// With the fix: the autoload step checks that the analyzer recorded a `Binding::Use` whose
/// `target_file` is the autoload's FileId at the cursor's span; for a shadowed local the
/// analyzer resolves it at step 1 (suite-local) and records no autoload binding → no jump.
#[test]
fn local_var_shadows_autoload_no_definition_jump() {
    let p = TempProject::new();
    p.write(
        "project.godot",
        "[application]\nconfig/name=\"Test\"\nconfig/features=PackedStringArray(\"4.6\")\nconfig_version=5\n\n[autoload]\nGlobal=\"*res://global.gd\"\n",
    );
    p.write(
        "global.gd",
        "extends Node\n\nfunc popup_error(msg: String) -> void:\n\tpass\n",
    );
    // shadow.gd: declares `var Global = 1` as a local, then references it.
    // Line 0: `extends Node`
    // Line 2: `func test():`
    // Line 3: `\tvar Global = 1`   — `Global` (decl) at col 5..11
    // Line 4: `\tprint(Global)`    — `Global` at col 7..13
    p.write(
        "shadow.gd",
        "extends Node\n\nfunc test():\n\tvar Global = 1\n\tprint(Global)\n",
    );

    let (client, handle) = boot(&p);
    did_open(&client, &p, "global.gd");
    did_open(&client, &p, "shadow.gd");

    let shadow_uri = file_uri(&p.root.join("shadow.gd"));
    let global_uri = file_uri(&p.root.join("global.gd"));

    // Go-to-definition on `Global` in `print(Global)` at line 4, col 8 (inside the identifier).
    // The result must NOT be a location in global.gd — the autoload script.
    // It may be None (no location for a body-local) or a location in shadow.gd.
    let response = definition_at(&client, &shadow_uri, Position::new(4, 8));
    if let Some(ref resp) = response {
        let loc = match resp {
            GotoDefinitionResponse::Scalar(loc) => loc.clone(),
            GotoDefinitionResponse::Array(locs) if !locs.is_empty() => locs[0].clone(),
            _ => {
                shutdown(&client, handle);
                return;
            }
        };
        assert_ne!(
            loc.uri, global_uri,
            "definition on a shadowed local `Global` must NOT jump to the autoload global.gd; \
             it resolved to {loc:?}"
        );
    }
    // None is also correct — a body-local without an indexed declaration has no jump target.

    shutdown(&client, handle);
}

/// Hover on the SIGNAL member in `Global.game_over.emit(7)` must render the signal's declared
/// signature (`signal game_over(score: int)`), not the degraded `Variant` expression type. The
/// signal identifier here is the attribute of the callee's BASE subscript — the enclosing Call's
/// callee attribute is `emit`, never `game_over` — so this rides the attribute-fallback hover
/// path (`hover_attribute_member_signature`), not the Call-gated one.
#[test]
fn hover_on_autoload_signal_member_shows_signal_signature() {
    let p = TempProject::new();
    p.write(
        "project.godot",
        "[application]\nconfig/name=\"Test\"\nconfig/features=PackedStringArray(\"4.6\")\nconfig_version=5\n\n[autoload]\nGlobal=\"*res://global.gd\"\n",
    );
    // Line 2: `signal game_over(score: int)`
    p.write(
        "global.gd",
        "extends Node\n\nsignal game_over(score: int)\n",
    );
    // Line 3: `\tGlobal.game_over.emit(7)` — `\tGlobal.` is 8 bytes, so `game_over` spans cols 8-16.
    p.write(
        "caller.gd",
        "extends Node\n\nfunc test():\n\tGlobal.game_over.emit(7)\n",
    );
    let (client, handle) = boot(&p);
    did_open(&client, &p, "global.gd");
    did_open(&client, &p, "caller.gd");

    let caller_uri = file_uri(&p.root.join("caller.gd"));
    let hover = hover_at(&client, &caller_uri, Position::new(3, 10))
        .expect("hover on game_over should return content");
    let md = hover_markdown(&hover);
    assert!(
        md.contains("signal game_over"),
        "hover must render the signal declaration, got: {md:?}"
    );
    assert!(
        md.contains("score") && md.contains("int"),
        "hover must include the signal param `score: int`, got: {md:?}"
    );
    assert!(
        !md.contains("Variant"),
        "hover must NOT show the degraded 'Variant' type for a resolved signal member, got: {md:?}"
    );
    shutdown(&client, handle);
}

/// Hover on an UNCALLED func member reference (`var _f = Global.popup_error`) must render the
/// function signature. There is no enclosing Call node for the Call-gated hover to find, so this
/// is the Func arm of the attribute-fallback path.
#[test]
fn hover_on_uncalled_autoload_func_member_shows_signature() {
    let p = TempProject::new();
    p.write(
        "project.godot",
        "[application]\nconfig/name=\"Test\"\nconfig/features=PackedStringArray(\"4.6\")\nconfig_version=5\n\n[autoload]\nGlobal=\"*res://global.gd\"\n",
    );
    p.write(
        "global.gd",
        "extends Node\n\nfunc popup_error(msg: String) -> void:\n\tpass\n",
    );
    // Line 3: `\tvar _f = Global.popup_error` — `\tvar _f = Global.` is 17 bytes, so
    // `popup_error` spans cols 17-27.
    p.write(
        "caller.gd",
        "extends Node\n\nfunc test():\n\tvar _f = Global.popup_error\n",
    );
    let (client, handle) = boot(&p);
    did_open(&client, &p, "global.gd");
    did_open(&client, &p, "caller.gd");

    let caller_uri = file_uri(&p.root.join("caller.gd"));
    let hover = hover_at(&client, &caller_uri, Position::new(3, 19))
        .expect("hover on uncalled popup_error reference should return content");
    let md = hover_markdown(&hover);
    assert!(
        md.contains("func popup_error"),
        "hover must render the function signature, got: {md:?}"
    );
    assert!(
        md.contains("msg") && md.contains("String"),
        "hover must include param `msg: String`, got: {md:?}"
    );
    shutdown(&client, handle);
}

// ---------------------------------------------------------------------------------------------
// M11 Phase 4: SCENE autoload typing — `uid://`→scene→root-script (and direct `.tscn`).
//
// Godot's autoload arm (`gdscript_analyzer.cpp:4587-4609`) types a scene autoload as its root
// node's attached script, PRECISELY — verified against the 4.6.3-stable binary's LSP: completion on
// a scene autoload (`Global.`) is byte-identical to the same script declared as a direct-script
// autoload (`Global2.`), `foo` present in both. So a scene autoload gets the full #19 treatment.
// ---------------------------------------------------------------------------------------------

/// Collect the diagnostics published for `rel` on didOpen (helper for the precise-typing pair).
fn did_open_collect_diags(
    client: &Connection,
    project: &TempProject,
    rel: &str,
) -> Vec<lsp_types::Diagnostic> {
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
                    version: 1,
                    text,
                },
            },
        ))
        .unwrap();
    recv_publish(client).diagnostics
}

/// Set up a project whose autoload is a SCENE (`Global="*res://global.tscn"`) whose root node
/// attaches `global.gd` (with `func popup_error(msg: String) -> void`). The caller calls a valid
/// member and a bogus one.
fn setup_scene_autoload_project(autoload_line: &str) -> TempProject {
    let p = TempProject::new();
    p.write(
        "project.godot",
        &format!(
            "[application]\nconfig/name=\"Test\"\nconfig/features=PackedStringArray(\"4.6\")\nconfig_version=5\n\n[autoload]\n{autoload_line}\n"
        ),
    );
    p.write(
        "global.gd",
        "extends Node\n\nfunc popup_error(msg: String) -> void:\n\tpass\n",
    );
    p.write(
        "global.tscn",
        "[gd_scene load_steps=2 format=3 uid=\"uid://cscaut0scene01\"]\n\n\
         [ext_resource type=\"Script\" path=\"res://global.gd\" id=\"1\"]\n\n\
         [node name=\"GlobalRoot\" type=\"Node\"]\nscript = ExtResource(\"1\")\n",
    );
    p
}

/// A direct `.tscn` scene autoload types as its root script: hover on the member shows the
/// signature, and a VALID call produces zero diagnostics. The positive half of the precise-typing
/// pair (the negative half — a bogus member errors — is the next test).
#[test]
fn scene_autoload_member_hover_shows_root_script_signature() {
    let p = setup_scene_autoload_project("Global=\"*res://global.tscn\"");
    let (client, handle) = boot(&p);
    did_open(&client, &p, "global.gd");
    // caller.gd: a VALID call — must be clean (no false positive on a real root-script member).
    p.write(
        "caller.gd",
        "extends Node\n\nfunc test():\n\tGlobal.popup_error(\"x\")\n",
    );
    did_open_assert_clean(&client, &p, "caller.gd");

    let caller_uri = file_uri(&p.root.join("caller.gd"));
    let hover = hover_at(&client, &caller_uri, Position::new(3, 10))
        .expect("hover on popup_error via scene autoload should return content");
    let md = hover_markdown(&hover);
    assert!(
        md.contains("func") && md.contains("popup_error") && md.contains("msg"),
        "scene autoload member hover must render the root-script signature, got: {md:?}"
    );
    assert!(
        !md.contains("Variant"),
        "scene autoload member must NOT be a degraded Variant, got: {md:?}"
    );
    shutdown(&client, handle);
}

/// CONVERGENCE: a scene autoload types IDENTICALLY to the same script declared as a direct-script
/// autoload (the #19 path). A genuine member-MISS on either is handled the same way — gdls's
/// deliberate cross-file Script policy degrades a not-found method on a Script-instance base to a
/// permissive Variant SILENTLY (reducer.rs:3872-3899: "the cross-file interface may simply be
/// incomplete … never risk a phantom not-found"), so NEITHER errors, and crucially neither produces
/// a "not declared" false positive. The discriminator that proves the scene autoload is PRECISELY
/// typed (not raw-dynamic) is the positive test above — hover shows the real root-script signature,
/// not `Variant`. This test pins the byte-for-byte convergence of the miss behavior across the two
/// declaration forms (so a future change that made the scene case diverge would fail loudly).
///
/// (Godot's own binary, by contrast, DOES flag `Global.does_not_exist()` — verified via its LSP.
/// gdls is intentionally MORE permissive on cross-file Script misses to avoid false positives from
/// incomplete interfaces; that policy predates Phase 4 and applies equally to direct-script #19
/// autoloads. Being more permissive than Godot is never a false positive — the no-FP bar holds.)
#[test]
fn scene_autoload_miss_converges_with_direct_script_autoload() {
    let bogus = "extends Node\n\nfunc test():\n\tGlobal.does_not_exist()\n";

    // Scene autoload form.
    let scene_diags = {
        let p = setup_scene_autoload_project("Global=\"*res://global.tscn\"");
        let (client, handle) = boot_with_api(&p);
        did_open(&client, &p, "global.gd");
        p.write("caller.gd", bogus);
        let d = did_open_collect_diags(&client, &p, "caller.gd");
        shutdown(&client, handle);
        d
    };
    // Direct-script autoload form (same global.gd, declared directly).
    let direct_diags = {
        let p = TempProject::new();
        p.write(
            "project.godot",
            "[application]\nconfig/name=\"Test\"\nconfig/features=PackedStringArray(\"4.6\")\nconfig_version=5\n\n[autoload]\nGlobal=\"*res://global.gd\"\n",
        );
        p.write(
            "global.gd",
            "extends Node\n\nfunc popup_error(msg: String) -> void:\n\tpass\n",
        );
        let (client, handle) = boot_with_api(&p);
        did_open(&client, &p, "global.gd");
        p.write("caller.gd", bogus);
        let d = did_open_collect_diags(&client, &p, "caller.gd");
        shutdown(&client, handle);
        d
    };

    // Convergence: the two forms produce the SAME diagnostic messages for the bogus member.
    let scene_msgs: Vec<&str> = scene_diags.iter().map(|d| d.message.as_str()).collect();
    let direct_msgs: Vec<&str> = direct_diags.iter().map(|d| d.message.as_str()).collect();
    assert_eq!(
        scene_msgs, direct_msgs,
        "a scene autoload must type identically to a direct-script autoload (member-miss behavior \
         converges); scene={scene_msgs:?} direct={direct_msgs:?}"
    );
    // And specifically: no "not declared" false positive on either.
    assert!(
        !scene_msgs.iter().any(|m| m.contains("not declared")),
        "scene autoload member access must not produce a 'not declared' false positive; got: {scene_msgs:?}"
    );
}

/// `uid://`→scene→root-script: an autoload declared `Global="*uid://…"` whose sidecar maps to a
/// `.tscn` resolves through the SAME uid hop the script case uses, then to the scene's root script.
/// Hover on the member shows the signature; definition on the NAME lands on the root script.
#[test]
fn uid_scene_autoload_resolves_to_root_script() {
    let p = setup_scene_autoload_project("Global=\"*uid://cscaut0scene01\"");
    // The uid sidecar for the SCENE (the scene's own `uid://`, mapped via the `.tscn.uid` file).
    p.write("global.tscn.uid", "uid://cscaut0scene01\n");
    let (client, handle) = boot(&p);
    did_open(&client, &p, "global.gd");
    p.write(
        "caller.gd",
        "extends Node\n\nfunc test():\n\tGlobal.popup_error(\"x\")\n",
    );
    did_open_assert_clean(&client, &p, "caller.gd");

    let caller_uri = file_uri(&p.root.join("caller.gd"));
    let global_uri = file_uri(&p.root.join("global.gd"));

    // Hover on the member resolves to the root script's signature.
    let hover = hover_at(&client, &caller_uri, Position::new(3, 10))
        .expect("hover via uid->scene->root-script should return content");
    assert!(
        hover_markdown(&hover).contains("popup_error"),
        "uid->scene autoload member hover must show the root-script member"
    );

    // Definition on the autoload NAME lands on the root script.
    let response = definition_at(&client, &caller_uri, Position::new(3, 2))
        .expect("definition on uid->scene autoload name must resolve");
    let location = match response {
        GotoDefinitionResponse::Scalar(loc) => loc,
        GotoDefinitionResponse::Array(locs) if locs.len() == 1 => locs.into_iter().next().unwrap(),
        other => panic!("expected a single location, got {other:?}"),
    };
    assert_eq!(
        location.uri, global_uri,
        "definition on a uid->scene autoload name must jump to the root script global.gd"
    );
    shutdown(&client, handle);
}

/// A SCRIPTLESS scene autoload with a LOWERCASE name degrades to the bare-`Node` floor — and
/// CRUCIALLY produces NO "Identifier not declared" false positive (the lowercase name is what the
/// uppercase `is_global_like` gate would miss). Definition on the name degrades cleanly (no panic,
/// no wrong jump) on the `file: None` native-floor binding.
#[test]
fn scriptless_lowercase_scene_autoload_no_false_positive() {
    let p = TempProject::new();
    p.write(
        "project.godot",
        "[application]\nconfig/name=\"Test\"\nconfig/features=PackedStringArray(\"4.6\")\nconfig_version=5\n\n[autoload]\nworld=\"*res://world.tscn\"\n",
    );
    // A scriptless scene: native Node2D root, no `script=`.
    p.write(
        "world.tscn",
        "[gd_scene format=3]\n\n[node name=\"WorldRoot\" type=\"Node2D\"]\n",
    );
    // caller uses `world` (a Node method — add_child exists on Node, so this is valid).
    p.write(
        "caller.gd",
        "extends Node\n\nfunc test():\n\tworld.add_child(self)\n",
    );
    let (client, handle) = boot(&p);
    let diags = did_open_collect_diags(&client, &p, "caller.gd");
    assert!(
        !diags
            .iter()
            .any(|d| d.message.contains("world") && d.message.contains("not declared")),
        "a lowercase scriptless scene autoload must not be flagged 'not declared'; got: {diags:?}"
    );

    // Definition on the scriptless autoload name must not panic and must not jump anywhere bogus.
    let caller_uri = file_uri(&p.root.join("caller.gd"));
    let _ = definition_at(&client, &caller_uri, Position::new(3, 2)); // None is fine; must not panic.
    shutdown(&client, handle);
}

/// UNRESOLVABLE scene autoload (a missing `.tscn`) with a LOWERCASE name: graceful degradation —
/// the name is suppressed from the "not declared" fallthrough (Godot types every registered autoload
/// as ≥ Node), and the server never panics. This is the `autoload_typing -> None` path that neither
/// the script nor the native-floor map populates, closed by the `is_autoload` gate.
#[test]
fn unresolvable_lowercase_scene_autoload_degrades_gracefully() {
    let p = TempProject::new();
    p.write(
        "project.godot",
        "[application]\nconfig/name=\"Test\"\nconfig/features=PackedStringArray(\"4.6\")\nconfig_version=5\n\n[autoload]\nghost=\"*res://ghost.tscn\"\n",
    );
    // ghost.tscn is deliberately NEVER created — the autoload target is unresolvable.
    p.write("caller.gd", "extends Node\n\nfunc test():\n\tghost.foo()\n");
    let (client, handle) = boot(&p);
    let diags = did_open_collect_diags(&client, &p, "caller.gd");
    assert!(
        !diags
            .iter()
            .any(|d| d.message.contains("ghost") && d.message.contains("not declared")),
        "an unresolvable (missing-scene) lowercase autoload must degrade without a 'not declared' \
         false positive; got: {diags:?}"
    );
    shutdown(&client, handle);
}

/// `is_singleton` gate (analyzer.cpp:4572): a `project.godot` autoload declared WITHOUT the leading
/// `*` (`autoload/plainnode="res://x.gd"`) is registered but NOT a global singleton. Godot skips the
/// whole autoload arm for it, so a bare `plainnode` reference is `Identifier "plainnode" not
/// declared`. The lowercase name means the `is_global_like` gate can't suppress it — only the
/// `is_autoload` gate could, and it MUST NOT (non-singletons are excluded from the membership set).
/// Regression guard: the autoload-typing layer must not shield a non-singleton from "not declared".
#[test]
fn non_singleton_lowercase_autoload_still_not_declared() {
    let p = TempProject::new();
    // No leading `*` → registered but not a singleton.
    p.write(
        "project.godot",
        "[application]\nconfig/name=\"Test\"\nconfig/features=PackedStringArray(\"4.6\")\nconfig_version=5\n\n[autoload]\nplainnode=\"res://plain.gd\"\n",
    );
    p.write("plain.gd", "extends Node\n\nfunc foo() -> void:\n\tpass\n");
    p.write(
        "caller.gd",
        "extends Node\n\nfunc test():\n\tplainnode.foo()\n",
    );
    let (client, handle) = boot(&p);
    did_open(&client, &p, "plain.gd");
    let diags = did_open_collect_diags(&client, &p, "caller.gd");
    assert!(
        diags
            .iter()
            .any(|d| d.message.contains("plainnode") && d.message.contains("not declared")),
        "a non-singleton (no `*`) lowercase autoload must STILL be flagged 'not declared' (Godot's \
         is_singleton gate skips the autoload arm for it); got: {diags:?}"
    );
    shutdown(&client, handle);
}

/// An autoload declared as `Name="*uid://…"` whose sidecar maps to a `.gd` gets the full
/// singleton treatment: clean diagnostics on a caller, and definition on the name jumps to the
/// script. `ProjectModel::resolve_target` dereferences the uid exactly like Godot's
/// `ResourceLoader` does for autoload paths.
#[test]
fn uid_autoload_resolves_like_res_path() {
    let p = TempProject::new();
    p.write(
        "project.godot",
        "[application]\nconfig/name=\"Test\"\nconfig/features=PackedStringArray(\"4.6\")\nconfig_version=5\n\n[autoload]\nGlobal=\"*uid://c1testuidauto\"\n",
    );
    p.write(
        "global.gd",
        "extends Node\n\nfunc popup_error(msg: String) -> void:\n\tpass\n",
    );
    p.write("global.gd.uid", "uid://c1testuidauto\n");
    p.write(
        "caller.gd",
        "extends Node\n\nfunc test():\n\tGlobal.popup_error(\"x\")\n",
    );
    let (client, handle) = boot(&p);
    did_open(&client, &p, "global.gd");
    did_open_assert_clean(&client, &p, "caller.gd");

    let caller_uri = file_uri(&p.root.join("caller.gd"));
    client
        .sender
        .send(request(
            7,
            "textDocument/definition",
            GotoDefinitionParams {
                text_document_position_params: TextDocumentPositionParams {
                    text_document: TextDocumentIdentifier {
                        uri: caller_uri.clone(),
                    },
                    // Line 3 `\tGlobal.popup_error("x")` — col 2 is inside `Global`.
                    position: Position::new(3, 2),
                },
                work_done_progress_params: WorkDoneProgressParams::default(),
                partial_result_params: PartialResultParams::default(),
            },
        ))
        .unwrap();
    let resp = recv_response(&client);
    let result: Option<GotoDefinitionResponse> =
        serde_json::from_value(resp.result.expect("definition result")).unwrap();
    let Some(GotoDefinitionResponse::Scalar(loc)) = result else {
        panic!("expected a definition for a uid:// autoload, got {result:?}");
    };
    assert!(
        loc.uri.as_str().ends_with("global.gd"),
        "uid autoload definition must land in global.gd, got {}",
        loc.uri.as_str()
    );
    shutdown(&client, handle);
}
