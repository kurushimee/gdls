//! #406, end to end: a call that neither the open file nor its cross-file ancestry declares is a
//! proven miss and must publish an error, while the same call against a chain gdls could not fully
//! walk must publish nothing. The whole point of the issue is that these two used to be
//! indistinguishable, and both came out silent.

mod common;

use common::{file_uri, notification, recv, request, shutdown, TempProject};
use lsp_server::{Connection, Message};
use lsp_types::{
    DiagnosticSeverity, DidOpenTextDocumentParams, InitializeParams, InitializedParams,
    TextDocumentItem,
};

const BASE_GD: &str = "\
class_name CallBase
extends Node
const KON := 3
func boost() -> void:
\tpass
";

/// A base whose tail does not parse. Error recovery may have dropped a declaration, so an absence
/// measured against its interface proves nothing.
const BROKEN_BASE_GD: &str = "\
class_name BrokenCallBase
extends Node
func late() -> void:
\tpass
func (( -> :
";

fn project() -> TempProject {
    let p = TempProject::new();
    p.write(
        "project.godot",
        "config_version=5\n\n[application]\n\nconfig/name=\"Test\"\nconfig/features=PackedStringArray(\"4.6\")\n",
    );
    p.write("extension_api.json", common::MINI_API);
    p.write("base.gd", BASE_GD);
    p.write("broken.gd", BROKEN_BASE_GD);
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

/// The headline row: the chain reaches a native root through a cleanly parsed base, so `nofunc()`
/// is genuinely absent and says so.
#[test]
fn proven_miss_on_a_cross_file_base_publishes_an_error() {
    let p = project();
    p.write(
        "child.gd",
        "extends CallBase\nfunc go() -> void:\n\tboost()\n\tnofunc()\n",
    );
    let (client, handle) = boot(&p);
    assert_eq!(
        open_errors(&client, &p, "child.gd"),
        vec![r#"Function "nofunc()" not found in base self."#]
    );
    shutdown(&client, handle);
}

/// A name that IS in the base but is not callable answers with its shape, never with not-found.
#[test]
fn non_function_member_publishes_the_shape_not_a_miss() {
    let p = project();
    p.write(
        "child.gd",
        "extends CallBase\nfunc go() -> void:\n\tKON()\n",
    );
    let (client, handle) = boot(&p);
    let errors = open_errors(&client, &p, "child.gd");
    assert!(
        errors.contains(&r#"Member "KON" is not a function."#.to_string()),
        "{errors:?}"
    );
    assert!(
        !errors.iter().any(|m| m.contains("not found in base")),
        "{errors:?}"
    );
    shutdown(&client, handle);
}

/// The silence half: the base did not parse, so gdls cannot prove `late()` or anything else is
/// missing from it and says nothing at all.
#[test]
fn miss_against_a_base_that_failed_to_parse_stays_silent() {
    let p = project();
    p.write(
        "child.gd",
        "extends BrokenCallBase\nfunc go() -> void:\n\tlate()\n\tnofunc()\n",
    );
    let (client, handle) = boot(&p);
    assert_eq!(open_errors(&client, &p, "child.gd"), Vec::<String>::new());
    shutdown(&client, handle);
}

// ===================================================================================================
// #417 — the static-miss error through a real cross-file interface.
// ===================================================================================================

/// A static call on a `class_name` from another file, with the real index and the real interface
/// extractor behind it. The unit tests mock the `CrossFileQuery`; this is the row that proves the
/// `parse_clean` bit and the chain walk line up end to end.
#[test]
fn static_miss_on_a_cross_file_class_name_publishes_an_error() {
    let p = project();
    p.write(
        "child.gd",
        "extends Node\nfunc go() -> void:\n\tCallBase.nope_static()\n",
    );
    let (client, handle) = boot(&p);
    assert_eq!(
        open_errors(&client, &p, "child.gd"),
        vec![r#"Static function "nope_static()" not found in base "CallBase"."#]
    );
    shutdown(&client, handle);
}

/// The same call against a base whose file does not parse cleanly says nothing. Error recovery may
/// have dropped the declaration, so its absence from the interface proves nothing.
#[test]
fn static_miss_on_an_unparseable_cross_file_base_stays_silent() {
    let p = project();
    p.write(
        "child.gd",
        "extends Node\nfunc go() -> void:\n\tBrokenCallBase.nope_static()\n",
    );
    let (client, handle) = boot(&p);
    assert_eq!(open_errors(&client, &p, "child.gd"), Vec::<String>::new());
    shutdown(&client, handle);
}
