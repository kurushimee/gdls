//! M9 (#67): `textDocument/documentHighlight` — the in-file subset of `references`, tagged with
//! `DocumentHighlightKind::Read`/`Write`.
//!
//! Covers the phase-1 acceptance criteria:
//!   1. `documentHighlightProvider` advertised in `InitializeResult`.
//!   2. A local var written (incl. `+=`) then read returns every in-file occurrence, `Write` on the
//!      assignment + compound-assignment targets and the initializing declaration, `Read` elsewhere.
//!   3. Results are scoped to the request file — a same-named member with a cross-file typed access
//!      (which `references` WOULD return) yields no out-of-file highlight.
//!   4. Ranges are the identifier token, not the whole line.

mod common;

use common::{file_uri, notification, request, shutdown, TempProject};
use lsp_server::Connection;
use lsp_types::{
    DidOpenTextDocumentParams, DocumentHighlight, DocumentHighlightKind, DocumentHighlightParams,
    InitializeParams, InitializeResult, InitializedParams, OneOf, PartialResultParams, Position,
    TextDocumentIdentifier, TextDocumentItem, TextDocumentPositionParams, WorkDoneProgressParams,
};

/// Initialize against `project`, returning the parsed `InitializeResult` (so a test can assert on
/// advertised capabilities), then send `initialized` and open `files`, draining diagnostics.
fn init_and_open(project: &TempProject, client: &Connection, files: &[&str]) -> InitializeResult {
    let init = InitializeParams {
        initialization_options: Some(serde_json::json!({
            "projectRoot": project.root.as_str(),
            "autoDumpExtensionApi": false,
            "extensionApiPath": project.root.join("extension_api.json").as_str(),
        })),
        ..Default::default()
    };
    client.sender.send(request(1, "initialize", init)).unwrap();
    let init_resp = common::recv_response(client);
    assert!(
        init_resp.error.is_none(),
        "initialize errored: {:?}",
        init_resp.error
    );
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
                        version: (i + 2) as i32,
                        text,
                    },
                },
            ))
            .unwrap();
    }
    // Drain all publishDiagnostics pushes.
    while common::try_recv(client, std::time::Duration::from_millis(300)).is_some() {}
    result
}

fn highlight_params(uri: &lsp_types::Uri, line: u32, character: u32) -> DocumentHighlightParams {
    DocumentHighlightParams {
        text_document_position_params: TextDocumentPositionParams {
            text_document: TextDocumentIdentifier { uri: uri.clone() },
            position: Position { line, character },
        },
        work_done_progress_params: WorkDoneProgressParams::default(),
        partial_result_params: PartialResultParams::default(),
    }
}

/// Criterion 1: the server advertises `documentHighlightProvider` in `InitializeResult`.
#[test]
fn document_highlight_provider_advertised() {
    let p = TempProject::new();
    p.write("project.godot", "config_version=5\n");
    p.write("extension_api.json", common::MINI_API);
    p.write("a.gd", "extends Node\n");

    let (server, client) = Connection::memory();
    let server_thread = std::thread::spawn(move || gd_server::serve(server));
    let init = init_and_open(&p, &client, &["a.gd"]);

    let provider = init
        .capabilities
        .document_highlight_provider
        .expect("documentHighlightProvider must be advertised");
    // A plain options struct (no workDoneProgress) is what the handler ships — accept either the
    // bool or options projection, just assert the capability is present and not `false`.
    match provider {
        OneOf::Left(b) => assert!(
            b,
            "documentHighlightProvider must not be advertised as false"
        ),
        OneOf::Right(_) => {}
    }

    shutdown(&client, server_thread);
}

/// Criteria 2 + 4: a local var declared with an initializer, then assigned (`=`), compound-assigned
/// (`+=`), and read (`print(x)`). Every in-file occurrence is returned; the declaration and both
/// assignment targets are `Write`, the read is `Read`; each range is the narrow `count` identifier
/// token (cols width 5), not the whole line.
#[test]
fn document_highlight_local_var_read_write_decl() {
    let p = TempProject::new();
    p.write("project.godot", "config_version=5\n");
    p.write("extension_api.json", common::MINI_API);
    // Line 0: `extends Node`
    // Line 2: `func run() -> void:`
    // Line 3: `\tvar count = 0`   — decl `count` at cols 5..10 (tab + `var ` = 5 bytes), initialized → WRITE
    // Line 4: `\tcount = 1`       — assignment LHS `count` at cols 1..6 → WRITE
    // Line 5: `\tcount += 2`      — compound-assignment LHS `count` at cols 1..6 → WRITE
    // Line 6: `\tprint(count)`    — read `count` at cols 7..12 (`\tprint(` = 7 bytes) → READ
    p.write(
        "loc.gd",
        "extends Node\n\nfunc run() -> void:\n\tvar count = 0\n\tcount = 1\n\tcount += 2\n\tprint(count)\n",
    );

    let (server, client) = Connection::memory();
    let server_thread = std::thread::spawn(move || gd_server::serve(server));
    init_and_open(&p, &client, &["loc.gd"]);

    let uri = file_uri(&p.root.join("loc.gd"));
    // Click on the read occurrence inside `print(count)` (line 6, col 9 — inside `count` at 7..12).
    client
        .sender
        .send(request(
            10,
            "textDocument/documentHighlight",
            highlight_params(&uri, 6, 9),
        ))
        .unwrap();
    let resp = common::recv_response(&client);
    assert!(
        resp.error.is_none(),
        "documentHighlight errored: {:?}",
        resp.error
    );
    let mut hls: Vec<DocumentHighlight> =
        serde_json::from_value(resp.result.expect("documentHighlight result")).unwrap();
    hls.sort_by_key(|h| (h.range.start.line, h.range.start.character));

    // Exactly four occurrences: decl (3,5) Write, (4,1) Write, (5,1) Write, (6,7) Read.
    let kind_at = |line: u32, character: u32| -> DocumentHighlightKind {
        hls.iter()
            .find(|h| h.range.start == Position { line, character })
            .unwrap_or_else(|| panic!("no highlight at ({line},{character}); got {hls:?}"))
            .kind
            .unwrap_or_else(|| panic!("highlight at ({line},{character}) has no kind; got {hls:?}"))
    };
    assert_eq!(
        hls.len(),
        4,
        "expected exactly 4 in-file occurrences of `count`; got {hls:?}"
    );
    assert_eq!(
        kind_at(3, 5),
        DocumentHighlightKind::WRITE,
        "the initializing declaration `var count = 0` must be Write; got {hls:?}"
    );
    assert_eq!(
        kind_at(4, 1),
        DocumentHighlightKind::WRITE,
        "the `count = 1` assignment target must be Write; got {hls:?}"
    );
    assert_eq!(
        kind_at(5, 1),
        DocumentHighlightKind::WRITE,
        "the `count += 2` compound-assignment target must be Write; got {hls:?}"
    );
    assert_eq!(
        kind_at(6, 7),
        DocumentHighlightKind::READ,
        "the `print(count)` read must be Read; got {hls:?}"
    );

    // Criterion 4: every range is the narrow `count` identifier token (width 5), single-line,
    // never the whole line.
    for h in &hls {
        assert_eq!(
            h.range.start.line, h.range.end.line,
            "highlight range must be single-line (the identifier token); got {h:?}"
        );
        assert_eq!(
            h.range.end.character - h.range.start.character,
            5,
            "highlight range must span the `count` identifier (5 chars), not the line; got {h:?}"
        );
    }

    shutdown(&client, server_thread);
}

/// Refusal path (`_feature-workflow.md` §4): a cursor that lands on no identifier degrades to the
/// LSP `null` wire response — never a panic, never a guess. Click an empty line.
#[test]
fn document_highlight_off_identifier_returns_null() {
    let p = TempProject::new();
    p.write("project.godot", "config_version=5\n");
    p.write("extension_api.json", common::MINI_API);
    // Line 0: `extends Node`; line 1: blank; line 2: `var hp: int = 0`.
    p.write("a.gd", "extends Node\n\nvar hp: int = 0\n");

    let (server, client) = Connection::memory();
    let server_thread = std::thread::spawn(move || gd_server::serve(server));
    init_and_open(&p, &client, &["a.gd"]);

    let uri = file_uri(&p.root.join("a.gd"));
    // Click on the blank line 1, col 0 — no identifier under the cursor.
    client
        .sender
        .send(request(
            30,
            "textDocument/documentHighlight",
            highlight_params(&uri, 1, 0),
        ))
        .unwrap();
    let resp = common::recv_response(&client);
    assert!(
        resp.error.is_none(),
        "documentHighlight must not error on an off-identifier cursor: {:?}",
        resp.error
    );
    // The result is the LSP `null` — `Option<Vec<DocumentHighlight>>` deserializes to `None`.
    let hls: Option<Vec<DocumentHighlight>> =
        serde_json::from_value(resp.result.expect("documentHighlight result")).unwrap();
    assert!(
        hls.is_none(),
        "documentHighlight on an off-identifier cursor must be null; got {hls:?}"
    );

    shutdown(&client, server_thread);
}

/// Criterion 3: documentHighlight is scoped to the request file. `a.gd` declares `class_name AClass`
/// with `var hp`; `c.gd` reaches it through a body-local typed var (`a.hp`). `references` on `hp`'s
/// declaration WOULD return that cross-file `a.hp` access — documentHighlight must NOT: every
/// returned range belongs to `a.gd`, and the in-file occurrences (decl + `self.hp` read) are still
/// present.
#[test]
fn document_highlight_is_scoped_to_request_file() {
    let p = TempProject::new();
    p.write("project.godot", "config_version=5\n");
    p.write("extension_api.json", common::MINI_API);
    // a.gd: `class_name AClass`, member `var hp` (decl at line 2, cols 4..6), read `self.hp` at
    // line 4 cols 13..15.
    p.write(
        "a.gd",
        "class_name AClass\nextends Node\nvar hp: int = 0\nfunc fa() -> int:\n\treturn self.hp\n",
    );
    // c.gd reaches `hp` through a body-local typed var — a genuine cross-file reference that
    // `references` returns but documentHighlight (this file's request) must exclude.
    // Line 3: `\treturn a.hp` — `a.hp`'s `hp` attribute at cols 10..12.
    p.write(
        "c.gd",
        "extends Node\nfunc fc() -> int:\n\tvar a: AClass = AClass.new()\n\treturn a.hp\n",
    );

    let (server, client) = Connection::memory();
    let server_thread = std::thread::spawn(move || gd_server::serve(server));
    init_and_open(&p, &client, &["a.gd", "c.gd"]);

    let a_uri = file_uri(&p.root.join("a.gd"));
    let c_uri = file_uri(&p.root.join("c.gd"));

    // Click on `hp`'s declaration in a.gd (line 2, col 5).
    client
        .sender
        .send(request(
            20,
            "textDocument/documentHighlight",
            highlight_params(&a_uri, 2, 5),
        ))
        .unwrap();
    let resp = common::recv_response(&client);
    assert!(
        resp.error.is_none(),
        "documentHighlight errored: {:?}",
        resp.error
    );
    let hls: Vec<DocumentHighlight> =
        serde_json::from_value(resp.result.expect("documentHighlight result")).unwrap();

    // The in-file declaration (line 2, col 4) is present and is a Write (initializing decl).
    let decl = hls
        .iter()
        .find(|h| h.range.start == Position::new(2, 4))
        .unwrap_or_else(|| panic!("the `hp` declaration must be highlighted; got {hls:?}"));
    assert_eq!(
        decl.kind,
        Some(DocumentHighlightKind::WRITE),
        "the initializing `var hp: int = 0` declaration must be Write; got {hls:?}"
    );
    // The in-file `self.hp` read (line 4, col 13) is present and is a Read.
    let read = hls
        .iter()
        .find(|h| h.range.start == Position::new(4, 13))
        .unwrap_or_else(|| panic!("the in-file `self.hp` read must be highlighted; got {hls:?}"));
    assert_eq!(
        read.kind,
        Some(DocumentHighlightKind::READ),
        "the `self.hp` read must be Read; got {hls:?}"
    );
    // Exact count: a.gd has precisely two in-file `hp` occurrences (the decl + the `self.hp` read).
    // Asserting the count guards against silent over-collection (e.g. a cross-file leak surfacing).
    assert_eq!(
        hls.len(),
        2,
        "a.gd must have exactly 2 in-file `hp` occurrences (decl + self.hp read); got {hls:?}"
    );

    // Scoping: documentHighlight returns ranges only — they carry no URI, so the proof of in-file
    // scoping is that the cross-file `a.hp` access in c.gd (line 3, col 10) is NOT among them. That
    // (line 3, col 10) range exists in c.gd but must not appear in a.gd's highlight set; were the
    // cross-file fan-out still wired, a Location at c.gd's URI would surface (documentHighlight
    // would still drop it, but the strongest guard is the count + the c.gd-shaped range absence).
    assert!(
        !hls.iter().any(|h| h.range.start == Position::new(3, 10)),
        "documentHighlight must not include the cross-file c.gd `a.hp` access; got {hls:?}"
    );

    // Re-query c.gd directly to confirm its OWN occurrence is highlighted there (the symbol is
    // genuinely present project-wide; documentHighlight simply scopes per request file).
    client
        .sender
        .send(request(
            21,
            "textDocument/documentHighlight",
            highlight_params(&c_uri, 3, 10),
        ))
        .unwrap();
    let resp_c = common::recv_response(&client);
    assert!(
        resp_c.error.is_none(),
        "documentHighlight (c.gd) errored: {:?}",
        resp_c.error
    );
    let hls_c: Vec<DocumentHighlight> =
        serde_json::from_value(resp_c.result.expect("documentHighlight result")).unwrap();
    assert!(
        hls_c.iter().any(|h| h.range.start == Position::new(3, 10)),
        "documentHighlight on c.gd's `a.hp` must highlight that access in c.gd; got {hls_c:?}"
    );

    shutdown(&client, server_thread);
}

/// Member Read/Write — the analyzer-dependent path with no raw-scan fallback (member uses collect
/// only via the binding `Use` records). `self.hp = 5` (attribute-assignee Write), bare `hp += 1`
/// (compound Write), and `return self.hp` (Read) must each classify correctly when the cursor is on
/// the member declaration. Complements the local-var case in `..._local_var_read_write_decl`.
#[test]
fn document_highlight_member_read_write() {
    let p = TempProject::new();
    p.write("project.godot", "config_version=5\n");
    p.write("extension_api.json", common::MINI_API);
    // Line 0: `class_name BClass`
    // Line 1: `extends Node`
    // Line 2: `var hp: int = 0`     — member decl `hp` at cols 4..6, initialized → WRITE
    // Line 3: `func setit() -> void:`
    // Line 4: `\tself.hp = 5`        — attribute write target `hp` at cols 6..8 (`\tself.`=6) → WRITE
    // Line 5: `\thp += 1`            — bare compound-assign LHS `hp` at cols 1..3 → WRITE
    // Line 6: `func getit() -> int:`
    // Line 7: `\treturn self.hp`     — read `hp` at cols 13..15 (`\treturn self.`=13) → READ
    p.write(
        "b.gd",
        "class_name BClass\nextends Node\nvar hp: int = 0\nfunc setit() -> void:\n\tself.hp = 5\n\thp += 1\nfunc getit() -> int:\n\treturn self.hp\n",
    );

    let (server, client) = Connection::memory();
    let server_thread = std::thread::spawn(move || gd_server::serve(server));
    init_and_open(&p, &client, &["b.gd"]);

    let uri = file_uri(&p.root.join("b.gd"));
    // Click on the member declaration `var hp` (line 2, col 5).
    client
        .sender
        .send(request(
            40,
            "textDocument/documentHighlight",
            highlight_params(&uri, 2, 5),
        ))
        .unwrap();
    let resp = common::recv_response(&client);
    assert!(
        resp.error.is_none(),
        "documentHighlight errored: {:?}",
        resp.error
    );
    let hls: Vec<DocumentHighlight> =
        serde_json::from_value(resp.result.expect("documentHighlight result")).unwrap();

    let kind_at = |line: u32, character: u32| -> DocumentHighlightKind {
        hls.iter()
            .find(|h| h.range.start == Position { line, character })
            .unwrap_or_else(|| panic!("no highlight at ({line},{character}); got {hls:?}"))
            .kind
            .unwrap_or_else(|| panic!("highlight at ({line},{character}) has no kind; got {hls:?}"))
    };
    assert_eq!(
        kind_at(2, 4),
        DocumentHighlightKind::WRITE,
        "the initializing member decl `var hp` must be Write; got {hls:?}"
    );
    assert_eq!(
        kind_at(4, 6),
        DocumentHighlightKind::WRITE,
        "the `self.hp = 5` attribute write target must be Write; got {hls:?}"
    );
    assert_eq!(
        kind_at(5, 1),
        DocumentHighlightKind::WRITE,
        "the bare `hp += 1` compound-assignment target must be Write; got {hls:?}"
    );
    assert_eq!(
        kind_at(7, 13),
        DocumentHighlightKind::READ,
        "the `return self.hp` read must be Read; got {hls:?}"
    );

    shutdown(&client, server_thread);
}

/// #106: documentHighlight on an in-file enum VALUE highlights its declaration token AND its use,
/// by-identity — never an unrelated same-named `const`. Guards the references/documentHighlight decl
/// parity (the EnumValue decl token is emitted explicitly; the use scan emits use sites only).
#[test]
fn document_highlight_enum_value_decl_and_use_not_unrelated_const() {
    let p = TempProject::new();
    p.write("project.godot", "config_version=5\n");
    p.write("extension_api.json", common::MINI_API);
    // Line 1 `enum Direction { NORTH }`  → value decl `NORTH` at col 17
    // Line 2 `const NORTH := 99`         → UNRELATED const decl `NORTH` at col 6
    // Line 4 `\tvar a = Direction.NORTH` → enum value use `NORTH` at col 19
    // Line 5 `\tvar b = NORTH`           → UNRELATED const use `NORTH` at col 9
    p.write(
        "e.gd",
        "extends Node\nenum Direction { NORTH }\nconst NORTH := 99\nfunc go() -> void:\n\tvar a = Direction.NORTH\n\tvar b = NORTH\n\tprint(a + b)\n",
    );
    let (server, client) = Connection::memory();
    let server_thread = std::thread::spawn(move || gd_server::serve(server));
    init_and_open(&p, &client, &["e.gd"]);
    let uri = file_uri(&p.root.join("e.gd"));
    // Click the enum value USE `Direction.NORTH` (line 4, col 19).
    client
        .sender
        .send(request(
            10,
            "textDocument/documentHighlight",
            highlight_params(&uri, 4, 19),
        ))
        .unwrap();
    let resp = common::recv_response(&client);
    assert!(
        resp.error.is_none(),
        "documentHighlight errored: {:?}",
        resp.error
    );
    let mut hls: Vec<DocumentHighlight> =
        serde_json::from_value(resp.result.expect("documentHighlight result")).unwrap();
    hls.sort_by_key(|h| (h.range.start.line, h.range.start.character));
    let starts: Vec<(u32, u32)> = hls
        .iter()
        .map(|h| (h.range.start.line, h.range.start.character))
        .collect();
    // Exactly the enum value's decl (1,17) + its use (4,19) — never the unrelated `const NORTH`
    // decl (2,6) or its use (5,9).
    assert_eq!(
        starts,
        vec![(1, 17), (4, 19)],
        "documentHighlight on the enum value must highlight its decl + use only, never the \
         unrelated `const NORTH` (2,6)/(5,9); got {hls:?}"
    );
    shutdown(&client, server_thread);
}

/// #164 (documentHighlight twin): a `Member` USE clicked at a CROSS-FILE attribute site (`a.hp`,
/// target in another file) must NOT highlight the REQUEST file's own same-named `var hp` declaration.
/// The decl-side `find_in_file_definition(name)` resolved by NAME and over-collected the request
/// file's decl. documentHighlight is single-file: a cross-file member's declaration lives elsewhere,
/// so it is simply not part of this buffer's highlight set.
///
/// `a.gd` declares `var hp` (the cursor's resolved target). `c.gd` has its OWN `var hp` PLUS the
/// cross-file `a.hp` access. Click on `hp` in `c.gd`'s `a.hp`: c.gd's own `var hp` decl must NOT be
/// highlighted (it is a different symbol); only c.gd's `a.hp` use is.
#[test]
fn document_highlight_cross_file_member_use_excludes_own_same_named_decl() {
    let p = TempProject::new();
    p.write("project.godot", "config_version=5\n");
    p.write("extension_api.json", common::MINI_API);
    // a.gd: `class_name AClass`, member `var hp` — the resolved target of `a.hp` below.
    p.write("a.gd", "class_name AClass\nextends Node\nvar hp: int = 0\n");
    // c.gd declares its OWN `var hp` (line 1, col 4..6 — the same-named decl #164 over-collected),
    // and accesses A's `hp` cross-file via a body-local typed var.
    // Line 0: `extends Node`
    // Line 1: `var hp: int = 9`             — c.gd's OWN decl `hp` at cols 4..6 (must NOT highlight)
    // Line 2: `func fc() -> int:`
    // Line 3: `\tvar a: AClass = AClass.new()`
    // Line 4: `\treturn a.hp`                — cross-file `a.hp` use, `hp` at cols 10..12 (target = A)
    p.write(
        "c.gd",
        "extends Node\nvar hp: int = 9\nfunc fc() -> int:\n\tvar a: AClass = AClass.new()\n\treturn a.hp\n",
    );

    let (server, client) = Connection::memory();
    let server_thread = std::thread::spawn(move || gd_server::serve(server));
    init_and_open(&p, &client, &["a.gd", "c.gd"]);

    let c_uri = file_uri(&p.root.join("c.gd"));
    // Click on `hp` in c.gd's cross-file `a.hp` (line 4, col 10).
    client
        .sender
        .send(request(
            70,
            "textDocument/documentHighlight",
            highlight_params(&c_uri, 4, 10),
        ))
        .unwrap();
    let resp = common::recv_response(&client);
    assert!(
        resp.error.is_none(),
        "documentHighlight errored: {:?}",
        resp.error
    );
    let hls: Vec<DocumentHighlight> =
        serde_json::from_value(resp.result.expect("documentHighlight result")).unwrap();

    // c.gd's OWN `var hp` decl (line 1, col 4) is a DIFFERENT symbol — must NOT be highlighted.
    assert!(
        !hls.iter().any(|h| h.range.start == Position::new(1, 4)),
        "documentHighlight on the cross-file `a.hp` use must NOT highlight c.gd's own `var hp` \
         declaration at (1,4); got {hls:?}"
    );
    // The genuine cross-file `a.hp` use in c.gd (line 4, col 10) IS this file's occurrence.
    assert!(
        hls.iter().any(|h| h.range.start == Position::new(4, 10)),
        "documentHighlight must highlight c.gd's own `a.hp` access at (4,10); got {hls:?}"
    );

    shutdown(&client, server_thread);
}
