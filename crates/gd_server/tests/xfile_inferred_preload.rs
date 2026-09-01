//! #521, end to end: an `:=` member whose initializer is a `preload` of a non-script resource, or a
//! global float constant, must carry its real type across a file boundary. Both used to arrive at
//! the reader as `Variant`, so hover said nothing and a strict session reported an unsafe access on
//! a member whose type was perfectly knowable.

mod common;

use common::{file_uri, notification, recv, request, shutdown, TempProject};
use lsp_server::{Connection, Message};
use lsp_types::{
    DidOpenTextDocumentParams, InitializeParams, InitializedParams, PublishDiagnosticsParams,
    TextDocumentItem,
};

const LIB_GD: &str = "\
class_name PreloadLib
extends Node
var scene := preload(\"res://thing.tscn\")
var glob := PI
";

const READER_GD: &str = "\
extends Node
func use(l: PreloadLib) -> void:
\tprint(l.scene)
\tprint(l.glob)
";

/// The same two members, each used in a way its real type forbids. As `Variant` both lines were
/// silent.
const BAD_READER_GD: &str = "\
extends Node
func use(l: PreloadLib) -> void:
\tl.scene.nosuchmethod()
\tvar s: String = l.glob\n\tprint(s)
";

fn project() -> TempProject {
    let p = TempProject::new();
    p.write(
        "project.godot",
        "config_version=5\n\n[application]\n\nconfig/name=\"Test\"\nconfig/features=PackedStringArray(\"4.6\")\n",
    );
    p.write("extension_api.json", common::MINI_API);
    p.write(
        "thing.tscn",
        "[gd_scene load_steps=1 format=3]\n\n[node name=\"Root\" type=\"Node\"]\n",
    );
    p.write("lib.gd", LIB_GD);
    p.write("reader.gd", READER_GD);
    p.write("bad_reader.gd", BAD_READER_GD);
    p
}

fn boot(p: &TempProject) -> (Connection, std::thread::JoinHandle<anyhow::Result<()>>) {
    let (server, client) = Connection::memory();
    let handle = std::thread::spawn(move || gd_server::serve(server));
    let init = InitializeParams {
        initialization_options: Some(serde_json::json!({
            "projectRoot": p.root.as_str(),
            "autoDumpExtensionApi": false,
            "extensionApiPath": p.root.join("extension_api.json").as_str(),
            "strict": { "profile": "strict" },
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

fn open(client: &Connection, uri: &lsp_types::Uri, text: &str) -> PublishDiagnosticsParams {
    client
        .sender
        .send(notification(
            "textDocument/didOpen",
            DidOpenTextDocumentParams {
                text_document: TextDocumentItem {
                    uri: uri.clone(),
                    language_id: "gdscript".to_string(),
                    version: 1,
                    text: text.to_string(),
                },
            },
        ))
        .unwrap();
    recv_publish(client)
}

#[test]
fn a_preloaded_scene_and_a_global_constant_cross_the_file_boundary() {
    let p = project();
    let (client, server_thread) = boot(&p);

    let uri = file_uri(&p.root.join("reader.gd"));
    let clean = open(&client, &uri, READER_GD);
    assert!(
        clean.diagnostics.is_empty(),
        "reading either member is well typed; got: {:?}",
        clean.diagnostics
    );

    let bad_uri = file_uri(&p.root.join("bad_reader.gd"));
    let bad = open(&client, &bad_uri, BAD_READER_GD);
    let messages: Vec<String> = bad.diagnostics.iter().map(|d| d.message.clone()).collect();
    assert_eq!(messages.len(), 2, "one row per misuse; got: {messages:?}");
    assert!(
        messages[0].contains("PackedScene"),
        "the `.tscn` preload must reach the reader as PackedScene, not Variant; got: {}",
        messages[0]
    );
    assert!(
        messages[1].contains("float"),
        "a member inferred from PI must reach the reader as float, not Variant; got: {}",
        messages[1]
    );

    shutdown(&client, server_thread);
}
