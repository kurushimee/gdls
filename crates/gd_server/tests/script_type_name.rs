//! #419, end to end: a script with no `class_name` names itself by its `res://` path in a
//! diagnostic, never by its absolute path on disk.
//!
//! The absolute form is wrong twice over. It does not match Godot, and it puts the user's
//! filesystem layout in an editor popup — on Windows a drive letter and a home directory name. The
//! test asserts on a published diagnostic rather than on the helper, because the helper was already
//! correct and it was one call site reaching past it.

mod common;

use common::{file_uri, notification, recv, request, shutdown, TempProject};
use lsp_server::{Connection, Message};
use lsp_types::{DidOpenTextDocumentParams, InitializeParams, InitializedParams, TextDocumentItem};

const LIB_GD: &str = "\
extends Node
func real() -> void:
\tpass
";

fn project() -> TempProject {
    let p = TempProject::new();
    p.write(
        "project.godot",
        "config_version=5\n\n[application]\n\nconfig/name=\"Test\"\nconfig/features=PackedStringArray(\"4.6\")\n\n\
         [debug]\n\ngdscript/warnings/unsafe_method_access=1\ngdscript/warnings/unsafe_property_access=1\n",
    );
    p.write("extension_api.json", common::MINI_API);
    p.write("lib.gd", LIB_GD);
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

fn open_messages(client: &Connection, p: &TempProject, rel: &str) -> Vec<String> {
    let abs = p.root.join(rel);
    let text = std::fs::read_to_string(abs.as_std_path()).expect("read file");
    client
        .sender
        .send(notification(
            "textDocument/didOpen",
            DidOpenTextDocumentParams {
                text_document: TextDocumentItem {
                    uri: file_uri(&abs),
                    language_id: "gdscript".to_string(),
                    version: 1,
                    text,
                },
            },
        ))
        .unwrap();
    let msg = recv(client);
    let Message::Notification(notif) = msg else {
        panic!("expected publishDiagnostics after didOpen, got {msg:?}");
    };
    assert_eq!(notif.method, "textDocument/publishDiagnostics");
    let params: lsp_types::PublishDiagnosticsParams = serde_json::from_value(notif.params).unwrap();
    params.diagnostics.into_iter().map(|d| d.message).collect()
}

#[test]
fn a_nameless_script_is_named_by_its_res_path() {
    let p = project();
    p.write(
        "child.gd",
        "extends Node\nvar g := preload(\"res://lib.gd\").new()\nfunc go() -> void:\n\tg.nope_m()\n\tprint(g.nope_p)\n",
    );
    let (client, handle) = boot(&p);
    let messages = open_messages(&client, &p, "child.gd");
    let named: Vec<&String> = messages
        .iter()
        .filter(|m| m.contains("inferred type"))
        .collect();
    assert!(
        named.iter().any(|m| m.contains("\"res://lib.gd\"")),
        "expected the res:// spelling; got {messages:?}"
    );
    // The temp root is the absolute path this must never leak, and its separator differs per
    // platform, so match on the root itself rather than on a hand-built string.
    assert!(
        !messages.iter().any(|m| m.contains(p.root.as_str())),
        "a message leaked the absolute project path: {messages:?}"
    );
    shutdown(&client, handle);
}
