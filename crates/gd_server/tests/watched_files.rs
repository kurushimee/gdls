//! M7 (#60) — dynamic `didChangeWatchedFiles`: the one-shot `client/registerCapability` (gated
//! on the client's dynamicRegistration), client events keeping the index fresh with the native
//! watcher dead (the Helix scenario — its only watch path), the duplicate-delivery dedupe gate,
//! and the project-reload routing of non-`.gd` client events.

mod common;

use std::time::{Duration, Instant};

use common::{file_uri, notification, recv, recv_response, request, sample_project, try_recv};
use crossbeam_channel::{Receiver, Sender};
use lsp_server::{Connection, Message, Response};
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

/// Boot with gdls's REAL filesystem watcher — the ordinary session shape. `FileWatcher::new`
/// succeeds on a temp directory, so this is the "native watcher armed" path #264 branches on.
fn boot_real(
    p: &common::TempProject,
    capabilities: ClientCapabilities,
) -> (Connection, std::thread::JoinHandle<anyhow::Result<()>>) {
    let (server, client) = Connection::memory();
    let server_thread = std::thread::spawn(move || gd_server::serve(server));
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
    (client, server_thread)
}

/// Pull the registered glob list off the one `client/registerCapability` the server sends, and
/// answer it so the outbound entry is consumed.
fn registered_globs(client: &Connection) -> Vec<String> {
    let req = loop {
        if let Message::Request(req) = recv(client) {
            break req;
        }
    };
    assert_eq!(req.method, "client/registerCapability");
    let registration = &req.params["registrations"][0];
    assert_eq!(registration["id"], "gdls-watched-files");
    assert_eq!(registration["method"], "workspace/didChangeWatchedFiles");
    let globs: Vec<String> = registration["registerOptions"]["watchers"]
        .as_array()
        .expect("watchers array")
        .iter()
        .map(|w| w["globPattern"].as_str().unwrap().to_owned())
        .collect();
    client
        .sender
        .send(Message::Response(Response::new_ok(
            req.id,
            serde_json::Value::Null,
        )))
        .unwrap();
    globs
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
        if r.id == lsp_server::RequestId::from(id) {
            break r;
        }
    };
    assert!(resp.error.is_none());
    serde_json::to_string(&resp.result).unwrap()
}

/// Registration is sent exactly when offered: with `dynamicRegistration: true` the client
/// receives one `client/registerCapability` for the watch globs after `initialized`; without it,
/// nothing arrives. This boot has no native watcher, so the `**/*` asset catch-all is included
/// (#264's fallback path).
#[test]
fn registration_sent_iff_dynamic_registration_offered() {
    let p = sample_project();
    let (client, _watcher_tx, server_thread) = boot_injected(&p, dynamic_registration_caps());

    assert_eq!(
        registered_globs(&client),
        vec![
            "**/*.gd",
            "**/*.tscn",
            "**/project.godot",
            "**/*.gdextension",
            "**/extension_api.json",
            "**/doc_classes/*.xml",
            "**/*",
        ]
    );
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

/// #226 on the fallback path: with NO native watcher, the glob set must ask the client to report
/// ARBITRARY ASSET changes (textures, audio, `.tres`, extension-less files like `LICENSE`). The
/// asset set is defined by EXCLUSION (everything that is not a `.gd` script / `.tscn` scene /
/// engine-managed file), so no positive extension allowlist can express it — only a `**/*`
/// catch-all matches the same file set `AssetIndex::build` indexes. Without it, a client whose
/// only freshness channel is `didChangeWatchedFiles` (the Helix scenario) is never told about a
/// newly-created `icon.png`, so the asset index goes stale for `load`/`preload` completion until a
/// restart. `classify_client_event` re-applies `is_excluded` server-side, so the broad glob does
/// not pollute the index.
#[test]
fn register_watched_files_includes_asset_glob_without_a_native_watcher() {
    let p = sample_project();
    let (client, _watcher_tx, server_thread) = boot_injected(&p, dynamic_registration_caps());

    let globs = registered_globs(&client);
    assert!(
        globs.iter().any(|g| g == "**/*"),
        "with no native watcher the glob set must include the `**/*` catch-all so the client \
         reports arbitrary-asset create/delete; got {globs:?}"
    );
    shutdown(&client, server_thread);
}

/// #264: the other side of the same trade. When gdls armed its OWN filesystem watcher, that
/// watcher already reports asset create/delete — so asking the client to watch the entire
/// workspace buys nothing and costs it a great many inotify handles over `.git/`, `.import/`,
/// `build/` and every exported binary. The catch-all is dropped; the specific globs stay, since
/// the engine-managed files are few and a duplicate delivery costs one fingerprint comparison.
#[test]
fn register_watched_files_omits_the_catch_all_when_the_native_watcher_is_armed() {
    let p = sample_project();
    let (client, server_thread) = boot_real(&p, dynamic_registration_caps());

    assert_eq!(
        registered_globs(&client),
        vec![
            "**/*.gd",
            "**/*.tscn",
            "**/project.godot",
            "**/*.gdextension",
            "**/extension_api.json",
            "**/doc_classes/*.xml",
        ],
        "a session with its own watcher must not ask the client to watch the whole workspace"
    );
    shutdown(&client, server_thread);
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

/// #226 end-to-end funnel (NOT the regression guard — `register_watched_files_includes_asset_glob`
/// owns that): once an asset event reaches the server, a CLIENT-delivered asset CREATE makes the new
/// arbitrary asset live for `load("res://…")` completion and a DELETE drops it — the asset index
/// stays fresh through the `didChangeWatchedFiles` funnel alone (the Helix scenario), the same
/// freshness scripts get in `client_events_alone_keep_the_index_fresh`. The event is injected
/// directly: client-side glob-matching is the client's job, so this proves the processing path while
/// the registration test proves the client is told to watch assets in the first place.
#[test]
fn client_events_alone_keep_the_asset_index_fresh() {
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

    // Open a buffer whose cursor sits inside a `load("res://")` string — completion there lists the
    // project's `res://` entries (scripts + scenes + arbitrary assets).
    let probe_uri = file_uri(&p.root.join("probe.gd"));
    let probe_src = "extends Node\n\nfunc f() -> void:\n\tvar c = load(\"res://\")\n";
    p.write("probe.gd", probe_src);
    client
        .sender
        .send(notification(
            "textDocument/didOpen",
            DidOpenTextDocumentParams {
                text_document: TextDocumentItem {
                    uri: probe_uri.clone(),
                    language_id: "gdscript".to_string(),
                    version: 1,
                    text: probe_src.to_string(),
                },
            },
        ))
        .unwrap();

    // A `load("res://")` completion lists the next path segment. Helper: the set of insert texts.
    let complete_res_root = |id: i32| -> String {
        client
            .sender
            .send(request(
                id,
                "textDocument/completion",
                // `\tvar c = load("res://")` — `res://` occupies cols 15..21; cursor at col 21.
                serde_json::json!({
                    "textDocument": { "uri": probe_uri.as_str() },
                    "position": { "line": 3, "character": 21 },
                }),
            ))
            .unwrap();
        let resp = loop {
            let r = recv_response(&client);
            if r.id == lsp_server::RequestId::from(id) {
                break r;
            }
        };
        assert!(resp.error.is_none(), "completion errored: {:?}", resp.error);
        serde_json::to_string(&resp.result).unwrap()
    };

    // Baseline: the fresh `media/` subdir does not exist yet, so it is not offered.
    assert!(
        !complete_res_root(20).contains("res://media/"),
        "media/ must not be offered before its asset exists"
    );

    // CREATED: write a brand-new arbitrary asset under a previously-absent dir, tell the server via
    // the client channel only (native watcher is dead).
    let asset_path = p.root.join("media/icon.png");
    p.write("media/icon.png", "PNG-PLACEHOLDER");
    let asset_uri = file_uri(&asset_path);
    client
        .sender
        .send(file_event(&asset_uri, FileChangeType::CREATED))
        .unwrap();
    // The new asset's directory is now offered for `res://` completion — the index went live.
    assert!(
        complete_res_root(21).contains("res://media/"),
        "creating an asset via a client event must make its dir live for load() completion"
    );

    // DELETED: remove on disk, tell the server via the client channel only.
    p.remove("media/icon.png");
    client
        .sender
        .send(file_event(&asset_uri, FileChangeType::DELETED))
        .unwrap();
    assert!(
        !complete_res_root(22).contains("res://media/"),
        "deleting the only asset under media/ via a client event must drop it from completion"
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

/// #519: the same freshness, for a class named only inside a FUNCTION BODY. No interface records
/// such a use, so `name_referencers` could not see it, and the reverse-dependency closure could not
/// either — a name that fails to resolve leaves no edge to traverse, so creating the class reached
/// nobody and the consumer's "Identifier not declared" stood forever. `relink_referencers` now also
/// scans each interface's `body_refs`.
///
/// The negative rides along: an unrelated open buffer must not be republished, because a
/// `class_name` edit invalidates the files that name it, not the project.
#[test]
fn creating_a_class_clears_a_body_only_reference_and_leaves_others_alone() {
    let p = sample_project();
    let (client, _watcher_tx, server_thread) = boot_injected(&p, dynamic_registration_caps());
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

    let open = |rel: &str, text: &str| {
        p.write(rel, text);
        let uri = file_uri(&p.root.join(rel));
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
        uri
    };
    // The consumer names `Fresh` only in a body; the other file never names it at all. A cast
    // rather than a construction, so the assertion reads the reference and not `Node`'s abstractness
    // in the trimmed fixture API.
    let user_uri = open(
        "user.gd",
        "extends Node\n\nfunc go(x) -> void:\n\tprint(x as Fresh)\n",
    );
    let other_uri = open(
        "other.gd",
        "extends Node\n\nfunc unrelated() -> void:\n\tprint(1)\n",
    );

    // Drain the two didOpen publishes and check the consumer is broken to begin with.
    let mut user_broken = false;
    for _ in 0..2 {
        loop {
            if let Message::Notification(n) = recv(&client) {
                if n.method == "textDocument/publishDiagnostics" {
                    if n.params["uri"] == serde_json::json!(user_uri.as_str()) {
                        user_broken = serde_json::to_string(&n.params).unwrap().contains("Fresh");
                    }
                    break;
                }
            }
        }
    }
    assert!(
        user_broken,
        "`Fresh` does not exist yet, so `user.gd` is broken"
    );

    // The class appears.
    p.write("src/fresh.gd", "class_name Fresh\nextends Node\n");
    client
        .sender
        .send(file_event(
            &file_uri(&p.root.join("src/fresh.gd")),
            FileChangeType::CREATED,
        ))
        .unwrap();

    // Collect every publish that arrives, until the consumer's clean one shows up.
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut republished_uris: Vec<String> = Vec::new();
    let mut user_clean = false;
    while Instant::now() < deadline && !user_clean {
        if let Some(Message::Notification(n)) = try_recv(&client, Duration::from_millis(200)) {
            if n.method != "textDocument/publishDiagnostics" {
                continue;
            }
            let uri = n.params["uri"].as_str().unwrap_or_default().to_string();
            republished_uris.push(uri.clone());
            if uri == user_uri.as_str() {
                user_clean = n.params["diagnostics"].as_array().unwrap().is_empty();
            }
        }
    }
    assert!(
        user_clean,
        "creating `Fresh` must clear the body-only reference; publishes were {republished_uris:?}"
    );
    assert!(
        !republished_uris.iter().any(|u| u == other_uri.as_str()),
        "a file that never names `Fresh` must not be republished; got {republished_uris:?}"
    );

    shutdown(&client, server_thread);
}
