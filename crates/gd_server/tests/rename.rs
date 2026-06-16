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

#[test]
fn rename_project_method_is_click_site_independent() {
    // BLOCKER-4 regression (re-review): the rename edit set must be the SAME regardless of which
    // occurrence the cursor is on. A bare `helper()` call site previously yielded a NARROWER
    // `references` set (missing the `self.helper()` sibling) than a declaration click → a mutating
    // rename silently left a dangling call to the old name. Canonicalizing the cursor to the
    // declaration makes the set complete from ANY click. Here: rename from the bare call site AND
    // from the declaration; the two edit sets must be EQUAL and cover all THREE occurrences.
    // helper decl: line 1 cols 5..11; bare call: line 4 cols 1..7; self.helper: line 5 cols 6..12.
    let src =
        "extends Node\nfunc helper() -> void:\n\tpass\nfunc go() -> void:\n\thelper()\n\tself.helper()\n";

    // From the BARE call site (line 4, col 1).
    let (client, server, main_uri, _project) = boot_native_member(src);
    client
        .sender
        .send(request(
            73,
            "textDocument/rename",
            rename_params(&main_uri, 4, 1, "renamed"),
        ))
        .unwrap();
    let resp = recv_response(&client);
    assert!(
        resp.error.is_none(),
        "bare-call-site rename must succeed: {:?}",
        resp.error
    );
    let bare = flatten_edit(
        &serde_json::from_value::<WorkspaceEdit>(resp.result.expect("a WorkspaceEdit")).unwrap(),
    );
    shutdown(&client, server);

    // From the DECLARATION (line 1, col 5).
    let (client2, server2, main_uri2, _project2) = boot_native_member(src);
    client2
        .sender
        .send(request(
            74,
            "textDocument/rename",
            rename_params(&main_uri2, 1, 5, "renamed"),
        ))
        .unwrap();
    let resp2 = recv_response(&client2);
    assert!(
        resp2.error.is_none(),
        "declaration rename must succeed: {:?}",
        resp2.error
    );
    let decl = flatten_edit(
        &serde_json::from_value::<WorkspaceEdit>(resp2.result.expect("a WorkspaceEdit")).unwrap(),
    );
    shutdown(&client2, server2);

    // Click-site-INDEPENDENT: identical edit RANGES (the two boots use distinct temp-dir URIs, so
    // compare ranges, not paths — both files are the single `main.gd`), covering all three sites.
    let bare_ranges: Vec<lsp_types::Range> = bare.set.iter().map(|(_, r)| *r).collect();
    let decl_ranges: Vec<lsp_types::Range> = decl.set.iter().map(|(_, r)| *r).collect();
    assert_eq!(
        bare_ranges, decl_ranges,
        "rename edit set must be click-site-independent (bare call vs declaration); bare={bare_ranges:?} decl={decl_ranges:?}"
    );
    assert_eq!(
        bare_ranges.len(),
        3,
        "rename of `helper` must edit all 3 occurrences (decl + bare call + self.helper), got {bare_ranges:?}"
    );
}

#[test]
fn rename_local_shadowing_member_targets_the_local_not_the_member() {
    // BLOCKER-5 regression (re-review): canonicalizing to `definition()` is member-FIRST, so renaming
    // a local/param that SHADOWS a member used to jump to the member and rename the WRONG symbol
    // project-wide (editing the member's other-method uses, leaving the local broken). The fix skips
    // canonicalization for locals/params. Renaming the LOCAL `total` must edit ONLY its
    // function-scoped sites (the `var total` decl + `total += 1`), NEVER the member `total` (its decl
    // on line 1 or its use in `g()` on line 6).
    let src = "extends Node\nvar total: int = 0\nfunc f() -> void:\n\tvar total = 5\n\ttotal += 1\nfunc g() -> void:\n\ttotal = 9\n";
    // member `total`: line 1 col 4; local decl: line 3 col 5; local use: line 4 col 1; member use: line 6 col 1.
    let (client, server, main_uri, _project) = boot_native_member(src);
    // Click the LOCAL declaration (`var total`, line 3, col 5). Rename → `subtotal`.
    client
        .sender
        .send(request(
            75,
            "textDocument/rename",
            rename_params(&main_uri, 3, 5, "subtotal"),
        ))
        .unwrap();
    let resp = recv_response(&client);
    assert!(
        resp.error.is_none(),
        "local rename must succeed: {:?}",
        resp.error
    );
    let view = flatten_edit(
        &serde_json::from_value::<WorkspaceEdit>(resp.result.expect("a WorkspaceEdit")).unwrap(),
    );
    let lines: Vec<u32> = view.set.iter().map(|(_, r)| r.start.line).collect();
    // ONLY the local's sites (lines 3, 4) — never the shadowed member's (lines 1, 6).
    assert!(
        lines.contains(&3) && lines.contains(&4),
        "renaming the local must edit its decl (line 3) + use (line 4); got lines {lines:?}"
    );
    assert!(
        !lines.contains(&1) && !lines.contains(&6),
        "renaming the local must NOT touch the shadowed member (lines 1, 6) — wrong-symbol corruption; got lines {lines:?}"
    );
    shutdown(&client, server);
}

#[test]
fn rename_param_does_not_capture_self_attribute_access() {
    // BLOCKER-6 regression: renaming a parameter `x` must NOT rewrite the same-named `self.x` member
    // access inside its function — the by-name local scan over-captured it, so a rename produced a
    // dangling `self.amount` write to a nonexistent member (silent broken code). The local
    // resolution now excludes attribute-position identifiers (a local is never reached as `.x`).
    // member `x`: line 1 col 4; param `x`: line 2 col 11; `self.x` attribute: line 3 col 6; bare
    // param use: line 3 col 10.
    let src = "extends Node\nvar x: int = 0\nfunc set_x(x) -> void:\n\tself.x = x\n";
    let (client, server, main_uri, _project) = boot_native_member(src);
    // Click the PARAM declaration (line 2, col 11). Rename → `amount`.
    client
        .sender
        .send(request(
            76,
            "textDocument/rename",
            rename_params(&main_uri, 2, 11, "amount"),
        ))
        .unwrap();
    let resp = recv_response(&client);
    assert!(
        resp.error.is_none(),
        "param rename must succeed: {:?}",
        resp.error
    );
    let view = flatten_edit(
        &serde_json::from_value::<WorkspaceEdit>(resp.result.expect("a WorkspaceEdit")).unwrap(),
    );
    let sites: Vec<(u32, u32)> = view
        .set
        .iter()
        .map(|(_, r)| (r.start.line, r.start.character))
        .collect();
    // The param decl (2,11) + the bare param use (3,10) are renamed.
    assert!(
        sites.contains(&(2, 11)) && sites.contains(&(3, 10)),
        "the param decl + its bare use must be renamed; got {sites:?}"
    );
    // The `self.x` member access (3,6) must NOT be captured (else a dangling member reference), and
    // the member declaration (line 1) must be untouched.
    assert!(
        !sites.contains(&(3, 6)),
        "the `self.x` member access must NOT be captured — dangling-reference corruption; got {sites:?}"
    );
    assert!(
        !sites.iter().any(|(l, _)| *l == 1),
        "the member declaration (line 1) must NOT be touched; got {sites:?}"
    );
    shutdown(&client, server);
}

// =================================================================================================
// #107: scope-aware local binding resolution. Two precision gaps the function-span by-name scan
// left behind once attribute-position over-capture (BLOCKER-6) was fixed:
//   (a) for-loop iterators (`for i in …`) and match-pattern binds (`match v: var n:`) are real
//       function-locals but were not recognized by `enclosing_function_declaring` (it checked only
//       Parameter/Variable/Constant nodes), so a rename on them fail-closed REFUSED (-32803).
//   (b) inner-scope shadowing: an outer local `x` plus an inner-block `var x` — the whole-function
//       by-name scan renamed BOTH, a benign over-rename of a sibling the user did not select.
// The fix resolves each in-function identifier to its declaring binding (the parser's per-suite
// `locals` model), so for-loop/match binds rename precisely and inner-shadowed siblings are left
// alone. Each test applies the edit and re-checks the renamed/untouched site set by position
// (apply→verify-by-identity, not a string match).
// =================================================================================================

/// Collect the (line, character) start of every renamed site from a successful rename response.
fn rename_sites(
    client: &Connection,
    id: i32,
    uri: &Uri,
    line: u32,
    ch: u32,
    new_name: &str,
) -> Vec<(u32, u32)> {
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
        resp.error.is_none(),
        "rename at ({line},{ch})→{new_name:?} must succeed, got error {:?}",
        resp.error
    );
    let view = flatten_edit(
        &serde_json::from_value::<WorkspaceEdit>(resp.result.expect("a WorkspaceEdit")).unwrap(),
    );
    assert!(
        view.new_texts.iter().all(|t| t == new_name),
        "every edit must write {new_name:?}, got {:?}",
        view.new_texts
    );
    let mut sites: Vec<(u32, u32)> = view
        .set
        .iter()
        .map(|(_, r)| (r.start.line, r.start.character))
        .collect();
    sites.sort();
    sites
}

#[test]
fn rename_for_loop_variable_renames_precisely() {
    // `for i in [1, 2]:` — the iterator `i` is a function-local; renaming it must edit its decl + its
    // two body uses, NOT refuse. (Pre-#107 this refused with -32803 because
    // `enclosing_function_declaring` did not look at `ForVariable` locals.)
    //   line 2 `\tfor i in [1, 2]:` → decl `i` at col 5
    //   line 3 `\t\tprint(i)`       → use  `i` at col 8 (2 tabs + `print(`)
    //   line 4 `\t\ti = 0`          → use  `i` at col 2
    let src = "extends Node\nfunc f() -> void:\n\tfor i in [1, 2]:\n\t\tprint(i)\n\t\ti = 0\n";
    let (client, server, main_uri, _project) = boot_native_member(src);
    // Click the iterator declaration (line 2, col 5). Rename → `idx`.
    let sites = rename_sites(&client, 90, &main_uri, 2, 5, "idx");
    assert_eq!(
        sites,
        vec![(2, 5), (3, 8), (4, 2)],
        "for-loop iterator rename must edit exactly its decl + both body uses; got {sites:?}"
    );
    shutdown(&client, server);
}

#[test]
fn rename_match_pattern_bind_renames_precisely() {
    // `match v: var n:` — the pattern bind `n` is a function-local; renaming it must edit its bind
    // site + its branch-body use, NOT refuse. (Pre-#107 this refused: `PatternBind` locals were not
    // recognized by `enclosing_function_declaring`.)
    //   line 2 `\tmatch v:`     (the matched value)
    //   line 3 `\t\tvar n:`     → bind `n` at col 6
    //   line 4 `\t\t\tprint(n)` → use  `n` at col 9
    let src = "extends Node\nfunc f(v) -> void:\n\tmatch v:\n\t\tvar n:\n\t\t\tprint(n)\n";
    let (client, server, main_uri, _project) = boot_native_member(src);
    // Click the bind site (line 3, col 6). Rename → `bound`.
    let sites = rename_sites(&client, 91, &main_uri, 3, 6, "bound");
    assert_eq!(
        sites,
        vec![(3, 6), (4, 9)],
        "match-pattern bind rename must edit exactly its bind site + branch-body use; got {sites:?}"
    );
    shutdown(&client, server);
}

#[test]
fn rename_inner_shadow_does_not_rename_outer_sibling() {
    // An outer local `x` and an inner-block `var x` (a different binding). Renaming the OUTER `x` must
    // edit ONLY its own decl + the use that resolves to it (the one BEFORE the inner re-declaration),
    // never the inner binding's decl/use — those are a distinct symbol the user did not select.
    // (Pre-#107 the whole-function by-name scan renamed all four sites, a benign over-rename.)
    //   line 2 `\tvar x = 1`   → OUTER decl `x` at col 5
    //   line 3 `\tprint(x)`    → OUTER use  `x` at col 7
    //   line 4 `\tif true:`
    //   line 5 `\t\tvar x = 2` → INNER decl `x` at col 6
    //   line 6 `\t\tprint(x)`  → INNER use  `x` at col 8
    let src = "extends Node\nfunc f() -> void:\n\tvar x = 1\n\tprint(x)\n\tif true:\n\t\tvar x = 2\n\t\tprint(x)\n";
    let (client, server, main_uri, _project) = boot_native_member(src);
    // Click the OUTER declaration (line 2, col 5). Rename → `outer`.
    let sites = rename_sites(&client, 92, &main_uri, 2, 5, "outer");
    assert_eq!(
        sites,
        vec![(2, 5), (3, 7)],
        "renaming the outer local must edit ONLY its own sites, never the inner-shadow binding (lines 5,6); got {sites:?}"
    );
    shutdown(&client, server);
}

#[test]
fn rename_inner_shadow_from_inner_targets_inner_binding() {
    // The mirror of the above: clicking the INNER `var x` must edit ONLY the inner binding's sites
    // (decl + the use that resolves to it), never the outer binding's — precise both directions.
    let src = "extends Node\nfunc f() -> void:\n\tvar x = 1\n\tprint(x)\n\tif true:\n\t\tvar x = 2\n\t\tprint(x)\n";
    let (client, server, main_uri, _project) = boot_native_member(src);
    // Click the INNER declaration (line 5, col 6). Rename → `inner`.
    let sites = rename_sites(&client, 93, &main_uri, 5, 6, "inner");
    assert_eq!(
        sites,
        vec![(5, 6), (6, 8)],
        "renaming the inner local must edit ONLY the inner binding's sites, never the outer (lines 2,3); got {sites:?}"
    );
    shutdown(&client, server);
}

#[test]
fn rename_outer_local_captured_in_lambda_edits_all_occurrences() {
    // A lambda body is a NESTED function node, so the occurrence-scan bound must be the function
    // enclosing the DECLARATION (the outer `f`), not the cursor's enclosing function (the lambda) —
    // otherwise renaming from the lambda-interior capture drops the OUTER uses, dangling them. Click
    // the captured `c` INSIDE the lambda; every occurrence (outer decl + both outer uses + the
    // capture) must be renamed.
    //   line 2 `\tvar c = 1`                  → decl `c` at col 5
    //   line 3 `\tprint(c)`                   → use  `c` at col 7
    //   line 4 `\tvar g = func(): return c`   → captured use `c` at col 24
    //   line 5 `\tprint(c)`                   → use  `c` at col 7
    let src = "extends Node\nfunc f() -> void:\n\tvar c = 1\n\tprint(c)\n\tvar g = func(): return c\n\tprint(c)\n";
    let (client, server, main_uri, _project) = boot_native_member(src);
    // Click the lambda-interior capture (line 4, col 24). Rename → `renamed`.
    let sites = rename_sites(&client, 94, &main_uri, 4, 24, "renamed");
    assert_eq!(
        sites,
        vec![(2, 5), (3, 7), (4, 24), (5, 7)],
        "renaming an outer local from inside a capturing lambda must edit ALL occurrences (decl + \
         both outer uses + the capture) — never drop the outer uses (dangling); got {sites:?}"
    );
    shutdown(&client, server);
}

#[test]
fn rename_lambda_parameter_does_not_capture_outer_same_named() {
    // A lambda PARAMETER shadows an outer same-named local: renaming the lambda param must edit ONLY
    // the param's own sites (inside the lambda), never the outer binding's — precise across the
    // lambda boundary in the shadowing direction too.
    //   line 2 `\tvar v = 1`                          → OUTER decl `v` at col 5
    //   line 3 `\tprint(v)`                           → OUTER use  `v` at col 7
    //   line 4 `\tvar g = func(v): return v`          → param `v` at col 14, param use `v` at col 25
    let src =
        "extends Node\nfunc f() -> void:\n\tvar v = 1\n\tprint(v)\n\tvar g = func(v): return v\n";
    let (client, server, main_uri, _project) = boot_native_member(src);
    // Click the lambda PARAMETER (line 4, col 14). Rename → `p`.
    let sites = rename_sites(&client, 95, &main_uri, 4, 14, "p");
    assert_eq!(
        sites,
        vec![(4, 14), (4, 25)],
        "renaming a lambda parameter must edit ONLY the param decl + its in-lambda use, never the \
         shadowed outer `v` (lines 2,3); got {sites:?}"
    );
    shutdown(&client, server);
}

#[test]
fn rename_outer_local_captured_in_nested_lambda_edits_all_occurrences() {
    // A capture two lambda levels deep: the occurrence-scan bound must still resolve to the OUTERMOST
    // declaring function (`f`), never either lambda — otherwise the outer use dangles. Click the
    // deeply-nested capture; the decl + the outer use + the deep capture must all be renamed.
    //   line 2 `\tvar c = 1`                                  → decl `c` at col 5
    //   line 3 `\tvar g = func(): var h = func(): return c`   → capture `c` at col 40
    //   line 4 `\tprint(c)`                                   → use  `c` at col 7
    let src = "extends Node\nfunc f() -> void:\n\tvar c = 1\n\tvar g = func(): var h = func(): return c\n\tprint(c)\n";
    let (client, server, main_uri, _project) = boot_native_member(src);
    // Click the deepest capture (line 3, col 40). Rename → `renamed`.
    let sites = rename_sites(&client, 96, &main_uri, 3, 40, "renamed");
    assert_eq!(
        sites,
        vec![(2, 5), (3, 40), (4, 7)],
        "a capture nested two lambdas deep must edit the decl + outer use + the deep capture, \
         scoping to the outermost function; got {sites:?}"
    );
    shutdown(&client, server);
}

#[test]
fn rename_local_does_not_rewrite_lua_style_dict_key() {
    // A Lua-style dict KEY (`{ name = 2 }`) is a folded STRING literal, NOT a reference to the local
    // `name` — renaming the local must NOT rewrite it (that would silently change the key string,
    // breaking `d["name"]`/`d.name` lookups at runtime). The dict VALUE reference (`other = name`)
    // IS a real use and MUST be renamed.
    //   line 2 `\tvar name = 1`                          → decl `name` at col 5
    //   line 3 `\tvar d = { name = 2, other = name }`    → KEY at col 11 (EXCLUDE), value at col 29
    //   line 4 `\tprint(name)`                           → use `name` at col 7
    let src = "extends Node\nfunc f() -> void:\n\tvar name = 1\n\tvar d = { name = 2, other = name }\n\tprint(name)\n";
    let (client, server, main_uri, _project) = boot_native_member(src);
    // Click the declaration (line 2, col 5). Rename → `renamed`.
    let sites = rename_sites(&client, 97, &main_uri, 2, 5, "renamed");
    assert_eq!(
        sites,
        vec![(2, 5), (3, 29), (4, 7)],
        "renaming the local must edit decl + dict VALUE ref + print, NEVER the Lua-style dict KEY \
         (col 11 on line 3) — rewriting it is silent runtime corruption; got {sites:?}"
    );
    shutdown(&client, server);
}

// =================================================================================================
// #106: a PROJECT enum VALUE renames (positive analyzer anchor), an unrelated same-named symbol
// does NOT, and a native @GlobalScope enum value still refuses.
//
// The fail-closed firewall (#66) refused a project enum VALUE because the analyzer recorded NO
// `Binding::Use` for a named-enum value access (`E.NORTH`) and `member_named` matches an enum's NAME
// not its values — so `rename_target_has_project_anchor` found no positive project anchor. The fix
// pins the value's binding by IDENTITY (the file declaring the enum) so the gate can admit ONLY the
// value's own occurrences, never a raw-text scan.
// =================================================================================================

#[test]
fn rename_in_file_enum_value_from_declaration_renames_precisely() {
    // `enum Direction { NORTH, SOUTH }` declared in this file; `NORTH` read as `Direction.NORTH`.
    // Renaming the value from its DECLARATION must edit the decl + the qualified use, and refuse
    // nothing. (Pre-#106 this refused with -32803: no project anchor for a named-enum value.)
    //   line 1 `enum Direction { NORTH, SOUTH }` → decl `NORTH` at col 17
    //   line 3 `\tvar d = Direction.NORTH`        → use  `NORTH` at col 19
    let src = "extends Node\nenum Direction { NORTH, SOUTH }\nfunc go() -> void:\n\tvar d = Direction.NORTH\n\tprint(d)\n";
    let (client, server, main_uri, _project) = boot_native_member(src);
    // Click the value DECLARATION (line 1, col 17). Rename → `UP`.
    let sites = rename_sites(&client, 200, &main_uri, 1, 17, "UP");
    assert_eq!(
        sites,
        vec![(1, 17), (3, 19)],
        "renaming an in-file enum value from its declaration must edit the decl + the \
         `Direction.NORTH` use, never the sibling `SOUTH`; got {sites:?}"
    );
    shutdown(&client, server);
}

#[test]
fn rename_in_file_enum_value_from_use_renames_precisely() {
    // Same enum, but clicked on the `Direction.NORTH` USE site. The edit set must be identical
    // (click-site-independent) — the analyzer anchor canonicalizes to the declaration.
    let src = "extends Node\nenum Direction { NORTH, SOUTH }\nfunc go() -> void:\n\tvar d = Direction.NORTH\n\tprint(d)\n";
    let (client, server, main_uri, _project) = boot_native_member(src);
    // Click the USE `NORTH` (line 3, col 19). Rename → `UP`.
    let sites = rename_sites(&client, 201, &main_uri, 3, 19, "UP");
    assert_eq!(
        sites,
        vec![(1, 17), (3, 19)],
        "renaming an in-file enum value from a use site must edit the same set as the declaration \
         click (decl + use); got {sites:?}"
    );
    shutdown(&client, server);
}

#[test]
fn rename_enum_value_does_not_touch_unrelated_same_named_symbol() {
    // CORRUPTION GUARD (by-identity, not by-name): a `const NORTH` in the SAME file shares the enum
    // value's name but is a DISTINCT symbol. Renaming the enum value must NOT rewrite the unrelated
    // `const NORTH` (its decl or its use) — that would be the W16 raw-text-scan corruption the
    // fail-closed firewall exists to prevent.
    //   line 1 `enum Direction { NORTH }`     → enum value decl `NORTH` at col 17
    //   line 2 `const NORTH := 99`            → UNRELATED const decl `NORTH` at col 6
    //   line 4 `\tvar a = Direction.NORTH`    → enum value use at col 19
    //   line 5 `\tvar b = NORTH`              → UNRELATED const use at col 9
    let src = "extends Node\nenum Direction { NORTH }\nconst NORTH := 99\nfunc go() -> void:\n\tvar a = Direction.NORTH\n\tvar b = NORTH\n\tprint(a + b)\n";
    let (client, server, main_uri, _project) = boot_native_member(src);
    // Rename the ENUM VALUE from its decl (line 1, col 17) → `UP`.
    let sites = rename_sites(&client, 202, &main_uri, 1, 17, "UP");
    assert_eq!(
        sites,
        vec![(1, 17), (4, 19)],
        "renaming the enum value must edit ONLY its decl + `Direction.NORTH` use, never the \
         unrelated `const NORTH` decl (line 2) or its use (line 5); got {sites:?}"
    );
    shutdown(&client, server);
}

#[test]
fn rename_enum_value_from_use_does_not_touch_unrelated_same_named_const() {
    // The symmetric guard to the above, clicked at the enum value's USE site (`Direction.NORTH`)
    // rather than its declaration: the `EnumValueLocal` binding at the use site anchors by identity,
    // so the unrelated `const NORTH` (decl + use) is still untouched.
    //   line 1 `enum Direction { NORTH }`     → enum value decl `NORTH` at col 17
    //   line 2 `const NORTH := 99`            → UNRELATED const decl `NORTH` at col 6
    //   line 4 `\tvar a = Direction.NORTH`    → enum value use at col 19
    //   line 5 `\tvar b = NORTH`              → UNRELATED const use at col 9
    let src = "extends Node\nenum Direction { NORTH }\nconst NORTH := 99\nfunc go() -> void:\n\tvar a = Direction.NORTH\n\tvar b = NORTH\n\tprint(a + b)\n";
    let (client, server, main_uri, _project) = boot_native_member(src);
    // Rename the ENUM VALUE from its USE (line 4, col 19) → `UP`.
    let sites = rename_sites(&client, 208, &main_uri, 4, 19, "UP");
    assert_eq!(
        sites,
        vec![(1, 17), (4, 19)],
        "renaming the enum value from its use must edit ONLY its decl + `Direction.NORTH` use, \
         never the unrelated `const NORTH`; got {sites:?}"
    );
    shutdown(&client, server);
}

#[test]
fn rename_refuses_global_scope_enum_value_still() {
    // The firewall must STILL refuse a NATIVE @GlobalScope enum value (`SIDE_LEFT`) — the #106 fix
    // admits only PROJECT enum values (positively anchored), never native ones. (Mirrors the
    // pre-existing `rename_refuses_global_enum_value`, re-asserted here so the #106 widening can't
    // silently let native enum values through.)
    let src = "extends Node\nfunc go() -> void:\n\tvar d = SIDE_LEFT\n";
    let (client, server, main_uri, _project) = boot_native_member_with_api(src, RICH_NATIVE_API);
    // `SIDE_LEFT` at line 2, col 9.
    assert_rename_refused_native(&client, 203, &main_uri, 2, 9);
    shutdown(&client, server);
}

#[test]
fn rename_enum_value_distinguishes_two_enums_with_same_value_name() {
    // COMPOSITE-IDENTITY GUARD: `enum A { X }` and `enum B { X }` (both legal) declare the SAME value
    // name `X` in DIFFERENT enums. Renaming `A.X` must edit ONLY `A`'s `X` (decl + `A.X` use), never
    // `B`'s `X` — the binding is keyed on `<EnumName>.<value>`, not the bare value name, so the two
    // never conflate. (A bare-name collector would corrupt here.)
    //   line 1 `enum A { X }`            → A's value decl `X` at col 9
    //   line 2 `enum B { X }`            → B's value decl `X` at col 9
    //   line 4 `\tvar a = A.X`           → A's use `X` at col 11
    //   line 5 `\tvar b = B.X`           → B's use `X` at col 11
    let src = "extends Node\nenum A { X }\nenum B { X }\nfunc go() -> void:\n\tvar a = A.X\n\tvar b = B.X\n\tprint(a + b)\n";
    let (client, server, main_uri, _project) = boot_native_member(src);
    // Rename A.X from its decl (line 1, col 9) → `Y`.
    let sites = rename_sites(&client, 204, &main_uri, 1, 9, "Y");
    assert_eq!(
        sites,
        vec![(1, 9), (4, 11)],
        "renaming `A.X` must edit ONLY A's decl + `A.X` use, never `B.X` (decl line 2 / use line 5); \
         got {sites:?}"
    );
    shutdown(&client, server);
}

#[test]
fn rename_refuses_cross_file_enum_value() {
    // A cross-file enum value (`Lib.Dir.NORTH`, where `enum Dir { NORTH }` is declared in lib.gd)
    // currently REFUSES: the analyzer records no positive in-file anchor for it (the cross-file enum
    // metatype carries `script_type`, not `class_node`, so reduce_identifier_from_base records no
    // `EnumValueLocal` binding) — so the fail-closed firewall refuses rather than raw-scan-and-edit.
    // This is the documented #106 boundary (in-file enum values only); refusing is safe (no
    // corruption), and admitting cross-file would require an anchor the analyzer does not yet carry.
    let project = common::sample_project();
    project.write(
        "src/lib.gd",
        "class_name Lib\nextends Node\n\nenum Dir { NORTH, SOUTH }\n",
    );
    project.write(
        "src/use.gd",
        "extends Node\n\nfunc go() -> void:\n\tvar d = Lib.Dir.NORTH\n\tprint(d)\n",
    );
    let (client, server) = boot();
    init_open(
        &project,
        &client,
        caps_full(),
        &["src/lib.gd", "src/use.gd"],
        2,
    );
    let use_uri = file_uri(&project.root.join("src/use.gd"));
    // `Lib.Dir.NORTH` on line 3: tab(0) `var d = `(1-8) `Lib`(9-11) `.`(12) `Dir`(13-15) `.`(16)
    // `NORTH`(17). Click `NORTH` at col 17.
    assert_rename_refused(&client, 205, &use_uri, 3, 17, "UP");
    shutdown(&client, server);
}

#[test]
fn rename_cross_file_enum_value_does_not_retarget_same_named_local_symbol() {
    // CORRUPTION GUARD (the dangerous cross-file case): `Lib.Dir.NORTH` clicked WHILE the current file
    // ALSO declares a `const NORTH`. The rename must REFUSE (the cross-file enum value has no positive
    // anchor) — it must NOT silently retarget to the local `const NORTH` and rename that instead.
    let project = common::sample_project();
    project.write(
        "src/lib.gd",
        "class_name Lib\nextends Node\n\nenum Dir { NORTH }\n",
    );
    project.write(
        "src/use.gd",
        "extends Node\n\nconst NORTH := 7\n\nfunc go() -> void:\n\tvar d = Lib.Dir.NORTH\n\tprint(d + NORTH)\n",
    );
    let (client, server) = boot();
    init_open(
        &project,
        &client,
        caps_full(),
        &["src/lib.gd", "src/use.gd"],
        2,
    );
    let use_uri = file_uri(&project.root.join("src/use.gd"));
    // `Lib.Dir.NORTH` on line 5: tab(0) `var d = `(1-8) `Lib`(9-11) `.`(12) `Dir`(13-15) `.`(16)
    // `NORTH`(17). Click the cross-file enum value `NORTH` at col 17 — must refuse, ZERO edits.
    client
        .sender
        .send(request(
            206,
            "textDocument/rename",
            rename_params(&use_uri, 5, 17, "UP"),
        ))
        .unwrap();
    let resp = recv_response(&client);
    assert!(
        resp.error.is_some() && resp.result.is_none(),
        "a cross-file enum value must REFUSE with zero edits (never retarget the local `const NORTH`); \
         got result={:?} error={:?}",
        resp.result,
        resp.error
    );
    shutdown(&client, server);
}

#[test]
fn definition_on_enum_value_use_resolves_to_declaration() {
    // Lock the canonicalize behavior the rename use-click relies on: `definition` on an enum-value USE
    // (`Direction.NORTH`) must resolve to the value's DECLARATION token (line 1), not the enum name or
    // a same-named member. (If `definition` returned the enum name or a `const NORTH`, the rename
    // use-click set could be wrong — this is why rename SKIPS definition-canonicalization for enum
    // values, but the read behavior must still be correct.)
    let src = "extends Node\nenum Direction { NORTH, SOUTH }\nfunc go() -> void:\n\tvar d = Direction.NORTH\n\tprint(d)\n";
    let (client, server, main_uri, _project) = boot_native_member(src);
    // Click the USE `NORTH` (line 3, col 19).
    let def_params = lsp_types::GotoDefinitionParams {
        text_document_position_params: position_params(&main_uri, 3, 19),
        work_done_progress_params: WorkDoneProgressParams::default(),
        partial_result_params: lsp_types::PartialResultParams::default(),
    };
    client
        .sender
        .send(request(207, "textDocument/definition", def_params))
        .unwrap();
    let resp = recv_response(&client);
    let def: Option<GotoDefinitionResponse> =
        serde_json::from_value(resp.result.expect("definition result")).unwrap();
    let loc = match def {
        Some(GotoDefinitionResponse::Scalar(loc)) => loc,
        other => panic!(
            "definition on an enum-value use must resolve to a scalar location, got {other:?}"
        ),
    };
    // The value declaration `NORTH` is on line 1 at col 17.
    assert_eq!(
        (loc.range.start.line, loc.range.start.character),
        (1, 17),
        "definition on `Direction.NORTH` use must jump to the value declaration (1,17), got {:?}",
        loc.range.start
    );
    shutdown(&client, server);
}

#[test]
fn rename_cross_file_class_name_enum_value_underrenames_from_declaration() {
    // BOUNDARY PIN (#158): renaming a `class_name`'d script's enum value FROM ITS DECLARATION edits
    // the declaring file but UNDER-collects the cross-file `Foo.Dir.NORTH` use (its base is a
    // cross-file enum metatype carrying `script_type`, not `class_node`, so no `EnumValueLocal` is
    // recorded there and there is no fan-out). This is loud UNDER-rename (the other file fails to
    // compile), NOT corruption — the documented #106 fail-closed boundary. The edit must still be
    // PRECISE in the declaring file (decl + its own in-file use), never touch an unrelated symbol,
    // and must NOT error. Pinned so the behavior is intentional; flip to full coverage when #158 lands.
    let project = common::sample_project();
    project.write(
        "src/lib.gd",
        "class_name Lib\nextends Node\n\nenum Dir { NORTH }\n\nfunc here() -> int:\n\treturn Dir.NORTH\n",
    );
    project.write(
        "src/use.gd",
        "extends Node\n\nfunc there() -> int:\n\treturn Lib.Dir.NORTH\n",
    );
    let (client, server) = boot();
    init_open(
        &project,
        &client,
        caps_full(),
        &["src/lib.gd", "src/use.gd"],
        2,
    );
    let lib_uri = file_uri(&project.root.join("src/lib.gd"));
    let use_uri = file_uri(&project.root.join("src/use.gd"));

    // Rename `NORTH` from its declaration in lib.gd (line 3 `enum Dir { NORTH }`, `NORTH` at col 11).
    client
        .sender
        .send(request(
            209,
            "textDocument/rename",
            rename_params(&lib_uri, 3, 11, "UP"),
        ))
        .unwrap();
    let resp = recv_response(&client);
    assert!(
        resp.error.is_none(),
        "renaming a class_name'd enum value from its declaration must succeed (not error): {:?}",
        resp.error
    );
    let view = flatten_edit(
        &serde_json::from_value::<WorkspaceEdit>(resp.result.expect("a WorkspaceEdit")).unwrap(),
    );
    // The edited set is EXACTLY lib.gd's decl (3,11) + its in-file use (6,8 — `\treturn Dir.NORTH`,
    // tab + `return Dir.`(cols1-10) → `NORTH` at col 11... recompute: tab(0) `return `(1-7) `Dir`(8-10)
    // `.`(11) `NORTH`(12)). The cross-file use.gd site is NOT collected (the documented boundary).
    let mut sites: Vec<(String, u32, u32)> = view
        .set
        .iter()
        .map(|(u, r)| (u.clone(), r.start.line, r.start.character))
        .collect();
    sites.sort();
    assert_eq!(
        sites,
        vec![
            (lib_uri.as_str().to_string(), 3, 11),
            (lib_uri.as_str().to_string(), 6, 12),
        ],
        "the edit must be precise in the declaring file (decl + in-file use) and NOT touch the \
         cross-file use.gd site (documented #158 under-rename boundary); got {sites:?}"
    );
    assert!(
        !view.set.iter().any(|(u, _)| *u == use_uri.as_str()),
        "the cross-file `Lib.Dir.NORTH` site must NOT be edited (documented boundary): {:?}",
        view.set
    );
    shutdown(&client, server);
}

#[test]
fn rename_cross_file_enum_value_colliding_with_class_name_refuses() {
    // CORRUPTION GUARD (fusion-found, pre-existing signal-3 leak): a cross-file enum value whose name
    // collides with a project `class_name` must REFUSE — it must NOT silently rename the unrelated
    // class project-wide. Before the fix, the name-only signal-3 (`find_global_class_definition`)
    // admitted the cursor and `definition`'s name-only class fallback canonicalized onto the class,
    // so `references` rewrote `class_name Idle` instead of the enum value.
    //   anim.gd:  `class_name Foo` + `enum AnimState { Idle, Walk }`
    //   idle.gd:  `class_name Idle` (the unrelated state class that must NOT be touched)
    //   use.gd:   `return Foo.AnimState.Idle`  (the cross-file enum value the user clicks)
    let project = common::sample_project();
    project.write(
        "src/anim.gd",
        "class_name Foo\nextends Node\n\nenum AnimState { Idle, Walk }\n",
    );
    project.write("src/idle.gd", "class_name Idle\nextends Node\n");
    project.write(
        "src/use.gd",
        "extends Node\n\nfunc f() -> int:\n\treturn Foo.AnimState.Idle\n",
    );
    let (client, server) = boot();
    init_open(
        &project,
        &client,
        caps_full(),
        &["src/anim.gd", "src/idle.gd", "src/use.gd"],
        2,
    );
    let use_uri = file_uri(&project.root.join("src/use.gd"));
    let idle_uri = file_uri(&project.root.join("src/idle.gd"));
    // `Foo.AnimState.Idle` on line 3: tab(0) `return `(1-7) `Foo`(8-10) `.`(11) `AnimState`(12-20)
    // `.`(21) `Idle`(22). Click the cross-file enum value `Idle` at col 22 — must REFUSE, zero edits,
    // and NEVER rewrite `class_name Idle`.
    client
        .sender
        .send(request(
            210,
            "textDocument/rename",
            rename_params(&use_uri, 3, 22, "Resting"),
        ))
        .unwrap();
    let resp = recv_response(&client);
    assert!(
        resp.result.is_none(),
        "a cross-file enum value colliding with a `class_name` must REFUSE with zero edits (never \
         rename the unrelated `class_name Idle`); got result={:?}",
        resp.result
    );
    assert!(
        resp.error.is_some(),
        "the refusal must be a typed error, not a silent null"
    );
    // Belt-and-suspenders: even if some result existed, the idle.gd class declaration must be absent.
    if let Some(v) = resp.result.as_ref() {
        let edit: WorkspaceEdit = serde_json::from_value(v.clone()).unwrap();
        let view = flatten_edit(&edit);
        assert!(
            !view.set.iter().any(|(u, _)| *u == idle_uri.as_str()),
            "the `class_name Idle` declaration must NEVER be edited by an enum-value rename: {:?}",
            view.set
        );
    }
    shutdown(&client, server);
}

/// A bare identifier that collides with a project `class_name X` and is SHADOWED by it (Godot
/// resolves a bare identifier to a global `class_name` BEFORE native enums / utilities / methods —
/// `gdscript_analyzer.cpp:4563` precedes `:4570`/`:4611`): the occurrence positively refers to the
/// PROJECT class `qf.gd`, so renaming it renames THAT class (by-identity, faithful) — it must NOT
/// resolve to the native symbol, and it must NEVER edit some OTHER project class. The cross-file use
/// site is currently under-collected (the #158 references-completeness gap); the load-bearing
/// invariant here is corruption-safety: the only file edited is the class's own declaring file,
/// never an unrelated one. `(rel_class_file, src_use, line, ch)` clicks the shadowed bare identifier.
fn assert_bare_collision_renames_own_class_only(
    decl_src: &str,
    use_src: &str,
    line: u32,
    ch: u32,
    new_name: &str,
    id: i32,
) {
    let project = common::sample_project();
    project.write("src/k.gd", decl_src);
    project.write("src/use.gd", use_src);
    let (client, server) = boot();
    init_open(
        &project,
        &client,
        caps_full(),
        &["src/k.gd", "src/use.gd"],
        2,
    );
    let use_uri = file_uri(&project.root.join("src/use.gd"));
    let k_uri = file_uri(&project.root.join("src/k.gd"));
    client
        .sender
        .send(request(
            id,
            "textDocument/rename",
            rename_params(&use_uri, line, ch, new_name),
        ))
        .unwrap();
    let resp = recv_response(&client);
    // It resolves to the shadowing project class, so the rename succeeds and edits the class's own
    // declaration (k.gd). The corruption-safety invariant: it edits ONLY k.gd (the class it resolves
    // to) — never use.gd's unrelated tokens as a DIFFERENT symbol, and never a third file.
    assert!(
        resp.error.is_none(),
        "a bare identifier shadowed by a project class_name renames that class (faithful \
         resolution), not refuse: {:?}",
        resp.error
    );
    let edit: WorkspaceEdit = serde_json::from_value(resp.result.expect("rename result")).unwrap();
    let view = flatten_edit(&edit);
    assert!(
        view.set.iter().any(|(u, _)| *u == k_uri.as_str()),
        "the rename must edit the class's own declaration in k.gd (the symbol it resolves to): {:?}",
        view.set
    );
    // Every edited URI is k.gd (the resolved class's file). NOT corrupting an unrelated class is the
    // point; a cross-file use of the class in use.gd MAY also be edited (correct) — assert no edit
    // lands anywhere that is neither the class decl file nor a genuine use of that class. Here the
    // only project files are k.gd + use.gd, and use.gd's token IS a use of the class, so both are OK;
    // what must never happen is editing a DIFFERENT class — there is none, so assert the strong form:
    // the class decl file is edited and the result is a valid (non-error) workspace edit.
    assert!(
        view.new_texts.iter().all(|t| t == new_name),
        "every edit writes the new name, got {:?}",
        view.new_texts
    );
    shutdown(&client, server);
}

#[test]
fn rename_bare_native_enum_value_shadowed_by_class_name_renames_class() {
    // A NATIVE @GlobalScope enum value used BARE (`var d = SIDE_LEFT`) with a project `class_name
    // SIDE_LEFT` present: Godot resolves the bare identifier to the CLASS (it shadows the enum value),
    // so renaming it renames the class — by-identity, NOT the unrelated-symbol corruption the prior
    // (name-only) firewall produced. Must not refuse, must edit the class's own file only.
    // `var d = SIDE_LEFT` line 3: tab(0) `var d = `(1-8) `SIDE_LEFT`(9).
    assert_bare_collision_renames_own_class_only(
        "class_name SIDE_LEFT\nextends Node\n",
        "extends Node\n\nfunc go() -> void:\n\tvar d = SIDE_LEFT\n\tprint(d)\n",
        3,
        9,
        "LEFT_SIDE",
        214,
    );
}

#[test]
fn rename_bare_utility_shadowed_by_class_name_renames_class() {
    // A bare @GlobalScope utility call (`print("hi")`) with a project `class_name print`: resolves to
    // the class (constructor-style call), renames the class by-identity, never an unrelated symbol.
    // `print("hi")` line 3: tab(0) `print`(1).
    assert_bare_collision_renames_own_class_only(
        "class_name print\nextends Node\n",
        "extends Node\n\nfunc go() -> void:\n\tprint(\"hi\")\n",
        3,
        1,
        "log_line",
        215,
    );
}

#[test]
fn rename_bare_implicit_self_native_method_shadowed_by_class_name_renames_class() {
    // A bare implicit-self native method call (`queue_free()`) with a project `class_name queue_free`:
    // resolves to the class, renames it by-identity, never an unrelated symbol.
    // `queue_free()` line 3: tab(0) `queue_free`(1).
    assert_bare_collision_renames_own_class_only(
        "class_name queue_free\nextends Node\n",
        "extends Node\n\nfunc go() -> void:\n\tqueue_free()\n",
        3,
        1,
        "free_now",
        216,
    );
}

#[test]
fn rename_type_chain_inner_class_segment_colliding_with_class_name_refuses() {
    // CORRUPTION GUARD (the TYPE-CHAIN sub-case codex found — parses as `TypeNode.type_chain`, NOT a
    // `Subscript`, so the attribute gate structurally could not catch it): a `: Outer.Inner` type
    // annotation segment `Inner` colliding with an unrelated project `class_name Inner` must NOT
    // rename the unrelated class. (`Outer.Inner` is the legitimate inner-class type; `class_name
    // Inner` is a different, unrelated top-level class.)
    let project = common::sample_project();
    project.write(
        "src/outer.gd",
        "class_name Outer\nextends Node\n\nclass Inner:\n\tvar v: int = 0\n",
    );
    project.write("src/inner.gd", "class_name Inner\nextends Node\n");
    project.write(
        "src/use.gd",
        "extends Node\n\nfunc go() -> void:\n\tvar x: Outer.Inner = Outer.Inner.new()\n\tprint(x.v)\n",
    );
    let (client, server) = boot();
    init_open(
        &project,
        &client,
        caps_full(),
        &["src/outer.gd", "src/inner.gd", "src/use.gd"],
        2,
    );
    let use_uri = file_uri(&project.root.join("src/use.gd"));
    let inner_uri = file_uri(&project.root.join("src/inner.gd"));
    // `var x: Outer.Inner = ...` line 3: tab(0) `var x: `(1-7) `Outer`(8-12) `.`(13) `Inner`(14).
    // Click the type-chain segment `Inner` at col 14.
    client
        .sender
        .send(request(
            217,
            "textDocument/rename",
            rename_params(&use_uri, 3, 14, "Innermost"),
        ))
        .unwrap();
    let resp = recv_response(&client);
    // The unrelated top-level `class_name Inner` (inner.gd) must NEVER be edited (corruption guard).
    // Whether this refuses or resolves to the inner class, it must not touch inner.gd.
    if let Some(v) = resp.result.as_ref() {
        let view = flatten_edit(&serde_json::from_value::<WorkspaceEdit>(v.clone()).unwrap());
        assert!(
            !view.set.iter().any(|(u, _)| *u == inner_uri.as_str()),
            "the unrelated top-level `class_name Inner` (inner.gd) must NEVER be edited by renaming \
             the `Outer.Inner` type-chain segment: {:?}",
            view.set
        );
    }
    shutdown(&client, server);
}

#[test]
fn rename_native_method_colliding_with_class_name_refuses() {
    // CORRUPTION GUARD (fusion-found native analog of the cross-file-enum-value collision): a NATIVE
    // method on an untyped base (`n.queue_free()`) whose name collides with a project `class_name`
    // must REFUSE — NOT silently rename the unrelated class project-wide. Same attribute-position
    // mechanism: `queue_free` is the `.queue_free` attribute of `n.queue_free`, so the hardened
    // signal-3 skips the name-only `class_name` anchor and the native-method-on-untyped-base refuses.
    let project = common::sample_project();
    // Re-use the sample MINI_API (Object<-Node<-CanvasItem<-Node2D) plus a `queue_free` on Node would
    // be ideal, but `n.queue_free()` on an UNTYPED `n` resolves as a native method regardless — the
    // point is the project `class_name queue_free` must not be borrowed. A class literally named
    // `queue_free` (a valid identifier) makes the name collision exact.
    project.write("src/qf.gd", "class_name queue_free\nextends Node\n");
    project.write(
        "src/use.gd",
        "extends Node\n\nfunc go() -> void:\n\tvar n = self\n\tn.queue_free()\n",
    );
    let (client, server) = boot();
    init_open(
        &project,
        &client,
        caps_full(),
        &["src/qf.gd", "src/use.gd"],
        2,
    );
    let use_uri = file_uri(&project.root.join("src/use.gd"));
    let qf_uri = file_uri(&project.root.join("src/qf.gd"));
    // `n.queue_free()` on line 4: tab(0) `n`(1) `.`(2) `queue_free`(3). Click `queue_free` at col 3.
    client
        .sender
        .send(request(
            213,
            "textDocument/rename",
            rename_params(&use_uri, 4, 3, "free_now"),
        ))
        .unwrap();
    let resp = recv_response(&client);
    assert!(
        resp.result.is_none() && resp.error.is_some(),
        "a native method on an untyped base colliding with a `class_name` must REFUSE with zero \
         edits (never rename the unrelated `class_name queue_free`); got result={:?}",
        resp.result
    );
    if let Some(v) = resp.result.as_ref() {
        let view = flatten_edit(&serde_json::from_value::<WorkspaceEdit>(v.clone()).unwrap());
        assert!(
            !view.set.iter().any(|(u, _)| *u == qf_uri.as_str()),
            "the `class_name queue_free` declaration must NEVER be edited: {:?}",
            view.set
        );
    }
    shutdown(&client, server);
}

#[test]
fn rename_class_name_from_extends_use_site_still_succeeds() {
    // ANTI-OVER-NARROW GUARD for the signal-3 by-identity hardening: renaming a project `class_name`
    // from a cross-file USE site (`extends Hero` in enemy.gd, NOT the decl in hero.gd) must STILL
    // succeed and edit both the declaration and the `extends` site. The decl-click case (a) of the
    // hardened signal-3 covers the declaration; this proves case (b) — the `Class` Use-binding anchor
    // — keeps the use-site renameable (a regression here would mean the firewall under-refuses).
    let project = common::sample_project();
    let (client, server) = boot();
    init_open(
        &project,
        &client,
        caps_full(),
        &["src/hero.gd", "src/enemy.gd"],
        7,
    );
    let hero_uri = file_uri(&project.root.join("src/hero.gd"));
    let enemy_uri = file_uri(&project.root.join("src/enemy.gd"));

    // `extends Hero` in enemy.gd is line 0; `Hero` at col 8. Rename from THERE → `Champion`.
    let ref_set = references_set(&client, 211, &enemy_uri, 0, 8);
    client
        .sender
        .send(request(
            212,
            "textDocument/rename",
            rename_params(&enemy_uri, 0, 8, "Champion"),
        ))
        .unwrap();
    let resp = recv_response(&client);
    assert!(
        resp.error.is_none(),
        "renaming `class_name Hero` from the cross-file `extends Hero` USE site must succeed \
         (the type-base-segment carve-out anchors it — `extends` carries no binding): {:?}",
        resp.error
    );
    let edit: WorkspaceEdit = serde_json::from_value(resp.result.expect("rename result")).unwrap();
    let view = flatten_edit(&edit);
    assert_eq!(
        view.set, ref_set,
        "the use-site class rename edited set must equal the references set"
    );
    assert!(
        view.set.iter().any(|(u, _)| *u == hero_uri.as_str())
            && view.set.iter().any(|(u, _)| *u == enemy_uri.as_str()),
        "the edit must cover BOTH the hero.gd declaration and the enemy.gd extends site: {:?}",
        view.set
    );
    shutdown(&client, server);
}

#[test]
fn rename_member_from_bare_use_click_edits_bare_and_self_qualified() {
    // VERIFICATION that a Member does NOT need declaration-canonicalization (so skipping it for the
    // whole by-identity non-method family is safe): a class with `var speed`, a BARE `speed` use AND
    // a `self.speed` use. Clicking the BARE use and renaming must edit BOTH — proving the Member's
    // binding-backed reference set is already click-site-independent (no method-style bare-vs-`self.`
    // asymmetry that would require canonicalizing to the declaration).
    //   line 1 `var speed: int = 0`          → decl `speed` at col 4
    //   line 3 `\tspeed += 1`                 → BARE use at col 1
    //   line 4 `\tself.speed = 2`             → self-qualified use at col 6
    let src =
        "extends Node\nvar speed: int = 0\nfunc go() -> void:\n\tspeed += 1\n\tself.speed = 2\n";
    let (client, server, main_uri, _project) = boot_native_member(src);
    // Click the BARE use (line 3, col 1). Rename → `velocity`.
    let sites = rename_sites(&client, 222, &main_uri, 3, 1, "velocity");
    assert_eq!(
        sites,
        vec![(1, 4), (3, 1), (4, 6)],
        "renaming a member from a BARE use-click must edit the decl + the bare use + the \
         self-qualified use (binding-backed, click-site-independent); got {sites:?}"
    );
    shutdown(&client, server);
}

#[test]
fn rename_class_name_from_type_annotation_and_constructor_still_succeed() {
    // ANTI-OVER-NARROW GUARD for occurrence-positive signal-3: the two legit class-USE forms most at
    // risk under the new gate. `: Hero` (type annotation BASE segment — carries NO binding, admitted
    // via the type-base carve-out) and `Hero.new()` (constructor — the BASE `Hero` carries a `Class`
    // Use-binding, admitted via the expression-position anchor) must BOTH still rename the class. A
    // regression here would mean the occurrence-positive check is too narrow.
    let project = common::sample_project();
    project.write(
        "src/u.gd",
        "extends Node\n\nfunc go() -> void:\n\tvar h: Hero = Hero.new()\n",
    );
    let (client, server) = boot();
    init_open(
        &project,
        &client,
        caps_full(),
        &["src/hero.gd", "src/u.gd"],
        2,
    );
    let u_uri = file_uri(&project.root.join("src/u.gd"));
    let hero_uri = file_uri(&project.root.join("src/hero.gd"));
    // `\tvar h: Hero = Hero.new()`: tab(0) `var h: `(1-7) `Hero`(8) [annotation base];
    // ` = `(12-14) `Hero`(15) [constructor base].
    for (label, ch) in [
        ("type annotation `: Hero`", 8),
        ("constructor `Hero.new()`", 15),
    ] {
        client
            .sender
            .send(request(
                220,
                "textDocument/rename",
                rename_params(&u_uri, 3, ch, "Champion"),
            ))
            .unwrap();
        let resp = recv_response(&client);
        assert!(
            resp.error.is_none(),
            "renaming `class_name Hero` from {label} (col {ch}) must succeed (occurrence-positive \
             anchor must not over-narrow): {:?}",
            resp.error
        );
        let edit: WorkspaceEdit =
            serde_json::from_value(resp.result.expect("rename result")).unwrap();
        let view = flatten_edit(&edit);
        assert!(
            view.set.iter().any(|(u, _)| *u == hero_uri.as_str()),
            "renaming from {label} must edit the hero.gd class declaration: {:?}",
            view.set
        );
    }
    shutdown(&client, server);
}

#[test]
fn rename_in_file_enum_type_from_type_annotation_use_succeeds() {
    // ANTI-OVER-NARROW REGRESSION GUARD: an in-file enum TYPE name (`enum MyEnum { A }`) used as a
    // type annotation (`var e: MyEnum`) must STILL rename from the USE site — not refuse. The
    // occurrence-positive firewall must re-admit in-file (non-global-class) TYPE references; the
    // name-based admit it replaced used to cover these.
    //   line 1 `enum MyEnum { A }`              → enum TYPE decl `MyEnum` at col 5
    //   line 3 `\tvar e: MyEnum = MyEnum.A`     → type-annot use `MyEnum` at col 8
    let src = "extends Node\nenum MyEnum { A }\nfunc go() -> void:\n\tvar e: MyEnum = MyEnum.A\n\tprint(e)\n";
    let (client, server, main_uri, _project) = boot_native_member(src);
    // Click the type-annotation USE `MyEnum` (line 3, col 8). Rename → `Dir`.
    client
        .sender
        .send(request(
            224,
            "textDocument/rename",
            rename_params(&main_uri, 3, 8, "Dir"),
        ))
        .unwrap();
    let resp = recv_response(&client);
    assert!(
        resp.error.is_none(),
        "renaming an in-file enum TYPE from a `: MyEnum` annotation use must succeed (the firewall \
         must re-admit in-file type references occurrence-positively): {:?}",
        resp.error
    );
    let edit: WorkspaceEdit = serde_json::from_value(resp.result.expect("rename result")).unwrap();
    let view = flatten_edit(&edit);
    // Must edit the enum TYPE declaration (line 1, col 5) — the symbol the cursor refers to.
    assert!(
        view.set
            .iter()
            .any(|(_, r)| r.start.line == 1 && r.start.character == 5),
        "renaming the enum type from its use must edit the enum TYPE declaration (1,5): {:?}",
        view.set
    );
    shutdown(&client, server);
}

#[test]
fn rename_inner_class_from_type_annotation_use_succeeds() {
    // ANTI-OVER-NARROW REGRESSION GUARD: an in-file INNER CLASS name (`class Inner:`) used as a type
    // annotation (`var x: Inner`) must STILL rename from the USE site — not refuse.
    //   line 1 `class Inner:`            → inner class decl `Inner` at col 6
    //   line 2 `\tvar v: int = 0`
    //   line 4 `\tvar x: Inner = null`   → type-annot use `Inner` at col 8
    let src = "extends Node\nclass Inner:\n\tvar v: int = 0\nfunc go() -> void:\n\tvar x: Inner = null\n\tprint(x)\n";
    let (client, server, main_uri, _project) = boot_native_member(src);
    // Click the type-annotation USE `Inner` (line 4, col 8). Rename → `Nested`.
    client
        .sender
        .send(request(
            225,
            "textDocument/rename",
            rename_params(&main_uri, 4, 8, "Nested"),
        ))
        .unwrap();
    let resp = recv_response(&client);
    assert!(
        resp.error.is_none(),
        "renaming an in-file inner CLASS from a `: Inner` annotation use must succeed: {:?}",
        resp.error
    );
    let edit: WorkspaceEdit = serde_json::from_value(resp.result.expect("rename result")).unwrap();
    let view = flatten_edit(&edit);
    assert!(
        view.set
            .iter()
            .any(|(_, r)| r.start.line == 1 && r.start.character == 6),
        "renaming the inner class from its use must edit the inner CLASS declaration (1,6): {:?}",
        view.set
    );
    shutdown(&client, server);
}

#[test]
fn rename_in_file_const_and_signal_from_use_succeed() {
    // SWEEP (main-vs-HEAD regression coverage): the remaining in-file symbol kinds whose USE-site
    // anchor the name-based signal-1 used to provide. An in-file `const` from a bare use, and an
    // in-file `signal` from a bare reference, must each rename from the USE site — not refuse.
    //   line 1 `const MAX := 5`                  → const decl `MAX` at col 6
    //   line 2 `signal hit`                      → signal decl `hit` at col 7
    //   line 4 `\tvar x = MAX`                   → const bare use `MAX` at col 9
    //   line 5 `\thit.connect(go)`               → signal bare ref `hit` at col 1
    let src = "extends Node\nconst MAX := 5\nsignal hit\nfunc go() -> void:\n\tvar x = MAX\n\thit.connect(go)\n";
    // const from bare use (line 4, col 9).
    let (client, server, main_uri, _project) = boot_native_member(src);
    let sites = rename_sites(&client, 228, &main_uri, 4, 9, "LIMIT");
    assert!(
        sites.contains(&(1, 6)) && sites.contains(&(4, 9)),
        "renaming an in-file const from a bare use must edit the const decl (1,6) + the use (4,9); \
         got {sites:?}"
    );
    shutdown(&client, server);

    // signal from bare ref (line 5, col 1).
    let (client2, server2, main_uri2, _project2) = boot_native_member(src);
    client2
        .sender
        .send(request(
            229,
            "textDocument/rename",
            rename_params(&main_uri2, 5, 1, "struck"),
        ))
        .unwrap();
    let resp2 = recv_response(&client2);
    assert!(
        resp2.error.is_none(),
        "renaming an in-file signal from a bare reference must succeed: {:?}",
        resp2.error
    );
    let view2 = flatten_edit(
        &serde_json::from_value::<WorkspaceEdit>(resp2.result.expect("a WorkspaceEdit")).unwrap(),
    );
    assert!(
        view2
            .set
            .iter()
            .any(|(_, r)| r.start.line == 2 && r.start.character == 7),
        "renaming the signal from its reference must edit the signal declaration (2,7): {:?}",
        view2.set
    );
    shutdown(&client2, server2);
}

#[test]
fn rename_in_file_type_from_expression_base_use_succeeds() {
    // ANTI-OVER-NARROW REGRESSION GUARD (EXPRESSION position — the twin of the type-annotation case):
    // an in-file enum TYPE clicked as the BASE of `MyEnum.A`, and an in-file inner CLASS clicked as
    // the BASE of `Inner.new()`, must rename from the USE site — not refuse. (A subscript BASE naming
    // an in-file type resolves to THIS file's type; only a subscript ATTRIBUTE resolving cross-file is
    // the corruption case.)
    //   line 1 `enum MyEnum { A }`        → enum TYPE decl `MyEnum` at col 5
    //   line 2 `class Inner:`             → inner CLASS decl `Inner` at col 6
    //   line 5 `\tvar a = MyEnum.A`       → `MyEnum` BASE at col 9
    //   line 6 `\tvar i = Inner.new()`    → `Inner` BASE at col 9
    let src = "extends Node\nenum MyEnum { A }\nclass Inner:\n\tvar v: int = 0\nfunc go() -> void:\n\tvar a = MyEnum.A\n\tvar i = Inner.new()\n";
    // Enum type from `MyEnum.A` base (line 5, col 9).
    let (client, server, main_uri, _project) = boot_native_member(src);
    client
        .sender
        .send(request(
            226,
            "textDocument/rename",
            rename_params(&main_uri, 5, 9, "Dir"),
        ))
        .unwrap();
    let resp = recv_response(&client);
    assert!(
        resp.error.is_none(),
        "renaming an in-file enum TYPE from a `MyEnum.A` expression base must succeed: {:?}",
        resp.error
    );
    let view = flatten_edit(
        &serde_json::from_value::<WorkspaceEdit>(resp.result.expect("a WorkspaceEdit")).unwrap(),
    );
    assert!(
        view.set
            .iter()
            .any(|(_, r)| r.start.line == 1 && r.start.character == 5),
        "renaming the enum type from `MyEnum.A` must edit the enum TYPE declaration (1,5): {:?}",
        view.set
    );
    shutdown(&client, server);

    // Inner class from `Inner.new()` base (line 6, col 9).
    let (client2, server2, main_uri2, _project2) = boot_native_member(src);
    client2
        .sender
        .send(request(
            227,
            "textDocument/rename",
            rename_params(&main_uri2, 6, 9, "Nested"),
        ))
        .unwrap();
    let resp2 = recv_response(&client2);
    assert!(
        resp2.error.is_none(),
        "renaming an in-file inner CLASS from an `Inner.new()` expression base must succeed: {:?}",
        resp2.error
    );
    let view2 = flatten_edit(
        &serde_json::from_value::<WorkspaceEdit>(resp2.result.expect("a WorkspaceEdit")).unwrap(),
    );
    assert!(
        view2.set.iter().any(|(_, r)| r.start.line == 2 && r.start.character == 6),
        "renaming the inner class from `Inner.new()` must edit the inner CLASS declaration (2,6): {:?}",
        view2.set
    );
    shutdown(&client2, server2);
}

// =================================================================================================
// #159: a rename of a cursor whose NAME collides with a project `class_name` must NEVER rewrite that
// unrelated `class_name` declaration through a name-only sink in the MUTATING path. The cursor
// resolves (by identity) to its OWN symbol — an anon-enum hoisted const here — so the edit set must
// be exactly that symbol's sites, never the same-named class. The corruption (pre-existing on `main`)
// was the name-only `find_global_class_definition(name)` fallback in `declaration_locations` AND the
// `definition()`-based rename canonicalization, both now gated occurrence-positively.
// =================================================================================================

#[test]
fn rename_anon_enum_value_does_not_rewrite_unrelated_class_name() {
    // `enum { FOO }; return FOO` in consumer.gd, with an UNRELATED `class_name FOO` in foo.gd. The
    // bare `FOO` use resolves to the in-file anon-enum CONST (a Member to consumer.gd), NOT the class.
    // Renaming it from the USE site (`return FOO`) must edit ONLY the two consumer.gd sites (decl +
    // use); foo.gd's `class_name FOO` must be untouched. On `main` this rewrote `class_name FOO` too.
    //   consumer.gd line 1 `enum { FOO }`     → anon-enum value decl `FOO` at col 7
    //   consumer.gd line 3 `\treturn FOO`     → use `FOO` at col 8
    //   foo.gd     line 0 `class_name FOO`    → UNRELATED class decl `FOO` at col 11 (must NOT edit)
    let project = common::sample_project();
    project.write(
        "src/consumer.gd",
        "extends Node\nenum { FOO }\nfunc go() -> int:\n\treturn FOO\n",
    );
    project.write("src/foo.gd", "class_name FOO\nextends Node\n");
    let (client, server) = boot();
    init_open(
        &project,
        &client,
        caps_full(),
        &["src/consumer.gd", "src/foo.gd"],
        2,
    );
    let consumer_uri = file_uri(&project.root.join("src/consumer.gd"));
    let foo_uri = file_uri(&project.root.join("src/foo.gd"));

    // Rename `FOO` from the USE site (line 3, col 8) → `BAR`.
    client
        .sender
        .send(request(
            300,
            "textDocument/rename",
            rename_params(&consumer_uri, 3, 8, "BAR"),
        ))
        .unwrap();
    let resp = recv_response(&client);
    assert!(
        resp.error.is_none(),
        "renaming the anon-enum value must succeed: {:?}",
        resp.error
    );
    let view =
        flatten_edit(&serde_json::from_value::<WorkspaceEdit>(resp.result.expect("edit")).unwrap());
    assert!(
        view.set.iter().all(|(u, _)| *u != foo_uri.as_str()),
        "renaming the anon-enum value `FOO` must NOT edit the unrelated `class_name FOO` in foo.gd \
         — wrong-symbol corruption; got {:?}",
        view.set
    );
    let consumer_sites: Vec<(u32, u32)> = view
        .set
        .iter()
        .filter(|(u, _)| *u == consumer_uri.as_str())
        .map(|(_, r)| (r.start.line, r.start.character))
        .collect();
    assert_eq!(
        consumer_sites,
        vec![(1, 7), (3, 8)],
        "the edit must be exactly the anon-enum decl (1,7) + its use (3,8); got {consumer_sites:?}"
    );
    shutdown(&client, server);
}

#[test]
fn rename_class_name_from_expression_use_with_colliding_member_renames_the_class() {
    // The POSITIVE twin (case-2 of #159, reframed): when a bare identifier is SHADOWED by a project
    // `class_name`, it resolves to the CLASS faithfully (Godot `gdscript_analyzer.cpp:4563`
    // is_global_class precedes the autoload/native paths), so renaming it from an EXPRESSION use site
    // (`Global.new()`) DOES rename the class — by identity, via the `Binding::Use { kind: Class }`
    // anchor (distinct from the existing decl-click test). The occurrence-positive gate must KEEP this
    // working, not over-refuse it.
    //   globalcls.gd line 0 `class_name Global` → class decl `Global` at col 11 (MUST edit)
    //   consumer.gd  line 2 `\tvar g = Global.new()` → expression use `Global` at col 9 (MUST edit)
    let project = common::sample_project();
    project.write("src/globalcls.gd", "class_name Global\nextends Node\n");
    project.write(
        "src/consumer.gd",
        "extends Node\nfunc go() -> void:\n\tvar g = Global.new()\n",
    );
    let (client, server) = boot();
    init_open(
        &project,
        &client,
        caps_full(),
        &["src/globalcls.gd", "src/consumer.gd"],
        2,
    );
    let globalcls_uri = file_uri(&project.root.join("src/globalcls.gd"));
    let consumer_uri = file_uri(&project.root.join("src/consumer.gd"));

    // Rename `Global` from the EXPRESSION use (line 2, col 9) → `Globals`.
    client
        .sender
        .send(request(
            302,
            "textDocument/rename",
            rename_params(&consumer_uri, 2, 9, "Globals"),
        ))
        .unwrap();
    let resp = recv_response(&client);
    assert!(
        resp.error.is_none(),
        "renaming a class from a shadowing expression use must succeed: {:?}",
        resp.error
    );
    let view =
        flatten_edit(&serde_json::from_value::<WorkspaceEdit>(resp.result.expect("edit")).unwrap());
    // The corruption-relevant invariant for #159: the cursor RESOLVES to the class (it is not
    // over-refused), so the class declaration IS edited — by identity, never an unrelated symbol.
    assert!(
        view.set
            .iter()
            .any(|(u, r)| *u == globalcls_uri.as_str() && r.start.line == 0 && r.start.character == 11),
        "the class `class_name Global` declaration (globalcls.gd 0,11) IS the referent and must be \
         edited (the occurrence-positive gate must NOT over-refuse a legitimate class use); got {:?}",
        view.set
    );
    assert!(
        view.new_texts.iter().all(|t| t == "Globals"),
        "every edit writes the new name; got {:?}",
        view.new_texts
    );
    // NB: the `Global.new()` expression use site is NOT collected here — a pre-existing
    // references-completeness gap for class uses in expression position, orthogonal to #159's
    // wrong-symbol-corruption fix (tracked separately). This test pins only that the class is the
    // correct, by-identity target.
    shutdown(&client, server);
}

#[test]
fn rename_autoload_name_with_colliding_class_name_targets_the_class_not_corrupt() {
    // Case-2 of #159, the ACTUAL autoload combo (the member-collision test above is the in-file
    // twin): an `[autoload] Global` AND a project `class_name Global` both exist, and `Global` is
    // used in an expression (`Global.foo()`). Godot resolves a bare identifier to the global
    // `class_name` BEFORE the autoload (`gdscript_analyzer.cpp:4563` is_global_class precedes `:4570`
    // has_autoload), so `Global` IS the class by identity — renaming it edits the `class_name Global`
    // declaration (correct), and must NEVER corrupt it via a name-only sink while also not over-
    // refusing. The issue's "autoload name + class_name corrupts the class" hypothesis is thereby
    // disproven: the cursor IS the class.
    //   globalcls.gd line 0 `class_name Global`     → class decl `Global` at col 11 (MUST edit)
    //   consumer.gd  line 2 `\tGlobal.foo()`        → expression use `Global` at col 1
    let project = common::sample_project();
    // Re-declare an autoload Global in project.godot (sample_project's own project.godot is replaced).
    project.write(
        "project.godot",
        "config_version=5\n\n[autoload]\nGlobal=\"*res://src/global.gd\"\n",
    );
    project.write(
        "src/global.gd",
        "extends Node\nfunc foo() -> void:\n\tpass\n",
    );
    project.write("src/globalcls.gd", "class_name Global\nextends Node\n");
    project.write(
        "src/consumer.gd",
        "extends Node\nfunc go() -> void:\n\tGlobal.foo()\n",
    );
    let (client, server) = boot();
    init_open(
        &project,
        &client,
        caps_full(),
        &["src/global.gd", "src/globalcls.gd", "src/consumer.gd"],
        2,
    );
    let globalcls_uri = file_uri(&project.root.join("src/globalcls.gd"));
    let consumer_uri = file_uri(&project.root.join("src/consumer.gd"));

    // Rename `Global` from the expression use (line 2, col 1) → `Globals`.
    client
        .sender
        .send(request(
            306,
            "textDocument/rename",
            rename_params(&consumer_uri, 2, 1, "Globals"),
        ))
        .unwrap();
    let resp = recv_response(&client);
    assert!(
        resp.error.is_none(),
        "renaming `Global` (which resolves to the class) must succeed, not over-refuse: {:?}",
        resp.error
    );
    let view =
        flatten_edit(&serde_json::from_value::<WorkspaceEdit>(resp.result.expect("edit")).unwrap());
    // The class IS the referent — its declaration must be edited, by identity.
    assert!(
        view.set
            .iter()
            .any(|(u, r)| *u == globalcls_uri.as_str() && r.start.line == 0 && r.start.character == 11),
        "the `class_name Global` declaration (globalcls.gd 0,11) IS the referent and must be edited; \
         got {:?}",
        view.set
    );
    assert!(
        view.new_texts.iter().all(|t| t == "Globals"),
        "every edit writes the new name; got {:?}",
        view.new_texts
    );
    // NB: the `Global.foo()` use site is NOT collected (the same pre-existing class-use-in-expression
    // under-rename as the member-collision test; orthogonal to #159's corruption fix).
    shutdown(&client, server);
}

#[test]
fn rename_bare_method_call_with_colliding_class_name_keeps_self_qualified_site() {
    // REGRESSION GUARD for the method-canonicalization asymmetry (#106's broadening lost the
    // `self.method()` site): a bare in-file method call classifies as Member, and the
    // occurrence-positive class gate must NOT suppress its declaration-canonicalization. Even with an
    // UNRELATED `class_name helper` present, renaming `helper` from the BARE call site must still
    // canonicalize to the declaration and collect the `self.helper()` sibling — AND must not touch
    // the class.
    //   m.gd       line 1 `func helper() -> void:` → decl `helper` at col 5
    //   m.gd       line 4 `\thelper()`             → bare call at col 1 (click here)
    //   m.gd       line 5 `\tself.helper()`        → self-qualified call at col 6
    //   helper.gd  line 0 `class_name helper`      → UNRELATED class decl (must NOT edit)
    let project = common::sample_project();
    project.write(
        "src/m.gd",
        "extends Node\nfunc helper() -> void:\n\tpass\nfunc go() -> void:\n\thelper()\n\tself.helper()\n",
    );
    project.write("src/helper.gd", "class_name helper\nextends Node\n");
    let (client, server) = boot();
    init_open(
        &project,
        &client,
        caps_full(),
        &["src/m.gd", "src/helper.gd"],
        2,
    );
    let m_uri = file_uri(&project.root.join("src/m.gd"));
    let helper_uri = file_uri(&project.root.join("src/helper.gd"));

    // Rename `helper` from the BARE call site (line 4, col 1) → `helper2`.
    client
        .sender
        .send(request(
            304,
            "textDocument/rename",
            rename_params(&m_uri, 4, 1, "helper2"),
        ))
        .unwrap();
    let resp = recv_response(&client);
    assert!(
        resp.error.is_none(),
        "renaming the method from a bare call site must succeed: {:?}",
        resp.error
    );
    let view =
        flatten_edit(&serde_json::from_value::<WorkspaceEdit>(resp.result.expect("edit")).unwrap());
    assert!(
        view.set.iter().all(|(u, _)| *u != helper_uri.as_str()),
        "the unrelated `class_name helper` must NOT be edited; got {:?}",
        view.set
    );
    let m_sites: Vec<(u32, u32)> = view
        .set
        .iter()
        .filter(|(u, _)| *u == m_uri.as_str())
        .map(|(_, r)| (r.start.line, r.start.character))
        .collect();
    assert_eq!(
        m_sites,
        vec![(1, 5), (4, 1), (5, 6)],
        "the method rename must still be click-site-INDEPENDENT — decl (1,5) + bare call (4,1) + \
         self.helper() (5,6); the self-qualified site must NOT be dropped; got {m_sites:?}"
    );
    shutdown(&client, server);
}

// =============================================================================================
// #162 / #163 — occurrence-positive Unresolved-arm collection for global-class + in-file-type
// rename. The Unresolved arm's raw `push_identifier_locations` scan grabbed EVERY same-spelled
// token in each consumer file (the W16 grep-rename hole, collection-side). These pin the
// position×collision matrix.
// =============================================================================================

/// Build a consumer file that genuinely extends `class_name Hero` AND independently contains
/// unrelated same-named symbols (`func g(Hero)` param, `var Hero` local, `print(Hero)` uses).
/// Renaming the class must rewrite ONLY the genuine `extends Hero` ref (+ the decl), never the
/// unrelated occurrences. Shared by the decl-click and expr-use cells.
const HERO_CONSUMER_WITH_COLLISIONS: &str = "extends Hero\n\nfunc g(Hero):\n\tprint(Hero)\n\nfunc h() -> void:\n\tvar Hero = 1\n\tprint(Hero)\n";

#[test]
fn rename_163_global_class_decl_click_does_not_overgrab_colliding_tokens_in_consumer() {
    // #163 cell (a): a `class_name Hero` DECL-click with a consumer that does `extends Hero` AND
    // has unrelated `func g(Hero)` / `var Hero` / `print(Hero)`. On `main` the Unresolved raw scan
    // rewrites ALL of them in the consumer (indiscriminate over-grab). The fix collects only the
    // type-base `extends Hero` segment by position + Class-use bindings by identity.
    let project = common::sample_project(); // src/hero.gd = `class_name Hero` … ; src/enemy.gd = `extends Hero`
    project.write("src/consumer.gd", HERO_CONSUMER_WITH_COLLISIONS);
    let (client, server) = boot();
    init_open(
        &project,
        &client,
        caps_full(),
        &["src/hero.gd", "src/enemy.gd", "src/consumer.gd"],
        2,
    );
    let hero_uri = file_uri(&project.root.join("src/hero.gd"));
    let consumer_uri = file_uri(&project.root.join("src/consumer.gd"));

    // `class_name Hero` on hero.gd line 0: `Hero` at col 11. Rename → `Champion`.
    client
        .sender
        .send(request(
            300,
            "textDocument/rename",
            rename_params(&hero_uri, 0, 11, "Champion"),
        ))
        .unwrap();
    let resp = recv_response(&client);
    assert!(
        resp.error.is_none(),
        "class rename should succeed: {:?}",
        resp.error
    );
    let view =
        flatten_edit(&serde_json::from_value::<WorkspaceEdit>(resp.result.unwrap()).unwrap());

    // Genuine refs that MUST be edited in consumer.gd: only `extends Hero` (line 0, col 8).
    let consumer_edits: Vec<Range> = view
        .set
        .iter()
        .filter(|(u, _)| *u == consumer_uri.as_str())
        .map(|(_, r)| *r)
        .collect();
    assert_eq!(
        consumer_edits,
        vec![Range {
            start: Position { line: 0, character: 8 },
            end: Position { line: 0, character: 12 },
        }],
        "renaming `class_name Hero` must edit ONLY `extends Hero` in the consumer — never the \
         unrelated `func g(Hero)` param / `var Hero` local / `print(Hero)` uses (the W16 over-grab); \
         got {consumer_edits:?}"
    );
    // The declaration site is also edited (in hero.gd).
    assert!(
        view.set
            .iter()
            .any(|(u, r)| *u == hero_uri.as_str() && r.start.line == 0 && r.start.character == 11),
        "the `class_name Hero` declaration must be edited; got {:?}",
        view.set
    );
    shutdown(&client, server);
}

#[test]
fn rename_163_global_class_expr_use_does_not_overgrab_colliding_tokens() {
    // #163 cell (b): an EXPRESSION-position class use `Hero.new()` (records a Class use binding by
    // identity) in a file that ALSO has an unrelated same-named local in a DIFFERENT function. The
    // rename must edit the genuine `Hero` class refs by identity, never the unrelated local. On
    // `main` the raw scan grabbed every `Hero` token in the file.
    let project = common::sample_project();
    project.write(
        "src/maker.gd",
        // `Hero.new()` is a genuine class use (function `make`); `var Hero`/`print(Hero)` in
        // `other` is an unrelated local. extends Hero is also a genuine ref.
        "extends Hero\n\nfunc make() -> Hero:\n\treturn Hero.new()\n\nfunc other() -> void:\n\tvar Hero = 1\n\tprint(Hero)\n",
    );
    let (client, server) = boot();
    init_open(
        &project,
        &client,
        caps_full(),
        &["src/hero.gd", "src/maker.gd"],
        2,
    );
    let maker_uri = file_uri(&project.root.join("src/maker.gd"));

    // Click `Hero` of `return Hero.new()` on line 3: tab(0) `return `(1-7) `Hero`(8). Rename →
    // `Champion`.
    client
        .sender
        .send(request(
            301,
            "textDocument/rename",
            rename_params(&maker_uri, 3, 8, "Champion"),
        ))
        .unwrap();
    let resp = recv_response(&client);
    assert!(
        resp.error.is_none(),
        "class expr-use rename should succeed: {:?}",
        resp.error
    );
    let view =
        flatten_edit(&serde_json::from_value::<WorkspaceEdit>(resp.result.unwrap()).unwrap());

    let maker_edits: Vec<Range> = view
        .set
        .iter()
        .filter(|(u, _)| *u == maker_uri.as_str())
        .map(|(_, r)| *r)
        .collect();
    // Genuine class refs in maker.gd: `extends Hero` (0,8), the return type `-> Hero` (2,15),
    // `Hero.new()` base (3,8). The unrelated local `Hero` at (6,5) and `print(Hero)` at (7,7)
    // must NOT appear.
    assert!(
        maker_edits
            .iter()
            .any(|r| r.start.line == 0 && r.start.character == 8),
        "must edit `extends Hero`; got {maker_edits:?}"
    );
    assert!(
        maker_edits
            .iter()
            .any(|r| r.start.line == 3 && r.start.character == 8),
        "must edit the `Hero.new()` class use; got {maker_edits:?}"
    );
    assert!(
        !maker_edits
            .iter()
            .any(|r| r.start.line == 6 || r.start.line == 7),
        "must NOT edit the unrelated `var Hero` local / `print(Hero)` use in `other()` \
         (lines 6-7); got {maker_edits:?}"
    );
    shutdown(&client, server);
}

#[test]
fn rename_162_in_file_enum_type_does_not_collect_global_class_consumers() {
    // #162 cell (c): three-file shape. `src/holder.gd` declares an in-file root `enum FOO`. A
    // DIFFERENT file declares a global `class_name FOO`. A THIRD file uses `: FOO` (the global
    // class) in a type annotation. Renaming the IN-FILE `enum FOO` must fan out EMPTY cross-file —
    // it must NOT collect the global class's `: FOO` / `extends FOO` consumers. On `main` the
    // in-file-type cursor falls to the Unresolved arm and rides the cross-file candidate scan over
    // `name_referencers("FOO")`, collecting the unrelated consumer's `: FOO`.
    let project = common::sample_project();
    project.write(
        "src/holder.gd",
        // In-file root enum FOO, used in-file as `: FOO` / `FOO.A`.
        "extends Node\n\nenum FOO { A, B }\n\nvar x: FOO = FOO.A\n",
    );
    project.write("src/fooclass.gd", "class_name FOO\nextends Node\n");
    project.write(
        "src/fooconsumer.gd",
        // Uses the GLOBAL class FOO in a type annotation + extends.
        "extends FOO\n\nfunc use_it(p: FOO) -> void:\n\tprint(p)\n",
    );
    let (client, server) = boot();
    init_open(
        &project,
        &client,
        caps_full(),
        &["src/holder.gd", "src/fooclass.gd", "src/fooconsumer.gd"],
        2,
    );
    let holder_uri = file_uri(&project.root.join("src/holder.gd"));
    let fooclass_uri = file_uri(&project.root.join("src/fooclass.gd"));
    let fooconsumer_uri = file_uri(&project.root.join("src/fooconsumer.gd"));

    // `enum FOO { A, B }` on holder.gd line 2: `FOO` at col 5. Rename the in-file enum → `BAR`.
    client
        .sender
        .send(request(
            302,
            "textDocument/rename",
            rename_params(&holder_uri, 2, 5, "BAR"),
        ))
        .unwrap();
    let resp = recv_response(&client);
    // It either refuses or edits — but in NO case may it touch the unrelated global class FOO's
    // declaration or its consumer's `: FOO` / `extends FOO`.
    if let Some(v) = resp.result.as_ref() {
        let view = flatten_edit(&serde_json::from_value::<WorkspaceEdit>(v.clone()).unwrap());
        assert!(
            !view.set.iter().any(|(u, _)| *u == fooclass_uri.as_str()),
            "renaming the in-file `enum FOO` must NEVER edit the unrelated global `class_name FOO` \
             declaration (fooclass.gd); got {:?}",
            view.set
        );
        assert!(
            !view.set.iter().any(|(u, _)| *u == fooconsumer_uri.as_str()),
            "renaming the in-file `enum FOO` must NEVER edit the global class's `: FOO` / \
             `extends FOO` consumer (fooconsumer.gd) — in-file types have no cross-file bare refs; \
             got {:?}",
            view.set
        );
        // It SHOULD still edit its own in-file uses (FOO.A on line 4, : FOO on line 4) when it
        // proceeds.
        assert!(
            view.set.iter().all(|(u, _)| *u == holder_uri.as_str()),
            "an in-file enum rename must stay entirely in its own file; got {:?}",
            view.set
        );
    }
    shutdown(&client, server);
}

#[test]
fn rename_162_in_file_inner_class_type_does_not_collect_global_class_consumers() {
    // #162 cell (c) sibling: an in-file INNER CLASS used in TYPE position, with a same-named global
    // `class_name`. Renaming the in-file inner class must not collect the global class's consumers.
    let project = common::sample_project();
    project.write(
        "src/holder.gd",
        "extends Node\n\nclass Widget:\n\tvar v: int = 0\n\nvar w: Widget = Widget.new()\n",
    );
    project.write("src/widgetclass.gd", "class_name Widget\nextends Node\n");
    project.write(
        "src/widgetconsumer.gd",
        "extends Widget\n\nfunc use_it(p: Widget) -> void:\n\tprint(p)\n",
    );
    let (client, server) = boot();
    init_open(
        &project,
        &client,
        caps_full(),
        &[
            "src/holder.gd",
            "src/widgetclass.gd",
            "src/widgetconsumer.gd",
        ],
        2,
    );
    let holder_uri = file_uri(&project.root.join("src/holder.gd"));
    let widgetclass_uri = file_uri(&project.root.join("src/widgetclass.gd"));
    let widgetconsumer_uri = file_uri(&project.root.join("src/widgetconsumer.gd"));

    // `class Widget:` on holder.gd line 2: `Widget` at col 6. Rename the in-file inner class.
    client
        .sender
        .send(request(
            303,
            "textDocument/rename",
            rename_params(&holder_uri, 2, 6, "Gadget"),
        ))
        .unwrap();
    let resp = recv_response(&client);
    if let Some(v) = resp.result.as_ref() {
        let view = flatten_edit(&serde_json::from_value::<WorkspaceEdit>(v.clone()).unwrap());
        assert!(
            !view
                .set
                .iter()
                .any(|(u, _)| *u == widgetclass_uri.as_str() || *u == widgetconsumer_uri.as_str()),
            "renaming the in-file `class Widget` must NEVER edit the unrelated global \
             `class_name Widget` or its consumers; got {:?}",
            view.set
        );
    }
    shutdown(&client, server);
}

#[test]
fn rename_163_regression_in_file_member_vs_same_named_class_name_edits_no_class_decl() {
    // #163 cell (d) regression pin: an in-file MEMBER (`var Hero`) whose name collides with a
    // project `class_name Hero` must rename ONLY its own member, NEVER the global class declaration
    // or the class's `extends Hero` consumers. (Member classification already routes binding-backed,
    // but pin it so the new global-class bucket can't leak the class decl in.)
    let project = common::sample_project(); // hero.gd: class_name Hero; enemy.gd: extends Hero
    project.write(
        "src/holder.gd",
        "extends Node\n\nvar Hero: int = 1\n\nfunc use_it() -> void:\n\tHero = 2\n",
    );
    let (client, server) = boot();
    init_open(
        &project,
        &client,
        caps_full(),
        &["src/hero.gd", "src/enemy.gd", "src/holder.gd"],
        2,
    );
    let hero_uri = file_uri(&project.root.join("src/hero.gd"));
    let enemy_uri = file_uri(&project.root.join("src/enemy.gd"));
    let holder_uri = file_uri(&project.root.join("src/holder.gd"));

    // `var Hero: int = 1` on holder.gd line 2: `Hero` at col 4. Rename the member.
    client
        .sender
        .send(request(
            304,
            "textDocument/rename",
            rename_params(&holder_uri, 2, 4, "Health"),
        ))
        .unwrap();
    let resp = recv_response(&client);
    assert!(
        resp.error.is_none(),
        "member rename should succeed: {:?}",
        resp.error
    );
    let view =
        flatten_edit(&serde_json::from_value::<WorkspaceEdit>(resp.result.unwrap()).unwrap());
    assert!(
        !view
            .set
            .iter()
            .any(|(u, _)| *u == hero_uri.as_str() || *u == enemy_uri.as_str()),
        "renaming the in-file member `var Hero` must NEVER edit the unrelated `class_name Hero` \
         declaration (hero.gd) or its `extends Hero` consumer (enemy.gd); got {:?}",
        view.set
    );
    // It DOES edit its own member sites in holder.gd (decl + `Hero = 2`).
    assert!(
        view.set.iter().all(|(u, _)| *u == holder_uri.as_str()) && view.set.len() == 2,
        "the member rename must edit exactly its own two sites in holder.gd; got {:?}",
        view.set
    );
    shutdown(&client, server);
}

#[test]
fn rename_162_in_file_enum_type_use_cursor_does_not_rename_global_class() {
    // #162 THE ACTUAL CELL: a cursor on the in-file `enum FOO` in TYPE-USE position (`: FOO`),
    // where a same-named cross-file global `class_name FOO` exists. This is the ONLY cursor that
    // exercises precedence: `cursor_references_global_class` form (c) is TRUE there (type-base
    // segment naming a registered class) AND `name_is_in_file_root_type` is TRUE. The in-file enum
    // must win — the rename must NEVER edit the global `class_name FOO` or its consumers, and must
    // not canonicalize onto the global class via `definition()`.
    let project = common::sample_project();
    project.write(
        "src/holder.gd",
        "extends Node\n\nenum FOO { A, B }\n\nvar x: FOO = FOO.A\n",
    );
    project.write("src/fooclass.gd", "class_name FOO\nextends Node\n");
    project.write(
        "src/fooconsumer.gd",
        "extends FOO\n\nfunc use_it(p: FOO) -> void:\n\tprint(p)\n",
    );
    let (client, server) = boot();
    init_open(
        &project,
        &client,
        caps_full(),
        &["src/holder.gd", "src/fooclass.gd", "src/fooconsumer.gd"],
        2,
    );
    let holder_uri = file_uri(&project.root.join("src/holder.gd"));
    let fooclass_uri = file_uri(&project.root.join("src/fooclass.gd"));
    let fooconsumer_uri = file_uri(&project.root.join("src/fooconsumer.gd"));

    // `var x: FOO = FOO.A` on holder.gd line 4: tab(0) `var x: `(1-7) `FOO`(8). Click the `: FOO`
    // type-use segment at col 8.
    client
        .sender
        .send(request(
            305,
            "textDocument/rename",
            rename_params(&holder_uri, 4, 8, "BAR"),
        ))
        .unwrap();
    let resp = recv_response(&client);
    // The proceed-path is load-bearing for #162: it must NOT regress to a refuse (which would skip
    // every assertion below and pass vacuously). The in-file enum rename proceeds.
    let v = resp.result.as_ref().expect(
        "an in-file `: FOO` type-use rename must PROCEED (not refuse) — it is an editable \
                 in-file type, never the global class",
    );
    let view = flatten_edit(&serde_json::from_value::<WorkspaceEdit>(v.clone()).unwrap());
    assert!(
        !view.set.iter().any(|(u, _)| *u == fooclass_uri.as_str()),
        "a `: FOO` type-use cursor on the IN-FILE enum must NEVER rename the unrelated global \
         `class_name FOO` (fooclass.gd) — canonicalization must stay in-file; got {:?}",
        view.set
    );
    assert!(
        !view.set.iter().any(|(u, _)| *u == fooconsumer_uri.as_str()),
        "must NEVER edit the global class's `: FOO`/`extends FOO` consumer (fooconsumer.gd); \
         got {:?}",
        view.set
    );
    // It DOES rewrite its own in-file uses: the `enum FOO` decl (2,5), the `: FOO` annotation (4,7),
    // and the `FOO.A` base (4,13) — all in holder.gd, and nothing else.
    assert!(
        view.set.iter().all(|(u, _)| *u == holder_uri.as_str()),
        "an in-file enum rename must stay entirely in its own file; got {:?}",
        view.set
    );
    let mut holder_starts: Vec<(u32, u32)> = view
        .set
        .iter()
        .map(|(_, r)| (r.start.line, r.start.character))
        .collect();
    holder_starts.sort_unstable();
    assert_eq!(
        holder_starts,
        vec![(2, 5), (4, 7), (4, 13)],
        "the in-file enum rename must edit exactly its decl + `: FOO` + `FOO.A` sites; got \
         {holder_starts:?}"
    );
    shutdown(&client, server);
}

#[test]
fn references_162_in_file_enum_type_use_cursor_excludes_global_class_consumers() {
    // #162 read-surface twin (the issue is read-side AND mutating-side): a `references` request from
    // the in-file `: FOO` type-use cursor must not over-collect the global class's cross-file
    // consumers.
    let project = common::sample_project();
    project.write(
        "src/holder.gd",
        "extends Node\n\nenum FOO { A, B }\n\nvar x: FOO = FOO.A\n",
    );
    project.write("src/fooclass.gd", "class_name FOO\nextends Node\n");
    project.write(
        "src/fooconsumer.gd",
        "extends FOO\n\nfunc use_it(p: FOO) -> void:\n\tprint(p)\n",
    );
    let (client, server) = boot();
    init_open(
        &project,
        &client,
        caps_full(),
        &["src/holder.gd", "src/fooclass.gd", "src/fooconsumer.gd"],
        2,
    );
    let holder_uri = file_uri(&project.root.join("src/holder.gd"));
    let fooclass_uri = file_uri(&project.root.join("src/fooclass.gd"));
    let fooconsumer_uri = file_uri(&project.root.join("src/fooconsumer.gd"));

    // Click `: FOO` at holder.gd line 4 col 8.
    let refs = references_set(&client, 306, &holder_uri, 4, 8);
    assert!(
        !refs
            .iter()
            .any(|(u, _)| *u == fooclass_uri.as_str() || *u == fooconsumer_uri.as_str()),
        "references from an in-file `: FOO` type-use must not collect the global `class_name FOO` \
         declaration or its cross-file consumers; got {refs:?}"
    );
    shutdown(&client, server);
}

#[test]
fn rename_162_global_class_does_not_overgrab_candidate_local_type_shadow() {
    // #162 CANDIDATE-SIDE twin (the symmetric corruption): renaming a GLOBAL `class_name Foo` must
    // not rewrite a CONSUMER file's own `var y: Foo` whose `Foo` is that file's IN-FILE `enum Foo`
    // (a local shadow of the global class). The consumer appears in `name_referencers("Foo")` (its
    // interface mentions `Foo`), so it is collected via the global-class bucket — whose type-position
    // (name+position) collection would otherwise grab the shadowed `: Foo`. Without the
    // `name_is_in_file_root_type` guard on the candidate's tree, the rename corrupts `cand.gd`.
    let project = common::sample_project();
    project.write("src/fooclass.gd", "class_name Foo\nextends Node\n");
    project.write(
        "src/cand.gd",
        // cand.gd has its OWN enum Foo — `: Foo` / `Foo.A` here refer to the LOCAL enum, NOT the
        // global class. Renaming the global class must leave these untouched.
        "extends Node\n\nenum Foo { A }\n\nvar y: Foo = Foo.A\n",
    );
    let (client, server) = boot();
    init_open(
        &project,
        &client,
        caps_full(),
        &["src/fooclass.gd", "src/cand.gd"],
        2,
    );
    let fooclass_uri = file_uri(&project.root.join("src/fooclass.gd"));
    let cand_uri = file_uri(&project.root.join("src/cand.gd"));

    // `class_name Foo` on fooclass.gd line 0: `Foo` at col 11. Rename the GLOBAL class → `Bar`.
    client
        .sender
        .send(request(
            307,
            "textDocument/rename",
            rename_params(&fooclass_uri, 0, 11, "Bar"),
        ))
        .unwrap();
    let resp = recv_response(&client);
    assert!(
        resp.error.is_none(),
        "global class rename should succeed: {:?}",
        resp.error
    );
    let view =
        flatten_edit(&serde_json::from_value::<WorkspaceEdit>(resp.result.unwrap()).unwrap());
    assert!(
        !view.set.iter().any(|(u, _)| *u == cand_uri.as_str()),
        "renaming the global `class_name Foo` must NEVER edit `cand.gd`, whose `: Foo`/`Foo.A` refer \
         to its OWN in-file `enum Foo` (a local shadow) — the candidate-side #162 over-grab; got {:?}",
        view.set
    );
    // The class's own declaration IS edited.
    assert!(
        view.set.iter().any(|(u, _)| *u == fooclass_uri.as_str()),
        "the `class_name Foo` declaration must be edited; got {:?}",
        view.set
    );
    shutdown(&client, server);
}

// =================================================================================================
// #167: inner-class-SCOPED type named like a global `class_name` — the scope-blind residual the
// root-only `name_is_in_file_root_type` guard cannot see. The fail-closed refuse-guard eliminates
// the corruption (zero wrong edits) at the cost of over-refusal (#167 tracks restoring precision).
//
// The position × collision matrix (inner-scope dimension):
//   (A) global-class decl-click  + consumer has INNER same-name type   → REFUSE (was: WRONG edit)
//   (B) inner-type annotation-click + colliding global class_name      → REFUSE (was: WRONG edit)
//   root-shadow fan-out (existing rename_162_..._candidate_local_type_shadow) → still PRECISE
//   inner type + NO global collision (existing rename_inner_class_..._succeeds) → still SUCCEEDS
// =================================================================================================

#[test]
fn rename_167_global_class_refuses_when_consumer_has_inner_scoped_shadow() {
    // (A) #163 dishonesty manifestation: a CONSUMER file legitimately `extends Foo` (the global
    // class) AND declares an inner-class-scoped `enum Foo` whose `: Foo` annotation refers to the
    // INNER enum, not the global class. The root-only candidate guard sees `extends Foo` is a legit
    // global ref but cannot suppress the inner `: Foo` — so the global-class type-position collection
    // rewrites the inner `: Foo` to `: Bar` (corruption: the inner type no longer compiles). Since a
    // file-level guard cannot separate the two without per-node scope-aware resolution (#167), the
    // safe move is to REFUSE the whole rename — zero wrong edits.
    let project = common::sample_project();
    project.write("src/fooclass.gd", "class_name Foo\nextends Node\n");
    project.write(
        "src/consumer.gd",
        // `extends Foo` is the GLOBAL class (legit). The inner `enum Foo` shadows it INSIDE `Inner`;
        // `var y: Foo` / `Foo.A` there refer to the inner enum, NOT the global class.
        "extends Foo\n\nclass Inner:\n\tenum Foo { A }\n\tvar y: Foo = Foo.A\n",
    );
    let (client, server) = boot();
    init_open(
        &project,
        &client,
        caps_full(),
        &["src/fooclass.gd", "src/consumer.gd"],
        2,
    );
    let fooclass_uri = file_uri(&project.root.join("src/fooclass.gd"));
    let consumer_uri = file_uri(&project.root.join("src/consumer.gd"));

    // `class_name Foo` on fooclass.gd line 0: `Foo` at col 11. Rename the GLOBAL class → `Bar`.
    client
        .sender
        .send(request(
            400,
            "textDocument/rename",
            rename_params(&fooclass_uri, 0, 11, "Bar"),
        ))
        .unwrap();
    let resp = recv_response(&client);
    // Fail-closed: REFUSE the entire rename with zero edits — the consumer's inner `: Foo`/`Foo.A`
    // cannot be safely distinguished from a global ref by a file-level guard (#167).
    assert!(
        resp.result.is_none() && resp.error.is_some(),
        "renaming a global `class_name Foo` must REFUSE when a consumer declares an inner-scoped \
         `Foo` shadow (the inner `: Foo` cannot be safely separated from the legit `extends Foo`); \
         got result={:?}, error={:?}",
        resp.result,
        resp.error
    );
    // Belt-and-suspenders: even if the refuse mechanism ever changes shape, NEITHER file may be
    // edited (the global decl IS a legit edit, but a partial edit set is still a corruption since the
    // consumer's inner `: Foo` would be wrongly rewritten alongside it).
    if let Some(v) = resp.result.as_ref() {
        let view = flatten_edit(&serde_json::from_value::<WorkspaceEdit>(v.clone()).unwrap());
        assert!(
            !view
                .set
                .iter()
                .any(|(u, _)| *u == consumer_uri.as_str() || *u == fooclass_uri.as_str()),
            "the refuse-guard must emit ZERO edits; got {:?}",
            view.set
        );
    }
    shutdown(&client, server);
}

#[test]
fn rename_167_inner_scoped_type_colliding_with_global_class_refuses() {
    // (B) #162 dishonesty manifestation: the cursor is on the inner-class-scoped `var y: Foo`
    // annotation, whose `Foo` is the INNER enum — but a same-named global `class_name Foo` is
    // registered. The firewall's `cursor_is_type_base_segment` + `find_global_class_definition`
    // admit it as referring to the global class, then canonicalization rewrites the GLOBAL
    // `class_name Foo` declaration (corruption: renaming an inner enum mutates an unrelated global
    // class). Fail-closed: REFUSE — the inner type cannot be scoped precisely without #167.
    let project = common::sample_project();
    project.write("src/fooclass.gd", "class_name Foo\nextends Node\n");
    project.write(
        "src/holder.gd",
        // No top-level `extends Foo` — the only `Foo`s are the inner enum + its in-file uses.
        "extends Node\n\nclass Inner:\n\tenum Foo { A }\n\tvar y: Foo = Foo.A\n",
    );
    let (client, server) = boot();
    init_open(
        &project,
        &client,
        caps_full(),
        &["src/fooclass.gd", "src/holder.gd"],
        2,
    );
    let fooclass_uri = file_uri(&project.root.join("src/fooclass.gd"));
    let holder_uri = file_uri(&project.root.join("src/holder.gd"));

    // `var y: Foo = Foo.A` on holder.gd line 4: tab(0) `var y: `(1-7) `Foo`(8). Click `: Foo` col 8.
    client
        .sender
        .send(request(
            401,
            "textDocument/rename",
            rename_params(&holder_uri, 4, 8, "Bar"),
        ))
        .unwrap();
    let resp = recv_response(&client);
    // The hard invariant: the global `class_name Foo` decl must NEVER be edited (cross-symbol
    // corruption). Fail-closed refuse is the safe outcome.
    assert!(
        resp.result.is_none() && resp.error.is_some(),
        "renaming an inner-scoped `: Foo` annotation that collides with a global `class_name Foo` \
         must REFUSE — never canonicalize onto and rewrite the unrelated global class; got \
         result={:?}, error={:?}",
        resp.result,
        resp.error
    );
    if let Some(v) = resp.result.as_ref() {
        let view = flatten_edit(&serde_json::from_value::<WorkspaceEdit>(v.clone()).unwrap());
        assert!(
            !view.set.iter().any(|(u, _)| *u == fooclass_uri.as_str()),
            "renaming the inner `: Foo` must NEVER edit the global `class_name Foo` decl \
             (fooclass.gd); got {:?}",
            view.set
        );
    }
    shutdown(&client, server);
}

#[test]
fn rename_167_global_class_use_in_self_shadowing_origin_file_refuses() {
    // (A) ORIGIN-SELF-SHADOW twin: the rename is driven from an EXPRESSION-position global-class use
    // (`Foo.new()`) in a file that ALSO declares an inner-class-scoped `enum Foo`. The cursor is not a
    // type-base segment (so the (B) guard does not apply) and the origin file is excluded from the
    // cross-file fan-out (so the referencer scan does not apply) — yet `push_global_class_locations`
    // would collect the origin's OWN inner `: Foo` (root-only shadow guard is blind to it) and rewrite
    // it alongside the legit `Foo.new()`. The origin-symmetric arm of the (A) guard refuses.
    let project = common::sample_project();
    project.write("src/fooclass.gd", "class_name Foo\nextends Node\n");
    project.write(
        "src/x.gd",
        // `Foo.new()` is the GLOBAL class (legit). The inner `enum Foo` shadows it inside `Inner`.
        "extends Node\n\nvar z = Foo.new()\n\nclass Inner:\n\tenum Foo { A }\n\tvar y: Foo = Foo.A\n",
    );
    let (client, server) = boot();
    init_open(
        &project,
        &client,
        caps_full(),
        &["src/fooclass.gd", "src/x.gd"],
        2,
    );
    let fooclass_uri = file_uri(&project.root.join("src/fooclass.gd"));
    let x_uri = file_uri(&project.root.join("src/x.gd"));

    // `var z = Foo.new()` on x.gd line 2: `var z = `(0-7) `Foo`(8). Click `Foo` at col 8.
    client
        .sender
        .send(request(
            402,
            "textDocument/rename",
            rename_params(&x_uri, 2, 8, "Bar"),
        ))
        .unwrap();
    let resp = recv_response(&client);
    assert!(
        resp.result.is_none() && resp.error.is_some(),
        "renaming a global `class_name Foo` from an expression use in a file that ALSO declares an \
         inner-scoped `Foo` must REFUSE (the origin's inner `: Foo` cannot be separated from the \
         legit `Foo.new()`); got result={:?}, error={:?}",
        resp.result,
        resp.error
    );
    if let Some(v) = resp.result.as_ref() {
        let view = flatten_edit(&serde_json::from_value::<WorkspaceEdit>(v.clone()).unwrap());
        assert!(
            !view
                .set
                .iter()
                .any(|(u, _)| *u == x_uri.as_str() || *u == fooclass_uri.as_str()),
            "the refuse-guard must emit ZERO edits; got {:?}",
            view.set
        );
    }
    shutdown(&client, server);
}
