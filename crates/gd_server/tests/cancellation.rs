//! M5 WP-O4 — end-to-end `$/cancelRequest` wire test against the in-memory server.
//!
//! The architecture is single-threaded today (the LSP loop blocks on the current handler until
//! it returns), so a cancel notification that arrives DURING a handler's run cannot interrupt it
//! — the cancel sits in the channel buffer until the handler returns and the loop re-enters
//! `select!`. The mid-flight-interrupt scenario the M5 plan §6B sketches requires either threaded
//! handlers (out of scope for Phase B) or queue pile-up (multiple requests pending so a cancel
//! arrives before a later one is dispatched).
//!
//! What these tests cover:
//! - Unknown-id cancel: server warn-logs, doesn't panic, keeps serving.
//! - Race after response: client sends cancel for an id whose response has already been sent;
//!   server warn-logs, doesn't double-respond, keeps serving.
//! - Pre-cancelled-token analyzer bail: tested separately in `gd_analyze/tests/governor.rs`
//!   (the token mechanism itself doesn't need the full LSP wire to validate).

use std::time::Duration;

use lsp_server::{Connection, Message, Notification, Request, RequestId};
use lsp_types::{
    CancelParams, ClientCapabilities, DidOpenTextDocumentParams, DocumentSymbolParams,
    InitializeParams, InitializedParams, NumberOrString, TextDocumentIdentifier, TextDocumentItem,
    Uri,
};

fn recv(conn: &Connection) -> Message {
    conn.receiver
        .recv_timeout(Duration::from_secs(5))
        .expect("timed out waiting for a message from the server")
}

fn try_recv(conn: &Connection, timeout: Duration) -> Option<Message> {
    conn.receiver.recv_timeout(timeout).ok()
}

fn request(id: i32, method: &str, params: serde_json::Value) -> Message {
    Message::Request(Request {
        id: RequestId::from(id),
        method: method.to_string(),
        params,
    })
}

fn notification(method: &str, params: serde_json::Value) -> Message {
    Message::Notification(Notification {
        method: method.to_string(),
        params,
    })
}

/// Drive an in-memory LSP session to the point where `documentSymbol` works (initialize +
/// initialized + didOpen on a trivial file). Returns the client connection and the open-buffer
/// URI; the caller drains the publishDiagnostics push, then drives whichever cancellation
/// scenario it wants.
fn init_session() -> (Connection, std::thread::JoinHandle<anyhow::Result<()>>, Uri) {
    let (server, client) = Connection::memory();
    let server_thread = std::thread::spawn(move || gd_server::serve(server));

    let init = InitializeParams {
        capabilities: ClientCapabilities::default(),
        ..Default::default()
    };
    client
        .sender
        .send(request(
            1,
            "initialize",
            serde_json::to_value(init).unwrap(),
        ))
        .unwrap();
    let _initialize_response = recv(&client);
    client
        .sender
        .send(notification(
            "initialized",
            serde_json::to_value(InitializedParams {}).unwrap(),
        ))
        .unwrap();

    let uri: Uri = "file:///test/a.gd".parse().unwrap();
    client
        .sender
        .send(notification(
            "textDocument/didOpen",
            serde_json::to_value(DidOpenTextDocumentParams {
                text_document: TextDocumentItem {
                    uri: uri.clone(),
                    language_id: "gdscript".to_string(),
                    version: 1,
                    text: "extends Node\nfunc main() -> void:\n\tpass\n".to_string(),
                },
            })
            .unwrap(),
        ))
        .unwrap();
    let _publish_diagnostics = recv(&client);

    (client, server_thread, uri)
}

fn shutdown(client: &Connection, server_thread: std::thread::JoinHandle<anyhow::Result<()>>) {
    client
        .sender
        .send(request(99, "shutdown", serde_json::Value::Null))
        .unwrap();
    let _ = recv(client);
    client
        .sender
        .send(notification("exit", serde_json::Value::Null))
        .unwrap();
    server_thread
        .join()
        .expect("server thread panicked")
        .expect("serve() returned an error");
}

#[test]
fn cancel_for_unknown_id_does_not_panic_or_block_subsequent_requests() {
    let (client, server_thread, uri) = init_session();

    // Send a cancel for an id that never existed. The server should warn-log and continue;
    // it must NOT send a response (LSP 3.17 — `$/cancelRequest` is a notification).
    client
        .sender
        .send(notification(
            "$/cancelRequest",
            serde_json::to_value(CancelParams {
                id: NumberOrString::Number(9999),
            })
            .unwrap(),
        ))
        .unwrap();

    // Send a normal request — if the cancel-arm panicked or wedged the loop, this hangs.
    client
        .sender
        .send(request(
            2,
            "textDocument/documentSymbol",
            serde_json::to_value(DocumentSymbolParams {
                text_document: TextDocumentIdentifier { uri: uri.clone() },
                work_done_progress_params: Default::default(),
                partial_result_params: Default::default(),
            })
            .unwrap(),
        ))
        .unwrap();

    let Message::Response(resp) = recv(&client) else {
        panic!("expected a documentSymbol response after the cancel");
    };
    assert_eq!(resp.id, RequestId::from(2));
    assert!(
        resp.error.is_none(),
        "documentSymbol should succeed after an unknown-id cancel; got error={:?}",
        resp.error
    );

    shutdown(&client, server_thread);
}

#[test]
fn cancel_after_response_is_an_idempotent_noop() {
    let (client, server_thread, uri) = init_session();

    // Send + receive a normal documentSymbol request first.
    client
        .sender
        .send(request(
            2,
            "textDocument/documentSymbol",
            serde_json::to_value(DocumentSymbolParams {
                text_document: TextDocumentIdentifier { uri: uri.clone() },
                work_done_progress_params: Default::default(),
                partial_result_params: Default::default(),
            })
            .unwrap(),
        ))
        .unwrap();
    let Message::Response(_first) = recv(&client) else {
        panic!("expected a documentSymbol response");
    };

    // Now cancel id 2 — the response has already been sent and `pending_requests` cleared, so
    // the cancel-arm warn-logs and drops with no double-response.
    client
        .sender
        .send(notification(
            "$/cancelRequest",
            serde_json::to_value(CancelParams {
                id: NumberOrString::Number(2),
            })
            .unwrap(),
        ))
        .unwrap();

    // No second response should arrive for id 2 (within a short window). Sending a fresh request
    // (id 3) lets us verify the loop is still healthy by waiting on ITS response.
    client
        .sender
        .send(request(
            3,
            "textDocument/documentSymbol",
            serde_json::to_value(DocumentSymbolParams {
                text_document: TextDocumentIdentifier { uri: uri.clone() },
                work_done_progress_params: Default::default(),
                partial_result_params: Default::default(),
            })
            .unwrap(),
        ))
        .unwrap();
    let third = recv(&client);
    let Message::Response(resp) = third else {
        panic!("expected a documentSymbol response for id 3; got {third:?}");
    };
    assert_eq!(
        resp.id,
        RequestId::from(3),
        "the response received after the stale cancel must be for the LATER request (id 3), \
         not a phantom double-response for the cancelled id 2"
    );

    shutdown(&client, server_thread);
}

#[test]
fn malformed_cancel_params_does_not_panic_or_block() {
    let (client, server_thread, uri) = init_session();

    // $/cancelRequest with no `id` field — the parse_params should warn-log and the loop keeps
    // going. A non-conforming client that sends `{}` or junk must not be able to kill the
    // session.
    client
        .sender
        .send(notification(
            "$/cancelRequest",
            serde_json::json!({"id": null}),
        ))
        .unwrap();
    // also try a malformed top-level shape (an array where an object is expected)
    client
        .sender
        .send(notification(
            "$/cancelRequest",
            serde_json::json!([1, 2, 3]),
        ))
        .unwrap();

    // No response should arrive from a notification ever.
    let stray = try_recv(&client, Duration::from_millis(100));
    assert!(
        stray.is_none(),
        "no response should arrive for any `$/cancelRequest` notification; got {stray:?}"
    );

    // Verify the loop is still healthy by issuing a normal request.
    client
        .sender
        .send(request(
            5,
            "textDocument/documentSymbol",
            serde_json::to_value(DocumentSymbolParams {
                text_document: TextDocumentIdentifier { uri: uri.clone() },
                work_done_progress_params: Default::default(),
                partial_result_params: Default::default(),
            })
            .unwrap(),
        ))
        .unwrap();
    let Message::Response(resp) = recv(&client) else {
        panic!("expected a documentSymbol response after the malformed cancels");
    };
    assert_eq!(resp.id, RequestId::from(5));
    assert!(resp.error.is_none());

    shutdown(&client, server_thread);
}
