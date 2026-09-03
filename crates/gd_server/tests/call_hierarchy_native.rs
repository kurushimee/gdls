//! Native and builtin callees in `callHierarchy/outgoingCalls` anchor into the materialized API
//! stubs — the same pages `definition`/hover use — instead of fabricating a (0,0) location in
//! the caller's own file; callees that resolve nowhere are omitted entirely (the
//! rust-analyzer/gopls convention). Expanding a stub-anchored item answers with a clean empty
//! list.

mod common;

use std::time::Duration;

use common::{file_uri, notification, recv, recv_response, request, shutdown, try_recv};
use lsp_server::Connection;
use lsp_types::{
    CallHierarchyIncomingCallsParams, CallHierarchyItem, CallHierarchyOutgoingCall,
    CallHierarchyOutgoingCallsParams, CallHierarchyPrepareParams, DidOpenTextDocumentParams,
    InitializeParams, InitializedParams, Position, TextDocumentIdentifier, TextDocumentItem,
    TextDocumentPositionParams, WorkDoneProgressParams,
};

const NODE_API: &str = r#"{
    "header": { "version_major": 4, "version_minor": 6, "version_patch": 3 },
    "classes": [
        {"name": "Object", "is_instantiable": true},
        {"name": "Node", "inherits": "Object", "is_instantiable": true,
         "methods": [{"name": "queue_free", "is_const": false, "is_static": false,
                      "is_vararg": false, "is_virtual": false, "hash": 1, "arguments": []}]}
    ],
    "builtin_classes": [
        {"name": "String",
         "methods": [{"name": "to_upper", "is_const": true, "is_static": false,
                      "is_vararg": false, "is_virtual": false, "hash": 2, "arguments": [],
                      "return_value": {"type": "String"}}]}
    ]
}"#;

struct NativeFixture {
    _dir: tempfile::TempDir,
    main_uri: lsp_types::Uri,
    stub_cache: std::path::PathBuf,
}

/// Boot a server over a temp project whose script calls one resolvable native method
/// (`queue_free()`), one name resolvable nowhere (`mystery()`), and one resolvable builtin-type
/// method (`s.to_upper()` on a String literal).
fn boot_native(client: &Connection) -> NativeFixture {
    let dir = tempfile::tempdir().expect("create fixture dir");
    let root = dir.path();
    std::fs::write(root.join("project.godot"), "").unwrap();
    let api_path = root.join("extension_api.json");
    std::fs::write(&api_path, NODE_API).unwrap();
    let stub_cache = root.join("stub-cache");
    let src = "extends Node\nfunc go() -> void:\n\tqueue_free()\n\tmystery()\n\tvar s := \"x\"\n\ts.to_upper()\n";
    let main_path = root.join("main.gd");
    std::fs::write(&main_path, src).unwrap();

    let init = InitializeParams {
        initialization_options: Some(serde_json::json!({
            "projectRoot": root.to_string_lossy().as_ref(),
            "extensionApiPath": api_path.to_string_lossy().as_ref(),
            "autoDumpExtensionApi": false,
            "stubCacheDir": stub_cache.to_string_lossy().as_ref(),
        })),
        ..Default::default()
    };
    client.sender.send(request(1, "initialize", init)).unwrap();
    let _ = recv(client);
    client
        .sender
        .send(notification("initialized", InitializedParams {}))
        .unwrap();

    let main_uri =
        file_uri(camino::Utf8Path::from_path(&root.join("main.gd")).expect("utf-8 fixture path"));
    client
        .sender
        .send(notification(
            "textDocument/didOpen",
            DidOpenTextDocumentParams {
                text_document: TextDocumentItem {
                    uri: main_uri.clone(),
                    language_id: "gdscript".to_string(),
                    version: 1,
                    text: src.to_string(),
                },
            },
        ))
        .unwrap();
    while try_recv(client, Duration::from_millis(500)).is_some() {}

    NativeFixture {
        _dir: dir,
        main_uri,
        stub_cache: stub_cache.to_path_buf(),
    }
}

fn outgoing_of_go(client: &Connection, fixture: &NativeFixture) -> Vec<CallHierarchyOutgoingCall> {
    let prepare = CallHierarchyPrepareParams {
        text_document_position_params: TextDocumentPositionParams {
            text_document: TextDocumentIdentifier {
                uri: fixture.main_uri.clone(),
            },
            position: Position {
                line: 1,
                character: 6, // mid-"go" on `func go(...)`
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
        .expect("prepare must return go's item");

    client
        .sender
        .send(request(
            11,
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
    let outgoing: Option<Vec<CallHierarchyOutgoingCall>> =
        serde_json::from_value(resp.result.unwrap()).unwrap();
    outgoing.expect("outgoing must answer")
}

/// The acceptance: `queue_free()` under a Node-rooted script yields a `to` item whose uri is
/// the Node stub with the range on the member's name token; the unresolvable `mystery()` yields
/// no item; and no item ever carries the caller's uri with a (0,0) anchor.
#[test]
fn outgoing_calls_anchor_native_callees_in_stubs() {
    let (server, client) = Connection::memory();
    let server_thread = std::thread::spawn(move || gd_server::serve(server));
    let fixture = boot_native(&client);

    let outgoing = outgoing_of_go(&client, &fixture);

    let qf = outgoing
        .iter()
        .find(|c| c.to.name == "queue_free")
        .unwrap_or_else(|| panic!("queue_free must appear; got {outgoing:?}"));
    let stub_path = gd_server::uri::uri_to_path(&qf.to.uri).expect("stub uri is a file path");
    assert!(
        stub_path.as_std_path().starts_with(&fixture.stub_cache),
        "the to-item anchors under the stub cache; got {stub_path:?}"
    );
    assert!(
        stub_path.as_str().ends_with("Node.gd"),
        "the DECLARING class owns the stub; got {stub_path:?}"
    );
    let stub_text = std::fs::read_to_string(stub_path.as_std_path()).expect("stub on disk");
    let line = stub_text
        .lines()
        .nth(qf.to.selection_range.start.line as usize)
        .expect("selectionRange line within the stub");
    assert_eq!(line, "func queue_free() -> void");
    // `func queue_free() -> void` — the name token sits at cols 5..15.
    assert_eq!(qf.to.selection_range.start.character, 5);
    assert_eq!(qf.to.selection_range.end.character, 15);
    assert!(qf.to.data.is_some(), "stub to-items carry data too");
    assert_eq!(
        qf.to.detail.as_deref(),
        Some("Node"),
        "the detail names the declaring native class"
    );
    // The call site rides fromRanges: `\tqueue_free()` on line 2, token at cols 1..11.
    assert!(
        qf.from_ranges
            .iter()
            .any(|r| r.start == Position::new(2, 1) && r.end == Position::new(2, 11)),
        "the call site's name token must be a from_range; got {:?}",
        qf.from_ranges
    );

    // The unresolvable callee is OMITTED — and nothing fabricates the pre-fix
    // caller-uri-(0,0) anchor.
    assert!(
        !outgoing.iter().any(|c| c.to.name == "mystery"),
        "an unresolvable callee must be omitted; got {outgoing:?}"
    );
    assert!(
        !outgoing.iter().any(|c| {
            c.to.uri == fixture.main_uri
                && c.to.range.start == Position::new(0, 0)
                && c.to.range.end == Position::new(0, 0)
        }),
        "no to-item may carry the caller's uri with a (0,0) anchor; got {outgoing:?}"
    );

    shutdown(&client, server_thread);
}

/// A builtin-type method call (`s.to_upper()` on a String literal) anchors into the builtin's
/// own stub page — the same page `definition` jumps to for the identical caret (#583) — while
/// the nowhere-resolvable callee stays omitted.
#[test]
fn outgoing_calls_anchor_builtin_callees_in_stubs() {
    let (server, client) = Connection::memory();
    let server_thread = std::thread::spawn(move || gd_server::serve(server));
    let fixture = boot_native(&client);

    let outgoing = outgoing_of_go(&client, &fixture);

    let tu = outgoing
        .iter()
        .find(|c| c.to.name == "to_upper")
        .unwrap_or_else(|| panic!("to_upper must appear; got {outgoing:?}"));
    let stub_path = gd_server::uri::uri_to_path(&tu.to.uri).expect("stub uri is a file path");
    assert!(
        stub_path.as_str().ends_with("String.gd"),
        "the builtin owns its stub page; got {stub_path:?}"
    );
    let stub_text = std::fs::read_to_string(stub_path.as_std_path()).expect("stub on disk");
    let line = stub_text
        .lines()
        .nth(tu.to.selection_range.start.line as usize)
        .expect("selectionRange line within the stub");
    assert_eq!(line, "func to_upper() -> void");
    assert!(
        tu.to.detail.as_deref() == Some("String"),
        "the detail names the builtin; got {:?}",
        tu.to.detail
    );
    // `\ts.to_upper()` on line 5 — the callee's name token sits at cols 3..11.
    assert!(
        tu.from_ranges
            .iter()
            .any(|r| r.start == Position::new(5, 3) && r.end == Position::new(5, 11)),
        "the call site's name token must be a from_range; got {:?}",
        tu.from_ranges
    );

    // The nowhere-resolvable callee is still omitted — #583 changes only the premise for
    // callees that DO resolve somewhere.
    assert!(
        !outgoing.iter().any(|c| c.to.name == "mystery"),
        "an unresolvable callee must still be omitted; got {outgoing:?}"
    );

    shutdown(&client, server_thread);
}

/// Expanding a stub-anchored item (the references view hands `to` items back verbatim) answers
/// with a clean EMPTY list in both directions — never an error, never an attempt to analyze the
/// API page as project code.
#[test]
fn call_hierarchy_on_stub_uri_returns_empty() {
    let (server, client) = Connection::memory();
    let server_thread = std::thread::spawn(move || gd_server::serve(server));
    let fixture = boot_native(&client);

    let outgoing = outgoing_of_go(&client, &fixture);
    let qf_item = outgoing
        .iter()
        .find(|c| c.to.name == "queue_free")
        .expect("queue_free item")
        .to
        .clone();

    client
        .sender
        .send(request(
            20,
            "callHierarchy/outgoingCalls",
            CallHierarchyOutgoingCallsParams {
                item: qf_item.clone(),
                work_done_progress_params: WorkDoneProgressParams::default(),
                partial_result_params: Default::default(),
            },
        ))
        .unwrap();
    let resp = recv_response(&client);
    assert!(resp.error.is_none(), "outgoing errored: {:?}", resp.error);
    let outgoing: Option<Vec<CallHierarchyOutgoingCall>> =
        serde_json::from_value(resp.result.unwrap()).unwrap();
    assert_eq!(
        outgoing,
        Some(Vec::new()),
        "outgoing on a stub item is empty, never null/error"
    );

    client
        .sender
        .send(request(
            21,
            "callHierarchy/incomingCalls",
            CallHierarchyIncomingCallsParams {
                item: qf_item,
                work_done_progress_params: WorkDoneProgressParams::default(),
                partial_result_params: Default::default(),
            },
        ))
        .unwrap();
    let resp = recv_response(&client);
    assert!(resp.error.is_none(), "incoming errored: {:?}", resp.error);
    let incoming: Option<Vec<lsp_types::CallHierarchyIncomingCall>> =
        serde_json::from_value(resp.result.unwrap()).unwrap();
    assert_eq!(
        incoming,
        Some(Vec::new()),
        "incoming on a stub item is empty, never null/error"
    );

    shutdown(&client, server_thread);
}
