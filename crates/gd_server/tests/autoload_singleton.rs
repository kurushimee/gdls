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

use common::{file_uri, notification, recv, request, shutdown, TempProject};
use lsp_server::{Connection, Message};
use lsp_types::{
    DidOpenTextDocumentParams, GotoDefinitionParams, GotoDefinitionResponse, Hover, HoverContents,
    HoverParams, InitializeParams, InitializedParams, Location, MarkupKind, PartialResultParams,
    Position, PublishDiagnosticsParams, ReferenceContext, ReferenceParams, TextDocumentIdentifier,
    TextDocumentItem, TextDocumentPositionParams, WorkDoneProgressParams,
};

/// Boot a server over a TempProject with UTF-8 position encoding negotiated.
fn boot(project: &TempProject) -> (Connection, std::thread::JoinHandle<anyhow::Result<()>>) {
    let (server, client) = Connection::memory();
    let handle = std::thread::spawn(move || gd_server::serve(server));
    let init = InitializeParams {
        initialization_options: Some(serde_json::json!({
            "projectRoot": project.root.as_str(),
        })),
        capabilities: lsp_types::ClientCapabilities {
            general: Some(lsp_types::GeneralClientCapabilities {
                position_encodings: Some(vec![lsp_types::PositionEncodingKind::UTF8]),
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
    // Drain the publishDiagnostics push.
    let _ = recv(client);
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
    // Parse the publishDiagnostics push and assert empty.
    let msg = recv(client);
    let Message::Notification(notif) = msg else {
        panic!("expected publishDiagnostics notification after didOpen, got {msg:?}");
    };
    assert_eq!(
        notif.method, "textDocument/publishDiagnostics",
        "expected publishDiagnostics, got {}",
        notif.method
    );
    let params: PublishDiagnosticsParams =
        serde_json::from_value(notif.params).expect("valid PublishDiagnosticsParams");
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
    let Message::Response(resp) = recv(client) else {
        panic!("expected hover response");
    };
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
    let Message::Response(resp) = recv(client) else {
        panic!("expected definition response");
    };
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
    let Message::Response(resp) = recv(client) else {
        panic!("expected references response");
    };
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
        "[application]\nconfig/name=\"Test\"\nconfig_version=5\n\n[autoload]\nGlobal=\"*res://global.gd\"\n",
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

/// Shadowing gate: a local `var Global = 1` inside a function shadows the autoload singleton.
/// Hover on the local `Global` reference must NOT show a Script type — it's an int local.
#[test]
fn local_var_shadows_autoload_no_script_hover() {
    let p = TempProject::new();
    p.write(
        "project.godot",
        "[application]\nconfig/name=\"Test\"\nconfig_version=5\n\n[autoload]\nGlobal=\"*res://global.gd\"\n",
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
        "[application]\nconfig/name=\"Test\"\nconfig_version=5\n\n[autoload]\nGlobal=\"*res://global.gd\"\n",
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
