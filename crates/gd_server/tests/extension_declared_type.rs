//! Integration tests for the extension-declared-type negative-claim carve-out.
//!
//! A dump generated without extension registration (a failed DLL load silently unregisters
//! the rest; a never-imported project) is engine-`Exact` yet blind to classes Godot's own
//! ClassDB carries. When the project declares a class via a `.gdextension` `[icons]` hint and
//! the dump lacks it, `var x: BTTask` must degrade silently — NOT emit
//! `Could not find type "BTTask" in the current scope.` (Godot with the extension loaded
//! compiles that script fine). An unknown name the project does NOT declare still errors,
//! exactly as before.

mod common;

use common::{file_uri, notification, recv, recv_response, request, shutdown, TempProject};
use lsp_server::{Connection, Message};
use lsp_types::{
    CompletionParams, CompletionResponse, DidOpenTextDocumentParams, Hover, HoverContents,
    HoverParams, InitializeParams, InitializedParams, PartialResultParams, Position,
    PositionEncodingKind, PublishDiagnosticsParams, TextDocumentIdentifier, TextDocumentItem,
    TextDocumentPositionParams, WorkDoneProgressParams,
};

/// Engine-`Exact` dump WITHOUT any extension class — the "dump ran without extension
/// registration" shape (a real dump whose extension load failed produces exactly this:
/// correct engine surface, zero GDExtension classes).
const NODE_API: &str = r#"{
    "header": {"version_major": 4, "version_minor": 6, "version_patch": 3},
    "classes": [
        {"name": "Object"},
        {"name": "Node", "inherits": "Object"}
    ]
}"#;

/// The extension's `[icons]` section is where gdls discovers which classes a GDExtension
/// declares (analyzer-visible even when the extension ships no doc XML and the dump missed it).
const LIMBO_GDEXTENSION: &str = "[configuration]\n\nentry_symbol = \"limbo_library_init\"\n\n[icons]\n\nBTTask=\"res://addons/limboai/icons/bt_task.svg\"\nBlackboard=\"res://addons/limboai/icons/blackboard.svg\"\n";

/// The partial-visibility shape: the dump captured ONE of the extension's hints (`Blackboard`)
/// but not the other. The all-or-nothing degradation notice stays silent here (one hint
/// resolves ⇒ "the extension is visible"), but the missing hint must still be recorded — the
/// analyzer may not negatively claim it either.
const PARTIAL_API: &str = r#"{
    "header": {"version_major": 4, "version_minor": 6, "version_patch": 3},
    "classes": [
        {"name": "Object"},
        {"name": "Node", "inherits": "Object"},
        {"name": "Blackboard", "inherits": "Object"}
    ]
}"#;

fn setup_project(with_extension: bool, api: &str) -> TempProject {
    let p = TempProject::new();
    p.write(
        "project.godot",
        "config_version=5\n\n[application]\n\nconfig/name=\"ExtTypes\"\nconfig/features=PackedStringArray(\"4.6\")\n",
    );
    p.write("extension_api.json", api);
    if with_extension {
        p.write("addons/limboai/limboai.gdextension", LIMBO_GDEXTENSION);
    }
    p.write("src/main.gd", "extends Node\n\nvar task: BTTask\n");
    p
}

fn boot(project: &TempProject) -> (Connection, std::thread::JoinHandle<anyhow::Result<()>>) {
    let (server, client) = Connection::memory();
    let handle = std::thread::spawn(move || gd_server::serve(server));
    let init = InitializeParams {
        initialization_options: Some(serde_json::json!({
            "projectRoot": project.root.as_str(),
            "autoDumpExtensionApi": false,
            "extensionApiPath": project.root.join("extension_api.json").as_str().to_owned(),
        })),
        capabilities: lsp_types::ClientCapabilities {
            general: Some(lsp_types::GeneralClientCapabilities {
                position_encodings: Some(vec![PositionEncodingKind::UTF8]),
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

/// Receive until the `publishDiagnostics` push arrives, skipping anything else the server sends
/// unprompted — a conforming client tolerates server notifications in any order.
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

fn open_and_collect(
    client: &Connection,
    project: &TempProject,
    rel: &str,
) -> PublishDiagnosticsParams {
    let abs = project.root.join(rel);
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
    recv_publish(client)
}

#[test]
fn extension_declared_class_type_degrades_silently() {
    let p = setup_project(true, NODE_API);
    let (client, handle) = boot(&p);
    let diags = open_and_collect(&client, &p, "src/main.gd").diagnostics;
    let offending: Vec<_> = diags
        .iter()
        .filter(|d| d.message.contains("Could not find type"))
        .collect();
    assert!(
        offending.is_empty(),
        "a class the project's .gdextension declares must degrade silently, not error: {offending:?}"
    );
    shutdown(&client, handle);
}

#[test]
fn a_partially_visible_extension_still_records_its_missing_hints() {
    // The dump carries `Blackboard` but not `BTTask`; the `[icons]` section declares both.
    // The degradation notice stays silent (one hint resolves), and before the unconditional
    // recording this was exactly the row that slipped through: BTTask still emitted
    // `Could not find type`.
    let p = setup_project(true, PARTIAL_API);
    let (client, handle) = boot(&p);
    let diags = open_and_collect(&client, &p, "src/main.gd").diagnostics;
    let offending: Vec<_> = diags
        .iter()
        .filter(|d| d.message.contains("Could not find type"))
        .collect();
    assert!(
        offending.is_empty(),
        "the missing half of a partially-visible extension must degrade silently: {offending:?}"
    );
    shutdown(&client, handle);
}

#[test]
fn undeclared_unknown_type_still_errors() {
    // Same dump, no `.gdextension`: nothing declares `BTTask`, so the Exact-provenance
    // negative claim stands (the gate must not swallow genuine typos).
    let p = setup_project(false, NODE_API);
    let (client, handle) = boot(&p);
    let diags = open_and_collect(&client, &p, "src/main.gd").diagnostics;
    assert!(
        diags
            .iter()
            .any(|d| d.message == r#"Could not find type "BTTask" in the current scope."#),
        "an undeclared unknown type must still emit the error: {diags:?}"
    );
    shutdown(&client, handle);
}

/// #536: gdls is sure enough that `BTTask` exists to refuse a "could not find type" claim about
/// it, so it is sure enough to offer it. Leaving it out of the list while staying silent about it
/// is the two halves disagreeing about the same name.
#[test]
fn an_extension_declared_class_is_offered_and_hovers_as_itself() {
    let p = setup_project(true, NODE_API);
    p.write("src/main.gd", "extends Node\n\nvar task: BTTask\n");
    let (client, handle) = boot(&p);
    let diags = open_and_collect(&client, &p, "src/main.gd").diagnostics;
    assert!(diags.is_empty(), "the carve-out still holds: {diags:?}");

    let uri = file_uri(&p.root.join("src/main.gd"));

    // The card names the class, not the `Variant` the analyzer's refusal leaves behind.
    let hover = hover_at(&client, &uri, Position::new(2, 12));
    let md = hover.as_ref().map(hover_markdown).unwrap_or_default();
    assert!(
        md.contains("<Native> class BTTask"),
        "an extension-declared class hovers as itself: {md:?}"
    );

    // Both declared names reach the type-position list, in a project whose dump carries neither.
    let labels = completion_labels(&client, &uri, Position::new(2, 16));
    assert!(
        labels.iter().any(|l| l == "BTTask") && labels.iter().any(|l| l == "Blackboard"),
        "both `[icons]` names are offered in a type position"
    );

    shutdown(&client, handle);
}

/// A name the dump DOES carry is offered once, from the class tier, not twice.
#[test]
fn a_partially_visible_extension_offers_each_name_once() {
    let p = setup_project(true, PARTIAL_API);
    p.write("src/main.gd", "extends Node\n\nvar task: BTTask\n");
    let (client, handle) = boot(&p);
    let _ = open_and_collect(&client, &p, "src/main.gd");
    let uri = file_uri(&p.root.join("src/main.gd"));
    let labels = completion_labels(&client, &uri, Position::new(2, 16));
    assert_eq!(
        labels.iter().filter(|l| *l == "Blackboard").count(),
        1,
        "the dump carries Blackboard, so the declared-missing tier must not repeat it"
    );
    assert!(labels.iter().any(|l| l == "BTTask"));
    shutdown(&client, handle);
}

fn hover_at(client: &Connection, uri: &lsp_types::Uri, position: Position) -> Option<Hover> {
    let params = HoverParams {
        text_document_position_params: TextDocumentPositionParams {
            text_document: TextDocumentIdentifier { uri: uri.clone() },
            position,
        },
        work_done_progress_params: WorkDoneProgressParams::default(),
    };
    client
        .sender
        .send(request(20, "textDocument/hover", params))
        .unwrap();
    let resp = recv_response(client);
    serde_json::from_value(resp.result.expect("hover result")).expect("valid Option<Hover>")
}

fn hover_markdown(hover: &Hover) -> String {
    match &hover.contents {
        HoverContents::Markup(m) => m.value.clone(),
        other => panic!("expected markup hover contents, got {other:?}"),
    }
}

fn completion_labels(client: &Connection, uri: &lsp_types::Uri, position: Position) -> Vec<String> {
    let params = CompletionParams {
        text_document_position: TextDocumentPositionParams {
            text_document: TextDocumentIdentifier { uri: uri.clone() },
            position,
        },
        work_done_progress_params: WorkDoneProgressParams::default(),
        partial_result_params: PartialResultParams::default(),
        context: None,
    };
    client
        .sender
        .send(request(21, "textDocument/completion", params))
        .unwrap();
    let resp = recv_response(client);
    let value = resp.result.expect("completion result");
    match serde_json::from_value::<CompletionResponse>(value).expect("valid CompletionResponse") {
        CompletionResponse::Array(items) => items.into_iter().map(|i| i.label).collect(),
        CompletionResponse::List(list) => list.items.into_iter().map(|i| i.label).collect(),
    }
}
