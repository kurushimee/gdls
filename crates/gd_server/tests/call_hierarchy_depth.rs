//! Outgoing call-hierarchy trees must be expandable past depth 2: every item a server emits
//! comes back verbatim when the client expands it (VS Code's references-view calls
//! `provideOutgoingCalls(call.item)` with the `to` item as-is), so `to` items need the same
//! `{uri, name}` data blob prepare/incoming items carry — and items whose data a client
//! stripped must re-resolve from `uri` + `selectionRange` instead of dead-ending.

mod common;

use std::time::Duration;

use common::{
    file_uri, notification, recv, recv_response, request, sample_project, shutdown, try_recv,
};
use lsp_server::Connection;
use lsp_types::{
    CallHierarchyIncomingCall, CallHierarchyIncomingCallsParams, CallHierarchyItem,
    CallHierarchyOutgoingCall, CallHierarchyOutgoingCallsParams, CallHierarchyPrepareParams,
    DidOpenTextDocumentParams, InitializeParams, InitializedParams, Position,
    TextDocumentIdentifier, TextDocumentItem, TextDocumentPositionParams, WorkDoneProgressParams,
};

/// a.gd → b.gd → c.gd: `fa` calls `b.fb()` through a typed param, `fb` calls `c.fc()` through a
/// typed local. Both dotted calls resolve cross-file, so each hop's `to` item points at the next
/// file.
fn chain_project() -> common::TempProject {
    let project = sample_project();
    project.write(
        "src/a.gd",
        "extends Node\nfunc fa(b: BLib) -> void:\n\tb.fb()\n",
    );
    project.write(
        "src/b.gd",
        "class_name BLib\nextends Node\nfunc fb() -> void:\n\tvar c: CLib = CLib.new()\n\tc.fc()\n",
    );
    project.write(
        "src/c.gd",
        "class_name CLib\nextends Node\nfunc fc() -> void:\n\tpass\n",
    );
    project
}

fn boot(project: &common::TempProject, client: &Connection) {
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

    let abs = project.root.join("src/a.gd");
    let text = std::fs::read_to_string(abs.as_std_path()).unwrap();
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
    while try_recv(client, Duration::from_millis(500)).is_some() {}
}

fn prepare_fa(client: &Connection, project: &common::TempProject) -> CallHierarchyItem {
    let a_uri = file_uri(&project.root.join("src/a.gd"));
    let prepare = CallHierarchyPrepareParams {
        text_document_position_params: TextDocumentPositionParams {
            text_document: TextDocumentIdentifier { uri: a_uri },
            position: Position {
                line: 1,
                character: 6, // mid-"fa" on `func fa(...)`
            },
        },
        work_done_progress_params: WorkDoneProgressParams::default(),
    };
    client
        .sender
        .send(request(10, "textDocument/prepareCallHierarchy", prepare))
        .unwrap();
    let resp = recv_response(client);
    let items: Option<Vec<CallHierarchyItem>> =
        serde_json::from_value(resp.result.unwrap()).unwrap();
    let item = items
        .and_then(|v| v.into_iter().next())
        .expect("prepare must return fa's item");
    assert_eq!(item.name, "fa");
    // #263: a call-hierarchy item for a GDScript `func` is a METHOD, matching documentSymbol and
    // completion — one symbol must not draw two different glyphs depending on which surface
    // produced it.
    assert_eq!(item.kind, lsp_types::SymbolKind::METHOD);
    item
}

fn outgoing_of(
    client: &Connection,
    id: i32,
    item: CallHierarchyItem,
) -> Option<Vec<CallHierarchyOutgoingCall>> {
    client
        .sender
        .send(request(
            id,
            "callHierarchy/outgoingCalls",
            CallHierarchyOutgoingCallsParams {
                item,
                work_done_progress_params: WorkDoneProgressParams::default(),
                partial_result_params: Default::default(),
            },
        ))
        .unwrap();
    let resp = recv_response(client);
    assert!(resp.error.is_none(), "outgoing errored: {:?}", resp.error);
    serde_json::from_value(resp.result.unwrap()).unwrap()
}

/// The acceptance chain: expanding fa's outgoing tree reaches fc (depth 3) by handing each
/// `to` item back verbatim — pre-fix the second hop returned `null` because `to` items carried
/// no data and the handlers resolved exclusively through it.
#[test]
fn outgoing_expands_to_depth_three_across_files() {
    let project = chain_project();
    let (server, client) = Connection::memory();
    let server_thread = std::thread::spawn(move || gd_server::serve(server));
    boot(&project, &client);

    let fa = prepare_fa(&client, &project);

    let hop1 = outgoing_of(&client, 11, fa).expect("fa's outgoing must answer");
    let fb = hop1
        .iter()
        .find(|c| c.to.name == "fb")
        .unwrap_or_else(|| panic!("fa must call fb; got {hop1:?}"));
    assert!(
        fb.to.uri.as_str().ends_with("/b.gd"),
        "fb's item points at its declaring file; got {}",
        fb.to.uri.as_str()
    );
    assert!(
        fb.to.data.is_some(),
        "to items must carry the {{uri, name}} data blob; got {:?}",
        fb.to
    );
    assert_eq!(
        fb.to.detail.as_deref(),
        Some("res://src/b.gd"),
        "cross-file to-items disambiguate by their res:// detail"
    );

    // Hop 2: the client hands fb's `to` item back verbatim.
    let hop2 = outgoing_of(&client, 12, fb.to.clone())
        .expect("expanding a to-item must answer, not null — the depth-2 wall");
    let fc = hop2
        .iter()
        .find(|c| c.to.name == "fc")
        .unwrap_or_else(|| panic!("fb must call fc; got {hop2:?}"));
    assert!(fc.to.uri.as_str().ends_with("/c.gd"));
    assert!(fc.to.data.is_some());

    // Hop 3: fc's body is `pass` — a clean EMPTY list, not null.
    let hop3 = outgoing_of(&client, 13, fc.to.clone());
    assert_eq!(
        hop3,
        Some(Vec::new()),
        "a leaf function's outgoing calls are empty, never null"
    );

    shutdown(&client, server_thread);
}

/// Switching direction on a `to` item — "show incoming calls" in the references view — must
/// return the callee's callers instead of null.
#[test]
fn incoming_on_an_outgoing_to_item_returns_callers() {
    let project = chain_project();
    let (server, client) = Connection::memory();
    let server_thread = std::thread::spawn(move || gd_server::serve(server));
    boot(&project, &client);

    let fa = prepare_fa(&client, &project);
    let hop1 = outgoing_of(&client, 20, fa).expect("fa's outgoing must answer");
    let fb = hop1.iter().find(|c| c.to.name == "fb").expect("fb item");

    client
        .sender
        .send(request(
            21,
            "callHierarchy/incomingCalls",
            CallHierarchyIncomingCallsParams {
                item: fb.to.clone(),
                work_done_progress_params: WorkDoneProgressParams::default(),
                partial_result_params: Default::default(),
            },
        ))
        .unwrap();
    let resp = recv_response(&client);
    assert!(resp.error.is_none(), "incoming errored: {:?}", resp.error);
    let incoming: Option<Vec<CallHierarchyIncomingCall>> =
        serde_json::from_value(resp.result.unwrap()).unwrap();
    let incoming = incoming.expect("incoming on a to-item must answer, not null");
    assert!(
        incoming.iter().any(|c| c.from.name == "fa"),
        "fb's incoming must include its caller fa; got {incoming:?}"
    );

    shutdown(&client, server_thread);
}

/// The robustness layer: an item whose `data` a client stripped (or synthesized without one)
/// re-resolves from `uri` + `selectionRange.start` and still answers.
#[test]
fn outgoing_resolves_item_without_data() {
    let project = chain_project();
    let (server, client) = Connection::memory();
    let server_thread = std::thread::spawn(move || gd_server::serve(server));
    boot(&project, &client);

    let fa = prepare_fa(&client, &project);
    let hop1 = outgoing_of(&client, 30, fa).expect("fa's outgoing must answer");
    let mut fb_item = hop1
        .iter()
        .find(|c| c.to.name == "fb")
        .expect("fb item")
        .to
        .clone();
    fb_item.data = None;

    let hop2 = outgoing_of(&client, 31, fb_item)
        .expect("a data-less item must re-resolve from uri + selectionRange");
    assert!(
        hop2.iter().any(|c| c.to.name == "fc"),
        "the data-less fb item must still expand to fc; got {hop2:?}"
    );

    shutdown(&client, server_thread);
}
