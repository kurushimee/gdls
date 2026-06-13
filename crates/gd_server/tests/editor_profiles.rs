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
//! M8 (#64) extends the walk with the **`textDocument/completion`** gated projections, also derived
//! per profile from its own `textDocument.completion` flags (so a new capture extends the walk
//! automatically):
//! - `completionItem.snippetSupport` → a callable inserts a `($0)` snippet vs a bare name;
//! - `completionItem.insertReplaceSupport` → an `InsertReplaceEdit` vs a plain `TextEdit`;
//! - `completionItem.commitCharactersSupport` → items carry commit characters vs none;
//! - `completionItem.documentationFormat` → `completionItem/resolve` renders Markdown vs PlainText
//!   docs (absent ⇒ the conservative PlainText downgrade — NOT hover's Markdown default);
//! - `completionItemKind.valueSet` → a server kind outside the negotiated set (here a signal's
//!   `EVENT` = 23, outside the LSP-default 1..=18) is clamped to `None` rather than sent as a number.
//!
//! `textDocument/signatureHelp` (M8 #65) is **deliberately not driven here**: that handler and its
//! capability live on the stacked `feat/m8-signaturehelp` branch, not this completion branch, so it
//! is unregistered and would return method-not-found — its six-profile walk extends this file on
//! that branch (the stacked geometry means these completion additions are already present there).
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

/// A self-contained file for the M8 completion-projection walk, kept SEPARATE from
/// [`DOCUMENTED_SRC`] so its members never shift the hover/`UNUSED_VARIABLE` positions the M7
/// assertions pin. Single-file (the Phase-3 resolve doc lookup needs the member's declaring file to
/// equal the requesting file — see `tests/completion.rs::resolve_fills_docs_…`), with one of every
/// gate-relevant member: a `##`-documented **property** (`hp`), a **signal** (`hit` → `EVENT` = 23,
/// outside the LSP-default kind set, the cross-profile clamp discriminator), and a **method**
/// (`attack` → callable, exercises the snippet gate). The trailing `c.` is the member-access site.
const COMPLETION_PROBE_SRC: &str = "\
class_name Consumer
extends Node

## Hit points in [b]units[/b].
var hp: int = 10

signal hit

func attack() -> void:
\tpass

func use(c: Consumer) -> void:
\tc.
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
    // The M8 completion-projection probe, on disk before boot so it is in the eager-interface index.
    p.write("src/consumer.gd", COMPLETION_PROBE_SRC);
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

    // M8 (#64): the completion gated-projection walk against the self-contained probe file.
    check_completion_projection(name, profile, &p, &client);

    common::shutdown(&client, server_thread);
}

/// Drive `textDocument/completion` (+ a `completionItem/resolve` round-trip) for one profile and
/// assert every M8 completion gate's projection, derived from the profile's OWN
/// `textDocument.completion` JSON flags — so this never hard-codes per-editor expectations. The
/// server is the same booted session as [`check_profile`]; only the probe file is new.
fn check_completion_projection(
    name: &str,
    profile: &serde_json::Value,
    p: &common::TempProject,
    client: &Connection,
) {
    // What the profile advertises (the source of truth for what to expect), via the same `flag()` /
    // raw-JSON probes the rest of this walk uses.
    let want_snippet = flag(
        profile,
        &[
            "textDocument",
            "completion",
            "completionItem",
            "snippetSupport",
        ],
    );
    let want_insert_replace = flag(
        profile,
        &[
            "textDocument",
            "completion",
            "completionItem",
            "insertReplaceSupport",
        ],
    );
    let want_commit = flag(
        profile,
        &[
            "textDocument",
            "completion",
            "completionItem",
            "commitCharactersSupport",
        ],
    );
    // documentationFormat: first of {markdown, plaintext}; ABSENT ⇒ the conservative PlainText
    // downgrade (NOT hover's Markdown default — `CompletionCaps::negotiate`).
    let doc_formats = profile["textDocument"]["completion"]["completionItem"]
        ["documentationFormat"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect::<Vec<_>>()
        });
    let want_markdown_docs = doc_formats
        .as_ref()
        .and_then(|fmts| fmts.iter().find(|f| *f == "markdown" || *f == "plaintext"))
        .map(|first| first == "markdown")
        .unwrap_or(false);
    // completionItemKind.valueSet: the kinds the client can render. Absent ⇒ the LSP-default set
    // (1..=18). EVENT (a signal's kind) is 23 — present iff the client enumerated a set reaching it.
    let kind_set: Option<Vec<i64>> = profile["textDocument"]["completion"]["completionItemKind"]
        ["valueSet"]
        .as_array()
        .map(|a| a.iter().filter_map(serde_json::Value::as_i64).collect());
    let event_supported = match &kind_set {
        Some(set) => set.contains(&23), // CompletionItemKind::EVENT
        None => false,                  // default 1..=18 excludes EVENT
    };

    // Drive completion at the `c.` member-access site (line 12, after the `.` ⇒ column 3).
    let probe_uri = file_uri(&p.root.join("src/consumer.gd"));
    client
        .sender
        .send(notification(
            "textDocument/didOpen",
            lsp_types::DidOpenTextDocumentParams {
                text_document: lsp_types::TextDocumentItem {
                    uri: probe_uri.clone(),
                    language_id: "gdscript".to_string(),
                    version: 1,
                    text: COMPLETION_PROBE_SRC.to_string(),
                },
            },
        ))
        .unwrap();
    client
        .sender
        .send(request(
            20,
            "textDocument/completion",
            serde_json::json!({
                "textDocument": { "uri": probe_uri.as_str() },
                "position": { "line": 12, "character": 3 },
            }),
        ))
        .unwrap();
    let raw = response_result(name, client, 20);
    // Anti-catalog W18: a completion is a `CompletionList` object with `items`, never a bare array.
    let items = raw["items"]
        .as_array()
        .unwrap_or_else(|| panic!("{name}: completion is a CompletionList with items, got {raw}"))
        .clone();
    let find = |label: &str| -> serde_json::Value {
        items
            .iter()
            .find(|i| i["label"] == label)
            .unwrap_or_else(|| panic!("{name}: member `{label}` offered; items={items:?}"))
            .clone()
    };
    let attack = find("attack");
    let hp = find("hp");
    let hit = find("hit");

    // (1) snippetSupport: a callable inserts a `($0)` snippet (insertTextFormat == Snippet == 2)
    // iff the client opted in, else a bare name and no insertTextFormat.
    let attack_format = attack["insertTextFormat"].as_i64();
    let attack_new_text = attack["textEdit"]["newText"]
        .as_str()
        .or_else(|| attack["textEdit"]["replace"].as_str())
        .or_else(|| attack["insertText"].as_str())
        .unwrap_or("");
    if want_snippet {
        assert_eq!(
            attack_format,
            Some(2),
            "{name}: snippetSupport ⇒ a callable's insertTextFormat is Snippet(2)"
        );
        // The newText lives under whichever edit arm the insertReplace gate selected.
        let nt = attack["textEdit"]["newText"]
            .as_str()
            .unwrap_or(attack_new_text);
        assert!(
            nt.contains("$0"),
            "{name}: snippet newText carries the $0 tab-stop: {nt:?}"
        );
    } else {
        assert_eq!(
            attack_format, None,
            "{name}: no snippetSupport ⇒ insertTextFormat absent (plain text)"
        );
        let nt = attack["textEdit"]["newText"].as_str().unwrap_or("");
        assert!(
            !nt.contains("$0"),
            "{name}: no snippetSupport ⇒ no $0 in newText: {nt:?}"
        );
    }

    // (2) insertReplaceSupport: the textEdit is an InsertReplaceEdit (has `insert` + `replace`) iff
    // advertised, else a plain TextEdit (has `range` + `newText`).
    let is_insert_replace =
        attack["textEdit"].get("insert").is_some() && attack["textEdit"].get("replace").is_some();
    assert_eq!(
        is_insert_replace, want_insert_replace,
        "{name}: insertReplaceSupport ⇒ InsertReplaceEdit, else a plain TextEdit"
    );

    // (3) commitCharactersSupport: items carry commitCharacters iff advertised.
    let any_commit = items.iter().any(|i| !i["commitCharacters"].is_null());
    assert_eq!(
        any_commit, want_commit,
        "{name}: commitCharacters present on items iff commitCharactersSupport"
    );

    // (4) completionItemKind clamp: the signal `hit` is EVENT (23). Outside the default 1..=18 set
    // it is dropped to `None` (kind absent); a client enumerating a set that reaches 23 keeps it.
    // The method `attack` is METHOD (2) — always inside any reasonable set — so its kind survives.
    assert!(
        !attack["kind"].is_null(),
        "{name}: METHOD (2) is inside every kind set, so attack keeps its kind"
    );
    assert_eq!(
        !hit["kind"].is_null(),
        event_supported,
        "{name}: signal EVENT(23) kept iff the negotiated valueSet reaches it, else clamped to None"
    );

    // (5) documentationFormat: resolve the documented property `hp`; its documentation MarkupKind
    // follows the gate (Markdown renders `[b]…[/b]` as `**…**`; PlainText strips the BBCode).
    client
        .sender
        .send(request(21, "completionItem/resolve", &hp))
        .unwrap();
    let resolved = response_result(name, client, 21);
    let doc_kind = resolved["documentation"]["kind"].as_str().unwrap_or("");
    let doc_value = resolved["documentation"]["value"].as_str().unwrap_or("");
    if want_markdown_docs {
        assert_eq!(
            doc_kind, "markdown",
            "{name}: markdown-preferring client ⇒ markdown docs"
        );
        assert!(
            doc_value.contains("**units**"),
            "{name}: BBCode [b] renders as markdown emphasis: {doc_value:?}"
        );
    } else {
        assert_eq!(
            doc_kind, "plaintext",
            "{name}: no/plaintext documentationFormat ⇒ plaintext docs"
        );
        assert!(
            doc_value.contains("units") && !doc_value.contains("**"),
            "{name}: BBCode stripped for plaintext: {doc_value:?}"
        );
    }
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
