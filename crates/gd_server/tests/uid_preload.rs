//! #447, end to end: `preload("uid://…")` resolves through the project's `.uid` sidecars, so the
//! const it declares carries the target's real type instead of degrading to `Variant`. Godot
//! dereferences a uid at the `FileAccess` layer, which means every spelling that accepts `res://`
//! accepts `uid://` too — the const's type, the dependency edge that re-analyzes a consumer, and
//! the sidecar going stale mid-session.

mod common;

use common::{file_uri, notification, recv, recv_response, request, shutdown, TempProject};
use lsp_server::{Connection, Message};
use lsp_types::{
    DiagnosticSeverity, DidOpenTextDocumentParams, DocumentLink, DocumentLinkParams,
    GotoDefinitionParams, Hover, HoverParams, InitializeParams, InitializedParams, Location,
    Position, TextDocumentIdentifier, TextDocumentItem, TextDocumentPositionParams,
};

const LIB_GD: &str = "\
class_name UidLib
extends Node
func greet() -> void:
\tpass
";

fn project() -> TempProject {
    let p = TempProject::new();
    p.write(
        "project.godot",
        "config_version=5\n\n[application]\n\nconfig/name=\"Test\"\nconfig/features=PackedStringArray(\"4.6\")\n",
    );
    p.write("extension_api.json", common::MINI_API);
    p.write("lib.gd", LIB_GD);
    p.write("lib.gd.uid", "uid://ctest447\n");
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

fn open_errors(client: &Connection, p: &TempProject, rel: &str) -> Vec<String> {
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
    params
        .diagnostics
        .into_iter()
        .filter(|d| d.severity == Some(DiagnosticSeverity::ERROR))
        .map(|d| d.message)
        .collect()
}

/// The headline row: the uid preload types the const as the script it names, so a member the script
/// does not declare is a proven miss. Before the sidecar map reached the index this resolved to
/// nothing, the const typed `Variant`, and the whole file went silent.
#[test]
fn a_uid_preload_types_the_const_as_its_target() {
    let p = project();
    p.write(
        "user.gd",
        "extends Node\nconst Lib = preload(\"uid://ctest447\")\nfunc go() -> void:\n\tLib.nofunc()\n",
    );
    let (client, handle) = boot(&p);
    let errors = open_errors(&client, &p, "user.gd");
    assert!(
        errors
            .iter()
            .any(|m| m.contains("nofunc") && m.contains("UidLib")),
        "the miss names the uid's target script; got {errors:?}"
    );
    shutdown(&client, handle);
}

/// A uid with no sidecar resolves to nothing, and the file stays silent rather than claiming a miss
/// against a target gdls never read. Never lie: an unresolved dependency is an under-report.
#[test]
fn a_uid_without_a_sidecar_stays_silent() {
    let p = project();
    p.write(
        "user.gd",
        "extends Node\nconst Lib = preload(\"uid://cmissing\")\nfunc go() -> void:\n\tLib.nofunc()\n",
    );
    let (client, handle) = boot(&p);
    let errors = open_errors(&client, &p, "user.gd");
    assert!(
        !errors.iter().any(|m| m.contains("nofunc")),
        "an unresolvable uid must not produce a member claim; got {errors:?}"
    );
    shutdown(&client, handle);
}

/// A path-`extends` written as a uid reaches the same base a `res://` one does, so an inherited
/// member resolves and an absent one is still a miss.
#[test]
fn a_uid_path_extends_reaches_the_base() {
    let p = project();
    p.write(
        "user.gd",
        "extends \"uid://ctest447\"\nfunc go() -> void:\n\tgreet()\n\tnofunc()\n",
    );
    let (client, handle) = boot(&p);
    let errors = open_errors(&client, &p, "user.gd");
    assert_eq!(
        errors,
        vec![r#"Function "nofunc()" not found in base self."#],
        "the inherited member resolves and only the absent one is reported"
    );
    shutdown(&client, handle);
}

/// The `result` payload of the next response, unwrapped.
fn recv_result(client: &Connection) -> serde_json::Value {
    recv_response(client).result.expect("a result payload")
}

/// The read-only navigation surface follows a uid the same way it follows a `res://` path: the
/// literal is a link, hovers as its target, and ctrl-clicks to it. All three used to decline on the
/// `res://` prefix check alone, so a uid literal was inert text.
#[test]
fn the_read_only_surface_follows_a_uid_literal() {
    let p = project();
    let src = "extends Node\nconst Lib = preload(\"uid://ctest447\")\n";
    p.write("user.gd", src);
    let (client, handle) = boot(&p);
    let abs = p.root.join("user.gd");
    let uri = file_uri(&abs);
    let _ = open_errors(&client, &p, "user.gd");
    let lib_uri = file_uri(&p.root.join("lib.gd"));

    // documentLink: one link, pointing at the uid's target.
    client
        .sender
        .send(request(
            10,
            "textDocument/documentLink",
            DocumentLinkParams {
                text_document: TextDocumentIdentifier { uri: uri.clone() },
                work_done_progress_params: Default::default(),
                partial_result_params: Default::default(),
            },
        ))
        .unwrap();
    let links: Vec<DocumentLink> =
        serde_json::from_value(recv_result(&client)).expect("documentLink result");
    assert_eq!(links.len(), 1, "exactly one link; got {links:?}");
    assert_eq!(
        links[0].target.as_ref(),
        Some(&lib_uri),
        "the uid literal links to the file its sidecar names"
    );

    // The cursor inside the literal: hover names the target, definition jumps to it.
    let inside = Position {
        line: 1,
        character: 25,
    };
    client
        .sender
        .send(request(
            11,
            "textDocument/hover",
            HoverParams {
                text_document_position_params: TextDocumentPositionParams {
                    text_document: TextDocumentIdentifier { uri: uri.clone() },
                    position: inside,
                },
                work_done_progress_params: Default::default(),
            },
        ))
        .unwrap();
    let hover: Option<Hover> = serde_json::from_value(recv_result(&client)).expect("hover result");
    let rendered = serde_json::to_string(&hover.expect("a uid literal hovers")).unwrap();
    assert!(
        rendered.contains("lib.gd"),
        "the hover names the resolved file; got {rendered}"
    );

    client
        .sender
        .send(request(
            12,
            "textDocument/definition",
            GotoDefinitionParams {
                text_document_position_params: TextDocumentPositionParams {
                    text_document: TextDocumentIdentifier { uri },
                    position: inside,
                },
                work_done_progress_params: Default::default(),
                partial_result_params: Default::default(),
            },
        ))
        .unwrap();
    let def: Location = serde_json::from_value(recv_result(&client)).expect("definition result");
    assert_eq!(def.uri, lib_uri, "ctrl-click on a uid opens its target");

    shutdown(&client, handle);
}
