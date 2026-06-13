//! M9 (#66) gate: `textDocument/prepareRename` + `textDocument/rename` over an in-memory
//! connection (the `tests/lifecycle.rs` / `references_xfile.rs` Connection rig).
//!
//! The whole discipline under test is **refuse rather than corrupt**: a rename either edits the
//! EXACT set `references` resolves (declaration + every reference, cross-file), or it refuses with
//! a typed request error and ZERO edits. Coverage (phase-6 criteria 1–5; the Pixelorama
//! round-trip — criterion 6 — is the orchestrator's, not here):
//!
//!   1. `renameProvider` advertised with `prepareProvider: true`; both handlers dispatched.
//!   2. a cross-file rename (`class_name Hero` used in `enemy.gd`) edits the declaration + every
//!      reference, and the edited (uri, range) set EQUALS `references`' output on the same symbol.
//!   3. with `workspace.workspaceEdit.documentChanges` advertised → versioned `TextDocumentEdit`s
//!      carrying the open buffers' current versions (zero stale-version edits); without it → the
//!      legacy `changes` map. Both projections asserted.
//!   4. `prepareRename` REFUSES (typed request error, `error.is_some()` — NOT a null result) on a
//!      native symbol AND on a materialized stub file.
//!   5. invalid new names each REFUSE with ZERO edits: empty string, an invalid identifier
//!      (`1bad`, `has space`), a GDScript keyword (`func`, `if`), and a name colliding with an
//!      existing member in scope.

mod common;

use std::collections::HashMap;

use common::{file_uri, notification, recv_response, request, shutdown, TempProject};
use lsp_server::Connection;
use lsp_types::{
    DidOpenTextDocumentParams, DocumentChanges, GotoDefinitionResponse, InitializeParams,
    InitializeResult, InitializedParams, Location, Position, PrepareRenameResponse, Range,
    ReferenceContext, ReferenceParams, RenameParams, TextDocumentIdentifier, TextDocumentItem,
    TextDocumentPositionParams, TextEdit, Uri, WorkDoneProgressParams, WorkspaceEdit,
};

/// Boot the server thread over an in-memory connection and return the client side. The server
/// reads `initialize` from the connection, so the caller drives the handshake. `Connection::memory()`
/// returns `(server, client)` (the order `references_xfile.rs` relies on).
fn boot() -> (Connection, std::thread::JoinHandle<anyhow::Result<()>>) {
    let (server, client) = Connection::memory();
    let handle = std::thread::spawn(move || gd_server::serve(server));
    (client, handle)
}

/// `initialize` + `initialized`, with the given client capabilities, then open every requested
/// file. Returns the parsed `InitializeResult` so a test can assert on advertised server
/// capabilities. `version_base` is the version of the first opened file (each subsequent file
/// increments), so a test can assert documentChanges versions match the didOpen versions.
fn init_open(
    project: &TempProject,
    client: &Connection,
    caps: serde_json::Value,
    files: &[&str],
    version_base: i32,
) -> InitializeResult {
    let init = InitializeParams {
        initialization_options: Some(serde_json::json!({
            "projectRoot": project.root.as_str(),
            "autoDumpExtensionApi": false,
            "extensionApiPath": project.root.join("extension_api.json").as_str(),
        })),
        capabilities: serde_json::from_value(caps).expect("client caps"),
        ..Default::default()
    };
    client.sender.send(request(1, "initialize", init)).unwrap();
    let init_resp = recv_response(client);
    let result: InitializeResult =
        serde_json::from_value(init_resp.result.expect("initialize result")).unwrap();
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
                        version: version_base + i as i32,
                        text,
                    },
                },
            ))
            .unwrap();
    }
    // Drain the publishDiagnostics pushes the opens trigger.
    while common::try_recv(client, std::time::Duration::from_millis(300)).is_some() {}
    result
}

/// Caps advertising both rename gates (prepareSupport + documentChanges).
fn caps_full() -> serde_json::Value {
    serde_json::json!({
        "textDocument": { "rename": { "prepareSupport": true } },
        "workspace": { "workspaceEdit": { "documentChanges": true } }
    })
}

/// Caps advertising NEITHER rename gate (a minimal client): prepare downgrades to a bare range,
/// rename downgrades to the legacy `changes` map.
fn caps_minimal() -> serde_json::Value {
    serde_json::json!({})
}

fn position_params(uri: &Uri, line: u32, character: u32) -> TextDocumentPositionParams {
    TextDocumentPositionParams {
        text_document: TextDocumentIdentifier { uri: uri.clone() },
        position: Position { line, character },
    }
}

fn rename_params(uri: &Uri, line: u32, character: u32, new_name: &str) -> RenameParams {
    RenameParams {
        text_document_position: position_params(uri, line, character),
        new_name: new_name.to_string(),
        work_done_progress_params: WorkDoneProgressParams::default(),
    }
}

/// The full set of (uri, range) pairs from a `references` request (include_declaration: true).
fn references_set(
    client: &Connection,
    id: i32,
    uri: &Uri,
    line: u32,
    ch: u32,
) -> Vec<(String, Range)> {
    let params = ReferenceParams {
        text_document_position: position_params(uri, line, ch),
        context: ReferenceContext {
            include_declaration: true,
        },
        work_done_progress_params: WorkDoneProgressParams::default(),
        partial_result_params: lsp_types::PartialResultParams::default(),
    };
    client
        .sender
        .send(request(id, "textDocument/references", params))
        .unwrap();
    let resp = recv_response(client);
    let locs: Vec<Location> =
        serde_json::from_value(resp.result.expect("references result")).unwrap();
    let mut set: Vec<(String, Range)> = locs
        .into_iter()
        .map(|l| (l.uri.as_str().to_string(), l.range))
        .collect();
    set.sort_by(|a, b| a.0.cmp(&b.0).then(cmp_range(&a.1, &b.1)));
    set
}

fn cmp_range(a: &Range, b: &Range) -> std::cmp::Ordering {
    (a.start.line, a.start.character, a.end.line, a.end.character).cmp(&(
        b.start.line,
        b.start.character,
        b.end.line,
        b.end.character,
    ))
}

/// Flatten a `WorkspaceEdit` (either shape) into a sorted (uri, range) set + the per-uri new_text,
/// plus the per-uri version when the documentChanges shape is used (None for the changes map).
struct EditView {
    set: Vec<(String, Range)>,
    new_texts: Vec<String>,
    /// (uri, version) for the documentChanges shape; empty for the changes map.
    versions: Vec<(String, Option<i32>)>,
}

fn flatten_edit(edit: &WorkspaceEdit) -> EditView {
    let mut set: Vec<(String, Range)> = Vec::new();
    let mut new_texts: Vec<String> = Vec::new();
    let mut versions: Vec<(String, Option<i32>)> = Vec::new();
    match (&edit.document_changes, &edit.changes) {
        (Some(DocumentChanges::Edits(tde)), None) => {
            for e in tde {
                versions.push((
                    e.text_document.uri.as_str().to_string(),
                    e.text_document.version,
                ));
                for oneof in &e.edits {
                    if let lsp_types::OneOf::Left(te) = oneof {
                        set.push((e.text_document.uri.as_str().to_string(), te.range));
                        new_texts.push(te.new_text.clone());
                    } else {
                        panic!("annotated edit not expected");
                    }
                }
            }
        }
        (None, Some(changes)) => {
            for (uri, edits) in changes {
                for te in edits {
                    set.push((uri.as_str().to_string(), te.range));
                    new_texts.push(te.new_text.clone());
                }
            }
        }
        other => panic!("exactly one WorkspaceEdit field must be populated, got {other:?}"),
    }
    set.sort_by(|a, b| a.0.cmp(&b.0).then(cmp_range(&a.1, &b.1)));
    EditView {
        set,
        new_texts,
        versions,
    }
}

// =================================================================================================
// Criterion 1: capability advertised.
// =================================================================================================

#[test]
fn rename_provider_advertised_with_prepare() {
    let project = common::sample_project();
    let (client, server) = boot();
    let result = init_open(&project, &client, caps_full(), &[], 2);

    match result.capabilities.rename_provider {
        Some(lsp_types::OneOf::Right(opts)) => {
            assert_eq!(
                opts.prepare_provider,
                Some(true),
                "prepareProvider must be advertised"
            );
        }
        other => panic!("expected RenameOptions with prepareProvider, got {other:?}"),
    }
    shutdown(&client, server);
}

// =================================================================================================
// Criterion 2 + 3: cross-file rename equals references; both WorkspaceEdit projections.
// =================================================================================================

#[test]
fn cross_file_rename_edits_equal_references_documentchanges_shape() {
    // `class_name Hero` (hero.gd line 0) is referenced by `extends Hero` (enemy.gd line 0). A
    // rename must edit BOTH sites, the edited (uri, range) set must EQUAL `references`' output, and
    // — with documentChanges advertised — each TextDocumentEdit must carry the file's open version.
    let project = common::sample_project();
    let (client, server) = boot();
    init_open(
        &project,
        &client,
        caps_full(),
        &["src/hero.gd", "src/enemy.gd"],
        7, // hero.gd version 7, enemy.gd version 8
    );
    let hero_uri = file_uri(&project.root.join("src/hero.gd"));
    let enemy_uri = file_uri(&project.root.join("src/enemy.gd"));

    // `class_name Hero` — `Hero` starts at column 11 on line 0.
    let ref_set = references_set(&client, 10, &hero_uri, 0, 11);
    assert!(
        ref_set.iter().any(|(u, _)| *u == hero_uri.as_str()),
        "references must include the hero.gd declaration: {ref_set:?}"
    );
    assert!(
        ref_set.iter().any(|(u, _)| *u == enemy_uri.as_str()),
        "references must include the enemy.gd extends site: {ref_set:?}"
    );

    client
        .sender
        .send(request(
            11,
            "textDocument/rename",
            rename_params(&hero_uri, 0, 11, "Champion"),
        ))
        .unwrap();
    let resp = recv_response(&client);
    assert!(
        resp.error.is_none(),
        "rename should succeed: {:?}",
        resp.error
    );
    let edit: WorkspaceEdit = serde_json::from_value(resp.result.expect("rename result")).unwrap();
    let view = flatten_edit(&edit);

    // The edited set EQUALS the references set.
    assert_eq!(
        view.set, ref_set,
        "edited (uri,range) set must equal the references set"
    );
    // Every edit writes the new name.
    assert!(
        view.new_texts.iter().all(|t| t == "Champion"),
        "every TextEdit must write the new name, got {:?}",
        view.new_texts
    );
    // documentChanges shape: versions match the didOpen versions (zero stale-version edits).
    let hero_ver = view
        .versions
        .iter()
        .find(|(u, _)| *u == hero_uri.as_str())
        .expect("hero.gd in documentChanges");
    assert_eq!(
        hero_ver.1,
        Some(7),
        "hero.gd version must match its didOpen version"
    );
    let enemy_ver = view
        .versions
        .iter()
        .find(|(u, _)| *u == enemy_uri.as_str())
        .expect("enemy.gd in documentChanges");
    assert_eq!(
        enemy_ver.1,
        Some(8),
        "enemy.gd version must match its didOpen version"
    );
    // No stale versions: every version is Some (both files are open).
    assert!(
        view.versions.iter().all(|(_, v)| v.is_some()),
        "open buffers must carry a version (no stale/None), got {:?}",
        view.versions
    );

    shutdown(&client, server);
}

#[test]
fn cross_file_member_rename_succeeds_and_equals_references() {
    // A cross-file MEMBER rename (the harder half of criterion 2, and the path the fail-closed gate
    // could subtly break — it is admitted via classify→Member, signal #5). `var speed` is declared
    // in `lib.gd` (class_name Lib) and read through a typed var in `a.gd` (`l.speed`). Renaming
    // `speed` at its declaration must edit BOTH files and the edited set must EQUAL `references`.
    let project = common::sample_project();
    project.write(
        "src/lib.gd",
        "class_name Lib\nextends Node\n\nvar speed: int = 5\n",
    );
    project.write(
        "src/a.gd",
        "extends Node\n\nfunc run() -> void:\n\tvar l: Lib = Lib.new()\n\tl.speed = 9\n",
    );
    let (client, server) = boot();
    init_open(
        &project,
        &client,
        caps_full(),
        &["src/lib.gd", "src/a.gd"],
        2,
    );
    let lib_uri = file_uri(&project.root.join("src/lib.gd"));
    let a_uri = file_uri(&project.root.join("src/a.gd"));

    // `var speed: int = 5` is line 3 of lib.gd; `speed` at column 4.
    let ref_set = references_set(&client, 10, &lib_uri, 3, 4);
    assert!(
        ref_set.iter().any(|(u, _)| *u == lib_uri.as_str()),
        "references must include the lib.gd declaration: {ref_set:?}"
    );
    assert!(
        ref_set.iter().any(|(u, _)| *u == a_uri.as_str()),
        "references must include the a.gd use site: {ref_set:?}"
    );

    client
        .sender
        .send(request(
            11,
            "textDocument/rename",
            rename_params(&lib_uri, 3, 4, "velocity"),
        ))
        .unwrap();
    let resp = recv_response(&client);
    assert!(
        resp.error.is_none(),
        "cross-file member rename must succeed: {:?}",
        resp.error
    );
    let edit: WorkspaceEdit = serde_json::from_value(resp.result.expect("rename result")).unwrap();
    let view = flatten_edit(&edit);
    assert_eq!(
        view.set, ref_set,
        "cross-file member edited set must equal the references set"
    );
    assert!(
        view.new_texts.iter().all(|t| t == "velocity"),
        "every edit writes the new name, got {:?}",
        view.new_texts
    );

    shutdown(&client, server);
}

// `lsp_types::Uri` trips `clippy::mutable_key_type` as a HashMap key (it caches parsed components
// in a `Cell`); the `changes` map IS `HashMap<Uri, _>` by the LSP wire shape, and we only read it.
#[allow(clippy::mutable_key_type)]
#[test]
fn cross_file_rename_changes_map_shape_without_documentchanges() {
    // Same rename, but the client did NOT advertise documentChanges → the legacy `changes` map,
    // and the document_changes field must be None.
    let project = common::sample_project();
    let (client, server) = boot();
    init_open(
        &project,
        &client,
        caps_minimal(),
        &["src/hero.gd", "src/enemy.gd"],
        2,
    );
    let hero_uri = file_uri(&project.root.join("src/hero.gd"));
    let enemy_uri = file_uri(&project.root.join("src/enemy.gd"));

    let ref_set = references_set(&client, 10, &hero_uri, 0, 11);

    client
        .sender
        .send(request(
            11,
            "textDocument/rename",
            rename_params(&hero_uri, 0, 11, "Champion"),
        ))
        .unwrap();
    let resp = recv_response(&client);
    assert!(
        resp.error.is_none(),
        "rename should succeed: {:?}",
        resp.error
    );
    let edit: WorkspaceEdit = serde_json::from_value(resp.result.expect("rename result")).unwrap();

    assert!(
        edit.document_changes.is_none(),
        "without documentChanges advertised, document_changes must be None"
    );
    let changes: &HashMap<Uri, Vec<TextEdit>> = edit
        .changes
        .as_ref()
        .expect("changes map must be populated");
    assert!(
        changes.keys().any(|u| u.as_str() == hero_uri.as_str()),
        "changes must include hero.gd"
    );
    assert!(
        changes.keys().any(|u| u.as_str() == enemy_uri.as_str()),
        "changes must include enemy.gd"
    );

    let view = flatten_edit(&edit);
    assert_eq!(
        view.set, ref_set,
        "changes-map edited set must equal the references set"
    );

    shutdown(&client, server);
}

// =================================================================================================
// Criterion 4: prepareRename refuses on a native symbol AND a stub file.
// =================================================================================================

#[test]
fn prepare_rename_succeeds_on_project_symbol() {
    // A sanity baseline before the refusals: prepareRename on the project `class_name Hero` returns
    // a RangeWithPlaceholder (prepareSupport advertised) carrying the current name.
    let project = common::sample_project();
    let (client, server) = boot();
    init_open(&project, &client, caps_full(), &["src/hero.gd"], 2);
    let hero_uri = file_uri(&project.root.join("src/hero.gd"));

    client
        .sender
        .send(request(
            20,
            "textDocument/prepareRename",
            position_params(&hero_uri, 0, 11),
        ))
        .unwrap();
    let resp = recv_response(&client);
    assert!(
        resp.error.is_none(),
        "prepare should succeed: {:?}",
        resp.error
    );
    let pr: Option<PrepareRenameResponse> =
        serde_json::from_value(resp.result.expect("prepare result")).unwrap();
    match pr {
        Some(PrepareRenameResponse::RangeWithPlaceholder { placeholder, .. }) => {
            assert_eq!(placeholder, "Hero", "placeholder must be the current name");
        }
        other => panic!("expected RangeWithPlaceholder, got {other:?}"),
    }
    shutdown(&client, server);
}

#[test]
fn prepare_rename_bare_range_without_prepare_support() {
    // A client that did NOT advertise prepareSupport still gets a prepare answer (so the keybinding
    // works) — but the bare `Range` variant, not RangeWithPlaceholder.
    let project = common::sample_project();
    let (client, server) = boot();
    init_open(&project, &client, caps_minimal(), &["src/hero.gd"], 2);
    let hero_uri = file_uri(&project.root.join("src/hero.gd"));

    client
        .sender
        .send(request(
            20,
            "textDocument/prepareRename",
            position_params(&hero_uri, 0, 11),
        ))
        .unwrap();
    let resp = recv_response(&client);
    assert!(resp.error.is_none());
    let pr: Option<PrepareRenameResponse> =
        serde_json::from_value(resp.result.expect("prepare result")).unwrap();
    assert!(
        matches!(pr, Some(PrepareRenameResponse::Range(_))),
        "without prepareSupport, expected a bare Range, got {pr:?}"
    );
    shutdown(&client, server);
}

#[test]
fn prepare_rename_refuses_native_symbol() {
    // `extends Node2D` (hero.gd line 1) — `Node2D` is a native engine class. prepareRename must
    // REFUSE with a typed request error (error.is_some()), NOT a null result.
    let project = common::sample_project();
    let (client, server) = boot();
    init_open(&project, &client, caps_full(), &["src/hero.gd"], 2);
    let hero_uri = file_uri(&project.root.join("src/hero.gd"));

    // `extends Node2D` — `Node2D` starts at column 8 on line 1.
    client
        .sender
        .send(request(
            21,
            "textDocument/prepareRename",
            position_params(&hero_uri, 1, 8),
        ))
        .unwrap();
    let resp = recv_response(&client);
    assert!(
        resp.error.is_some(),
        "prepareRename on a native symbol must refuse with a typed error, not a null result \
         (result={:?})",
        resp.result
    );
    let err = resp.error.unwrap();
    assert!(
        err.message.contains("native"),
        "refusal message should name the native nature, got {:?}",
        err.message
    );
    shutdown(&client, server);
}

#[test]
fn prepare_rename_refuses_inside_stub_file() {
    // Materialize a native stub: a `definition` on `Node2D` returns a Location into the stub page.
    // Open that stub, then prepareRename inside it → REFUSE (the file is not editable project
    // source) with a typed error.
    let project = common::sample_project();
    let (client, server) = boot();
    init_open(&project, &client, caps_full(), &["src/hero.gd"], 2);
    let hero_uri = file_uri(&project.root.join("src/hero.gd"));

    // definition on `Node2D` (line 1, col 8) → the stub Location.
    let def_params = lsp_types::GotoDefinitionParams {
        text_document_position_params: position_params(&hero_uri, 1, 8),
        work_done_progress_params: WorkDoneProgressParams::default(),
        partial_result_params: lsp_types::PartialResultParams::default(),
    };
    client
        .sender
        .send(request(30, "textDocument/definition", def_params))
        .unwrap();
    let def_resp = recv_response(&client);
    let def: Option<GotoDefinitionResponse> =
        serde_json::from_value(def_resp.result.expect("definition result")).unwrap();
    let stub_loc = match def {
        Some(GotoDefinitionResponse::Scalar(loc)) => loc,
        other => panic!("expected a scalar definition into the stub, got {other:?}"),
    };
    assert!(
        stub_loc.uri.as_str().ends_with("/Node2D.gd"),
        "definition should land in the Node2D stub, got {}",
        stub_loc.uri.as_str()
    );

    // Open the stub page as a buffer, then prepareRename on its class-name header.
    let stub_text = std::fs::read_to_string(
        gd_server::uri::uri_to_path(&stub_loc.uri)
            .expect("stub path")
            .as_std_path(),
    )
    .expect("read stub");
    client
        .sender
        .send(notification(
            "textDocument/didOpen",
            DidOpenTextDocumentParams {
                text_document: TextDocumentItem {
                    uri: stub_loc.uri.clone(),
                    language_id: "gdscript".to_string(),
                    version: 1,
                    text: stub_text,
                },
            },
        ))
        .unwrap();
    while common::try_recv(&client, std::time::Duration::from_millis(300)).is_some() {}

    client
        .sender
        .send(request(
            31,
            "textDocument/prepareRename",
            // The class-name header line/col of the stub; the exact identifier is at the position
            // the stub records, but any identifier inside the stub must refuse, so target the
            // class name token.
            TextDocumentPositionParams {
                text_document: TextDocumentIdentifier {
                    uri: stub_loc.uri.clone(),
                },
                position: stub_loc.range.start,
            },
        ))
        .unwrap();
    let resp = recv_response(&client);
    assert!(
        resp.error.is_some(),
        "prepareRename inside a generated stub must refuse with a typed error, not null \
         (result={:?})",
        resp.result
    );
    let msg = resp.error.unwrap().message;
    // The refusal names either the stub file or the native symbol — both are valid refusals for a
    // stub page (the cursor on the class header resolves the native class too).
    assert!(
        msg.contains("stub") || msg.contains("native"),
        "stub refusal must carry a human message naming the stub/native nature, got {msg:?}"
    );
    shutdown(&client, server);
}

// =================================================================================================
// Criterion 5: invalid new names each refuse with ZERO edits.
// =================================================================================================

/// Send a rename and assert it refused (typed error) with NO result payload (zero edits).
fn assert_rename_refused(
    client: &Connection,
    id: i32,
    uri: &Uri,
    line: u32,
    ch: u32,
    new_name: &str,
) {
    client
        .sender
        .send(request(
            id,
            "textDocument/rename",
            rename_params(uri, line, ch, new_name),
        ))
        .unwrap();
    let resp = recv_response(client);
    assert!(
        resp.error.is_some(),
        "rename to {new_name:?} must refuse with a typed error (result={:?})",
        resp.result
    );
    assert!(
        resp.result.is_none(),
        "a refused rename must carry ZERO edits (no result), got {:?}",
        resp.result
    );
}

#[test]
fn rename_refuses_invalid_new_names() {
    // Rename the project `var hp` (hero.gd line 3, `var hp` — `hp` at column 4). Every invalid new
    // name must refuse with zero edits: empty, invalid identifiers, keywords.
    let project = common::sample_project();
    let (client, server) = boot();
    init_open(&project, &client, caps_full(), &["src/hero.gd"], 2);
    let hero_uri = file_uri(&project.root.join("src/hero.gd"));

    // `var hp: int = 10` is line 3; `hp` is at column 4.
    assert_rename_refused(&client, 40, &hero_uri, 3, 4, ""); // empty
    assert_rename_refused(&client, 41, &hero_uri, 3, 4, "1bad"); // not an identifier (leading digit)
    assert_rename_refused(&client, 42, &hero_uri, 3, 4, "has space"); // two tokens
    assert_rename_refused(&client, 43, &hero_uri, 3, 4, "func"); // keyword
    assert_rename_refused(&client, 44, &hero_uri, 3, 4, "if"); // keyword

    shutdown(&client, server);
}

#[test]
fn rename_refuses_member_collision() {
    // A class with two members; renaming one to the other's name must refuse (member collision),
    // zero edits.
    let project = common::sample_project();
    project.write(
        "src/dup.gd",
        "extends Node\n\nvar alpha: int = 1\nvar beta: int = 2\n\nfunc use() -> void:\n\talpha = beta\n",
    );
    let (client, server) = boot();
    init_open(&project, &client, caps_full(), &["src/dup.gd"], 2);
    let dup_uri = file_uri(&project.root.join("src/dup.gd"));

    // `var alpha` is line 2; `alpha` at column 4. Renaming `alpha` → `beta` collides.
    assert_rename_refused(&client, 50, &dup_uri, 2, 4, "beta");

    // Sanity: renaming `alpha` → a free name succeeds (the collision check is not over-broad).
    client
        .sender
        .send(request(
            51,
            "textDocument/rename",
            rename_params(&dup_uri, 2, 4, "gamma"),
        ))
        .unwrap();
    let ok = recv_response(&client);
    assert!(
        ok.error.is_none(),
        "renaming to a free name must succeed: {:?}",
        ok.error
    );

    shutdown(&client, server);
}

// =================================================================================================
// Corruption firewall on the MUTATING path: a native MEMBER access from a project file. This is the
// catastrophic case the feature exists to refuse — `references` resolves a native member through a
// project-wide raw text scan, so without the gate a rename would mass-edit calls to an engine
// method. Both the bare implicit-self call (`queue_free()`) and the typed-var attribute call
// (`n.queue_free()`) must REFUSE via `textDocument/rename` directly (not just prepareRename), with
// ZERO edits. The native DB here actually carries `queue_free` so the resolution is meaningful.
// =================================================================================================

/// A native `Object ← Node` DB where `Node` has a real `queue_free` method, so a member access
/// resolves to a concrete native member (the precondition for the gate's definition→stub step).
const NODE_WITH_METHOD_API: &str = r#"{
    "header": { "version_major": 4, "version_minor": 6, "version_patch": 3 },
    "classes": [
        {"name": "Object", "is_instantiable": true},
        {"name": "Node", "inherits": "Object", "is_instantiable": true,
         "methods": [{"name": "queue_free", "is_const": false, "is_static": false,
                      "is_vararg": false, "is_virtual": false, "hash": 1, "arguments": []}]}
    ]
}"#;

/// A richer native dump for the fail-closed-gate tests: `Object ← Node`, the `Vector2` builtin, a
/// `@GlobalScope` `Side` enum (value `SIDE_LEFT`), an `Error` enum (value `OK`), and the `print`
/// utility. These are exactly the engine-symbol categories the OLD fail-open gate let through — a
/// rename clicked on any of them must now refuse.
const RICH_NATIVE_API: &str = r#"{
    "header": { "version_major": 4, "version_minor": 6, "version_patch": 3 },
    "global_enums": [
        {"name": "Side", "values": [{"name": "SIDE_LEFT", "value": 0}, {"name": "SIDE_RIGHT", "value": 2}]},
        {"name": "Error", "values": [{"name": "OK", "value": 0}, {"name": "FAILED", "value": 1}]}
    ],
    "utility_functions": [
        {"name": "print", "category": "general", "is_vararg": true, "arguments": []}
    ],
    "builtin_classes": [
        {"name": "Vector2", "members": [{"name": "x", "type": "float"}, {"name": "y", "type": "float"}]}
    ],
    "classes": [
        {"name": "Object", "is_instantiable": true},
        {"name": "Node", "inherits": "Object", "is_instantiable": true,
         "methods": [{"name": "queue_free", "is_const": false, "is_static": false,
                      "is_vararg": false, "is_virtual": false, "hash": 1, "arguments": []}]}
    ]
}"#;

/// Boot a project whose `main.gd` exercises a native target, with a real stub cache dir so the
/// definition→stub gate step has somewhere to materialize. `api` is the `extension_api.json` body.
/// Returns (client, server thread, main uri, project).
fn boot_native_member_with_api(
    src: &str,
    api: &str,
) -> (
    Connection,
    std::thread::JoinHandle<anyhow::Result<()>>,
    Uri,
    TempProject,
) {
    let project = TempProject::new();
    project.write("project.godot", "config_version=5\n");
    project.write("extension_api.json", api);
    project.write("main.gd", src);
    let stub_cache = project.root.join("stub-cache");

    let (server, client) = Connection::memory();
    let handle = std::thread::spawn(move || gd_server::serve(server));
    let init = InitializeParams {
        initialization_options: Some(serde_json::json!({
            "projectRoot": project.root.as_str(),
            "extensionApiPath": project.root.join("extension_api.json").as_str(),
            "autoDumpExtensionApi": false,
            "stubCacheDir": stub_cache.as_str(),
        })),
        capabilities: serde_json::from_value(caps_full()).unwrap(),
        ..Default::default()
    };
    client.sender.send(request(1, "initialize", init)).unwrap();
    let _ = recv_response(&client);
    client
        .sender
        .send(notification("initialized", InitializedParams {}))
        .unwrap();
    let main_uri = file_uri(&project.root.join("main.gd"));
    client
        .sender
        .send(notification(
            "textDocument/didOpen",
            DidOpenTextDocumentParams {
                text_document: TextDocumentItem {
                    uri: main_uri.clone(),
                    language_id: "gdscript".to_string(),
                    version: 1,
                    text: src.to_string(),
                },
            },
        ))
        .unwrap();
    while common::try_recv(&client, std::time::Duration::from_millis(400)).is_some() {}
    (client, handle, main_uri, project)
}

/// The native-member fixture (the original two tests' rig): `Node` with `queue_free`.
fn boot_native_member(
    src: &str,
) -> (
    Connection,
    std::thread::JoinHandle<anyhow::Result<()>>,
    Uri,
    TempProject,
) {
    boot_native_member_with_api(src, NODE_WITH_METHOD_API)
}

/// Send a rename and assert it refused for the NATIVE reason specifically: `error.is_some()`, zero
/// edits, the error code is `RequestFailed` (-32803, not the -32602 an invalid name would give),
/// and the message names the native nature. `new_name` is a deliberately-VALID identifier so the
/// only thing that can refuse is the native gate — a green result here proves the firewall fired
/// for the right reason, not by accident.
fn assert_rename_refused_native(client: &Connection, id: i32, uri: &Uri, line: u32, ch: u32) {
    client
        .sender
        .send(request(
            id,
            "textDocument/rename",
            rename_params(uri, line, ch, "release_me"),
        ))
        .unwrap();
    let resp = recv_response(client);
    assert!(
        resp.error.is_some(),
        "rename of a native member must refuse with a typed error (result={:?})",
        resp.result
    );
    assert!(
        resp.result.is_none(),
        "a refused native-member rename must carry ZERO edits, got {:?}",
        resp.result
    );
    let err = resp.error.unwrap();
    assert_eq!(
        err.code, -32803,
        "native refusal must use RequestFailed (-32803), not an invalid-name code; got {} / {:?}",
        err.code, err.message
    );
    assert!(
        err.message.contains("native"),
        "native refusal message must name the native nature, got {:?}",
        err.message
    );
}

#[test]
fn rename_refuses_native_member_bare_call() {
    // `queue_free()` (implicit-self bare call to a native method). Line 2, `\tqueue_free()` —
    // `queue_free` at column 1. `textDocument/rename` must refuse with zero edits, for the native
    // reason (a green pass here is the corruption-firewall proof on the mutating path).
    let src = "extends Node\nfunc go() -> void:\n\tqueue_free()\n";
    let (client, server, main_uri, _project) = boot_native_member(src);
    assert_rename_refused_native(&client, 60, &main_uri, 2, 1);
    shutdown(&client, server);
}

#[test]
fn rename_refuses_native_member_typed_attribute_call() {
    // `n.queue_free()` through a typed var (`var n: Node`). The attribute identifier over a Native
    // base must refuse via `textDocument/rename` with zero edits — the mass-edit-an-engine-method
    // corruption case.
    let src = "extends Node\nfunc go() -> void:\n\tvar n: Node = self\n\tn.queue_free()\n";
    let (client, server, main_uri, _project) = boot_native_member(src);
    // Line 3 is `\tn.queue_free()`; `queue_free` starts after `\tn.` → column 3.
    assert_rename_refused_native(&client, 61, &main_uri, 3, 3);
    shutdown(&client, server);
}

// =================================================================================================
// BLOCKER-1 regression: the fail-OPEN holes the inverted (fail-closed) gate must now close. Each of
// these was proven to emit a corrupting edit with `error: None` under the old gate — a click on a
// builtin type, a @GlobalScope enum value, or a global utility, all of which `references` then
// raw-scanned in the current file. The fixed gate refuses each with -32803 + zero edits.
// =================================================================================================

#[test]
fn rename_refuses_builtin_typed_cursor() {
    // `var v: Vector2 = Vector2()` clicked on the `Vector2` TYPE annotation. `Vector2` is a builtin
    // (it lives in `builtin_named`, NOT `class_named` — the exact hole the old gate missed).
    let src = "extends Node\nfunc go() -> void:\n\tvar v: Vector2 = Vector2()\n";
    let (client, server, main_uri, _project) = boot_native_member_with_api(src, RICH_NATIVE_API);
    // Line 2 `\tvar v: Vector2 ...`: tab(col0) + `var v: `(cols1-7) → `Vector2` at col 8.
    assert_rename_refused_native(&client, 70, &main_uri, 2, 8);
    shutdown(&client, server);
}

#[test]
fn rename_refuses_global_enum_value() {
    // `var d = SIDE_LEFT` clicked on `SIDE_LEFT` — a @GlobalScope enum value (`global_enum_value`),
    // which `definition()` has no arm for, so the old gate passed and `references` edited the
    // engine constant in-file.
    let src = "extends Node\nfunc go() -> void:\n\tvar d = SIDE_LEFT\n";
    let (client, server, main_uri, _project) = boot_native_member_with_api(src, RICH_NATIVE_API);
    // Line 2 `\tvar d = SIDE_LEFT`: tab(col0) + `var d = `(cols1-8) → `SIDE_LEFT` at col 9.
    assert_rename_refused_native(&client, 71, &main_uri, 2, 9);
    shutdown(&client, server);
}

#[test]
fn rename_refuses_global_utility() {
    // `print("hi")` clicked on `print` — a @GlobalScope utility (`utility`). A bare callee that
    // classifies as Unresolved, so the old gate passed and `references` raw-scanned `print` in-file.
    let src = "extends Node\nfunc go() -> void:\n\tprint(\"hi\")\n";
    let (client, server, main_uri, _project) = boot_native_member_with_api(src, RICH_NATIVE_API);
    // Line 2 `\tprint("hi")`: tab(col0) → `print` at col 1.
    assert_rename_refused_native(&client, 72, &main_uri, 2, 1);
    shutdown(&client, server);
}

// =================================================================================================
// BLOCKER-2 regression: the NEW NAME side. Renaming a project `class_name` to an engine type or to
// an already-registered project `class_name` is a global-registry collision the same-file member
// check cannot see — both must refuse with -32602 + zero edits.
// =================================================================================================

/// Assert a rename refused with the INVALID-NAME code (-32602), zero edits — the new-name-side
/// (BLOCKER-2) refusal, distinct from the native-target -32803 refusal.
fn assert_rename_refused_invalid_name(
    client: &Connection,
    id: i32,
    uri: &Uri,
    line: u32,
    ch: u32,
    new_name: &str,
) {
    client
        .sender
        .send(request(
            id,
            "textDocument/rename",
            rename_params(uri, line, ch, new_name),
        ))
        .unwrap();
    let resp = recv_response(client);
    assert!(
        resp.error.is_some(),
        "rename to {new_name:?} must refuse (result={:?})",
        resp.result
    );
    assert!(
        resp.result.is_none(),
        "a refused rename must carry ZERO edits, got {:?}",
        resp.result
    );
    assert_eq!(
        resp.error.unwrap().code,
        -32602,
        "new-name collision must use InvalidParams (-32602)"
    );
}

#[test]
fn rename_class_to_native_type_refused() {
    // `class_name Hero` → `Node` (an engine class): would declare `class_name Node`, colliding with
    // the engine class. Refuse on the NEW-NAME side (-32602), zero edits.
    let src = "class_name Hero\nextends Node\n\nfunc attack() -> void:\n\tpass\n";
    let (client, server, main_uri, _project) = boot_native_member_with_api(src, RICH_NATIVE_API);
    // `class_name Hero`: `Hero` at col 11.
    assert_rename_refused_invalid_name(&client, 80, &main_uri, 0, 11, "Node");
    shutdown(&client, server);
}

#[test]
fn rename_class_to_existing_project_class_refused() {
    // `class_name Hero` → `Villain`, where `Villain` is ALREADY a project `class_name` in another
    // file: two files would declare the same global class. Refuse on the NEW-NAME side (-32602).
    let api = RICH_NATIVE_API;
    let project = TempProject::new();
    project.write("project.godot", "config_version=5\n");
    project.write("extension_api.json", api);
    project.write(
        "hero.gd",
        "class_name Hero\nextends Node\n\nfunc attack() -> void:\n\tpass\n",
    );
    project.write("villain.gd", "class_name Villain\nextends Node\n");
    let stub_cache = project.root.join("stub-cache");

    let (server, client) = Connection::memory();
    let handle = std::thread::spawn(move || gd_server::serve(server));
    let init = InitializeParams {
        initialization_options: Some(serde_json::json!({
            "projectRoot": project.root.as_str(),
            "extensionApiPath": project.root.join("extension_api.json").as_str(),
            "autoDumpExtensionApi": false,
            "stubCacheDir": stub_cache.as_str(),
        })),
        capabilities: serde_json::from_value(caps_full()).unwrap(),
        ..Default::default()
    };
    client.sender.send(request(1, "initialize", init)).unwrap();
    let _ = recv_response(&client);
    client
        .sender
        .send(notification("initialized", InitializedParams {}))
        .unwrap();
    let hero_uri = file_uri(&project.root.join("hero.gd"));
    let hero_text = std::fs::read_to_string(project.root.join("hero.gd").as_std_path()).unwrap();
    client
        .sender
        .send(notification(
            "textDocument/didOpen",
            DidOpenTextDocumentParams {
                text_document: TextDocumentItem {
                    uri: hero_uri.clone(),
                    language_id: "gdscript".to_string(),
                    version: 1,
                    text: hero_text,
                },
            },
        ))
        .unwrap();
    while common::try_recv(&client, std::time::Duration::from_millis(400)).is_some() {}

    // `class_name Hero`: `Hero` at col 11. Rename → `Villain` (already a project class_name).
    assert_rename_refused_invalid_name(&client, 81, &hero_uri, 0, 11, "Villain");

    // Sanity: renaming to a FREE class name still succeeds (the registry check is not over-broad).
    client
        .sender
        .send(request(
            82,
            "textDocument/rename",
            rename_params(&hero_uri, 0, 11, "Paladin"),
        ))
        .unwrap();
    let ok = recv_response(&client);
    assert!(
        ok.error.is_none(),
        "renaming a class to a free name must still succeed: {:?}",
        ok.error
    );

    shutdown(&client, handle);
}

// =================================================================================================
// BLOCKER-3 regression (re-review of the first fix): a native method on a NON-Native-typed base
// (`n.queue_free()` where `n` is untyped/Variant or script-typed) slipped the first fix's blanket
// method-role exemption and was raw-scanned project-wide. The positive call-callee anchor (the
// Binding::Call callee must resolve to a project Script file) closes it. Plus the over-refusal
// regression: a project symbol named like a @GlobalScope utility (`max`/`min`) must still rename —
// the project anchor is checked BEFORE the engine-name refusal, so it shadows the native name.
// =================================================================================================

#[test]
fn rename_refuses_native_method_untyped_base() {
    // `func go(n): n.queue_free()` — `n` is UNTYPED, so `definition` returns None (no Native type to
    // anchor) and signal 3 never fires. The first fix's blanket method-role `true` then waved this
    // through and `references` raw-scanned `queue_free` project-wide. The positive call-callee anchor
    // (callee has no project `script_file`) now refuses: ZERO edits, -32803.
    let src = "extends Node\nfunc go(n) -> void:\n\tn.queue_free()\n";
    let (client, server, main_uri, _project) = boot_native_member(src);
    // Line 2 `\tn.queue_free()`; `queue_free` after `\tn.` → column 3.
    client
        .sender
        .send(request(
            70,
            "textDocument/rename",
            rename_params(&main_uri, 2, 3, "release_me"),
        ))
        .unwrap();
    let resp = recv_response(&client);
    assert!(
        resp.error.is_some() && resp.result.is_none(),
        "a native method on an untyped base must refuse with ZERO edits (no project-wide raw-scan); \
         got result={:?}",
        resp.result
    );
    assert_eq!(
        resp.error.unwrap().code,
        -32803,
        "an unrenameable (non-project) target refuses with RequestFailed (-32803)"
    );
    shutdown(&client, server);
}

#[test]
fn rename_succeeds_on_project_local_named_like_utility() {
    // A project LOCAL named `max` — `db.utility("max")` is Some, so the first fix's context-free
    // signal-2 wrongly refused it. The fixed gate checks the project anchor (enclosing-function
    // local) FIRST, so the local shadows the native utility name and renames normally.
    let src = "extends Node\nfunc go() -> void:\n\tvar max := 5\n\tmax += 1\n\tprint(max)\n";
    let (client, server, main_uri, _project) = boot_native_member_with_api(src, RICH_NATIVE_API);
    // `var max` on line 2, col 5. Rename → `limit`.
    client
        .sender
        .send(request(
            71,
            "textDocument/rename",
            rename_params(&main_uri, 2, 5, "limit"),
        ))
        .unwrap();
    let resp = recv_response(&client);
    assert!(
        resp.error.is_none(),
        "a project local named `max` must rename (project anchor shadows the utility name): {:?}",
        resp.error
    );
    let edit: WorkspaceEdit =
        serde_json::from_value(resp.result.expect("rename returns a WorkspaceEdit")).unwrap();
    let view = flatten_edit(&edit);
    assert!(
        view.set.len() >= 2 && view.new_texts.iter().all(|t| t == "limit"),
        "rename of local `max` must edit its decl + uses to `limit`, got {:?}",
        view.set
    );
    shutdown(&client, server);
}

#[test]
fn rename_succeeds_on_project_member_named_like_utility() {
    // A project MEMBER named `min` (also a utility) must STILL rename — anchored via the in-file
    // member declaration, before the engine-name refusal.
    let src = "extends Node\nvar min: int = 0\nfunc go() -> void:\n\tmin += 1\n";
    let (client, server, main_uri, _project) = boot_native_member_with_api(src, RICH_NATIVE_API);
    // `var min` on line 1, col 4. Rename → `floor_value`.
    client
        .sender
        .send(request(
            72,
            "textDocument/rename",
            rename_params(&main_uri, 1, 4, "floor_value"),
        ))
        .unwrap();
    let resp = recv_response(&client);
    assert!(
        resp.error.is_none(),
        "a project member named `min` must rename (in-file member anchor): {:?}",
        resp.error
    );
    let edit: WorkspaceEdit =
        serde_json::from_value(resp.result.expect("rename returns a WorkspaceEdit")).unwrap();
    let view = flatten_edit(&edit);
    assert!(
        view.set.len() >= 2 && view.new_texts.iter().all(|t| t == "floor_value"),
        "rename of member `min` must edit its decl + use, got {:?}",
        view.set
    );
    shutdown(&client, server);
}
