//! M6-E gate: `textDocument/references` through cross-file `Binding::Call` edges.
//!
//! When `lib.gd` defines `func helper()` and `a.gd`/`b.gd` each call it through a typed local,
//! find-references on `helper`'s declaration must include both cross-file call sites.

mod common;

use common::{file_uri, notification, recv, request, shutdown, TempProject};
use lsp_server::{Connection, Message};
use lsp_types::{
    DidOpenTextDocumentParams, InitializeParams, InitializedParams, Location, Position,
    ReferenceContext, ReferenceParams, TextDocumentIdentifier, TextDocumentItem,
    TextDocumentPositionParams, WorkDoneProgressParams,
};

fn init_and_open(project: &TempProject, client: &Connection, files: &[&str]) {
    let init = InitializeParams {
        initialization_options: Some(serde_json::json!({
            "projectRoot": project.root.as_str(),
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

    for (i, rel) in files.iter().enumerate() {
        let abs = project.root.join(rel);
        let text = std::fs::read_to_string(abs.as_std_path()).expect("read file");
        let uri = file_uri(&abs);
        client
            .sender
            .send(notification(
                "textDocument/didOpen",
                DidOpenTextDocumentParams {
                    text_document: TextDocumentItem {
                        uri,
                        language_id: "gdscript".to_string(),
                        version: (i + 2) as i32,
                        text,
                    },
                },
            ))
            .unwrap();
    }
    // Drain all publishDiagnostics pushes.
    while common::try_recv(client, std::time::Duration::from_millis(300)).is_some() {}
}

/// M6-E: references on `helper` in `lib.gd` must return the call sites in `a.gd` and `b.gd`.
#[test]
fn references_finds_cross_file_method_calls() {
    let p = TempProject::new();
    p.write("project.godot", "config_version=5\n");
    p.write("extension_api.json", common::MINI_API);

    // lib.gd defines `class_name Lib` and `func helper()`.
    // Line 0: `class_name Lib`
    // Line 1: `extends Node`
    // Line 3: `func helper():`
    // Line 4: `\tpass`
    // `helper` identifier at line 3, col 5..11.
    p.write(
        "lib.gd",
        "class_name Lib\nextends Node\n\nfunc helper():\n\tpass\n",
    );

    // a.gd calls helper() via a typed parameter (so `Lib` appears in its interface,
    // making it a `name_referencers("Lib")` candidate).
    // Line 0: `extends Node`
    // Line 2: `func test(l: Lib):`
    // Line 3: `\tl.helper()`  — `helper` at col 3..9
    p.write("a.gd", "extends Node\n\nfunc test(l: Lib):\n\tl.helper()\n");

    // b.gd also calls helper() via a typed parameter.
    // Line 0: `extends Node`
    // Line 2: `func run(x: Lib):`
    // Line 3: `\tx.helper()`  — `helper` at col 3..9
    p.write("b.gd", "extends Node\n\nfunc run(x: Lib):\n\tx.helper()\n");

    let (server, client) = Connection::memory();
    let server_thread = std::thread::spawn(move || gd_server::serve(server));
    init_and_open(&p, &client, &["lib.gd", "a.gd", "b.gd"]);

    let lib_uri = file_uri(&p.root.join("lib.gd"));
    let params = ReferenceParams {
        text_document_position: TextDocumentPositionParams {
            text_document: TextDocumentIdentifier {
                uri: lib_uri.clone(),
            },
            // Click on `helper` at line 3, col 7.
            position: Position {
                line: 3,
                character: 7,
            },
        },
        context: ReferenceContext {
            include_declaration: false,
        },
        work_done_progress_params: WorkDoneProgressParams::default(),
        partial_result_params: Default::default(),
    };
    client
        .sender
        .send(request(10, "textDocument/references", params))
        .unwrap();
    let Message::Response(resp) = recv(&client) else {
        panic!("expected references response");
    };
    assert!(resp.error.is_none(), "references errored: {:?}", resp.error);
    let locs: Vec<Location> =
        serde_json::from_value(resp.result.expect("references result")).unwrap();

    // Must include call sites from a.gd AND b.gd.
    let a_uri = file_uri(&p.root.join("a.gd"));
    let b_uri = file_uri(&p.root.join("b.gd"));
    let has_a = locs.iter().any(|l| l.uri == a_uri);
    let has_b = locs.iter().any(|l| l.uri == b_uri);
    assert!(
        has_a,
        "references must include call site in a.gd; got: {locs:?}"
    );
    assert!(
        has_b,
        "references must include call site in b.gd; got: {locs:?}"
    );

    // The call sites must be the `helper` identifier range (narrow), not the whole call expression.
    for loc in locs.iter().filter(|l| l.uri == a_uri || l.uri == b_uri) {
        assert_eq!(
            loc.range.start.character, 3,
            "call site range should start at `helper` identifier col 3, got {loc:?}"
        );
    }

    shutdown(&client, server_thread);
}
