//! M7 §7.4 — the editor capability-profile walk: boot the in-memory server with each VENDORED
//! real-client `ClientCapabilities` JSON (`tests/fixtures/client_caps/*.json`) and assert every
//! M7 gated projection against what that profile actually advertises. Dropping a new capture
//! into the fixtures directory extends the walk automatically — the assertions derive from the
//! profile's own flags, so this file never hard-codes per-editor expectations.
//!
//! Per profile, the walk checks:
//! - server-initiated progress (`window/workDoneProgress/create`) appears iff
//!   `window.workDoneProgress`;
//! - the dynamic watched-files registration appears iff
//!   `workspace.didChangeWatchedFiles.dynamicRegistration`;
//! - diagnostics tags appear iff `tagSupport` lists `Unnecessary`; `codeDescription` iff
//!   `codeDescriptionSupport`;
//! - hover's `MarkupKind` follows the `hover.contentFormat` preference order (markdown default);
//! - `documentSymbol` returns the hierarchical shape iff `hierarchicalDocumentSymbolSupport`;
//! - the pull-diagnostics round-trip (full → unchanged) serves every client;
//! - `workspace/didChangeConfiguration` triggers a `workspace/configuration` pull iff
//!   `workspace.configuration`.
//!
//! Every milestone from M8 on extends this list with its own gated projections.

mod common;

use std::time::Duration;

use common::{file_uri, notification, recv, request, sample_project, try_recv};
use lsp_server::{Connection, Message, RequestId, Response};
use lsp_types::{ClientCapabilities, InitializeParams, InitializedParams, Uri};

const DOCUMENTED_SRC: &str = "\
class_name Probe
extends Node

## Probe speed in [b]pixels[/b].
var speed := 1.0

func f():
\tvar unused = 1
";

fn profiles() -> Vec<(String, serde_json::Value)> {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/client_caps");
    let mut out = Vec::new();
    for entry in std::fs::read_dir(&dir).expect("fixtures dir exists") {
        let path = entry.expect("dir entry").path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let name = path.file_stem().unwrap().to_string_lossy().to_string();
        let raw = std::fs::read_to_string(&path).expect("profile readable");
        out.push((name, serde_json::from_str(&raw).expect("profile parses")));
    }
    assert!(!out.is_empty(), "at least one vendored profile");
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

/// JSON-path probe into the raw profile (the source of truth for what to expect).
fn flag(profile: &serde_json::Value, path: &[&str]) -> bool {
    let mut cur = profile;
    for key in path {
        match cur.get(key) {
            Some(v) => cur = v,
            None => return false,
        }
    }
    cur.as_bool().unwrap_or(false)
}

#[test]
fn every_vendored_profile_gets_its_exact_gated_projections() {
    for (name, profile) in profiles() {
        check_profile(&name, &profile);
    }
}

fn check_profile(name: &str, profile: &serde_json::Value) {
    let capabilities: ClientCapabilities = serde_json::from_value(profile.clone())
        .unwrap_or_else(|e| panic!("{name}: profile must deserialize as ClientCapabilities: {e}"));

    let expect_progress = flag(profile, &["window", "workDoneProgress"]);
    let expect_registration = flag(
        profile,
        &["workspace", "didChangeWatchedFiles", "dynamicRegistration"],
    );
    let expect_config_pull = flag(profile, &["workspace", "configuration"]);
    let expect_hierarchical = flag(
        profile,
        &[
            "textDocument",
            "documentSymbol",
            "hierarchicalDocumentSymbolSupport",
        ],
    );
    let expect_code_description = flag(
        profile,
        &[
            "textDocument",
            "publishDiagnostics",
            "codeDescriptionSupport",
        ],
    );
    let expect_tags = profile["textDocument"]["publishDiagnostics"]["tagSupport"]["valueSet"]
        .as_array()
        .is_some_and(|set| set.iter().any(|v| v.as_i64() == Some(1)));
    let expected_hover_kind = profile["textDocument"]["hover"]["contentFormat"][0]
        .as_str()
        .unwrap_or("markdown")
        .to_string();

    let p = sample_project();
    p.write("src/probe.gd", DOCUMENTED_SRC);
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
            assert!(resp.error.is_none(), "{name}: initialize failed");
            break;
        }
    }
    client
        .sender
        .send(notification("initialized", InitializedParams {}))
        .unwrap();

    // Startup window: collect the server-initiated requests (progress create / registration),
    // answering each, until the stream goes quiet.
    let mut saw_create = false;
    let mut saw_registration = false;
    while let Some(msg) = try_recv(&client, Duration::from_millis(500)) {
        if let Message::Request(req) = msg {
            match req.method.as_str() {
                "window/workDoneProgress/create" => saw_create = true,
                "client/registerCapability" => saw_registration = true,
                other => panic!("{name}: unexpected server request {other}"),
            }
            client
                .sender
                .send(Message::Response(Response::new_ok(
                    req.id,
                    serde_json::Value::Null,
                )))
                .unwrap();
        }
    }
    assert_eq!(
        saw_create, expect_progress,
        "{name}: workDoneProgress/create iff window.workDoneProgress"
    );
    assert_eq!(
        saw_registration, expect_registration,
        "{name}: registration iff didChangeWatchedFiles.dynamicRegistration"
    );

    // Diagnostics metadata gates.
    let uri = file_uri(&p.root.join("src/probe.gd"));
    client
        .sender
        .send(notification(
            "textDocument/didOpen",
            lsp_types::DidOpenTextDocumentParams {
                text_document: lsp_types::TextDocumentItem {
                    uri: uri.clone(),
                    language_id: "gdscript".to_string(),
                    version: 1,
                    text: DOCUMENTED_SRC.to_string(),
                },
            },
        ))
        .unwrap();
    let publish = loop {
        if let Message::Notification(n) = recv(&client) {
            if n.method == "textDocument/publishDiagnostics" {
                break n.params;
            }
        }
    };
    let unused = publish["diagnostics"]
        .as_array()
        .unwrap()
        .iter()
        .find(|d| d["code"] == "UNUSED_VARIABLE")
        .unwrap_or_else(|| panic!("{name}: UNUSED_VARIABLE fires"))
        .clone();
    assert_eq!(
        !unused["tags"].is_null(),
        expect_tags,
        "{name}: tags iff tagSupport(Unnecessary)"
    );
    assert_eq!(
        !unused["codeDescription"].is_null(),
        expect_code_description,
        "{name}: codeDescription iff codeDescriptionSupport"
    );

    // Hover format follows contentFormat[0] (markdown default).
    let (hover_kind, hover_value) = request_hover(name, &client, 10, &uri);
    assert_eq!(hover_kind, expected_hover_kind, "{name}: hover MarkupKind");
    assert!(
        !hover_value.contains("[b]"),
        "{name}: no raw BBCode on the wire"
    );

    // documentSymbol shape gate.
    client
        .sender
        .send(request(
            11,
            "textDocument/documentSymbol",
            serde_json::json!({ "textDocument": { "uri": uri.as_str() } }),
        ))
        .unwrap();
    let symbols = response_result(name, &client, 11);
    let first = &symbols
        .as_array()
        .unwrap_or_else(|| panic!("{name}: documentSymbol returns an array"))[0];
    assert_eq!(
        first.get("range").is_some(),
        expect_hierarchical,
        "{name}: hierarchical DocumentSymbol iff supported (flat SymbolInformation otherwise)"
    );

    // Pull diagnostics serve every client; the resultId round-trip answers `unchanged`.
    client
        .sender
        .send(request(
            12,
            "textDocument/diagnostic",
            serde_json::json!({ "textDocument": { "uri": uri.as_str() } }),
        ))
        .unwrap();
    let full = response_result(name, &client, 12);
    assert_eq!(full["kind"], "full", "{name}: first pull is full");
    let result_id = full["resultId"]
        .as_str()
        .unwrap_or_else(|| panic!("{name}: full report carries a resultId"));
    client
        .sender
        .send(request(
            13,
            "textDocument/diagnostic",
            serde_json::json!({
                "textDocument": { "uri": uri.as_str() },
                "previousResultId": result_id,
            }),
        ))
        .unwrap();
    let unchanged = response_result(name, &client, 13);
    assert_eq!(
        unchanged["kind"], "unchanged",
        "{name}: resultId round-trip"
    );

    // Runtime config: the pull path fires iff workspace.configuration.
    client
        .sender
        .send(notification(
            "workspace/didChangeConfiguration",
            serde_json::json!({ "settings": null }),
        ))
        .unwrap();
    let mut saw_config_pull = false;
    while let Some(msg) = try_recv(&client, Duration::from_millis(400)) {
        if let Message::Request(req) = msg {
            if req.method == "workspace/configuration" {
                saw_config_pull = true;
                client
                    .sender
                    .send(Message::Response(Response::new_ok(
                        req.id,
                        serde_json::json!([null]),
                    )))
                    .unwrap();
            }
        }
    }
    assert_eq!(
        saw_config_pull, expect_config_pull,
        "{name}: workspace/configuration pull iff advertised"
    );

    common::shutdown(&client, server_thread);
}

fn request_hover(name: &str, client: &Connection, id: i32, uri: &Uri) -> (String, String) {
    client
        .sender
        .send(request(
            id,
            "textDocument/hover",
            serde_json::json!({
                "textDocument": { "uri": uri.as_str() },
                // `speed` in `var speed := 1.0` (line 4, 0-based).
                "position": { "line": 4, "character": 5 },
            }),
        ))
        .unwrap();
    let result = response_result(name, client, id);
    assert!(!result.is_null(), "{name}: hover returns content");
    (
        result["contents"]["kind"]
            .as_str()
            .unwrap_or("")
            .to_string(),
        result["contents"]["value"]
            .as_str()
            .unwrap_or("")
            .to_string(),
    )
}

fn response_result(name: &str, client: &Connection, id: i32) -> serde_json::Value {
    loop {
        if let Message::Response(resp) = recv(client) {
            if resp.id == RequestId::from(id) {
                assert!(resp.error.is_none(), "{name}: request {id} errored");
                return resp.result.unwrap_or(serde_json::Value::Null);
            }
        }
    }
}
