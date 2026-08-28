//! M7 (#61) — pull diagnostics (`textDocument/diagnostic`): byte-identity with push, the
//! `resultId`/`unchanged` round-trip, cross-file (epoch) invalidation, and the advertised
//! capability shape. `workspace/diagnostic` stays method-not-found by design (`docs/09 §5`).

mod common;

use common::{file_uri, notification, recv, recv_response, request, sample_project, shutdown};
use lsp_server::{Connection, Message, RequestId};
use lsp_types::{
    ClientCapabilities, DiagnosticTag, DidChangeTextDocumentParams, DidCloseTextDocumentParams,
    DidOpenTextDocumentParams, InitializeParams, InitializedParams, PublishDiagnosticsParams,
    TextDocumentClientCapabilities, TextDocumentContentChangeEvent, TextDocumentIdentifier,
    TextDocumentItem, Uri, VersionedTextDocumentIdentifier,
};

/// Boot over the sample project advertising the full diagnostics-metadata surface (tags +
/// codeDescription), so the byte-identity test covers the gated fields too.
fn boot() -> (
    common::TempProject,
    Connection,
    std::thread::JoinHandle<anyhow::Result<()>>,
) {
    let p = sample_project();
    let (server, client) = Connection::memory();
    let server_thread = std::thread::spawn(move || gd_server::serve(server));

    let init = InitializeParams {
        capabilities: ClientCapabilities {
            text_document: Some(TextDocumentClientCapabilities {
                publish_diagnostics: Some(lsp_types::PublishDiagnosticsClientCapabilities {
                    tag_support: Some(lsp_types::TagSupport {
                        value_set: vec![DiagnosticTag::UNNECESSARY],
                    }),
                    code_description_support: Some(true),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        },
        initialization_options: Some(serde_json::json!({
            "projectRoot": p.root.as_str(),
            "autoDumpExtensionApi": false,
            "extensionApiPath": p.root.join("extension_api.json").as_str(),
        })),
        ..Default::default()
    };
    client.sender.send(request(1, "initialize", init)).unwrap();
    let resp = recv_response(&client);
    assert!(resp.error.is_none());
    client
        .sender
        .send(notification("initialized", InitializedParams {}))
        .unwrap();
    (p, client, server_thread)
}

fn did_open(client: &Connection, uri: &Uri, text: &str) -> PublishDiagnosticsParams {
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
    recv_publish_for(client, uri)
}

/// Receive notifications until a `publishDiagnostics` for `uri` arrives.
fn recv_publish_for(client: &Connection, uri: &Uri) -> PublishDiagnosticsParams {
    loop {
        if let Message::Notification(note) = recv(client) {
            if note.method == "textDocument/publishDiagnostics" {
                let params: PublishDiagnosticsParams = serde_json::from_value(note.params).unwrap();
                if &params.uri == uri {
                    return params;
                }
            }
        }
    }
}

/// Send `textDocument/diagnostic` and return the report's raw JSON value.
fn pull(client: &Connection, id: i32, uri: &Uri, previous: Option<&str>) -> serde_json::Value {
    client
        .sender
        .send(request(
            id,
            "textDocument/diagnostic",
            serde_json::json!({
                "textDocument": { "uri": uri.as_str() },
                "previousResultId": previous,
            }),
        ))
        .unwrap();
    let resp = loop {
        let resp = recv_response(client);
        if resp.id == RequestId::from(id) {
            break resp;
        }
    };
    assert!(resp.error.is_none(), "pull errored: {:?}", resp.error);
    resp.result.expect("pull returns a report")
}

const UNUSED_SRC: &str = "extends Node\nfunc f():\n\tvar x = 1\n";

/// The acceptance bar: pull and push are the SAME computation — items byte-identical, including
/// the capability-gated tags and codeDescription metadata.
#[test]
fn pull_items_are_byte_identical_to_push() {
    let (p, client, server_thread) = boot();
    let uri = file_uri(&p.root.join("unused.gd"));
    let pushed = did_open(&client, &uri, UNUSED_SRC);
    assert!(
        !pushed.diagnostics.is_empty(),
        "the fixture must produce diagnostics"
    );

    let report = pull(&client, 2, &uri, None);
    assert_eq!(report["kind"], "full");
    let pushed_json = serde_json::to_value(&pushed.diagnostics).unwrap();
    assert_eq!(
        report["items"], pushed_json,
        "pull and push must serialize byte-identically"
    );

    shutdown(&client, server_thread);
}

/// `previousResultId` round-trip: an unchanged file answers `unchanged` with the same id; an
/// edit invalidates the id and the next pull is `full` with a new one.
#[test]
fn result_id_round_trips_and_edits_invalidate() {
    let (p, client, server_thread) = boot();
    let uri = file_uri(&p.root.join("unused.gd"));
    let _ = did_open(&client, &uri, UNUSED_SRC);

    let first = pull(&client, 2, &uri, None);
    assert_eq!(first["kind"], "full");
    let id1 = first["resultId"]
        .as_str()
        .expect("full report carries an id")
        .to_string();

    let unchanged = pull(&client, 3, &uri, Some(&id1));
    assert_eq!(unchanged["kind"], "unchanged");
    assert_eq!(unchanged["resultId"].as_str(), Some(id1.as_str()));

    // Edit → the old id no longer matches; the pull is full with a fresh id.
    client
        .sender
        .send(notification(
            "textDocument/didChange",
            DidChangeTextDocumentParams {
                text_document: VersionedTextDocumentIdentifier {
                    uri: uri.clone(),
                    version: 2,
                },
                content_changes: vec![TextDocumentContentChangeEvent {
                    range: None,
                    range_length: None,
                    text: format!("{UNUSED_SRC}# edited\n"),
                }],
            },
        ))
        .unwrap();
    let _push_v2 = recv_publish_for(&client, &uri);

    let after_edit = pull(&client, 4, &uri, Some(&id1));
    assert_eq!(after_edit["kind"], "full");
    let id2 = after_edit["resultId"].as_str().expect("a fresh id");
    assert_ne!(id2, id1, "an edit must produce a different resultId");

    shutdown(&client, server_thread);
}

/// `interFileDependencies: true` made concrete: editing a DEPENDENCY's interface invalidates the
/// dependent's resultId (the epoch component), so a pull with the stale id returns `full`.
#[test]
fn dependency_interface_edit_invalidates_dependent_result_id() {
    let (p, client, server_thread) = boot();
    let enemy_uri = file_uri(&p.root.join("src/enemy.gd"));
    let hero_uri = file_uri(&p.root.join("src/hero.gd"));

    let _ = did_open(
        &client,
        &enemy_uri,
        "extends Hero\n\nfunc flee():\n\tpass\n",
    );
    let first = pull(&client, 2, &enemy_uri, None);
    let id1 = first["resultId"].as_str().expect("an id").to_string();

    // Change Hero's INTERFACE (a new member) through an open buffer — the reverse-dependency
    // closure bumps enemy.gd's epoch.
    let _ = did_open(
        &client,
        &hero_uri,
        "class_name Hero\nextends Node2D\n\nvar hp: int = 10\n\nfunc attack() -> void:\n\tpass\n",
    );
    client
        .sender
        .send(notification(
            "textDocument/didChange",
            DidChangeTextDocumentParams {
                text_document: VersionedTextDocumentIdentifier {
                    uri: hero_uri.clone(),
                    version: 2,
                },
                content_changes: vec![TextDocumentContentChangeEvent {
                    range: None,
                    range_length: None,
                    text: "class_name Hero\nextends Node2D\n\nvar hp: int = 10\nvar armor: int = 5\n\nfunc attack() -> void:\n\tpass\n".to_string(),
                }],
            },
        ))
        .unwrap();
    let _hero_push = recv_publish_for(&client, &hero_uri);

    let after = pull(&client, 3, &enemy_uri, Some(&id1));
    assert_eq!(
        after["kind"], "full",
        "a dependency interface change must invalidate the dependent's id (epoch component)"
    );
    assert_ne!(after["resultId"].as_str(), Some(id1.as_str()));

    shutdown(&client, server_thread);
}

/// The advertised provider shape, and the deliberate `workspace/diagnostic` skip.
#[test]
fn advertises_diagnostic_provider_and_skips_workspace_diagnostic() {
    let p = sample_project();
    let (server, client) = Connection::memory();
    let server_thread = std::thread::spawn(move || gd_server::serve(server));
    let init = InitializeParams {
        initialization_options: Some(serde_json::json!({
            "projectRoot": p.root.as_str(),
            "autoDumpExtensionApi": false,
            "extensionApiPath": p.root.join("extension_api.json").as_str(),
        })),
        ..Default::default()
    };
    client.sender.send(request(1, "initialize", init)).unwrap();
    let resp = recv_response(&client);
    let caps = &resp.result.expect("initialize result")["capabilities"];
    assert_eq!(
        caps["diagnosticProvider"],
        serde_json::json!({
            "identifier": "gdls",
            "interFileDependencies": true,
            "workspaceDiagnostics": false,
        }),
        "exact advertised shape"
    );
    client
        .sender
        .send(notification("initialized", InitializedParams {}))
        .unwrap();

    client
        .sender
        .send(request(
            2,
            "workspace/diagnostic",
            serde_json::json!({ "previousResultIds": [] }),
        ))
        .unwrap();
    let resp = loop {
        let r = recv_response(&client);
        if r.id == RequestId::from(2) {
            break r;
        }
    };
    assert_eq!(
        resp.error.map(|e| e.code),
        Some(-32601),
        "workspace/diagnostic is a documented skip — method not found"
    );

    shutdown(&client, server_thread);
}

/// After `didClose`, a pull returns an empty full report with no resultId (nothing to pin).
#[test]
fn closed_buffer_pulls_empty_full_report_without_result_id() {
    let (p, client, server_thread) = boot();
    let uri = file_uri(&p.root.join("unused.gd"));
    let _ = did_open(&client, &uri, UNUSED_SRC);
    client
        .sender
        .send(notification(
            "textDocument/didClose",
            DidCloseTextDocumentParams {
                text_document: TextDocumentIdentifier { uri: uri.clone() },
            },
        ))
        .unwrap();
    let _clear_push = recv_publish_for(&client, &uri);

    let report = pull(&client, 5, &uri, None);
    assert_eq!(report["kind"], "full");
    assert_eq!(report["items"], serde_json::json!([]));
    assert!(report["resultId"].is_null(), "no id for a closed buffer");

    shutdown(&client, server_thread);
}

/// The generation component: a wholesale invalidation (project/native reload) that neither the
/// content hash nor the epoch can see must still change the resultId.
#[test]
fn project_reload_bumps_the_generation_component() {
    let p = sample_project();
    let options = gd_server::config::InitializationOptions::parse(Some(&serde_json::json!({
        "projectRoot": p.root.as_str(),
        "autoDumpExtensionApi": false,
        "extensionApiPath": p.root.join("extension_api.json").as_str(),
    })));
    let mut ws = gd_server::Workspace::load(&p.root, &options);
    let before = ws.analysis_generation();
    ws.reload_project_and_native(&options);
    assert!(
        ws.analysis_generation() > before,
        "a project/native reload clears the analysis cache and must advance the generation \
         (the exact increment is irrelevant — only inequality of resultIds matters)"
    );
}

/// #255: a file that names a project class ONLY inside a function body is still a dependent, and
/// editing that class must refresh it on BOTH channels.
///
/// `user.gd` calls `Dep.label()` — `Dep` appears in no `extends`, no member annotation, no
/// parameter type, no `preload`. Flipping `label()`'s return type is an interface change, so:
///   - **push**: `user.gd` gets a re-published `publishDiagnostics` carrying the new error. This
///     used to never arrive — the eager-interface pass built no dependency edge for a body-only
///     use, so the reverse-closure invalidation never reached the file.
///   - **pull**: the server sends `workspace/diagnostic/refresh`. gdls advertises
///     `diagnosticProvider.interFileDependencies: true` but never signalled, so a pull client's
///     cached report for `user.gd` stayed stale for the rest of the session; the client only
///     re-pulls what it is editing.
#[test]
fn body_only_dependency_edit_refreshes_the_dependent_on_both_channels() {
    let p = common::TempProject::new();
    p.write(
        "project.godot",
        "config_version=5\n\n[application]\n\nconfig/name=\"Test\"\n",
    );
    p.write("extension_api.json", common::MINI_API);
    p.write(
        "dep.gd",
        "class_name Dep\nextends Node\n\nstatic func label() -> String:\n\treturn \"x\"\n",
    );
    p.write(
        "user.gd",
        "extends Node\n\nfunc go() -> void:\n\tvar s: String = Dep.label()\n\tprint(s)\n",
    );

    let (server, client) = Connection::memory();
    let server_thread = std::thread::spawn(move || gd_server::serve(server));
    // #277: hand-built JSON, not a serialized `InitializeParams`. LSP 3.17 spells this key
    // `workspace.diagnostics.refreshSupport` (PLURAL) and every captured real client sends it that
    // way; lsp-types 0.97 names the field `workspace.diagnostic`, so a typed round-trip here would
    // exercise a wire key no editor sends and pass while the real one stayed blind.
    let init = serde_json::json!({
        "processId": serde_json::Value::Null,
        "rootUri": serde_json::Value::Null,
        "capabilities": { "workspace": { "diagnostics": { "refreshSupport": true } } },
        "initializationOptions": {
            "projectRoot": p.root.as_str(),
            "autoDumpExtensionApi": false,
            "extensionApiPath": p.root.join("extension_api.json").as_str(),
        },
    });
    client.sender.send(request(1, "initialize", init)).unwrap();
    assert!(recv_response(&client).error.is_none());
    client
        .sender
        .send(notification("initialized", InitializedParams {}))
        .unwrap();

    let dep_uri = file_uri(&p.root.join("dep.gd"));
    let user_uri = file_uri(&p.root.join("user.gd"));
    let clean = did_open(
        &client,
        &user_uri,
        "extends Node\n\nfunc go() -> void:\n\tvar s: String = Dep.label()\n\tprint(s)\n",
    );
    assert!(
        !has_type_error(&clean.diagnostics),
        "the fixture starts clean; got {:?}",
        clean.diagnostics
    );
    did_open(
        &client,
        &dep_uri,
        "class_name Dep\nextends Node\n\nstatic func label() -> String:\n\treturn \"x\"\n",
    );

    // Edit `dep.gd`: `label()` now returns `int`, so `user.gd`'s `var s: String = …` is a type error.
    client
        .sender
        .send(notification(
            "textDocument/didChange",
            DidChangeTextDocumentParams {
                text_document: VersionedTextDocumentIdentifier {
                    uri: dep_uri.clone(),
                    version: 2,
                },
                content_changes: vec![TextDocumentContentChangeEvent {
                    range: None,
                    range_length: None,
                    text:
                        "class_name Dep\nextends Node\n\nstatic func label() -> int:\n\treturn 1\n"
                            .to_string(),
                }],
            },
        ))
        .unwrap();

    // Collect both signals off the same wire, in whatever order they arrive.
    let mut pushed_to_user: Option<PublishDiagnosticsParams> = None;
    let mut saw_refresh = false;
    while pushed_to_user.is_none() || !saw_refresh {
        match recv(&client) {
            Message::Notification(note) if note.method == "textDocument/publishDiagnostics" => {
                let params: PublishDiagnosticsParams = serde_json::from_value(note.params).unwrap();
                if params.uri == user_uri {
                    pushed_to_user = Some(params);
                }
            }
            Message::Request(req) if req.method == "workspace/diagnostic/refresh" => {
                saw_refresh = true;
                client
                    .sender
                    .send(Message::Response(lsp_server::Response::new_ok(
                        req.id,
                        serde_json::Value::Null,
                    )))
                    .unwrap();
            }
            _ => {}
        }
    }

    let pushed = pushed_to_user.expect("loop only exits with a publish");
    assert!(
        has_type_error(&pushed.diagnostics),
        "the body-only dependent must be re-published with the new type error; got {:?}",
        pushed.diagnostics
    );

    // And the pull channel agrees — the same computation, now that the client knows to re-ask.
    let report = pull(&client, 3, &user_uri, None);
    assert_eq!(report["kind"], "full");
    let items: Vec<lsp_types::Diagnostic> =
        serde_json::from_value(report["items"].clone()).unwrap();
    assert!(
        has_type_error(&items),
        "the pull report must carry the same fresh error; got {items:?}"
    );

    shutdown(&client, server_thread);
}

/// The `Cannot assign a value of type int to variable "s" with specified type String.` family —
/// matched loosely so the assertion pins the *presence* of the cross-file type error, not Godot's
/// exact wording (which the fidelity corpus already owns).
fn has_type_error(diags: &[lsp_types::Diagnostic]) -> bool {
    diags
        .iter()
        .any(|d| d.message.contains("int") && d.message.contains("String"))
}
