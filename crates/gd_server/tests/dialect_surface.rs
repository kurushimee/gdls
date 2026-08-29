//! The dialect, end to end over the wire.
//!
//! `crates/gd_syntax/tests/dialect_delta.rs` and the analyzer's dialect tests pin each guarded
//! behavior at the library boundary. This file pins the ones a user can actually see in an editor,
//! through a booted server: the warning set that fires, whether `@warning_ignore` accepts a name,
//! and how a doc comment renders in hover. A guard that stops being threaded from
//! `initializationOptions.dialect` down to the parse or the analyze would pass every library test
//! and still show the wrong thing here.

use std::time::Duration;

use lsp_server::{Connection, Message, Notification, Request, RequestId};
use lsp_types::{
    ClientCapabilities, GeneralClientCapabilities, Hover, HoverContents, HoverParams,
    InitializeParams, InitializedParams, MarkupKind, Position, PositionEncodingKind,
    PublishDiagnosticsParams, TextDocumentClientCapabilities, TextDocumentIdentifier,
    TextDocumentItem, TextDocumentPositionParams, Uri, WorkDoneProgressParams,
};

fn recv(conn: &Connection) -> Message {
    conn.receiver
        .recv_timeout(Duration::from_secs(10))
        .expect("timed out waiting for a message from the server")
}

fn recv_response(conn: &Connection) -> lsp_server::Response {
    loop {
        if let Message::Response(resp) = recv(conn) {
            return resp;
        }
    }
}

fn recv_publish(conn: &Connection) -> PublishDiagnosticsParams {
    loop {
        if let Message::Notification(n) = recv(conn) {
            if n.method == "textDocument/publishDiagnostics" {
                return serde_json::from_value(n.params).expect("valid publishDiagnostics");
            }
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

/// Boot a server pinned to `tag` through `initializationOptions.dialect` — the same knob a user
/// sets when their `project.godot` is absent or wrong. Markdown hover is requested because one of
/// these tests reads hover content.
fn boot(tag: &str) -> (Connection, std::thread::JoinHandle<()>) {
    boot_with_api(tag, None)
}

/// [`boot`], optionally pinning a native surface. The `CONFUSABLE_TEMPORARY_MODIFICATION` arm only
/// fires on a *native* property, so that test needs a DB with `Line2D` in it; the rest are happy
/// with the empty one.
fn boot_with_api(tag: &str, api_path: Option<&str>) -> (Connection, std::thread::JoinHandle<()>) {
    let (server, client) = Connection::memory();
    let handle = std::thread::spawn(move || {
        gd_server::serve(server).expect("serve() returned an error");
    });
    let init = InitializeParams {
        capabilities: ClientCapabilities {
            general: Some(GeneralClientCapabilities {
                position_encodings: Some(vec![PositionEncodingKind::UTF8]),
                ..Default::default()
            }),
            text_document: Some(TextDocumentClientCapabilities {
                hover: Some(lsp_types::HoverClientCapabilities {
                    content_format: Some(vec![MarkupKind::Markdown]),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        },
        initialization_options: Some(serde_json::json!({
            "dialect": tag,
            "autoDumpExtensionApi": false,
            "extensionApiPath": api_path,
        })),
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
    assert!(resp.error.is_none(), "initialize errored: {:?}", resp.error);
    client
        .sender
        .send(notification(
            "initialized",
            serde_json::to_value(InitializedParams {}).unwrap(),
        ))
        .unwrap();
    (client, handle)
}

fn shutdown(client: &Connection, handle: std::thread::JoinHandle<()>) {
    client
        .sender
        .send(request(99, "shutdown", serde_json::Value::Null))
        .unwrap();
    let _ = recv_response(client);
    client
        .sender
        .send(notification("exit", serde_json::Value::Null))
        .unwrap();
    handle.join().expect("server thread panicked");
}

/// Open `text` and return the diagnostic messages the server publishes for it.
fn diagnostics_at(tag: &str, text: &str) -> Vec<String> {
    diagnostics_with_api(tag, text, None)
}

fn diagnostics_with_api(tag: &str, text: &str, api_path: Option<&str>) -> Vec<String> {
    let (client, handle) = boot_with_api(tag, api_path);
    let uri: Uri = "file:///test/dialect.gd".parse().unwrap();
    client
        .sender
        .send(notification(
            "textDocument/didOpen",
            serde_json::to_value(lsp_types::DidOpenTextDocumentParams {
                text_document: TextDocumentItem {
                    uri: uri.clone(),
                    language_id: "gdscript".to_string(),
                    version: 1,
                    text: text.to_string(),
                },
            })
            .unwrap(),
        ))
        .unwrap();
    let published = recv_publish(&client);
    assert_eq!(published.uri, uri);
    shutdown(&client, handle);
    published
        .diagnostics
        .into_iter()
        .map(|d| d.message)
        .collect()
}

/// Open `text`, hover at `position`, and return the rendered markdown.
fn hover_text_at(tag: &str, text: &str, position: Position) -> String {
    let (client, handle) = boot(tag);
    let uri: Uri = "file:///test/dialect.gd".parse().unwrap();
    client
        .sender
        .send(notification(
            "textDocument/didOpen",
            serde_json::to_value(lsp_types::DidOpenTextDocumentParams {
                text_document: TextDocumentItem {
                    uri: uri.clone(),
                    language_id: "gdscript".to_string(),
                    version: 1,
                    text: text.to_string(),
                },
            })
            .unwrap(),
        ))
        .unwrap();
    let _ = recv_publish(&client);
    client
        .sender
        .send(request(
            10,
            "textDocument/hover",
            serde_json::to_value(HoverParams {
                text_document_position_params: TextDocumentPositionParams {
                    text_document: TextDocumentIdentifier { uri },
                    position,
                },
                work_done_progress_params: WorkDoneProgressParams::default(),
            })
            .unwrap(),
        ))
        .unwrap();
    let resp = recv_response(&client);
    let hover: Option<Hover> =
        serde_json::from_value(resp.result.expect("hover result")).expect("valid Option<Hover>");
    shutdown(&client, handle);
    match hover.expect("hover must resolve here").contents {
        HoverContents::Markup(m) => m.value,
        other => panic!("expected markup hover, got {other:?}"),
    }
}

/// The analyzer conformance fixture, reused here because it is the only committed dump carrying
/// `Line2D` and `PackedVector2Array`.
fn trimmed_api_path() -> String {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../gd_types/tests/fixtures/trimmed_api.json")
        .to_string_lossy()
        .into_owned()
}

/// `CONFUSABLE_TEMPORARY_MODIFICATION` is new in 4.7, so a 4.6 project must never see it — the
/// pattern is perfectly ordinary code there.
#[test]
fn a_4_7_only_warning_fires_only_at_4_7() {
    const SRC: &str = "extends Line2D\n\nfunc _ready() -> void:\n\tpoints.clear()\n";
    const NEEDLE: &str = "will not be modified as a result of calling";
    let api = trimmed_api_path();

    let at_47 = diagnostics_with_api("4.7", SRC, Some(&api));
    assert!(
        at_47.iter().any(|m| m.contains(NEEDLE)),
        "4.7 must warn about the temporary modification: {at_47:?}"
    );
    let at_46 = diagnostics_with_api("4.6", SRC, Some(&api));
    assert!(
        !at_46.iter().any(|m| m.contains(NEEDLE)),
        "4.6 has no such warning: {at_46:?}"
    );
}

/// The `@warning_ignore` name check reads the same per-release warning table. Naming a warning that
/// does not exist yet is an error, so the very same line is clean at 4.7 and rejected at 4.6.
#[test]
fn warning_ignore_accepts_a_name_only_from_the_release_that_has_it() {
    const SRC: &str = "extends Node\n\n@warning_ignore(\"CONFUSABLE_TEMPORARY_MODIFICATION\")\nfunc test() -> void:\n\tpass\n";
    const NEEDLE: &str = "CONFUSABLE_TEMPORARY_MODIFICATION";

    let at_47 = diagnostics_at("4.7", SRC);
    assert!(
        !at_47.iter().any(|m| m.contains(NEEDLE)),
        "4.7 knows the name: {at_47:?}"
    );
    let at_46 = diagnostics_at("4.6", SRC);
    assert!(
        at_46.iter().any(|m| m.contains(NEEDLE)),
        "4.6 must reject a name it has never heard of: {at_46:?}"
    );
}

/// `_process_doc_line`'s `[br][br]` handling is 4.7-only, and hover is where a user sees it: 4.7
/// collapses the pair into one real paragraph break before the BBCode ever reaches the renderer,
/// where 4.6 hands both `[br]`s through and the renderer turns each into a markdown hard break.
#[test]
fn a_doc_comments_paragraph_break_renders_per_release() {
    const SRC: &str = "extends Node\n\n## First.[br][br]Second.\nvar speed := 1.0\n";
    // The `speed` in its own declaration.
    let position = Position {
        line: 3,
        character: 5,
    };

    let at_47 = hover_text_at("4.7", SRC, position);
    assert!(
        at_47.ends_with("First.\nSecond."),
        "4.7 collapses the pair into a paragraph break: {at_47:?}"
    );
    let at_46 = hover_text_at("4.6", SRC, position);
    assert!(
        at_46.ends_with("First.  \n  \nSecond."),
        "4.6 renders both `[br]`s as markdown hard breaks: {at_46:?}"
    );
}
