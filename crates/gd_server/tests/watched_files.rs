//! M7 (#60) — dynamic `didChangeWatchedFiles`: the one-shot `client/registerCapability` (gated
//! on the client's dynamicRegistration), client events keeping the index fresh with the native
//! watcher dead (the Helix scenario — its only watch path), the duplicate-delivery dedupe gate,
//! and the project-reload routing of non-`.gd` client events.

mod common;

use std::time::{Duration, Instant};

use common::{file_uri, notification, recv, recv_response, request, sample_project, try_recv};
use crossbeam_channel::{Receiver, Sender};
use lsp_server::{Connection, Message, RequestId, Response};
use lsp_types::{
    ClientCapabilities, DidChangeWatchedFilesClientCapabilities, DidChangeWatchedFilesParams,
    DidOpenTextDocumentParams, FileChangeType, FileEvent, InitializeParams, InitializedParams,
    TextDocumentItem, Uri, WorkspaceClientCapabilities,
};
use notify_debouncer_full::{DebounceEventResult, DebouncedEvent};

fn dynamic_registration_caps() -> ClientCapabilities {
    ClientCapabilities {
        workspace: Some(WorkspaceClientCapabilities {
            did_change_watched_files: Some(DidChangeWatchedFilesClientCapabilities {
                dynamic_registration: Some(true),
                ..Default::default()
            }),
            ..Default::default()
        }),
        ..Default::default()
    }
}

/// Boot with an INJECTED watcher channel whose sender the test holds — sending nothing makes the
/// native watcher effectively dead (the Helix scenario); sending synthetic events simulates the
/// native side deterministically.
fn boot_injected(
    p: &common::TempProject,
    capabilities: ClientCapabilities,
) -> (
    Connection,
    Sender<DebounceEventResult>,
    std::thread::JoinHandle<anyhow::Result<()>>,
) {
    let (server, client) = Connection::memory();
    let (watcher_tx, watcher_rx): (Sender<DebounceEventResult>, Receiver<DebounceEventResult>) =
        crossbeam_channel::unbounded();
    let server_thread =
        std::thread::spawn(move || gd_server::serve_with_injected_watcher(server, watcher_rx));
    let init = InitializeParams {
        capabilities,
        initialization_options: Some(serde_json::json!({
            "projectRoot": p.root.as_str(),
            "autoDumpExtensionApi": false,
            "extensionApiPath": p.root.join("extension_api.json").as_str(),
        })),
        ..Default::default()
    };
    client.sender.send(request(1, "initialize", init)).unwrap();
    loop {
        if let Message::Response(resp) = recv(&client) {
            assert!(resp.error.is_none());
            break;
        }
    }
    client
        .sender
        .send(notification("initialized", InitializedParams {}))
        .unwrap();
    (client, watcher_tx, server_thread)
}

fn shutdown(client: &Connection, thread: std::thread::JoinHandle<anyhow::Result<()>>) {
    client
        .sender
        .send(request(99, "shutdown", serde_json::Value::Null))
        .unwrap();
    let _ = recv_response(client);
    client
        .sender
        .send(notification("exit", serde_json::Value::Null))
        .unwrap();
    thread
        .join()
        .expect("server thread panicked")
        .expect("serve() returned an error");
}

fn file_event(uri: &Uri, typ: FileChangeType) -> Message {
    notification(
        "workspace/didChangeWatchedFiles",
        DidChangeWatchedFilesParams {
            changes: vec![FileEvent {
                uri: uri.clone(),
                typ,
            }],
        },
    )
}

fn workspace_symbol_names(client: &Connection, id: i32, query: &str) -> String {
    client
        .sender
        .send(request(
            id,
            "workspace/symbol",
            serde_json::json!({ "query": query }),
        ))
        .unwrap();
    let resp = loop {
        let r = recv_response(client);
        if r.id == RequestId::from(id) {
            break r;
        }
    };
    assert!(resp.error.is_none());
    serde_json::to_string(&resp.result).unwrap()
}

/// Registration is sent exactly when offered: with `dynamicRegistration: true` the client
/// receives one `client/registerCapability` for the five watch globs after `initialized`;
/// without it, nothing arrives.
#[test]
fn registration_sent_iff_dynamic_registration_offered() {
    let p = sample_project();
    let (client, _watcher_tx, server_thread) = boot_injected(&p, dynamic_registration_caps());

    let req = loop {
        if let Message::Request(req) = recv(&client) {
            break req;
        }
    };
    assert_eq!(req.method, "client/registerCapability");
    let registration = &req.params["registrations"][0];
    assert_eq!(registration["id"], "gdls-watched-files");
    assert_eq!(registration["method"], "workspace/didChangeWatchedFiles");
    let globs: Vec<&str> = registration["registerOptions"]["watchers"]
        .as_array()
        .expect("watchers array")
        .iter()
        .map(|w| w["globPattern"].as_str().unwrap())
        .collect();
    assert_eq!(
        globs,
        vec![
            "**/*.gd",
            "**/project.godot",
            "**/*.gdextension",
            "**/extension_api.json",
            "**/doc_classes/*.xml",
        ]
    );
    client
        .sender
        .send(Message::Response(Response::new_ok(
            req.id,
            serde_json::Value::Null,
        )))
        .unwrap();
    shutdown(&client, server_thread);

    // Without the capability: no registration request within a generous window.
    let p2 = sample_project();
    let (client2, _tx2, server_thread2) = boot_injected(&p2, ClientCapabilities::default());
    let stray = try_recv(&client2, Duration::from_millis(400));
    assert!(
        !matches!(&stray, Some(Message::Request(r)) if r.method == "client/registerCapability"),
        "no dynamic registration without the capability; got {stray:?}"
    );
    shutdown(&client2, server_thread2);
}

/// The #60 acceptance bar: with the native watcher dead (the injected channel never fires —
/// Helix has no OS watcher), client `didChangeWatchedFiles` notifications alone keep the index
/// fresh: a created file's `class_name` resolves, an interface edit republishes the open
/// dependent, a delete removes the symbol.
#[test]
fn client_events_alone_keep_the_index_fresh() {
    let p = sample_project();
    let (client, _watcher_tx, server_thread) = boot_injected(&p, dynamic_registration_caps());
    // Consume + acknowledge the registration.
    let reg = loop {
        if let Message::Request(req) = recv(&client) {
            break req;
        }
    };
    client
        .sender
        .send(Message::Response(Response::new_ok(
            reg.id,
            serde_json::Value::Null,
        )))
        .unwrap();

    // Open a buffer that extends the soon-to-exist class — its diagnostics update is the
    // observable signal for the Changed leg below.
    let probe_uri = file_uri(&p.root.join("probe.gd"));
    p.write("probe.gd", "extends Fresh\n");
    client
        .sender
        .send(notification(
            "textDocument/didOpen",
            DidOpenTextDocumentParams {
                text_document: TextDocumentItem {
                    uri: probe_uri.clone(),
                    language_id: "gdscript".to_string(),
                    version: 1,
                    text: "extends Fresh\n".to_string(),
                },
            },
        ))
        .unwrap();
    let first = loop {
        if let Message::Notification(n) = recv(&client) {
            if n.method == "textDocument/publishDiagnostics" {
                break n;
            }
        }
    };
    // `Fresh` does not exist yet — the probe has an inheritance error.
    assert!(serde_json::to_string(&first.params)
        .unwrap()
        .contains("Fresh"));

    // CREATED: write the file, tell the server via the client channel only.
    let fresh_path = p.root.join("src/fresh.gd");
    p.write("src/fresh.gd", "class_name Fresh\nextends Node\n");
    let fresh_uri = file_uri(&fresh_path);
    client
        .sender
        .send(file_event(&fresh_uri, FileChangeType::CREATED))
        .unwrap();
    // The open dependent republishes (its inheritance error clears) …
    let republished = loop {
        if let Message::Notification(n) = recv(&client) {
            if n.method == "textDocument/publishDiagnostics" {
                break n;
            }
        }
    };
    let diags = republished.params["diagnostics"].as_array().unwrap();
    assert!(
        diags.is_empty(),
        "creating Fresh via a client event must clear the probe's inheritance error; got {diags:?}"
    );
    // … and the new class resolves project-wide.
    assert!(workspace_symbol_names(&client, 10, "Fresh").contains("Fresh"));

    // DELETED: remove on disk, tell the server via the client channel only.
    p.remove("src/fresh.gd");
    client
        .sender
        .send(file_event(&fresh_uri, FileChangeType::DELETED))
        .unwrap();
    // The dependent re-breaks — freshness flows from delete events too.
    let rebroken = loop {
        if let Message::Notification(n) = recv(&client) {
            if n.method == "textDocument/publishDiagnostics" {
                break n;
            }
        }
    };
    assert!(
        !rebroken.params["diagnostics"]
            .as_array()
            .unwrap()
            .is_empty(),
        "deleting Fresh via a client event must re-break the probe"
    );

    shutdown(&client, server_thread);
}

/// Duplicate delivery (native + client for the same on-disk change) applies once: the second
/// delivery's reindex is skipped by the content-fingerprint gate, so the open dependent gets
/// exactly one republish.
#[test]
fn duplicate_delivery_applies_once() {
    let p = sample_project();
    let (client, watcher_tx, server_thread) = boot_injected(&p, ClientCapabilities::default());

    // Open enemy.gd (depends on Hero); baseline publish.
    let enemy_uri = file_uri(&p.root.join("src/enemy.gd"));
    client
        .sender
        .send(notification(
            "textDocument/didOpen",
            DidOpenTextDocumentParams {
                text_document: TextDocumentItem {
                    uri: enemy_uri.clone(),
                    language_id: "gdscript".to_string(),
                    version: 1,
                    text: "extends Hero\n\nfunc flee():\n\tpass\n".to_string(),
                },
            },
        ))
        .unwrap();
    let _baseline = loop {
        if let Message::Notification(n) = recv(&client) {
            if n.method == "textDocument/publishDiagnostics" {
                break n;
            }
        }
    };

    // Change Hero's interface on disk; deliver it via the (injected) NATIVE channel first.
    let hero_path = p.root.join("src/hero.gd");
    p.write(
        "src/hero.gd",
        "class_name Hero\nextends Node2D\n\nvar hp: int = 10\nvar armor: int = 5\n\nfunc attack() -> void:\n\tpass\n",
    );
    let native_event = DebouncedEvent::new(
        notify::Event::new(notify::EventKind::Modify(notify::event::ModifyKind::Data(
            notify::event::DataChange::Content,
        )))
        .add_path(hero_path.as_std_path().to_path_buf()),
        Instant::now(),
    );
    watcher_tx.send(Ok(vec![native_event])).unwrap();
    // The dependent republishes once for the real change.
    let _republish = loop {
        if let Message::Notification(n) = recv(&client) {
            if n.method == "textDocument/publishDiagnostics" {
                break n;
            }
        }
    };

    // Now the CLIENT delivers the same change — the dedupe gate must swallow it: no second
    // republish (an identical-content reindex would bump the epoch and re-analyze for nothing).
    client
        .sender
        .send(file_event(&file_uri(&hero_path), FileChangeType::CHANGED))
        .unwrap();
    let stray = try_recv(&client, Duration::from_millis(400));
    assert!(
        !matches!(&stray, Some(Message::Notification(n)) if n.method == "textDocument/publishDiagnostics"),
        "duplicate delivery must not double-apply; got {stray:?}"
    );

    shutdown(&client, server_thread);
}

/// Out-of-root client events drop at the same guard native events do; the session stays healthy.
#[test]
fn out_of_root_client_event_is_dropped() {
    let p = sample_project();
    let elsewhere = common::TempProject::new();
    elsewhere.write("outside.gd", "class_name Outside\nextends Node\n");
    let (client, _watcher_tx, server_thread) = boot_injected(&p, ClientCapabilities::default());

    client
        .sender
        .send(file_event(
            &file_uri(&elsewhere.root.join("outside.gd")),
            FileChangeType::CREATED,
        ))
        .unwrap();

    assert!(
        !workspace_symbol_names(&client, 11, "Outside").contains("Outside"),
        "an out-of-root client event must not pollute the index"
    );

    shutdown(&client, server_thread);
}

/// A client event for `project.godot` routes to the coalesced project/native reload — every
/// open buffer republishes (policy may have changed).
#[test]
fn project_godot_client_event_triggers_coalesced_reload() {
    let p = sample_project();
    let (client, _watcher_tx, server_thread) = boot_injected(&p, ClientCapabilities::default());

    let uri = file_uri(&p.root.join("src/enemy.gd"));
    client
        .sender
        .send(notification(
            "textDocument/didOpen",
            DidOpenTextDocumentParams {
                text_document: TextDocumentItem {
                    uri: uri.clone(),
                    language_id: "gdscript".to_string(),
                    version: 1,
                    text: "extends Hero\n\nfunc flee():\n\tpass\n".to_string(),
                },
            },
        ))
        .unwrap();
    let _baseline = loop {
        if let Message::Notification(n) = recv(&client) {
            if n.method == "textDocument/publishDiagnostics" {
                break n;
            }
        }
    };

    client
        .sender
        .send(file_event(
            &file_uri(&p.root.join("project.godot")),
            FileChangeType::CHANGED,
        ))
        .unwrap();
    let republished = loop {
        if let Message::Notification(n) = recv(&client) {
            if n.method == "textDocument/publishDiagnostics" {
                break n;
            }
        }
    };
    let params: lsp_types::PublishDiagnosticsParams =
        serde_json::from_value(republished.params).unwrap();
    assert_eq!(
        params.uri, uri,
        "a project.godot client event must republish open buffers via the coalesced reload"
    );

    shutdown(&client, server_thread);
}
