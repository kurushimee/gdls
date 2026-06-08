//! M6-G gate: `textDocument/implementation` on a method identifier returns override locations in
//! subclass files — direct and transitive — rather than just implementing classes (the previous
//! class-level BFS).

mod common;

use common::{file_uri, notification, recv, request, shutdown, TempProject};
use lsp_server::{Connection, Message};
use lsp_types::{
    DidOpenTextDocumentParams, GotoDefinitionParams, GotoDefinitionResponse, InitializeParams,
    InitializedParams, Position, TextDocumentIdentifier, TextDocumentItem,
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
    while common::try_recv(client, std::time::Duration::from_millis(300)).is_some() {}
}

/// M6-G: implementation on `act` in `base.gd` returns override locations in `mid.gd` and `leaf.gd`.
#[test]
fn implementation_on_method_returns_overrides() {
    let p = TempProject::new();
    p.write("project.godot", "config_version=5\n");
    p.write("extension_api.json", common::MINI_API);

    // base.gd: class_name Base; func act()
    // Line 0: `class_name Base`
    // Line 1: `extends Node`
    // Line 3: `func act():`
    // `act` identifier at line 3, col 5..8.
    p.write(
        "base.gd",
        "class_name Base\nextends Node\n\nfunc act():\n\tpass\n",
    );

    // mid.gd: class_name Mid; extends Base; overrides act()
    p.write(
        "mid.gd",
        "class_name Mid\nextends Base\n\nfunc act():\n\tpass\n",
    );

    // leaf.gd: extends Mid; overrides act() (transitive)
    p.write(
        "leaf.gd",
        "class_name Leaf\nextends Mid\n\nfunc act():\n\tpass\n",
    );

    let (server, client) = Connection::memory();
    let server_thread = std::thread::spawn(move || gd_server::serve(server));
    init_and_open(&p, &client, &["base.gd", "mid.gd", "leaf.gd"]);

    let base_uri = file_uri(&p.root.join("base.gd"));
    let params = GotoDefinitionParams {
        text_document_position_params: TextDocumentPositionParams {
            text_document: TextDocumentIdentifier {
                uri: base_uri.clone(),
            },
            // Click on `act` at line 3, col 6.
            position: Position {
                line: 3,
                character: 6,
            },
        },
        work_done_progress_params: WorkDoneProgressParams::default(),
        partial_result_params: Default::default(),
    };
    client
        .sender
        .send(request(10, "textDocument/implementation", params))
        .unwrap();
    let Message::Response(resp) = recv(&client) else {
        panic!("expected implementation response");
    };
    assert!(
        resp.error.is_none(),
        "implementation errored: {:?}",
        resp.error
    );
    let result_val = resp.result.expect("implementation result");
    assert!(
        !result_val.is_null(),
        "implementation on a method override must not return null; cursor on `act` in base.gd should find overrides"
    );
    let response: GotoDefinitionResponse =
        serde_json::from_value(result_val).expect("valid GotoDefinitionResponse");
    let locs = match response {
        GotoDefinitionResponse::Array(v) => v,
        other => panic!("expected Array response, got {other:?}"),
    };

    let mid_uri = file_uri(&p.root.join("mid.gd"));
    let leaf_uri = file_uri(&p.root.join("leaf.gd"));

    let has_mid = locs.iter().any(|l| l.uri == mid_uri);
    let has_leaf = locs.iter().any(|l| l.uri == leaf_uri);

    assert!(
        has_mid,
        "implementation must include mid.gd (direct override); got: {locs:?}"
    );
    assert!(
        has_leaf,
        "implementation must include leaf.gd (transitive override); got: {locs:?}"
    );

    shutdown(&client, server_thread);
}

/// Regression: a cursor on a `func` whose name collides with a registered `class_name` in an
/// UNNAMED script must NOT fall through to the class-identifier BFS. `find_method_overrides`
/// confirms the cursor is on a method of this file, so an unnamed script has no subclasses to
/// report — it must return an empty result rather than `None`. Were it `None`, `implementation`
/// would look the function name up in the class_name registry and wrongly surface the unrelated
/// class's subclasses.
#[test]
fn implementation_on_unnamed_script_func_does_not_leak_class_subclasses() {
    let p = TempProject::new();
    p.write("project.godot", "config_version=5\n");
    p.write("extension_api.json", common::MINI_API);

    // unnamed.gd: NO class_name; declares `func Helper()`.
    // Line 0: `extends Node`
    // Line 2: `func Helper():` — `Helper` identifier at cols 5..11.
    p.write("unnamed.gd", "extends Node\n\nfunc Helper():\n\tpass\n");

    // helper.gd: a registered class whose name equals the function above.
    p.write("helper.gd", "class_name Helper\nextends Node\n");

    // sub.gd: extends the registered `Helper` class — the would-be leaked subclass.
    p.write("sub.gd", "class_name Sub\nextends Helper\n");

    let (server, client) = Connection::memory();
    let server_thread = std::thread::spawn(move || gd_server::serve(server));
    init_and_open(&p, &client, &["unnamed.gd", "helper.gd", "sub.gd"]);

    let unnamed_uri = file_uri(&p.root.join("unnamed.gd"));
    let params = GotoDefinitionParams {
        text_document_position_params: TextDocumentPositionParams {
            text_document: TextDocumentIdentifier {
                uri: unnamed_uri.clone(),
            },
            // Click on `Helper` (the func name) at line 2, col 7.
            position: Position {
                line: 2,
                character: 7,
            },
        },
        work_done_progress_params: WorkDoneProgressParams::default(),
        partial_result_params: Default::default(),
    };
    client
        .sender
        .send(request(11, "textDocument/implementation", params))
        .unwrap();
    let Message::Response(resp) = recv(&client) else {
        panic!("expected implementation response");
    };
    assert!(
        resp.error.is_none(),
        "implementation errored: {:?}",
        resp.error
    );
    let result_val = resp.result.expect("implementation result");

    // The fixed handler returns an empty Array (no overrides), never the subclasses of the
    // identically-named `class_name Helper`. A `null` result would also be acceptable
    // semantically, but the method-override branch returns `Some(Vec::new())`.
    let sub_uri = file_uri(&p.root.join("sub.gd"));
    if !result_val.is_null() {
        let response: GotoDefinitionResponse =
            serde_json::from_value(result_val).expect("valid GotoDefinitionResponse");
        let locs = match response {
            GotoDefinitionResponse::Array(v) => v,
            other => panic!("expected Array response, got {other:?}"),
        };
        assert!(
            !locs.iter().any(|l| l.uri == sub_uri),
            "implementation on an unnamed-script func must not leak subclasses of an unrelated \
             class_name with the same name; got: {locs:?}"
        );
    }

    shutdown(&client, server_thread);
}
