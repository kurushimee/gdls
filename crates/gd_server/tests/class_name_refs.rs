//! #576 — one class, one set of references, from every anchor.
//!
//! A file's own `class_name` used to answer as two disjoint symbols. The declaration and every
//! type-position use (`-> Widget`, `: Widget`, `as Widget`) formed one set; every in-file
//! expression-position use (`Widget.new()`, `Widget.SIZE`, a bare `Widget` as a value) formed
//! another; they shared only the cross-file uses. `rename` inherited whichever set the cursor
//! landed in and left the file broken either way — the declaration renamed and the constructors
//! not, or the reverse.
//!
//! The cause was in the analyzer: the class-scope walk resolves a class under its own name and
//! recorded the hit as `BindingSymbolKind::Member`, while the global registry and the script chain
//! record the same resolution as `Class`. Two identities for one symbol, and the server's two
//! collectors are each identity-correct, so each saw half. An inner class had it worse — a
//! reference from inside its own body keyed on its own chain rather than its owner's and matched
//! nothing at all.
//!
//! The edit-count assertions are not the point; a corruption bug is only fixed when the rewritten
//! file is clean, so the tests below apply the edits and re-analyze.

mod common;

use std::time::Duration;

use common::{file_uri, notification, recv, recv_response, request, sample_project, shutdown};
use lsp_server::{Connection, Message};
use lsp_types::{
    DidOpenTextDocumentParams, DocumentChanges, InitializeParams, InitializedParams, Location,
    Position, PublishDiagnosticsParams, Range, ReferenceContext, ReferenceParams, RenameParams,
    TextDocumentIdentifier, TextDocumentItem, TextDocumentPositionParams, Uri,
    WorkDoneProgressParams, WorkspaceEdit,
};

const A_GD: &str = "class_name Widget\nextends Node\n\nconst SIZE := 4\n\nstatic func make() -> Widget:\n\treturn Widget.new()\n\nfunc uses() -> void:\n\tprint(Widget.SIZE)\n\tprint(Widget)\n\tvar t = Widget\n\tprint(t)\n\nfunc annotated(other: Widget) -> Widget:\n\treturn other as Widget\n";

const B_GD: &str = "extends Node\n\nfunc f() -> void:\n\tvar w := Widget.make()\n\tprint(w)\n";

const LIB_GD: &str = "class_name RcOuter\nextends Node\n\nclass Inner:\n\tconst IC := 1\n\tfunc go() -> Inner:\n\t\treturn Inner.new()\n\nfunc other() -> void:\n\tprint(Inner.IC)\n\tvar i := Inner.new()\n\tprint(i)\n";

const LIBUSE_GD: &str =
    "extends Node\n\nfunc f() -> void:\n\tvar i: RcOuter.Inner = RcOuter.Inner.new()\n\tprint(i)\n";

fn boot(project: &common::TempProject, client: &Connection) {
    let init = InitializeParams {
        initialization_options: Some(serde_json::json!({
            "projectRoot": project.root.as_str(),
            "autoDumpExtensionApi": false,
            "extensionApiPath": project.root.join("extension_api.json").as_str(),
        })),
        capabilities: serde_json::from_value(serde_json::json!({
            "workspace": { "workspaceEdit": { "documentChanges": true } }
        }))
        .expect("client caps"),
        ..Default::default()
    };
    client.sender.send(request(1, "initialize", init)).unwrap();
    let _ = recv(client);
    client
        .sender
        .send(notification("initialized", InitializedParams {}))
        .unwrap();
}

fn did_open(client: &Connection, uri: &Uri, text: &str, version: i32) -> Vec<String> {
    client
        .sender
        .send(notification(
            "textDocument/didOpen",
            DidOpenTextDocumentParams {
                text_document: TextDocumentItem {
                    uri: uri.clone(),
                    language_id: "gdscript".to_string(),
                    version,
                    text: text.to_string(),
                },
            },
        ))
        .unwrap();
    loop {
        let Message::Notification(note) = recv(client) else {
            continue;
        };
        if note.method != "textDocument/publishDiagnostics" {
            continue;
        }
        let params: PublishDiagnosticsParams = serde_json::from_value(note.params).unwrap();
        if &params.uri == uri {
            return params.diagnostics.into_iter().map(|d| d.message).collect();
        }
    }
}

/// Re-request a file's diagnostics on an already-booted client by reopening it at a new version.
fn client_diags(client: &Connection, uri: &Uri, text: &str) -> Vec<String> {
    did_open(client, uri, text, 99)
}

fn position_params(uri: &Uri, line: u32, character: u32) -> TextDocumentPositionParams {
    TextDocumentPositionParams {
        text_document: TextDocumentIdentifier { uri: uri.clone() },
        position: Position { line, character },
    }
}

fn cmp_range(a: &Range, b: &Range) -> std::cmp::Ordering {
    (a.start.line, a.start.character).cmp(&(b.start.line, b.start.character))
}

/// The `(file name, line, column)` of every reference, sorted — the shape a human can read in a
/// failure message.
fn references(client: &Connection, uri: &Uri, line: u32, ch: u32) -> Vec<(String, u32, u32)> {
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
        .send(request(70, "textDocument/references", params))
        .unwrap();
    let resp = recv_response(client);
    let locs: Vec<Location> =
        serde_json::from_value(resp.result.expect("references result")).unwrap();
    let mut out: Vec<(String, u32, u32)> = locs
        .into_iter()
        .map(|l| {
            (
                l.uri
                    .as_str()
                    .rsplit('/')
                    .next()
                    .unwrap_or_default()
                    .to_string(),
                l.range.start.line,
                l.range.start.character,
            )
        })
        .collect();
    out.sort();
    out
}

fn rename(client: &Connection, uri: &Uri, line: u32, ch: u32, to: &str) -> WorkspaceEdit {
    let params = RenameParams {
        text_document_position: position_params(uri, line, ch),
        new_name: to.to_string(),
        work_done_progress_params: WorkDoneProgressParams::default(),
    };
    client
        .sender
        .send(request(71, "textDocument/rename", params))
        .unwrap();
    let resp = recv_response(client);
    serde_json::from_value(resp.result.expect("rename result")).expect("a WorkspaceEdit")
}

/// Per-file `(range, new_text)` edits, keyed by the file's base name.
fn edits_by_file(edit: &WorkspaceEdit) -> Vec<(String, Vec<(Range, String)>)> {
    let Some(DocumentChanges::Edits(tde)) = &edit.document_changes else {
        panic!("documentChanges was advertised; got {edit:?}");
    };
    tde.iter()
        .map(|e| {
            let name = e
                .text_document
                .uri
                .as_str()
                .rsplit('/')
                .next()
                .unwrap_or_default()
                .to_string();
            let mut es: Vec<(Range, String)> = e
                .edits
                .iter()
                .map(|o| match o {
                    lsp_types::OneOf::Left(te) => (te.range, te.new_text.clone()),
                    other => panic!("annotated edit not expected: {other:?}"),
                })
                .collect();
            es.sort_by(|a, b| cmp_range(&a.0, &b.0));
            (name, es)
        })
        .collect()
}

/// Apply non-overlapping edits last-to-first so earlier ranges stay valid.
fn apply(src: &str, edits: &[(Range, String)]) -> String {
    let line_starts: Vec<usize> = std::iter::once(0)
        .chain(src.match_indices('\n').map(|(i, _)| i + 1))
        .collect();
    let to_byte = |p: Position| line_starts[p.line as usize] + p.character as usize;
    let mut byte_edits: Vec<(usize, usize, String)> = edits
        .iter()
        .map(|(r, t)| (to_byte(r.start), to_byte(r.end), t.clone()))
        .collect();
    byte_edits.sort_by_key(|&(start, _, _)| std::cmp::Reverse(start));
    let mut out = src.to_string();
    for (start, end, text) in byte_edits {
        out.replace_range(start..end, &text);
    }
    out
}

struct Fixture {
    project: common::TempProject,
    files: Vec<(&'static str, &'static str)>,
}

impl Fixture {
    fn head_class() -> Fixture {
        let project = sample_project();
        let files = vec![("src/a.gd", A_GD), ("src/b.gd", B_GD)];
        for (rel, text) in &files {
            project.write(rel, text);
        }
        Fixture { project, files }
    }

    fn inner_class() -> Fixture {
        let project = sample_project();
        let files = vec![("src/lib.gd", LIB_GD), ("src/libuse.gd", LIBUSE_GD)];
        for (rel, text) in &files {
            project.write(rel, text);
        }
        Fixture { project, files }
    }

    /// Boot a server with every fixture file open, and hand back the client and its join handle.
    fn open(&self) -> (Connection, std::thread::JoinHandle<anyhow::Result<()>>) {
        let (server, client) = Connection::memory();
        let handle = std::thread::spawn(move || gd_server::serve(server));
        boot(&self.project, &client);
        for (i, (rel, text)) in self.files.iter().enumerate() {
            let uri = file_uri(&self.project.root.join(rel));
            did_open(&client, &uri, text, 1 + i as i32);
        }
        (client, handle)
    }

    fn uri(&self, rel: &str) -> Uri {
        file_uri(&self.project.root.join(rel))
    }
}

/// Every anchor answers with the same complete set. The reference columns are spelled out so a
/// regression names the site it lost rather than just a count.
#[test]
fn a_class_answers_the_same_references_from_every_anchor() {
    let fx = Fixture::head_class();
    let (client, handle) = fx.open();
    let a = fx.uri("src/a.gd");
    let b = fx.uri("src/b.gd");

    let expected: Vec<(String, u32, u32)> = vec![
        ("a.gd".into(), 0, 11),  // class_name Widget
        ("a.gd".into(), 5, 22),  // -> Widget
        ("a.gd".into(), 6, 8),   // return Widget.new()
        ("a.gd".into(), 9, 7),   // print(Widget.SIZE)
        ("a.gd".into(), 10, 7),  // print(Widget)
        ("a.gd".into(), 11, 9),  // var t = Widget
        ("a.gd".into(), 14, 22), // other: Widget
        ("a.gd".into(), 14, 33), // -> Widget
        ("a.gd".into(), 15, 17), // other as Widget
        ("b.gd".into(), 3, 10),  // Widget.make()
    ];

    for (tag, uri, line, ch) in [
        ("the declaration", &a, 0, 13),
        ("an expression use", &a, 6, 10),
        ("a bare value use", &a, 10, 10),
        ("a type annotation", &a, 14, 24),
        ("the cross-file use", &b, 3, 12),
    ] {
        assert_eq!(
            references(&client, uri, line, ch),
            expected,
            "anchored at {tag}"
        );
    }

    shutdown(&client, handle);
}

/// The corruption test. An edit count proves nothing here: apply the edits, then re-open the
/// rewritten files and assert the analyzer has nothing to say about them.
#[test]
fn renaming_a_class_from_any_anchor_leaves_both_files_clean() {
    for (tag, rel, line, ch) in [
        ("the declaration", "src/a.gd", 0, 13),
        ("an expression use", "src/a.gd", 6, 10),
        ("a type annotation", "src/a.gd", 14, 24),
        ("the cross-file use", "src/b.gd", 3, 12),
    ] {
        let fx = Fixture::head_class();
        let (client, handle) = fx.open();
        let edit = rename(&client, &fx.uri(rel), line, ch, "Gadget");
        let per_file = edits_by_file(&edit);
        shutdown(&client, handle);

        let rewritten: Vec<(&str, String)> = fx
            .files
            .iter()
            .map(|(rel, text)| {
                let name = rel.rsplit('/').next().unwrap_or_default();
                let edits = per_file
                    .iter()
                    .find(|(f, _)| f == name)
                    .map(|(_, e)| e.clone())
                    .unwrap_or_default();
                (*rel, apply(text, &edits))
            })
            .collect();

        for (rel, text) in &rewritten {
            assert!(
                !text.contains("Widget"),
                "anchored at {tag}: {rel} still names the old class:\n{text}"
            );
        }

        // Re-analyze the rewritten source. A rename is a pure renaming, so the diagnostic set
        // must be exactly what the original produced — a half-applied one adds an undeclared
        // class here. Compared against the original rather than against empty because the
        // fixture's trimmed dump marks `Node` abstract, so `Widget.new()` already reports.
        let before = Fixture::head_class();
        let (bclient, bhandle) = before.open();
        let baseline: Vec<Vec<String>> = before
            .files
            .iter()
            .map(|(rel, text)| {
                let uri = file_uri(&before.project.root.join(rel));
                client_diags(&bclient, &uri, text)
            })
            .collect();
        shutdown(&bclient, bhandle);

        let fresh = sample_project();
        for (rel, text) in &rewritten {
            fresh.write(rel, text);
        }
        let (server2, client2) = Connection::memory();
        let handle2 = std::thread::spawn(move || gd_server::serve(server2));
        boot(&fresh, &client2);
        for (i, ((rel, text), base)) in rewritten.iter().zip(&baseline).enumerate() {
            let diags = did_open(
                &client2,
                &file_uri(&fresh.root.join(rel)),
                text,
                1 + i as i32,
            );
            let renamed: Vec<String> = base.iter().map(|m| m.replace("Widget", "Gadget")).collect();
            assert_eq!(
                diags, renamed,
                "anchored at {tag}: the rewritten {rel} says something new"
            );
        }
        shutdown(&client2, handle2);
    }
}

/// An inner class referenced from inside its own body used to match nothing but itself, because
/// the use was keyed on the class's own chain instead of its owner's.
#[test]
fn an_inner_class_answers_the_same_from_inside_and_outside_its_body() {
    let fx = Fixture::inner_class();
    let (client, handle) = fx.open();
    let lib = fx.uri("src/lib.gd");
    let usef = fx.uri("src/libuse.gd");

    let from_decl = references(&client, &lib, 3, 7);
    assert_eq!(from_decl.len(), 7, "the whole set: {from_decl:?}");
    for (tag, uri, line, ch) in [
        ("its own body", &lib, 6, 10),
        ("the outer body", &lib, 9, 9),
        ("the outer body again", &lib, 10, 12),
        ("a cross-file use", &usef, 3, 16),
    ] {
        assert_eq!(
            references(&client, uri, line, ch),
            from_decl,
            "anchored at {tag}"
        );
    }

    shutdown(&client, handle);
}

/// A local that happens to share the class's name is a different symbol and stays untouched — the
/// unification widens which bindings carry the class's identity, never what a collector sweeps up
/// by name.
#[test]
fn a_local_shadowing_the_class_name_is_not_collected() {
    let project = sample_project();
    let src = "class_name ShadowMe\nextends Node\n\nfunc f() -> void:\n\tvar ShadowMe := 1\n\tprint(ShadowMe)\n\nfunc g() -> ShadowMe:\n\treturn ShadowMe.new()\n";
    project.write("src/shadow.gd", src);
    let (server, client) = Connection::memory();
    let handle = std::thread::spawn(move || gd_server::serve(server));
    boot(&project, &client);
    let uri = file_uri(&project.root.join("src/shadow.gd"));
    did_open(&client, &uri, src, 1);

    // Anchored on the class declaration: the two local lines must not appear.
    let refs = references(&client, &uri, 0, 13);
    assert!(
        !refs.iter().any(|(_, l, _)| *l == 4 || *l == 5),
        "the local is a different symbol; got {refs:?}"
    );

    shutdown(&client, handle);
}

/// documentHighlight shares the collector, so it healed with the rest; pin it so it cannot drift
/// back to answering by cursor position.
#[test]
fn document_highlight_agrees_from_every_anchor() {
    let fx = Fixture::head_class();
    let (client, handle) = fx.open();
    let a = fx.uri("src/a.gd");

    let mut sets = Vec::new();
    for (line, ch) in [(0u32, 13u32), (6, 10), (14, 24)] {
        client
            .sender
            .send(request(
                72,
                "textDocument/documentHighlight",
                lsp_types::DocumentHighlightParams {
                    text_document_position_params: position_params(&a, line, ch),
                    work_done_progress_params: WorkDoneProgressParams::default(),
                    partial_result_params: lsp_types::PartialResultParams::default(),
                },
            ))
            .unwrap();
        let resp = recv_response(&client);
        let hl: Vec<lsp_types::DocumentHighlight> =
            serde_json::from_value(resp.result.expect("documentHighlight result")).unwrap();
        let mut starts: Vec<(u32, u32)> = hl
            .into_iter()
            .map(|h| (h.range.start.line, h.range.start.character))
            .collect();
        starts.sort();
        sets.push(starts);
    }
    assert_eq!(sets[0], sets[1], "declaration versus expression anchor");
    assert_eq!(sets[0], sets[2], "declaration versus annotation anchor");
    assert_eq!(sets[0].len(), 9, "every in-file site: {:?}", sets[0]);

    shutdown(&client, handle);
}

/// The route into `definition` changes for an expression-position use; it must still land on the
/// declaration.
#[test]
fn definition_from_an_expression_use_still_reaches_the_declaration() {
    let fx = Fixture::head_class();
    let (client, handle) = fx.open();
    let a = fx.uri("src/a.gd");

    client
        .sender
        .send(request(
            73,
            "textDocument/definition",
            lsp_types::GotoDefinitionParams {
                text_document_position_params: position_params(&a, 6, 10),
                work_done_progress_params: WorkDoneProgressParams::default(),
                partial_result_params: lsp_types::PartialResultParams::default(),
            },
        ))
        .unwrap();
    let resp = recv_response(&client);
    let loc: Location =
        serde_json::from_value(resp.result.expect("definition result")).expect("one location");
    assert!(loc.uri.as_str().ends_with("a.gd"), "got {loc:?}");
    assert_eq!(loc.range.start, Position::new(0, 11));

    let _ = Duration::from_secs(0);
    shutdown(&client, handle);
}
