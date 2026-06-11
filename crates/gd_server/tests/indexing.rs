//! M2 exit gate (server side): build a [`Workspace`] over a real on-disk project and verify the eager
//! index resolves cross-file *names* (script `class_name`s and the native-DB chain), then drive the
//! full LSP loop against that project to confirm startup indexing + the shared parse cache keep
//! `documentSymbol` / `publishDiagnostics` working.

mod common;

use common::{file_uri, options_for, recv, recv_response, sample_project};
use gd_project::Resolution;
use gd_server::config::InitializationOptions;
use gd_server::Workspace;
use lsp_server::{Connection, Message, Notification, Request, RequestId};
use lsp_types::{
    DidOpenTextDocumentParams, DocumentSymbolParams, InitializeParams, InitializedParams,
    TextDocumentIdentifier, TextDocumentItem,
};

#[test]
fn workspace_resolves_cross_file_and_native_chain() {
    let project = sample_project();
    let ws = Workspace::load(&project.root, &options_for(&project));

    // The native dump loaded (engine classes).
    assert!(ws.native.class_count() >= 4, "native dump should load");

    // Both scripts were cold-indexed; Hero registered its class_name.
    assert_eq!(ws.index.file_count(), 2);
    assert!(ws.index.registry().get("Hero").is_some());

    // enemy.gd `extends Hero` resolves cross-file to hero.gd (a project script class).
    let enemy = ws
        .index
        .file_id(&project.root.join("src/enemy.gd"))
        .expect("enemy.gd indexed");
    let Resolution::Script(hero) = ws.index.resolve_base(enemy, &ws.native) else {
        panic!("enemy's base should resolve to the Hero script");
    };
    assert!(ws
        .index
        .path(hero)
        .is_some_and(|p| p.as_str().ends_with("hero.gd")));

    // hero.gd `extends Node2D` resolves into the native DB, which knows Node2D ⊂ Object.
    assert_eq!(ws.index.resolve_base(hero, &ws.native), Resolution::Native);
    assert!(ws.native.is_subclass_of_named("Node2D", "Object"));
}

#[test]
fn missing_dump_degrades_but_still_resolves_scripts() {
    let project = sample_project();
    // Remove the sample's root-level dump: since v1.0.1 an unmanaged `<root>/extension_api.json`
    // is a legitimate fallback source (the auto-dump resolution ladder), so "missing dump" must
    // mean genuinely missing — no extensionApiPath, no `.gdls` dump, no root file, dump disabled,
    // and (v1.0.2) the embedded stock fallback disabled too.
    project.remove("extension_api.json");
    let opts = InitializationOptions::parse(Some(&serde_json::json!({
        "projectRoot": project.root.as_str(),
        "autoDumpExtensionApi": false,
        "embeddedApiFallback": false,
    })));
    let ws = Workspace::load(&project.root, &opts);

    assert!(ws.native.is_empty(), "no dump ⇒ native types degrade");
    assert_eq!(ws.native.provenance(), gd_types::ApiProvenance::Absent);
    let enemy = ws
        .index
        .file_id(&project.root.join("src/enemy.gd"))
        .unwrap();
    // Hero still resolves (it's a project script), Node2D becomes Unknown (no DB) — never a crash.
    assert!(matches!(
        ws.index.resolve_base(enemy, &ws.native),
        Resolution::Script(_)
    ));
    let hero = ws.index.file_id(&project.root.join("src/hero.gd")).unwrap();
    assert_eq!(ws.index.resolve_base(hero, &ws.native), Resolution::Unknown);
}

/// v1.0.2 (issue #24): with every project-derived source missing and the default options, the
/// embedded stock surface steps in — builtins resolve (`Generic` provenance), so a fresh install
/// with no Godot binary anywhere still types `Node2D` instead of erroring on every native name.
#[test]
fn missing_dump_falls_back_to_embedded_stock_surface() {
    let project = sample_project();
    project.remove("extension_api.json");
    let opts = InitializationOptions::parse(Some(&serde_json::json!({
        "projectRoot": project.root.as_str(),
        "autoDumpExtensionApi": false,
    })));
    let ws = Workspace::load(&project.root, &opts);

    assert!(!ws.native.is_empty(), "embedded fallback must ingest");
    assert_eq!(ws.native.provenance(), gd_types::ApiProvenance::Generic);
    let hero = ws.index.file_id(&project.root.join("src/hero.gd")).unwrap();
    // hero.gd `extends Node2D` resolves natively through the embedded stock surface.
    assert_eq!(ws.index.resolve_base(hero, &ws.native), Resolution::Native);
}

// ---- The full LSP loop against the indexed project ----------------------------------------------

#[test]
fn server_indexes_at_startup_and_serves_symbols() {
    let project = sample_project();
    let (server, client) = Connection::memory();
    let server_thread = std::thread::spawn(move || gd_server::serve(server));

    // initialize with the project root + dump via initializationOptions.
    let init = InitializeParams {
        initialization_options: Some(serde_json::json!({
            "projectRoot": project.root.as_str(),
            "autoDumpExtensionApi": false,
            "extensionApiPath": project.root.join("extension_api.json").as_str(),
        })),
        ..Default::default()
    };
    client
        .sender
        .send(Message::Request(Request {
            id: RequestId::from(1),
            method: "initialize".to_string(),
            params: serde_json::to_value(init).unwrap(),
        }))
        .unwrap();
    let resp = recv_response(&client);
    assert!(resp.error.is_none(), "initialize errored: {:?}", resp.error);

    client
        .sender
        .send(Message::Notification(Notification {
            method: "initialized".to_string(),
            params: serde_json::to_value(InitializedParams {}).unwrap(),
        }))
        .unwrap();

    // didOpen hero.gd (already on disk + indexed) → expect an empty (clean) diagnostics push.
    let hero_uri = file_uri(&project.root.join("src/hero.gd"));
    let hero_src = std::fs::read_to_string(project.root.join("src/hero.gd")).unwrap();
    client
        .sender
        .send(Message::Notification(Notification {
            method: "textDocument/didOpen".to_string(),
            params: serde_json::to_value(DidOpenTextDocumentParams {
                text_document: TextDocumentItem {
                    uri: hero_uri.clone(),
                    language_id: "gdscript".to_string(),
                    version: 1,
                    text: hero_src,
                },
            })
            .unwrap(),
        }))
        .unwrap();
    let Message::Notification(note) = recv(&client) else {
        panic!("expected publishDiagnostics");
    };
    assert_eq!(note.method, "textDocument/publishDiagnostics");

    // documentSymbol → the cached parse projects Hero's members (hp, attack).
    client
        .sender
        .send(Message::Request(Request {
            id: RequestId::from(2),
            method: "textDocument/documentSymbol".to_string(),
            params: serde_json::to_value(DocumentSymbolParams {
                text_document: TextDocumentIdentifier { uri: hero_uri },
                work_done_progress_params: Default::default(),
                partial_result_params: Default::default(),
            })
            .unwrap(),
        }))
        .unwrap();
    let resp = recv_response(&client);
    assert!(resp.error.is_none());
    let body = serde_json::to_string(&resp.result.unwrap()).unwrap();
    assert!(
        body.contains("attack") && body.contains("hp"),
        "symbols: {body}"
    );

    // shutdown + exit.
    client
        .sender
        .send(Message::Request(Request {
            id: RequestId::from(3),
            method: "shutdown".to_string(),
            params: serde_json::Value::Null,
        }))
        .unwrap();
    let _ = recv(&client);
    client
        .sender
        .send(Message::Notification(Notification {
            method: "exit".to_string(),
            params: serde_json::Value::Null,
        }))
        .unwrap();
    server_thread
        .join()
        .expect("server thread panicked")
        .expect("serve() returned an error");
}
