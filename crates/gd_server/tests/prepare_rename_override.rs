//! #489: `prepareRename` and `rename` must agree on what is renameable.
//!
//! `prepareRename` exists so a client can ask "will a rename work here?" before it opens the input
//! box. gdls answered that question with only half the gate: it ran the native/stub firewall but
//! not the override-group check, so a method overriding an engine virtual got a range and a
//! placeholder, and the rename that followed refused. `_ready`, `_process`, and `_init` are most of
//! what a GDScript file declares, so this was the common case, not an edge one — sweeping the first
//! 60 files of the Pixelorama acceptance project turned up 82 positions and every one was this.

mod common;

use common::{file_uri, notification, recv_response, request, TempProject};
use lsp_server::Connection;
use lsp_types::{
    DidOpenTextDocumentParams, InitializeParams, InitializedParams, Position,
    TextDocumentIdentifier, TextDocumentItem, TextDocumentPositionParams, Uri,
};

/// `Node` declaring the `_ready` virtual, so an override of it roots in the engine.
const API: &str = r#"{
    "header": {"version_major": 4, "version_minor": 6, "version_patch": 3},
    "classes": [
        {"name": "Object"},
        {"name": "Node", "inherits": "Object",
         "methods": [{"name": "_ready", "is_const": false, "is_static": false,
                      "is_vararg": false, "is_virtual": true, "hash": 1, "arguments": []}]},
        {"name": "Node2D", "inherits": "Node"}
    ]
}"#;

const MAIN_GD: &str = "\
extends Node2D

func _ready() -> void:
\thelper()

func helper() -> void:
\tpass
";

fn project() -> TempProject {
    let p = TempProject::new();
    p.write(
        "project.godot",
        "config_version=5\n\n[application]\n\nconfig/name=\"Test\"\nconfig/features=PackedStringArray(\"4.6\")\n",
    );
    p.write("extension_api.json", API);
    p.write("main.gd", MAIN_GD);
    p
}

fn boot(p: &TempProject) -> (Connection, std::thread::JoinHandle<anyhow::Result<()>>, Uri) {
    let (server, client) = Connection::memory();
    let handle = std::thread::spawn(move || gd_server::serve(server));
    let init = InitializeParams {
        initialization_options: Some(serde_json::json!({
            "projectRoot": p.root.as_str(),
            "autoDumpExtensionApi": false,
            "extensionApiPath": p.root.join("extension_api.json").as_str(),
        })),
        capabilities: serde_json::from_value(serde_json::json!({
            "textDocument": { "rename": { "prepareSupport": true } },
            "workspace": { "workspaceEdit": { "documentChanges": true } }
        }))
        .expect("client caps"),
        ..Default::default()
    };
    client.sender.send(request(1, "initialize", init)).unwrap();
    let _ = recv_response(&client);
    client
        .sender
        .send(notification("initialized", InitializedParams {}))
        .unwrap();
    let uri = file_uri(&p.root.join("main.gd"));
    client
        .sender
        .send(notification(
            "textDocument/didOpen",
            DidOpenTextDocumentParams {
                text_document: TextDocumentItem {
                    uri: uri.clone(),
                    language_id: "gdscript".to_string(),
                    version: 1,
                    text: MAIN_GD.to_string(),
                },
            },
        ))
        .unwrap();
    (client, handle, uri)
}

fn pos(uri: &Uri, line: u32, character: u32) -> TextDocumentPositionParams {
    TextDocumentPositionParams {
        text_document: TextDocumentIdentifier { uri: uri.clone() },
        position: Position { line, character },
    }
}

/// Whether `prepareRename` refused, and whether `rename` refused, at the same position. The point
/// of the test is that the two are always equal.
fn both(client: &Connection, uri: &Uri, line: u32, character: u32) -> (bool, bool) {
    client
        .sender
        .send(request(
            20,
            "textDocument/prepareRename",
            pos(uri, line, character),
        ))
        .unwrap();
    let prepare_refused = recv_response(client).error.is_some();

    client
        .sender
        .send(request(
            21,
            "textDocument/rename",
            serde_json::json!({
                "textDocument": { "uri": uri.as_str() },
                "position": { "line": line, "character": character },
                "newName": "renamed_probe"
            }),
        ))
        .unwrap();
    let rename_refused = recv_response(client).error.is_some();

    (prepare_refused, rename_refused)
}

/// `func _ready()` overrides `Node._ready`. Both halves must refuse, and prepare's message must say
/// why — a client that shows it tells the user before they type a name.
#[test]
fn prepare_refuses_a_native_virtual_override_just_as_rename_does() {
    let p = project();
    let (client, handle, uri) = boot(&p);

    client
        .sender
        .send(request(20, "textDocument/prepareRename", pos(&uri, 2, 6)))
        .unwrap();
    let resp = recv_response(&client);
    let err = resp
        .error
        .expect("prepareRename must refuse an engine-virtual override, not offer a range");
    assert!(
        err.message.contains("_ready") && err.message.contains("native engine method"),
        "the refusal must say why: {}",
        err.message
    );

    let (prepare_refused, rename_refused) = both(&client, &uri, 2, 6);
    assert_eq!(
        (prepare_refused, rename_refused),
        (true, true),
        "prepare and rename must agree"
    );

    common::shutdown(&client, handle);
}

/// The control: an ordinary project method is renameable, and both halves say so. Without this a
/// passing test could just mean prepare refuses everything.
#[test]
fn an_ordinary_method_is_still_renameable_by_both() {
    let p = project();
    let (client, handle, uri) = boot(&p);

    let (prepare_refused, rename_refused) = both(&client, &uri, 5, 6);
    assert_eq!(
        (prepare_refused, rename_refused),
        (false, false),
        "a project method the file declares must stay renameable"
    );

    common::shutdown(&client, handle);
}

/// The call site of the override, not its declaration. rename canonicalizes a call onto the
/// declaration before checking, so prepare has to canonicalize too or the two disagree again.
#[test]
fn a_call_site_of_an_override_refuses_the_same_way() {
    let p = project();
    let (client, handle, uri) = boot(&p);

    let (prepare_refused, rename_refused) = both(&client, &uri, 3, 2);
    assert_eq!(
        prepare_refused, rename_refused,
        "prepare and rename must agree at a call site too"
    );

    common::shutdown(&client, handle);
}
