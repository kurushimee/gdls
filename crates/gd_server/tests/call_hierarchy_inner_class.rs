//! #360 — a call-hierarchy item names ONE method, not every method of that name in the file.
//!
//! The item's `data` blob used to be `{uri, name}` only, so a root `func tick()` and an inner
//! class's `func tick()` collapsed into the same item: both legs answered with the union of the
//! two methods' call sets, and even the item's own `range` could point at the wrong declaration.
//! A wrong answer that renders as a plausible tree is the anti-catalog's worst failure mode, and
//! two same-named methods in one file — a root class plus a small helper class — is ordinary
//! GDScript.
//!
//! The analyzer already recorded the callee's owning class (`CalleeTarget::Script::class_path`);
//! this pins that the caller side records it too and that both legs key on it.

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

/// `ch.gd` declares `tick` twice — once at the root, once inside `class In2` — and each calls a
/// different helper, so the two call sets are distinguishable. `caller.gd` calls the ROOT `tick`
/// through a typed value, so the incoming leg is distinguishable too.
fn twin_project() -> common::TempProject {
    let project = sample_project();
    project.write(
        "src/ch.gd",
        "class_name Ch\n\
         extends Node\n\
         \n\
         func helper() -> void:\n\
         \tpass\n\
         \n\
         func tick() -> void:\n\
         \thelper()\n\
         \n\
         class In2:\n\
         \tfunc inner_h() -> void:\n\
         \t\tpass\n\
         \tfunc tick() -> void:\n\
         \t\tinner_h()\n",
    );
    project.write(
        "src/caller.gd",
        "extends Node\nfunc run(c: Ch) -> void:\n\tc.tick()\n",
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

    for rel in ["src/ch.gd", "src/caller.gd"] {
        let abs = project.root.join(rel);
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
    }
    while try_recv(client, Duration::from_millis(500)).is_some() {}
}

fn prepare_at(
    client: &Connection,
    project: &common::TempProject,
    id: i32,
    line: u32,
    character: u32,
) -> CallHierarchyItem {
    let uri = file_uri(&project.root.join("src/ch.gd"));
    let prepare = CallHierarchyPrepareParams {
        text_document_position_params: TextDocumentPositionParams {
            text_document: TextDocumentIdentifier { uri },
            position: Position { line, character },
        },
        work_done_progress_params: WorkDoneProgressParams::default(),
    };
    client
        .sender
        .send(request(id, "textDocument/prepareCallHierarchy", prepare))
        .unwrap();
    let resp = recv_response(client);
    let items: Option<Vec<CallHierarchyItem>> =
        serde_json::from_value(resp.result.unwrap()).unwrap();
    items
        .and_then(|v| v.into_iter().next())
        .unwrap_or_else(|| panic!("prepare must answer at {line}:{character}"))
}

fn outgoing_of(client: &Connection, id: i32, item: CallHierarchyItem) -> Vec<String> {
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
    let calls: Option<Vec<CallHierarchyOutgoingCall>> =
        serde_json::from_value(resp.result.unwrap()).unwrap();
    calls
        .unwrap_or_default()
        .into_iter()
        .map(|c| c.to.name)
        .collect()
}

fn incoming_of(client: &Connection, id: i32, item: CallHierarchyItem) -> Vec<String> {
    client
        .sender
        .send(request(
            id,
            "callHierarchy/incomingCalls",
            CallHierarchyIncomingCallsParams {
                item,
                work_done_progress_params: WorkDoneProgressParams::default(),
                partial_result_params: Default::default(),
            },
        ))
        .unwrap();
    let resp = recv_response(client);
    assert!(resp.error.is_none(), "incoming errored: {:?}", resp.error);
    let calls: Option<Vec<CallHierarchyIncomingCall>> =
        serde_json::from_value(resp.result.unwrap()).unwrap();
    calls
        .unwrap_or_default()
        .into_iter()
        .map(|c| c.from.name)
        .collect()
}

/// Each `tick` answers with its OWN calls. The union — `["helper", "inner_h"]` on both — is the
/// defect.
#[test]
fn each_same_named_method_answers_with_its_own_outgoing_calls() {
    let project = twin_project();
    let (server, client) = Connection::memory();
    let server_thread = std::thread::spawn(move || gd_server::serve(server));
    boot(&project, &client);

    // `func tick()` at the root is line 6; `In2`'s is line 12.
    let root_tick = prepare_at(&client, &project, 10, 6, 6);
    let inner_tick = prepare_at(&client, &project, 11, 12, 7);

    assert_eq!(outgoing_of(&client, 12, root_tick), vec!["helper"]);
    assert_eq!(outgoing_of(&client, 13, inner_tick), vec!["inner_h"]);

    shutdown(&client, server_thread);
}

/// `caller.gd` calls the ROOT `tick` only, so the inner class's `tick` has no callers. Before
/// this, both reported `run`.
#[test]
fn only_the_called_method_reports_the_caller() {
    let project = twin_project();
    let (server, client) = Connection::memory();
    let server_thread = std::thread::spawn(move || gd_server::serve(server));
    boot(&project, &client);

    let root_tick = prepare_at(&client, &project, 10, 6, 6);
    let inner_tick = prepare_at(&client, &project, 11, 12, 7);

    assert_eq!(incoming_of(&client, 12, root_tick), vec!["run"]);
    assert!(
        incoming_of(&client, 13, inner_tick).is_empty(),
        "the inner class's tick has no callers"
    );

    shutdown(&client, server_thread);
}

/// The item's own `selectionRange` anchors at the declaration the cursor was on — not at the first
/// same-named `func` in arena order.
#[test]
fn the_item_anchors_at_the_declaration_under_the_cursor() {
    let project = twin_project();
    let (server, client) = Connection::memory();
    let server_thread = std::thread::spawn(move || gd_server::serve(server));
    boot(&project, &client);

    let root_tick = prepare_at(&client, &project, 10, 6, 6);
    let inner_tick = prepare_at(&client, &project, 11, 12, 7);

    assert_eq!(root_tick.selection_range.start.line, 6);
    assert_eq!(inner_tick.selection_range.start.line, 12);
    assert_ne!(
        root_tick.data, inner_tick.data,
        "two distinct methods must not share one data blob"
    );

    shutdown(&client, server_thread);
}

/// A root-class item omits `class_path` entirely, so an item a client cached before the field
/// existed still resolves — absent reads as the root class.
#[test]
fn a_root_class_item_omits_the_class_path() {
    let project = twin_project();
    let (server, client) = Connection::memory();
    let server_thread = std::thread::spawn(move || gd_server::serve(server));
    boot(&project, &client);

    let root_tick = prepare_at(&client, &project, 10, 6, 6);
    let data = root_tick.data.clone().expect("item carries data");
    assert!(
        data.get("class_path").is_none(),
        "the root class needs no path; got {data}"
    );

    let inner_tick = prepare_at(&client, &project, 11, 12, 7);
    let data = inner_tick.data.clone().expect("item carries data");
    assert_eq!(
        data.get("class_path"),
        Some(&serde_json::json!(["In2"])),
        "got {data}"
    );

    shutdown(&client, server_thread);
}
