//! SHADOWED_* warnings publish `relatedInformation` pointing at the shadowed declaration — the
//! spec's own canonical example for the field ("when symbol-names within a scope collide all
//! definitions can be marked"). The Godot-exact message (including its "at line N" text) stays
//! byte-identical: the structured location rides alongside in a field Godot never serializes,
//! so the conformance ratchets are untouched.

mod common;

use std::time::Duration;

use common::{file_uri, notification, recv, request, sample_project, shutdown, try_recv};
use lsp_server::{Connection, Message};
use lsp_types::{
    DidOpenTextDocumentParams, InitializeParams, InitializedParams, NumberOrString, Position,
    PublishDiagnosticsParams, TextDocumentItem, Uri,
};

fn boot_project(project: &common::TempProject, client: &Connection) {
    let init = InitializeParams {
        initialization_options: Some(serde_json::json!({
            "projectRoot": project.root.as_str(),
            "autoDumpExtensionApi": false,
            "extensionApiPath": project.root.join("extension_api.json").as_str(),
        })),
        ..Default::default()
    };
    client.sender.send(request(1, "initialize", init)).unwrap();
    let _ = recv(client);
    client
        .sender
        .send(notification("initialized", InitializedParams {}))
        .unwrap();
}

/// didOpen `uri` and return ITS publishDiagnostics (skipping unrelated notifications).
fn open_and_diags(client: &Connection, uri: &Uri, text: &str) -> PublishDiagnosticsParams {
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
    loop {
        let Some(Message::Notification(note)) = try_recv(client, Duration::from_secs(5)) else {
            panic!("expected a publishDiagnostics notification for {uri:?}");
        };
        if note.method != "textDocument/publishDiagnostics" {
            continue;
        }
        let params: PublishDiagnosticsParams = serde_json::from_value(note.params).unwrap();
        if &params.uri == uri {
            return params;
        }
    }
}

/// The cross-file case the message alone can't serve: SHADOWED_VARIABLE_BASE_CLASS names the
/// base class and a line number in TEXT, but the base lives in another file — the related
/// location makes it navigable, anchored at the member's name token in base.gd.
#[test]
fn shadowed_base_class_warning_carries_related_location() {
    let project = sample_project();
    project.write(
        "src/base.gd",
        "class_name RelBase\nextends Node\nvar hp: int = 1\n",
    );
    project.write(
        "src/derived.gd",
        "extends RelBase\nfunc f() -> void:\n\tvar hp = 2\n\tprint(hp)\n",
    );

    let (server, client) = Connection::memory();
    let server_thread = std::thread::spawn(move || gd_server::serve(server));
    boot_project(&project, &client);

    let derived_uri = file_uri(&project.root.join("src/derived.gd"));
    let derived_src =
        std::fs::read_to_string(project.root.join("src/derived.gd").as_std_path()).unwrap();
    let diags = open_and_diags(&client, &derived_uri, &derived_src);

    let diag = diags
        .diagnostics
        .iter()
        .find(|d| {
            d.code
                == Some(NumberOrString::String(
                    "SHADOWED_VARIABLE_BASE_CLASS".to_string(),
                ))
        })
        .unwrap_or_else(|| {
            panic!(
                "SHADOWED_VARIABLE_BASE_CLASS must fire; got {:?}",
                diags.diagnostics
            )
        });
    // The Godot message is byte-identical to the pre-related rendering.
    assert_eq!(
        diag.message,
        r#"The local variable "hp" is shadowing an already-declared variable at line 3 in the base class "RelBase"."#
    );
    let rel = diag
        .related_information
        .as_ref()
        .expect("the shadowed declaration rides as relatedInformation");
    assert_eq!(rel.len(), 1);
    assert_eq!(rel[0].message, "previous declaration is here");
    assert!(
        rel[0].location.uri.as_str().ends_with("/base.gd"),
        "the related location points into the BASE file; got {}",
        rel[0].location.uri.as_str()
    );
    // `var hp: int = 1` on base.gd line 3 (LSP line 2) — the range covers exactly `hp`.
    assert_eq!(rel[0].location.range.start, Position::new(2, 4));
    assert_eq!(rel[0].location.range.end, Position::new(2, 6));

    shutdown(&client, server_thread);
}

/// The same-file case: SHADOWED_VARIABLE's related location anchors the class member's name
/// token in the same document.
#[test]
fn shadowed_variable_warning_carries_related_location() {
    let project = sample_project();
    project.write(
        "src/own.gd",
        "extends Node\nvar count: int = 1\nfunc f() -> void:\n\tvar count = 2\n\tprint(count)\n",
    );

    let (server, client) = Connection::memory();
    let server_thread = std::thread::spawn(move || gd_server::serve(server));
    boot_project(&project, &client);

    let own_uri = file_uri(&project.root.join("src/own.gd"));
    let own_src = std::fs::read_to_string(project.root.join("src/own.gd").as_std_path()).unwrap();
    let diags = open_and_diags(&client, &own_uri, &own_src);

    let diag = diags
        .diagnostics
        .iter()
        .find(|d| d.code == Some(NumberOrString::String("SHADOWED_VARIABLE".to_string())))
        .unwrap_or_else(|| panic!("SHADOWED_VARIABLE must fire; got {:?}", diags.diagnostics));
    assert_eq!(
        diag.message,
        r#"The local variable "count" is shadowing an already-declared variable at line 2 in the current class."#
    );
    let rel = diag
        .related_information
        .as_ref()
        .expect("the shadowed declaration rides as relatedInformation");
    assert_eq!(rel.len(), 1);
    assert_eq!(rel[0].message, "previous declaration is here");
    assert_eq!(rel[0].location.uri, own_uri);
    // `var count: int = 1` on line 2 (LSP line 1) — the range covers exactly `count`.
    assert_eq!(rel[0].location.range.start, Position::new(1, 4));
    assert_eq!(rel[0].location.range.end, Position::new(1, 9));

    shutdown(&client, server_thread);
}
