//! M7 (#59) — runtime re-configuration: the `workspace/configuration` pull path, the
//! notification-payload path (sectioned and bare), the malformed-keeps-previous contract, and
//! the session-structural field guard.

mod common;

use std::time::Duration;

use common::{file_uri, notification, recv, request, sample_project, shutdown, try_recv};
use lsp_server::{Connection, Message, Response};
use lsp_types::{
    ClientCapabilities, DidOpenTextDocumentParams, InitializeParams, InitializedParams,
    PublishDiagnosticsParams, TextDocumentItem, Uri, WorkspaceClientCapabilities,
};

const UNUSED_SRC: &str = "extends Node\nfunc f():\n\tvar x = 1\n";

fn boot(
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

fn workspace_configuration_caps() -> ClientCapabilities {
    ClientCapabilities {
        workspace: Some(WorkspaceClientCapabilities {
            configuration: Some(true),
            ..Default::default()
        }),
        ..Default::default()
    }
}

/// Open a buffer with an UNUSED_VARIABLE warning and return its first publish.
fn open_unused(client: &Connection, uri: &Uri) -> PublishDiagnosticsParams {
    client
        .sender
        .send(notification(
            "textDocument/didOpen",
            DidOpenTextDocumentParams {
                text_document: TextDocumentItem {
                    uri: uri.clone(),
                    language_id: "gdscript".to_string(),
                    version: 1,
                    text: UNUSED_SRC.to_string(),
                },
            },
        ))
        .unwrap();
    recv_publish_for(client, uri)
}

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

fn has_unused(params: &PublishDiagnosticsParams) -> bool {
    params.diagnostics.iter().any(|d| {
        d.code
            == Some(lsp_types::NumberOrString::String(
                "UNUSED_VARIABLE".to_string(),
            ))
    })
}

const DISABLE_UNUSED: &str = r#"{ "strict": { "disableWarnings": ["UNUSED_VARIABLE"] } }"#;

/// The pull path: with `workspace.configuration` advertised, a `didChangeConfiguration` (even
/// with a useless payload — the convention) triggers a `workspace/configuration` request for
/// the `"gdls"` section, whose reply re-configures the live session: open buffers republish
/// under the new policy without a restart.
#[test]
fn configuration_pull_reconfigures_and_republishes() {
    let p = sample_project();
    let (client, server_thread) = boot(&p, workspace_configuration_caps());
    let uri = file_uri(&p.root.join("unused.gd"));
    assert!(has_unused(&open_unused(&client, &uri)));

    client
        .sender
        .send(notification(
            "workspace/didChangeConfiguration",
            serde_json::json!({ "settings": null }),
        ))
        .unwrap();

    // The server pulls the "gdls" section …
    let pull_req = loop {
        if let Message::Request(req) = recv(&client) {
            break req;
        }
    };
    assert_eq!(pull_req.method, "workspace/configuration");
    assert_eq!(
        pull_req.params["items"],
        serde_json::json!([{ "section": "gdls" }])
    );
    // … and applies the reply (one settings value per requested item).
    let section: serde_json::Value = serde_json::from_str(DISABLE_UNUSED).unwrap();
    client
        .sender
        .send(Message::Response(Response::new_ok(
            pull_req.id,
            serde_json::json!([section]),
        )))
        .unwrap();

    let republished = recv_publish_for(&client, &uri);
    assert!(
        !has_unused(&republished),
        "the runtime disableWarnings override must drop UNUSED_VARIABLE; got {:?}",
        republished.diagnostics
    );

    shutdown(&client, server_thread);
}

/// The payload path (no `workspace.configuration` capability): both the sectioned
/// `settings.gdls` shape and the bare settings object apply directly.
#[test]
fn notification_payload_applies_in_both_shapes() {
    for sectioned in [true, false] {
        let p = sample_project();
        let (client, server_thread) = boot(&p, ClientCapabilities::default());
        let uri = file_uri(&p.root.join("unused.gd"));
        assert!(has_unused(&open_unused(&client, &uri)));

        let section: serde_json::Value = serde_json::from_str(DISABLE_UNUSED).unwrap();
        let settings = if sectioned {
            serde_json::json!({ "gdls": section })
        } else {
            section
        };
        client
            .sender
            .send(notification(
                "workspace/didChangeConfiguration",
                serde_json::json!({ "settings": settings }),
            ))
            .unwrap();

        let republished = recv_publish_for(&client, &uri);
        assert!(
            !has_unused(&republished),
            "sectioned={sectioned}: the override must drop UNUSED_VARIABLE"
        );
        shutdown(&client, server_thread);
    }
}

/// Malformed runtime config keeps the PREVIOUS configuration: a `window/showMessage` warning is
/// surfaced, no republish happens, and the old policy still applies afterwards.
#[test]
fn malformed_runtime_config_keeps_previous_and_warns() {
    let p = sample_project();
    let (client, server_thread) = boot(&p, ClientCapabilities::default());
    let uri = file_uri(&p.root.join("unused.gd"));
    assert!(has_unused(&open_unused(&client, &uri)));

    client
        .sender
        .send(notification(
            "workspace/didChangeConfiguration",
            serde_json::json!({ "settings": { "gdls": { "strict": 42 } } }),
        ))
        .unwrap();

    // A showMessage warning arrives; NO republish does.
    let mut saw_show_message = false;
    while let Some(msg) = try_recv(&client, Duration::from_millis(300)) {
        match msg {
            Message::Notification(n) if n.method == "window/showMessage" => {
                saw_show_message = true;
            }
            Message::Notification(n) if n.method == "textDocument/publishDiagnostics" => {
                panic!("malformed config must not republish");
            }
            _ => {}
        }
    }
    assert!(
        saw_show_message,
        "malformed runtime config surfaces a window/showMessage warning"
    );

    // The previous policy still applies: a fresh buffer still warns.
    let uri2 = file_uri(&p.root.join("unused2.gd"));
    assert!(
        has_unused(&open_unused(&client, &uri2)),
        "the previous configuration must remain in force"
    );

    shutdown(&client, server_thread);
}

/// Session-structural fields are retained: a runtime `projectRoot` change is ignored (with a
/// logged warning) and the session keeps serving against the original root.
#[test]
fn structural_fields_are_ignored_at_runtime() {
    let p = sample_project();
    let (client, server_thread) = boot(&p, ClientCapabilities::default());
    let uri = file_uri(&p.root.join("unused.gd"));
    assert!(has_unused(&open_unused(&client, &uri)));

    client
        .sender
        .send(notification(
            "workspace/didChangeConfiguration",
            serde_json::json!({ "settings": { "gdls": { "projectRoot": "/nowhere" } } }),
        ))
        .unwrap();

    // No behavioral change: workspace/symbol still resolves Hero from the ORIGINAL root.
    client
        .sender
        .send(request(
            7,
            "workspace/symbol",
            serde_json::json!({ "query": "Hero" }),
        ))
        .unwrap();
    let resp = loop {
        if let Message::Response(r) = recv(&client) {
            break r;
        }
    };
    assert!(resp.error.is_none());
    let rendered = serde_json::to_string(&resp.result).unwrap();
    assert!(
        rendered.contains("Hero"),
        "the original project root must remain in force; got {rendered}"
    );

    shutdown(&client, server_thread);
}

/// A sparse payload only applies the groups it carries: a section with only `strict` must not
/// reset non-default `analyzer`/`memory` startup knobs to their defaults (group-level presence
/// gating). Observable here as: the sparse strict-only payload applies (warning disappears)
/// while a follow-up disabling the override restores it — and no spurious analyzer/memory
/// invalidation breaks the round-trip.
#[test]
fn sparse_payload_keeps_absent_groups() {
    let p = sample_project();
    let (client, server_thread) = boot(&p, ClientCapabilities::default());
    let uri = file_uri(&p.root.join("unused.gd"));
    assert!(has_unused(&open_unused(&client, &uri)));

    // Sparse: only `strict` provided — analyzer/memory keep their session values.
    client
        .sender
        .send(notification(
            "workspace/didChangeConfiguration",
            serde_json::json!({ "settings": { "gdls": { "strict": { "disableWarnings": ["UNUSED_VARIABLE"] } } } }),
        ))
        .unwrap();
    assert!(!has_unused(&recv_publish_for(&client, &uri)));

    // Round-trip back: a sparse payload restoring the default strict config re-enables it.
    client
        .sender
        .send(notification(
            "workspace/didChangeConfiguration",
            serde_json::json!({ "settings": { "gdls": { "strict": {} } } }),
        ))
        .unwrap();
    assert!(
        has_unused(&recv_publish_for(&client, &uri)),
        "providing the strict group as its default snapshot restores the default policy"
    );

    shutdown(&client, server_thread);
}

/// A no-op payload (identical config) causes no republish — no cache churn for nothing.
#[test]
fn unchanged_config_causes_no_republish() {
    let p = sample_project();
    let (client, server_thread) = boot(&p, ClientCapabilities::default());
    let uri = file_uri(&p.root.join("unused.gd"));
    assert!(has_unused(&open_unused(&client, &uri)));

    client
        .sender
        .send(notification(
            "workspace/didChangeConfiguration",
            serde_json::json!({ "settings": { "gdls": {} } }),
        ))
        .unwrap();

    let stray = try_recv(&client, Duration::from_millis(300));
    assert!(
        !matches!(&stray, Some(Message::Notification(n)) if n.method == "textDocument/publishDiagnostics"),
        "an unchanged configuration must not republish; got {stray:?}"
    );

    shutdown(&client, server_thread);
}
