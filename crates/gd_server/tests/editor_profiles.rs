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
//! M10 (#72–#75) extends the walk with the **presentation + code-action** gated projections, again
//! derived per profile from its own flags (so a new capture extends the walk automatically):
//! - **`semanticTokens`** (#72): the per-client legend remap — gdls's fixed 10-type STANDARD legend
//!   is intersected with the profile's advertised `tokenTypes`, so a `method` token (a class-member
//!   `func` like `paint` is a METHOD, since the script IS its root class) is emitted at the wire index
//!   `"method"` occupies in THAT profile's legend (helix advertises none ⇒ gdls's own index 4;
//!   neovim/zed remap it to 13). The `full → full/delta` edit-shape round-trip is driven once (delta
//!   is NOT a client-gated projection — `SemanticTokensCaps` captures only the legend + refresh — so
//!   it is asserted as an endpoint, not per-profile).
//! - **`inlayHint`** (#73): the `resolveSupport` gate — a `var x := …` type hint ships its tooltip
//!   EMBEDDED for a client without `resolveSupport` (helix) and DEFERRED (tooltip absent, `data`
//!   present, filled by an `inlayHint/resolve` round-trip) for a client with it (neovim/zed).
//! - **`documentColor`** (#74): the `Color(…)` literal is reported for EVERY profile (a bare
//!   `Simple(true)` provider with no client-capability path — the generic-LSP floor).
//! - **`codeAction`** (#75): all three vendored profiles advertise `codeActionLiteralSupport` +
//!   `resolveSupport`, so the walk asserts each profile's actual (rich) projection — a `CodeAction`
//!   literal with a DEFERRED edit + the additive `Diagnostic.data` tag gated on
//!   `publishDiagnostics.dataSupport` (helix lacks it), and the `source.fixAll` family separation.
//!   The DEGRADED paths (the `Command[]` fallback for a no-`codeActionLiteralSupport` client and the
//!   eager-edit path for a no-`resolveSupport` client) have no vendored profile to exercise them, so
//!   they are covered by the synthetic-capability unit tests in `tests/code_action.rs`
//!   (`command_fallback_triggers_correlated_apply_edit_*`,
//!   `code_action_computes_edit_eagerly_without_resolve_support`) — the vendored-real-client contract
//!   of THIS file forbids a synthetic minimal fixture.
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
    // The M9 projection probe (rename / foldingRange / workspaceSymbol gates), likewise on disk
    // before boot so its `class_name` is in the index for the workspace/symbol query.
    p.write("src/m9probe.gd", M9_PROBE_SRC);
    // The M10 projection probe (semanticTokens / inlayHint / documentColor / codeAction gates),
    // likewise on disk before boot so it is in the eager-interface index.
    p.write("src/m10probe.gd", M10_PROBE_SRC);
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

    // M9 (#66/#70/#71): the rename / foldingRange / workspaceSymbol gated-projection walk.
    check_m9_projection(name, profile, &p, &client);

    // M10 (#72/#73/#74/#75): the semanticTokens / inlayHint / documentColor / codeAction walk.
    check_m10_projection(name, profile, &p, &client);

    common::shutdown(&client, server_thread);
}

/// The M9 probe — a `class_name` (for the workspace/symbol query), a foldable function body, and a
/// function-local (`counter`) to rename.
const M9_PROBE_SRC: &str = "\
class_name M9ProbeClass
extends Node

func fold_region() -> void:
\tvar counter := 0
\tcounter += 1
\tprint(counter)
";

/// Drive the gated M9 capabilities for one profile and assert each projection from the profile's OWN
/// flags (never hard-coding per-editor expectations): rename `WorkspaceEdit` shape
/// (`documentChanges` vs `changes`) + `prepareRename` placeholder, foldingRange `lineFoldingOnly`,
/// and workspace/symbol `resolveSupport` (lazy `WorkspaceSymbol` vs eager `SymbolInformation`).
fn check_m9_projection(
    name: &str,
    profile: &serde_json::Value,
    p: &common::TempProject,
    client: &Connection,
) {
    let uri = file_uri(&p.root.join("src/m9probe.gd"));
    client
        .sender
        .send(notification(
            "textDocument/didOpen",
            lsp_types::DidOpenTextDocumentParams {
                text_document: lsp_types::TextDocumentItem {
                    uri: uri.clone(),
                    language_id: "gdscript".to_string(),
                    version: 1,
                    text: M9_PROBE_SRC.to_string(),
                },
            },
        ))
        .unwrap();
    while try_recv(client, Duration::from_millis(300)).is_some() {}

    // rename the local `counter` (line 4, col 5) — `documentChanges` iff workspaceEdit.documentChanges.
    let expect_doc_changes = flag(profile, &["workspace", "workspaceEdit", "documentChanges"]);
    client
        .sender
        .send(request(
            40,
            "textDocument/rename",
            serde_json::json!({
                "textDocument": { "uri": uri.as_str() },
                "position": { "line": 4, "character": 5 },
                "newName": "tally",
            }),
        ))
        .unwrap();
    let we = response_result(name, client, 40);
    assert_eq!(
        we.get("documentChanges").is_some(),
        expect_doc_changes,
        "{name}: rename documentChanges iff workspace.workspaceEdit.documentChanges"
    );
    assert_eq!(
        we.get("changes").is_some(),
        !expect_doc_changes,
        "{name}: rename legacy changes-map iff NOT documentChanges"
    );

    // prepareRename — `{range, placeholder}` iff rename.prepareSupport, else a bare range.
    let expect_prepare = flag(profile, &["textDocument", "rename", "prepareSupport"]);
    client
        .sender
        .send(request(
            41,
            "textDocument/prepareRename",
            serde_json::json!({
                "textDocument": { "uri": uri.as_str() },
                "position": { "line": 4, "character": 5 },
            }),
        ))
        .unwrap();
    let pr = response_result(name, client, 41);
    assert_eq!(
        pr.get("placeholder").is_some(),
        expect_prepare,
        "{name}: prepareRename placeholder iff rename.prepareSupport (bare range otherwise)"
    );

    // foldingRange — `lineFoldingOnly` drops the column fields.
    let line_only = flag(
        profile,
        &["textDocument", "foldingRange", "lineFoldingOnly"],
    );
    client
        .sender
        .send(request(
            42,
            "textDocument/foldingRange",
            serde_json::json!({ "textDocument": { "uri": uri.as_str() } }),
        ))
        .unwrap();
    let folds = response_result(name, client, 42);
    if let Some(f) = folds.as_array().and_then(|a| a.first()) {
        assert_eq!(
            f.get("startCharacter").is_none(),
            line_only,
            "{name}: foldingRange omits start/end columns iff lineFoldingOnly"
        );
    }

    // workspace/symbol — lazy `WorkspaceSymbol` (uri-only location, no range) iff resolveSupport,
    // else eager `SymbolInformation` (location carries a range).
    let resolve_support = profile["workspace"]["symbol"]["resolveSupport"].is_object();
    client
        .sender
        .send(request(
            43,
            "workspace/symbol",
            serde_json::json!({ "query": "M9ProbeClass" }),
        ))
        .unwrap();
    let syms = response_result(name, client, 43);
    if let Some(s) = syms.as_array().and_then(|a| a.first()) {
        let has_range = s["location"].get("range").is_some();
        assert_eq!(
            has_range, !resolve_support,
            "{name}: workspace/symbol location carries a range iff NOT resolveSupport (lazy WorkspaceSymbol otherwise)"
        );
    }
}

/// The M10 probe — a `func` declaration (`paint`, a `Function` semantic token: the legend-remap
/// discriminator), a `var x := Color(…)` walrus declaration (BOTH a `documentColor` literal AND an
/// inferred-type inlayHint carrying a tooltip), and an unused local (`leftover`, an
/// `UNUSED_VARIABLE` warning: the codeAction quickfix target). `print(tint)` keeps the class member
/// used (so only `leftover` is unused).
const M10_PROBE_SRC: &str = "\
class_name M10ProbeClass
extends Node

var tint := Color(1, 0, 0)

func paint() -> void:
\tvar leftover = 5
\tprint(tint)
";

/// gdls's own fixed STANDARD semantic-tokens legend (`crate::semantic_tokens::LEGEND_TYPES`), in wire
/// order. A profile that advertises no `tokenTypes` (helix) gets these indices verbatim; a profile
/// that advertises its own list gets each type REMAPPED to the index that name occupies in ITS list.
/// Kept in lockstep with the server table by the `legend_is_standard_names_only` unit test (which pins
/// the exact names) — this is the test-side mirror used to predict the per-profile wire index.
const GDLS_LEGEND_TYPES: &[&str] = &[
    "class",
    "enum",
    "enumMember",
    "function",
    "method",
    "property",
    "parameter",
    "variable",
    "event",
    "decorator",
];

/// Drive the gated M10 capabilities for one profile and assert each projection from the profile's OWN
/// flags (never hard-coding per-editor expectations): the semanticTokens per-client legend REMAP
/// (`Method` → the wire index `"method"` occupies in this profile's legend), the `full → full/delta`
/// edit-shape round-trip, the inlayHint `resolveSupport` tooltip deferral, documentColor (served for
/// every profile), and the codeAction rich-client projection (deferred `CodeAction` edit + the
/// `Diagnostic.data` tag gated on `publishDiagnostics.dataSupport`, plus `source.fixAll` separation).
fn check_m10_projection(
    name: &str,
    profile: &serde_json::Value,
    p: &common::TempProject,
    client: &Connection,
) {
    let uri = file_uri(&p.root.join("src/m10probe.gd"));
    client
        .sender
        .send(notification(
            "textDocument/didOpen",
            lsp_types::DidOpenTextDocumentParams {
                text_document: lsp_types::TextDocumentItem {
                    uri: uri.clone(),
                    language_id: "gdscript".to_string(),
                    version: 1,
                    text: M10_PROBE_SRC.to_string(),
                },
            },
        ))
        .unwrap();
    // Drain the open's publishDiagnostics, KEEPING the one for this URI — the codeAction request
    // below must echo the `UNUSED_VARIABLE` diagnostic back in its `context.diagnostics` (the handler
    // reads the context, not a fresh analysis).
    let mut diagnostics = serde_json::Value::Null;
    while let Some(msg) = try_recv(client, Duration::from_millis(400)) {
        if let Message::Notification(n) = msg {
            if n.method == "textDocument/publishDiagnostics"
                && n.params["uri"].as_str() == Some(uri.as_str())
            {
                diagnostics = n.params["diagnostics"].clone();
            }
        }
    }

    // ---- semanticTokens (#72): the per-client legend REMAP. -------------------------------------
    // gdls's legend is the 10 STANDARD types; it is intersected with the profile's advertised
    // `tokenTypes`, so a `method` token (gdls index 4 — a class-member `func` like `paint` is a
    // METHOD, not a free `function`, since the script IS its root class) is emitted at the wire index
    // `"method"` occupies in THIS profile's legend. helix advertises none ⇒ gdls's own index (4);
    // neovim/zed remap it (to 13). The discriminator is the `paint` method-name token.
    let client_types: Vec<String> = profile["textDocument"]["semanticTokens"]["tokenTypes"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    // The expected wire index of gdls's `method` type for this profile (its position in the client's
    // list, or gdls's own index when the client advertised no legend → the full legend).
    let gdls_method_index = GDLS_LEGEND_TYPES
        .iter()
        .position(|t| *t == "method")
        .unwrap();
    let expected_method_wire = if client_types.is_empty() {
        gdls_method_index as u64
    } else {
        client_types
            .iter()
            .position(|t| t == "method")
            .unwrap_or_else(|| panic!("{name}: profile advertises a `method` token type"))
            as u64
    };
    client
        .sender
        .send(request(
            50,
            "textDocument/semanticTokens/full",
            serde_json::json!({ "textDocument": { "uri": uri.as_str() } }),
        ))
        .unwrap();
    let st_full = response_result(name, client, 50);
    let data = st_full["data"]
        .as_array()
        .unwrap_or_else(|| panic!("{name}: semanticTokens/full returns a flat data array"));
    assert!(
        !data.is_empty(),
        "{name}: the probe emits at least the `paint` method token"
    );
    // Decode the flat 5-int stream `[deltaLine, deltaStart, length, tokenType, modifiers]` and collect
    // the set of token-type wire indices present. The `paint` token's type must be the remapped index.
    let token_types: Vec<u64> = data.chunks_exact(5).filter_map(|c| c[3].as_u64()).collect();
    assert!(
        token_types.contains(&expected_method_wire),
        "{name}: a method token is emitted at the per-client-legend wire index {expected_method_wire} \
         (client types: {client_types:?}); got type indices {token_types:?}"
    );

    // ---- semanticTokens (#72): the full → full/delta edit-shape endpoint. -----------------------
    // Delta is NOT a client-gated projection (`SemanticTokensCaps` has only legend + refresh), so it
    // is asserted ONCE as an endpoint: a delta request with the prior resultId returns the
    // `SemanticTokensDelta` (an `edits` array) shape, not a fresh full set, when nothing changed.
    let result_id = st_full["resultId"]
        .as_str()
        .unwrap_or_else(|| panic!("{name}: semanticTokens/full carries a resultId"));
    client
        .sender
        .send(request(
            51,
            "textDocument/semanticTokens/full/delta",
            serde_json::json!({
                "textDocument": { "uri": uri.as_str() },
                "previousResultId": result_id,
            }),
        ))
        .unwrap();
    let st_delta = response_result(name, client, 51);
    assert!(
        st_delta.get("edits").is_some(),
        "{name}: full/delta with the prior resultId returns a SemanticTokensDelta (edits array), got {st_delta}"
    );

    // ---- inlayHint (#73): the resolveSupport tooltip deferral. ----------------------------------
    // A `var tint := Color(…)` walrus declaration yields an inferred-type hint (`: Color`) carrying a
    // tooltip. With `inlayHint.resolveSupport` the tooltip is DEFERRED (absent on the hint, present in
    // `data`, filled by `inlayHint/resolve`); without it the tooltip is EMBEDDED eagerly.
    let want_inlay_resolve = profile["textDocument"]["inlayHint"]["resolveSupport"].is_object();
    client
        .sender
        .send(request(
            52,
            "textDocument/inlayHint",
            serde_json::json!({
                "textDocument": { "uri": uri.as_str() },
                "range": {
                    "start": { "line": 0, "character": 0 },
                    "end": { "line": 9, "character": 0 },
                },
            }),
        ))
        .unwrap();
    let hints = response_result(name, client, 52);
    let type_hint = hints
        .as_array()
        .unwrap_or_else(|| panic!("{name}: inlayHint returns an array"))
        .iter()
        .find(|h| h["label"].as_str().is_some_and(|l| l.contains("Color")))
        .unwrap_or_else(|| {
            panic!("{name}: the `var tint := Color(…)` type hint `: Color` is offered; got {hints}")
        })
        .clone();
    if want_inlay_resolve {
        assert!(
            type_hint["tooltip"].is_null() && !type_hint["data"].is_null(),
            "{name}: resolveSupport ⇒ tooltip DEFERRED (absent + data present); got {type_hint}"
        );
        // The resolve round-trip fills the tooltip from `data`.
        client
            .sender
            .send(request(53, "inlayHint/resolve", &type_hint))
            .unwrap();
        let resolved = response_result(name, client, 53);
        assert!(
            !resolved["tooltip"].is_null(),
            "{name}: inlayHint/resolve fills the deferred tooltip; got {resolved}"
        );
    } else {
        assert!(
            !type_hint["tooltip"].is_null(),
            "{name}: no resolveSupport ⇒ tooltip EMBEDDED eagerly; got {type_hint}"
        );
    }

    // ---- documentColor (#74): served for EVERY profile (no client-capability gate). -------------
    client
        .sender
        .send(request(
            54,
            "textDocument/documentColor",
            serde_json::json!({ "textDocument": { "uri": uri.as_str() } }),
        ))
        .unwrap();
    let colors = response_result(name, client, 54);
    let color_info = colors
        .as_array()
        .unwrap_or_else(|| panic!("{name}: documentColor returns an array"));
    assert_eq!(
        color_info.len(),
        1,
        "{name}: the `Color(1, 0, 0)` literal is the one reported color; got {colors}"
    );
    let red = &color_info[0]["color"];
    assert_eq!(red["red"].as_f64(), Some(1.0), "{name}: color red channel");
    assert_eq!(
        red["green"].as_f64(),
        Some(0.0),
        "{name}: color green channel"
    );
    // colorPresentation for that color always offers at least the float `Color(…)` form.
    client
        .sender
        .send(request(
            55,
            "textDocument/colorPresentation",
            serde_json::json!({
                "textDocument": { "uri": uri.as_str() },
                "color": red,
                "range": color_info[0]["range"],
            }),
        ))
        .unwrap();
    let presentations = response_result(name, client, 55);
    assert!(
        presentations
            .as_array()
            .is_some_and(|a| a.iter().any(|p| p["label"].as_str().is_some_and(|l| l.starts_with("Color(")))),
        "{name}: colorPresentation always offers the float Color(…) constructor form; got {presentations}"
    );

    // ---- codeAction (#75): the rich-client projection (all vendored profiles advertise -----------
    // codeActionLiteralSupport + resolveSupport, so the walk asserts the rich path; the degraded
    // Command/eager paths are covered by tests/code_action.rs). The `Diagnostic.data` tag is gated on
    // publishDiagnostics.dataSupport; `source.fixAll` is its own family.
    let literal_support =
        profile["textDocument"]["codeAction"]["codeActionLiteralSupport"].is_object();
    let resolve_support = profile["textDocument"]["codeAction"]["resolveSupport"].is_object();
    let data_support = flag(
        profile,
        &["textDocument", "publishDiagnostics", "dataSupport"],
    );

    // The published UNUSED_VARIABLE diagnostic — round-tripped into the request context (the handler
    // reads `context.diagnostics`, not a fresh analysis).
    let unused = diagnostics
        .as_array()
        .into_iter()
        .flatten()
        .find(|d| d["code"] == "UNUSED_VARIABLE")
        .unwrap_or_else(|| panic!("{name}: UNUSED_VARIABLE fires on `leftover`; got {diagnostics}"))
        .clone();
    // The additive `Diagnostic.data` tag is present iff publishDiagnostics.dataSupport (FIDELITY: the
    // tag is the ONLY thing dataSupport adds).
    assert_eq!(
        !unused["data"].is_null(),
        data_support,
        "{name}: Diagnostic.data tag present iff publishDiagnostics.dataSupport"
    );

    // quickfix family: the suppression + any mutating fix. With literal+resolve support (every
    // vendored profile) each action is a `CodeAction` literal whose `edit` is DEFERRED to resolve.
    client
        .sender
        .send(request(
            56,
            "textDocument/codeAction",
            serde_json::json!({
                "textDocument": { "uri": uri.as_str() },
                "range": unused["range"],
                "context": {
                    "diagnostics": [unused],
                    "only": ["quickfix"],
                },
            }),
        ))
        .unwrap();
    let actions = response_result(name, client, 56);
    let actions = actions
        .as_array()
        .unwrap_or_else(|| panic!("{name}: codeAction returns an array"));
    assert!(
        !actions.is_empty(),
        "{name}: a quickfix is offered for UNUSED_VARIABLE"
    );
    // The suppression action ("Ignore …") — a CodeAction literal (every vendored profile has
    // literalSupport), kind quickfix.
    let suppression = actions
        .iter()
        .find(|a| a["title"].as_str().is_some_and(|t| t.contains("Ignore")))
        .unwrap_or_else(|| {
            panic!("{name}: the @warning_ignore suppression is offered; got {actions:?}")
        })
        .clone();
    assert!(
        literal_support && suppression["kind"] == "quickfix",
        "{name}: with literalSupport the suppression is a CodeAction literal of kind quickfix"
    );
    if resolve_support {
        // resolveSupport ⇒ the suppression's edit is DEFERRED (absent + data present), filled by
        // codeAction/resolve.
        assert!(
            suppression["edit"].is_null() && !suppression["data"].is_null(),
            "{name}: resolveSupport ⇒ the suppression edit is deferred (data present, edit absent); got {suppression}"
        );
        client
            .sender
            .send(request(57, "codeAction/resolve", &suppression))
            .unwrap();
        let resolved = response_result(name, client, 57);
        assert!(
            !resolved["edit"].is_null(),
            "{name}: codeAction/resolve fills the deferred suppression edit; got {resolved}"
        );
    }

    // source.fixAll family separation: a `source.fixAll` filter yields ONLY source.fixAll actions
    // (never the per-diagnostic suppression). Meaningful only with literalSupport (the aggregate is a
    // multi-edit CodeAction literal); every vendored profile has it.
    client
        .sender
        .send(request(
            58,
            "textDocument/codeAction",
            serde_json::json!({
                "textDocument": { "uri": uri.as_str() },
                "range": unused["range"],
                "context": {
                    "diagnostics": [unused],
                    "only": ["source.fixAll"],
                },
            }),
        ))
        .unwrap();
    let fix_all = response_result(name, client, 58);
    let arr = fix_all.as_array().unwrap_or_else(|| {
        panic!("{name}: codeAction(source.fixAll) returns an array; got {fix_all}")
    });
    // The probe's UNUSED_VARIABLE on `leftover` has a deterministic `_`-prefix fix, which source.fixAll
    // aggregates (every vendored profile has literalSupport) → exactly one source.fixAll action. A
    // non-empty assertion guards against a vacuous separation check on an empty array.
    assert_eq!(
        arr.len(),
        1,
        "{name}: source.fixAll aggregates the one auto-fixable warning into one action; got {fix_all}"
    );
    assert!(
        arr.iter().all(|a| a["kind"]
            .as_str()
            .is_some_and(|k| k.starts_with("source.fixAll"))),
        "{name}: a source.fixAll filter yields ONLY source.fixAll actions (no quickfix/suppression); got {fix_all}"
    );
    assert!(
        !arr.iter()
            .any(|a| a["title"].as_str().is_some_and(|t| t.contains("Ignore"))),
        "{name}: the per-diagnostic suppression must NOT appear under a source.fixAll filter"
    );
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
