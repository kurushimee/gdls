//! M7 (#58) — `workDoneProgress` over the in-memory wire: the cold-start token's
//! create → begin → report* → end pairing, the capability-ungated silence rule (no create,
//! ever), and client-token progress on `references` (independent of the window capability).

mod common;

use std::time::Duration;

use common::{file_uri, notification, recv, request, sample_project, shutdown, try_recv};
use lsp_server::{Connection, Message, RequestId, Response};
use lsp_types::{
    ClientCapabilities, DidOpenTextDocumentParams, InitializeParams, InitializedParams,
    PartialResultParams, Position, ProgressParams, ProgressParamsValue, ProgressToken,
    ReferenceContext, ReferenceParams, TextDocumentIdentifier, TextDocumentItem,
    TextDocumentPositionParams, WindowClientCapabilities, WorkDoneProgress, WorkDoneProgressParams,
};

/// Boot over the sample project with the given capabilities; do NOT drain anything after the
/// `initialized` notification (progress tests inspect the raw stream).
fn boot_raw(
    p: &common::TempProject,
    capabilities: ClientCapabilities,
) -> (Connection, std::thread::JoinHandle<anyhow::Result<()>>) {
    let (server, client) = Connection::memory();
    let server_thread = std::thread::spawn(move || gd_server::serve(server));

    let init = InitializeParams {
        capabilities,
        initialization_options: Some(serde_json::json!({
            "projectRoot": p.root.as_str(),
            "autoDumpExtensionApi": false,
            "extensionApiPath": p.root.join("extension_api.json").as_str(),
        })),
        ..Default::default()
    };
    client.sender.send(request(1, "initialize", init)).unwrap();
    loop {
        if let Message::Response(resp) = recv(&client) {
            assert!(resp.error.is_none(), "initialize failed: {:?}", resp.error);
            break;
        }
    }
    client
        .sender
        .send(notification("initialized", InitializedParams {}))
        .unwrap();
    (client, server_thread)
}

fn window_progress_caps() -> ClientCapabilities {
    ClientCapabilities {
        window: Some(WindowClientCapabilities {
            work_done_progress: Some(true),
            ..Default::default()
        }),
        ..Default::default()
    }
}

/// Decode a `$/progress` notification, if that's what `msg` is.
fn as_progress(msg: &Message) -> Option<(ProgressToken, &'static str)> {
    let Message::Notification(n) = msg else {
        return None;
    };
    if n.method != "$/progress" {
        return None;
    }
    let params: ProgressParams = serde_json::from_value(n.params.clone()).unwrap();
    let kind = match &params.value {
        ProgressParamsValue::WorkDone(WorkDoneProgress::Begin(_)) => "begin",
        ProgressParamsValue::WorkDone(WorkDoneProgress::Report(_)) => "report",
        ProgressParamsValue::WorkDone(WorkDoneProgress::End(_)) => "end",
    };
    Some((params.token, kind))
}

/// With `window.workDoneProgress`, the cold start produces exactly one server-initiated token:
/// one `window/workDoneProgress/create` (string-id request), then `$/progress` begin → … → end,
/// begin first and end last, all on the create's token — no orphans.
#[test]
fn cold_start_progress_is_one_cleanly_paired_token() {
    let p = sample_project();
    let (client, server_thread) = boot_raw(&p, window_progress_caps());

    let mut create_token: Option<ProgressToken> = None;
    let mut arc: Vec<(ProgressToken, &'static str)> = Vec::new();
    // Collect until the end event lands (the cold start is small here; 10 s is generous).
    loop {
        let msg = recv(&client);
        if let Message::Request(req) = &msg {
            assert_eq!(req.method, "window/workDoneProgress/create");
            assert!(
                create_token.is_none(),
                "exactly one create for the cold start"
            );
            let params: lsp_types::WorkDoneProgressCreateParams =
                serde_json::from_value(req.params.clone()).unwrap();
            create_token = Some(params.token);
            // Spec-conforming success reply (null result).
            client
                .sender
                .send(Message::Response(Response::new_ok(
                    req.id.clone(),
                    serde_json::Value::Null,
                )))
                .unwrap();
            continue;
        }
        if let Some(event) = as_progress(&msg) {
            let done = event.1 == "end";
            arc.push(event);
            if done {
                break;
            }
        }
    }

    let token = create_token.expect("a create must precede any $/progress");
    assert!(
        arc.iter().all(|(t, _)| *t == token),
        "every event rides the created token; got {arc:?}"
    );
    assert_eq!(arc.first().map(|(_, k)| *k), Some("begin"), "begin first");
    assert_eq!(arc.last().map(|(_, k)| *k), Some("end"), "end last");
    assert_eq!(
        arc.iter().filter(|(_, k)| *k == "begin").count(),
        1,
        "exactly one begin"
    );

    // #265: the token is a wire value, not a log line. lsp-server's `Display` for a string
    // `RequestId` renders through `Debug` on purpose, so interpolating the outgoing id with
    // `{id}` used to embed literal quotes: `gdls/progress/"gdls-out-0"`.
    let ProgressToken::String(text) = &token else {
        panic!("the cold-start token is server-minted and always a string; got {token:?}");
    };
    assert!(
        !text.contains('"') && !text.contains('\\'),
        "the progress token must carry the outgoing id's own text, with no Debug quoting: {text:?}"
    );
    assert!(
        text.starts_with("gdls/progress/gdls-out-"),
        "the token keeps its readable shape: {text:?}"
    );

    shutdown(&client, server_thread);
}

/// Without `window.workDoneProgress`, the server sends NO create and NO `$/progress` — ever
/// (the spec forbids server-initiated progress without the capability). Verified across the
/// whole cold start by driving a normal request and inspecting everything that arrived.
#[test]
fn no_capability_means_no_create_and_no_progress() {
    let p = sample_project();
    let (client, server_thread) = boot_raw(&p, ClientCapabilities::default());

    // Give the cold start a beat to finish, then drive a normal round-trip so there's a clear
    // "everything before this response" window to audit.
    let uri = file_uri(&p.root.join("src/hero.gd"));
    client
        .sender
        .send(notification(
            "textDocument/didOpen",
            DidOpenTextDocumentParams {
                text_document: TextDocumentItem {
                    uri: uri.clone(),
                    language_id: "gdscript".to_string(),
                    version: 1,
                    text: "class_name Hero\nextends Node2D\n\nvar hp: int = 10\n\nfunc attack() -> void:\n\tpass\n".to_string(),
                },
            },
        ))
        .unwrap();
    client
        .sender
        .send(request(
            2,
            "textDocument/documentSymbol",
            lsp_types::DocumentSymbolParams {
                text_document: TextDocumentIdentifier { uri },
                work_done_progress_params: Default::default(),
                partial_result_params: Default::default(),
            },
        ))
        .unwrap();

    loop {
        let msg = recv(&client);
        assert!(
            !matches!(&msg, Message::Request(r) if r.method == "window/workDoneProgress/create"),
            "no create without window.workDoneProgress"
        );
        assert!(
            as_progress(&msg).is_none(),
            "no server-initiated $/progress without the capability; got {msg:?}"
        );
        if matches!(&msg, Message::Response(r) if r.id == RequestId::from(2)) {
            break;
        }
    }

    shutdown(&client, server_thread);
}

/// A client `workDoneToken` inside request params is its own opt-in: `references` reports
/// begin → end on exactly that token with NO create request — even when the client never
/// advertised `window.workDoneProgress`.
#[test]
fn references_honors_client_work_done_token_without_window_capability() {
    let p = sample_project();
    // The method-scan must find candidates, or the (deliberately) deferred `begin` never fires
    // — enemy.gd references `attack` so the request has real per-file work to report.
    p.write("src/enemy.gd", "extends Hero\n\nfunc flee():\n\tattack()\n");
    let (client, server_thread) = boot_raw(&p, ClientCapabilities::default());

    let uri = file_uri(&p.root.join("src/hero.gd"));
    client
        .sender
        .send(notification(
            "textDocument/didOpen",
            DidOpenTextDocumentParams {
                text_document: TextDocumentItem {
                    uri: uri.clone(),
                    language_id: "gdscript".to_string(),
                    version: 1,
                    text: "class_name Hero\nextends Node2D\n\nvar hp: int = 10\n\nfunc attack() -> void:\n\tpass\n".to_string(),
                },
            },
        ))
        .unwrap();

    let token = ProgressToken::String("tok-references-1".to_string());
    client
        .sender
        .send(request(
            3,
            "textDocument/references",
            ReferenceParams {
                text_document_position: TextDocumentPositionParams {
                    text_document: TextDocumentIdentifier { uri },
                    // `attack` on line 5 (`func attack() -> void:`).
                    position: Position {
                        line: 5,
                        character: 6,
                    },
                },
                work_done_progress_params: WorkDoneProgressParams {
                    work_done_token: Some(token.clone()),
                },
                partial_result_params: PartialResultParams::default(),
                context: ReferenceContext {
                    include_declaration: true,
                },
            },
        ))
        .unwrap();

    let mut saw_begin = false;
    let mut saw_end = false;
    loop {
        let msg = recv(&client);
        assert!(
            !matches!(&msg, Message::Request(r) if r.method == "window/workDoneProgress/create"),
            "client-token progress must not send a create"
        );
        if let Some((t, kind)) = as_progress(&msg) {
            assert_eq!(t, token, "progress rides the client's own token");
            match kind {
                "begin" => saw_begin = true,
                "end" => saw_end = true,
                _ => {}
            }
            continue;
        }
        if matches!(&msg, Message::Response(r) if r.id == RequestId::from(3)) {
            break;
        }
    }
    assert!(saw_begin, "begin on the client token before the response");
    assert!(saw_end, "end on the client token before the response");

    // No progress stragglers after the response (the arc closed with the handler).
    let extra = try_recv(&client, Duration::from_millis(150));
    if let Some(msg) = extra {
        assert!(
            as_progress(&msg).is_none(),
            "no $/progress after the response; got {msg:?}"
        );
    }

    shutdown(&client, server_thread);
}

/// A client that REJECTS the create must not break the session: the cold start completes, the
/// reporter is poisoned (suppressing whatever had not yet been sent — unit-tested in
/// `progress.rs`), and normal requests keep serving.
#[test]
fn rejected_create_degrades_gracefully() {
    let p = sample_project();
    let (client, server_thread) = boot_raw(&p, window_progress_caps());

    // Reject the create the moment it arrives.
    loop {
        let msg = recv(&client);
        if let Message::Request(req) = &msg {
            assert_eq!(req.method, "window/workDoneProgress/create");
            client
                .sender
                .send(Message::Response(Response::new_err(
                    req.id.clone(),
                    -32600,
                    "no progress for you".to_string(),
                )))
                .unwrap();
            break;
        }
    }

    // The session stays healthy.
    let uri = file_uri(&p.root.join("src/enemy.gd"));
    client
        .sender
        .send(notification(
            "textDocument/didOpen",
            DidOpenTextDocumentParams {
                text_document: TextDocumentItem {
                    uri: uri.clone(),
                    language_id: "gdscript".to_string(),
                    version: 1,
                    text: "extends Hero\n\nfunc flee():\n\tpass\n".to_string(),
                },
            },
        ))
        .unwrap();
    client
        .sender
        .send(request(
            4,
            "textDocument/documentSymbol",
            lsp_types::DocumentSymbolParams {
                text_document: TextDocumentIdentifier { uri },
                work_done_progress_params: Default::default(),
                partial_result_params: Default::default(),
            },
        ))
        .unwrap();
    loop {
        if let Message::Response(resp) = recv(&client) {
            assert_eq!(resp.id, RequestId::from(4));
            assert!(resp.error.is_none());
            break;
        }
    }

    shutdown(&client, server_thread);
}
