//! M0 exit gate: drive the full server over an in-memory connection and verify the LSP lifecycle,
//! capability advertisement + encoding negotiation, the diagnostics push path, a `documentSymbol`
//! response, and a clean shutdown/exit.

use std::time::Duration;

use lsp_server::{Connection, Message, Notification, Request, RequestId};
use lsp_types::{
    ClientCapabilities, DidOpenTextDocumentParams, DocumentSymbolClientCapabilities,
    DocumentSymbolParams, GeneralClientCapabilities, InitializeParams, InitializeResult,
    InitializedParams, PositionEncodingKind, PublishDiagnosticsParams,
    TextDocumentClientCapabilities, TextDocumentIdentifier, TextDocumentItem, Uri,
};

/// Receive one message from the server, failing the test rather than hanging if none arrives.
fn recv(conn: &Connection) -> Message {
    conn.receiver
        .recv_timeout(Duration::from_secs(5))
        .expect("timed out waiting for a message from the server")
}

/// `recv`, skipping server-initiated notifications (a `publishDiagnostics` can land later than a
/// timeout-based drain expected on a slow host) until a Response arrives.
fn recv_response(conn: &Connection) -> lsp_server::Response {
    loop {
        if let Message::Response(resp) = recv(conn) {
            return resp;
        }
    }
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

#[test]
fn m0_lifecycle_diagnostics_and_symbols() {
    let (server, client) = Connection::memory();
    let server_thread = std::thread::spawn(move || gd_server::serve(server));

    // 1) initialize — offer UTF-16 + UTF-8 (the server should prefer UTF-8) and advertise
    //    hierarchical documentSymbol support so step 4 gets the nested shape (the flat
    //    downgrade for clients without the capability is covered in symbols_and_diagnostics.rs).
    let init = InitializeParams {
        capabilities: ClientCapabilities {
            general: Some(GeneralClientCapabilities {
                position_encodings: Some(vec![
                    PositionEncodingKind::UTF16,
                    PositionEncodingKind::UTF8,
                ]),
                ..Default::default()
            }),
            text_document: Some(TextDocumentClientCapabilities {
                document_symbol: Some(DocumentSymbolClientCapabilities {
                    hierarchical_document_symbol_support: Some(true),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        },
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

    let resp = recv_response(&client);
    assert_eq!(resp.id, RequestId::from(1));
    assert!(resp.error.is_none(), "initialize errored: {:?}", resp.error);
    let result: InitializeResult =
        serde_json::from_value(resp.result.expect("initialize result")).unwrap();
    let caps = result.capabilities;
    assert_eq!(
        caps.position_encoding,
        Some(PositionEncodingKind::UTF8),
        "server should negotiate UTF-8 when the client offers it"
    );
    // Exactly the v1 surface must be advertised.
    assert!(caps.text_document_sync.is_some());
    assert!(caps.document_symbol_provider.is_some());
    assert!(caps.workspace_symbol_provider.is_some());
    assert!(caps.definition_provider.is_some());
    assert!(caps.references_provider.is_some());
    assert!(caps.hover_provider.is_some());
    assert!(caps.implementation_provider.is_some());
    assert!(caps.call_hierarchy_provider.is_some());

    // 2) initialized
    client
        .sender
        .send(notification(
            "initialized",
            serde_json::to_value(InitializedParams {}).unwrap(),
        ))
        .unwrap();

    // 3) didOpen a trivial file → expect an (empty) publishDiagnostics push.
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
                    text: "extends Node\n".to_string(),
                },
            })
            .unwrap(),
        ))
        .unwrap();

    let Message::Notification(note) = recv(&client) else {
        panic!("expected a publishDiagnostics notification");
    };
    assert_eq!(note.method, "textDocument/publishDiagnostics");
    let diags: PublishDiagnosticsParams = serde_json::from_value(note.params).unwrap();
    assert!(
        diags.diagnostics.is_empty(),
        "a valid `extends Node` file has no syntax diagnostics"
    );

    // 4) documentSymbol → a root Class named by the file basename (A1: unnamed scripts get a
    //    root Class wrapper; the handler fills the empty name with the URI basename "a.gd").
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

    let resp = recv_response(&client);
    assert_eq!(resp.id, RequestId::from(2));
    assert!(resp.error.is_none());
    // A1 changed documentSymbol: now returns a single root Class wrapping members. For an unnamed
    // script ("extends Node" with no class_name), the root name is the file basename "a.gd"
    // (filled by the handler) and children is absent (no members declared). The selectionRange is
    // zero-width at (0,0) since there's no class_name declaration to point at.
    assert_eq!(
        resp.result,
        Some(serde_json::json!([{
            "name": "a.gd",
            "kind": 5,
            "range": {"start": {"line": 0, "character": 0}, "end": {"line": 1, "character": 0}},
            "selectionRange": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 0}}
        }]))
    );

    // 5) shutdown + exit → server replies to shutdown, then exits cleanly.
    client
        .sender
        .send(request(3, "shutdown", serde_json::Value::Null))
        .unwrap();
    let resp = recv_response(&client);
    assert_eq!(resp.id, RequestId::from(3));
    client
        .sender
        .send(notification("exit", serde_json::Value::Null))
        .unwrap();

    let served = server_thread.join().expect("server thread panicked");
    served.expect("serve() returned an error");
}
