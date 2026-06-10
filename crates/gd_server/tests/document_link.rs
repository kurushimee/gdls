//! M6-C2 gate: `textDocument/documentLink` returns one link per `res://`-path string literal in
//! the file (inside preload/load calls). Non-res paths (`user://`, plain strings) produce no
//! link. The capability flag must also be advertised in `initialize`.

mod common;

use common::{file_uri, notification, recv, request, shutdown, TempProject};
use lsp_server::{Connection, Message};
use lsp_types::{
    ClientCapabilities, DocumentLinkParams, GeneralClientCapabilities, InitializeParams,
    InitializeResult, InitializedParams, PositionEncodingKind, TextDocumentIdentifier,
    TextDocumentItem,
};

fn boot(project: &TempProject) -> (Connection, std::thread::JoinHandle<anyhow::Result<()>>) {
    let (server, client) = Connection::memory();
    let handle = std::thread::spawn(move || gd_server::serve(server));

    let init = InitializeParams {
        capabilities: ClientCapabilities {
            general: Some(GeneralClientCapabilities {
                position_encodings: Some(vec![PositionEncodingKind::UTF8]),
                ..Default::default()
            }),
            ..Default::default()
        },
        initialization_options: Some(serde_json::json!({
            "projectRoot": project.root.as_str(),
        "autoDumpExtensionApi": false,
        })),
        ..Default::default()
    };
    client.sender.send(request(1, "initialize", init)).unwrap();
    let Message::Response(_) = recv(&client) else {
        panic!("expected initialize response");
    };
    client
        .sender
        .send(notification("initialized", InitializedParams {}))
        .unwrap();
    (client, handle)
}

fn did_open(client: &Connection, rel_path: &camino::Utf8Path, text: &str) {
    let uri = file_uri(rel_path);
    client
        .sender
        .send(notification(
            "textDocument/didOpen",
            lsp_types::DidOpenTextDocumentParams {
                text_document: TextDocumentItem {
                    uri,
                    language_id: "gdscript".to_string(),
                    version: 1,
                    text: text.to_string(),
                },
            },
        ))
        .unwrap();
    // Drain the publishDiagnostics push.
    let _ = recv(client);
}

/// `initialize` must advertise `documentLinkProvider` as part of the v1 capability set.
#[test]
fn initialize_advertises_document_link_provider() {
    let p = TempProject::new();
    p.write("project.godot", "");
    let (server, client) = Connection::memory();
    let handle = std::thread::spawn(move || gd_server::serve(server));

    let init = InitializeParams {
        initialization_options: Some(serde_json::json!({
            "projectRoot": p.root.as_str(),
        "autoDumpExtensionApi": false,
        })),
        ..Default::default()
    };
    client.sender.send(request(1, "initialize", init)).unwrap();
    let Message::Response(resp) = recv(&client) else {
        panic!("expected initialize response");
    };
    let result: InitializeResult =
        serde_json::from_value(resp.result.expect("initialize result")).unwrap();
    assert!(
        result.capabilities.document_link_provider.is_some(),
        "v1 capability set must advertise documentLinkProvider"
    );

    // Complete initialization so the server is in Running state before shutdown.
    client
        .sender
        .send(notification("initialized", InitializedParams {}))
        .unwrap();

    shutdown(&client, handle);
}

/// A file with `preload("res://foo.gd")` yields exactly one `DocumentLink` whose `target` URI
/// points at `foo.gd`. The link's range covers the string literal (quotes included).
#[test]
fn document_link_returns_res_path_links() {
    let p = TempProject::new();
    p.write("project.godot", "");
    p.write("foo.gd", "extends Node\n");
    // caller.gd: line 0 is `const F = preload("res://foo.gd")`
    // The string literal `"res://foo.gd"` spans cols 18..32.
    let caller_src = "const F = preload(\"res://foo.gd\")\n";
    p.write("caller.gd", caller_src);

    let (client, handle) = boot(&p);
    let caller_path = p.root.join("caller.gd");
    did_open(&client, &caller_path, caller_src);

    let caller_uri = file_uri(&caller_path);
    client
        .sender
        .send(request(
            10,
            "textDocument/documentLink",
            DocumentLinkParams {
                text_document: TextDocumentIdentifier {
                    uri: caller_uri.clone(),
                },
                work_done_progress_params: Default::default(),
                partial_result_params: Default::default(),
            },
        ))
        .unwrap();

    let Message::Response(resp) = recv(&client) else {
        panic!("expected documentLink response");
    };
    assert!(
        resp.error.is_none(),
        "documentLink errored: {:?}",
        resp.error
    );
    let links: Vec<lsp_types::DocumentLink> =
        serde_json::from_value(resp.result.expect("documentLink result")).unwrap();

    assert_eq!(links.len(), 1, "expected exactly one link, got {links:?}");
    let link = &links[0];

    // Target URI must point at foo.gd.
    let target = link.target.as_ref().expect("link must have a target");
    let target_str = target.as_str();
    assert!(
        target_str.ends_with("/foo.gd"),
        "link target should be foo.gd, got {target_str}"
    );

    // Range covers the string literal including quotes: col 18..32.
    assert_eq!(link.range.start.line, 0);
    assert_eq!(link.range.start.character, 18);
    assert_eq!(link.range.end.character, 32);

    shutdown(&client, handle);
}

/// A `res://` path pointing to a `.gd` file that does NOT exist in the project yields no link:
/// the existence gate (index membership for `.gd`, an `is_file` check for other resources) blocks
/// links to targets that aren't on disk.
#[test]
fn document_link_no_link_for_nonexistent_res_path() {
    let p = TempProject::new();
    p.write("project.godot", "");
    // `nonexistent.gd` is NOT written — only the referencing script is.
    let src = "const X = preload(\"res://nonexistent.gd\")\n";
    p.write("caller.gd", src);

    let (client, handle) = boot(&p);
    let path = p.root.join("caller.gd");
    did_open(&client, &path, src);

    let uri = file_uri(&path);
    client
        .sender
        .send(request(
            10,
            "textDocument/documentLink",
            DocumentLinkParams {
                text_document: TextDocumentIdentifier { uri },
                work_done_progress_params: Default::default(),
                partial_result_params: Default::default(),
            },
        ))
        .unwrap();

    let Message::Response(resp) = recv(&client) else {
        panic!("expected documentLink response");
    };
    assert!(
        resp.error.is_none(),
        "documentLink errored: {:?}",
        resp.error
    );
    let links: Vec<lsp_types::DocumentLink> =
        serde_json::from_value(resp.result.expect("documentLink result")).unwrap();
    assert!(
        links.is_empty(),
        "must not emit a link for a res:// path that doesn't exist in the project, got {links:?}"
    );

    shutdown(&client, handle);
}

/// A file with only `user://` paths or plain strings yields no document links
/// (only `res://` paths that can be resolved to on-disk files produce links).
#[test]
fn document_link_ignores_non_res_strings() {
    let p = TempProject::new();
    p.write("project.godot", "");
    // This file has no `res://` paths.
    let src = "const A = load(\"user://x.gd\")\nconst B = \"hello\"\n";
    p.write("nores.gd", src);

    let (client, handle) = boot(&p);
    let path = p.root.join("nores.gd");
    did_open(&client, &path, src);

    let uri = file_uri(&path);
    client
        .sender
        .send(request(
            10,
            "textDocument/documentLink",
            DocumentLinkParams {
                text_document: TextDocumentIdentifier { uri },
                work_done_progress_params: Default::default(),
                partial_result_params: Default::default(),
            },
        ))
        .unwrap();

    let Message::Response(resp) = recv(&client) else {
        panic!("expected documentLink response");
    };
    assert!(
        resp.error.is_none(),
        "documentLink errored: {:?}",
        resp.error
    );
    let links: Vec<lsp_types::DocumentLink> =
        serde_json::from_value(resp.result.expect("documentLink result")).unwrap();
    assert!(
        links.is_empty(),
        "no links expected for non-res strings, got {links:?}"
    );

    shutdown(&client, handle);
}

/// A `res://` literal that resolves to a real **non-GDScript** on-disk resource (`.tscn`/`.tres`/
/// asset) still produces a link. The index holds only `.gd`, so this drives the fallback path
/// (`Index::res_to_path` + `is_file`), not index membership — `preload`/`load` of scenes and assets
/// must link, not silently produce nothing.
#[test]
fn document_link_links_non_gd_resource() {
    let p = TempProject::new();
    p.write("project.godot", "");
    // A real scene file on disk — NOT a `.gd`, so it never enters the index.
    p.write("scenes/main.tscn", "[gd_scene]\n");
    let src = "const S = preload(\"res://scenes/main.tscn\")\n";
    p.write("caller.gd", src);

    let (client, handle) = boot(&p);
    let caller_path = p.root.join("caller.gd");
    did_open(&client, &caller_path, src);

    let caller_uri = file_uri(&caller_path);
    client
        .sender
        .send(request(
            10,
            "textDocument/documentLink",
            DocumentLinkParams {
                text_document: TextDocumentIdentifier { uri: caller_uri },
                work_done_progress_params: Default::default(),
                partial_result_params: Default::default(),
            },
        ))
        .unwrap();

    let Message::Response(resp) = recv(&client) else {
        panic!("expected documentLink response");
    };
    assert!(
        resp.error.is_none(),
        "documentLink errored: {:?}",
        resp.error
    );
    let links: Vec<lsp_types::DocumentLink> =
        serde_json::from_value(resp.result.expect("documentLink result")).unwrap();

    assert_eq!(
        links.len(),
        1,
        "a preload of an on-disk .tscn must produce one link, got {links:?}"
    );
    let target = links[0].target.as_ref().expect("link must have a target");
    assert!(
        target.as_str().ends_with("/scenes/main.tscn"),
        "link target should be the .tscn, got {}",
        target.as_str()
    );

    shutdown(&client, handle);
}

/// The fallback (non-`.gd`) path still gates on existence: a `res://` literal for a resource that
/// is not on disk produces no link — proving the `is_file` check, not index membership, blocks
/// dangling links.
#[test]
fn document_link_no_link_for_nonexistent_non_gd_resource() {
    let p = TempProject::new();
    p.write("project.godot", "");
    // `missing.tscn` is never written.
    let src = "const S = preload(\"res://missing.tscn\")\n";
    p.write("caller.gd", src);

    let (client, handle) = boot(&p);
    let caller_path = p.root.join("caller.gd");
    did_open(&client, &caller_path, src);

    let caller_uri = file_uri(&caller_path);
    client
        .sender
        .send(request(
            10,
            "textDocument/documentLink",
            DocumentLinkParams {
                text_document: TextDocumentIdentifier { uri: caller_uri },
                work_done_progress_params: Default::default(),
                partial_result_params: Default::default(),
            },
        ))
        .unwrap();

    let Message::Response(resp) = recv(&client) else {
        panic!("expected documentLink response");
    };
    assert!(
        resp.error.is_none(),
        "documentLink errored: {:?}",
        resp.error
    );
    let links: Vec<lsp_types::DocumentLink> =
        serde_json::from_value(resp.result.expect("documentLink result")).unwrap();
    assert!(
        links.is_empty(),
        "no link for a non-existent .tscn, got {links:?}"
    );

    shutdown(&client, handle);
}
