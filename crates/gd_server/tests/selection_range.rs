//! M9 (#70): `textDocument/selectionRange` — the "smart-select" ancestor chain. For each requested
//! cursor position the server returns one `SelectionRange` whose `parent` links walk innermost →
//! root, each parent range strictly containing its child.
//!
//! Covers the phase-2 acceptance criteria for selection:
//!   1. `selectionRangeProvider` advertised in `InitializeResult`.
//!   2. A cursor inside a nested expression yields a strictly-increasing ancestor chain (each parent
//!      strictly contains its child; innermost covers the cursor).
//!   3. Multiple positions are answered index-aligned (one chain per requested position).
//!   4. A position over no node / malformed input still returns a (degenerate) range, never panics.

mod common;

use common::{file_uri, notification, request, shutdown, TempProject};
use lsp_server::Connection;
use lsp_types::{
    ClientCapabilities, DidOpenTextDocumentParams, InitializeParams, InitializeResult,
    InitializedParams, PartialResultParams, Position, Range, SelectionRange, SelectionRangeParams,
    TextDocumentIdentifier, TextDocumentItem, Uri, WorkDoneProgressParams,
};

fn init_and_open(project: &TempProject, client: &Connection, files: &[(&str, &str)]) {
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

    client
        .sender
        .send(notification("initialized", InitializedParams {}))
        .unwrap();

    for (i, (rel, text)) in files.iter().enumerate() {
        project.write(rel, text);
        let abs = project.root.join(rel);
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
                        text: text.to_string(),
                    },
                },
            ))
            .unwrap();
    }
    while common::try_recv(client, std::time::Duration::from_millis(300)).is_some() {}
}

fn selection_params(uri: &Uri, positions: Vec<Position>) -> SelectionRangeParams {
    SelectionRangeParams {
        text_document: TextDocumentIdentifier { uri: uri.clone() },
        positions,
        work_done_progress_params: WorkDoneProgressParams::default(),
        partial_result_params: PartialResultParams::default(),
    }
}

fn request_selection(
    client: &Connection,
    id: i32,
    uri: &Uri,
    positions: Vec<Position>,
) -> Vec<SelectionRange> {
    client
        .sender
        .send(request(
            id,
            "textDocument/selectionRange",
            selection_params(uri, positions),
        ))
        .unwrap();
    let resp = common::recv_response(client);
    assert!(
        resp.error.is_none(),
        "selectionRange errored: {:?}",
        resp.error
    );
    serde_json::from_value(resp.result.expect("selectionRange result")).unwrap()
}

fn base_project() -> TempProject {
    let p = TempProject::new();
    p.write("project.godot", "config_version=5\n");
    p.write("extension_api.json", common::MINI_API);
    p
}

/// `a` strictly contains `b` (proper superset): `a.start <= b.start`, `b.end <= a.end`, and the two
/// are not the identical range.
fn strictly_contains(a: &Range, b: &Range) -> bool {
    let le = |x: &Position, y: &Position| (x.line, x.character) <= (y.line, y.character);
    le(&a.start, &b.start) && le(&b.end, &a.end) && a != b
}

/// Flatten a `SelectionRange` linked list into its ranges, innermost → outermost.
fn chain(mut sr: &SelectionRange) -> Vec<Range> {
    let mut out = vec![sr.range];
    while let Some(parent) = &sr.parent {
        out.push(parent.range);
        sr = parent;
    }
    out
}

/// Criterion 1: the server advertises `selectionRangeProvider`.
#[test]
fn selection_range_provider_advertised() {
    let p = base_project();
    let (server, client) = Connection::memory();
    let server_thread = std::thread::spawn(move || gd_server::serve(server));
    let init = InitializeParams {
        initialization_options: Some(serde_json::json!({
            "projectRoot": p.root.as_str(),
            "autoDumpExtensionApi": false,
            "extensionApiPath": p.root.join("extension_api.json").as_str(),
        })),
        capabilities: ClientCapabilities::default(),
        ..Default::default()
    };
    client.sender.send(request(1, "initialize", init)).unwrap();
    let init_resp = common::recv_response(&client);
    let result: InitializeResult =
        serde_json::from_value(init_resp.result.expect("initialize result")).unwrap();
    assert!(
        result.capabilities.selection_range_provider.is_some(),
        "selectionRangeProvider must be advertised"
    );
    client
        .sender
        .send(notification("exit", serde_json::Value::Null))
        .unwrap();
    let _ = server_thread.join();
}

/// Criterion 2: a cursor inside a deeply nested expression returns a strictly-increasing ancestor
/// chain. The cursor sits on the inner identifier `b` in `print(a + (b * c))` — the chain climbs
/// `b` → the multiply → the parenthesized add → the call args → the call → … → the suite, each
/// parent strictly containing its child.
#[test]
fn nested_expression_yields_strict_ancestor_chain() {
    let p = base_project();
    let (server, client) = Connection::memory();
    let server_thread = std::thread::spawn(move || gd_server::serve(server));
    // Line 3: `\tprint(a + (b * c))`
    //   tab(0) p r i n t ( a   +   ( b ...
    //   byte:  1 2 3 4 5 6 7 8 9 ...  `b` is at column 12 (0-based) — `\tprint(a + (` = 12 chars.
    let src = "extends Node\n\nfunc run(a: int, b: int, c: int) -> void:\n\tprint(a + (b * c))\n";
    init_and_open(&p, &client, &[("nested.gd", src)]);
    let uri = file_uri(&p.root.join("nested.gd"));

    // Cursor on `b` (line 3, column 12).
    let pos = Position {
        line: 3,
        character: 12,
    };
    let result = request_selection(&client, 10, &uri, vec![pos]);
    assert_eq!(
        result.len(),
        1,
        "one chain for one position; got {result:?}"
    );
    let ranges = chain(&result[0]);

    // The chain must be more than one deep (a nested expression has ancestors).
    assert!(
        ranges.len() >= 3,
        "expected a multi-level ancestor chain for a nested expr; got {ranges:?}"
    );

    // The innermost range must cover the cursor.
    let innermost = ranges[0];
    let covers = |r: &Range, pos: &Position| {
        let le = |x: &Position, y: &Position| (x.line, x.character) <= (y.line, y.character);
        le(&r.start, pos) && le(pos, &r.end)
    };
    assert!(
        covers(&innermost, &pos),
        "innermost range {innermost:?} must cover the cursor {pos:?}"
    );
    // The innermost range should be the single-char `b` identifier.
    assert_eq!(
        innermost,
        Range {
            start: Position {
                line: 3,
                character: 12
            },
            end: Position {
                line: 3,
                character: 13
            }
        },
        "innermost selection should be the `b` identifier; got {innermost:?}"
    );

    // Each parent strictly contains its child — the defining property.
    for w in ranges.windows(2) {
        assert!(
            strictly_contains(&w[1], &w[0]),
            "parent {:?} must strictly contain child {:?}; full chain {ranges:?}",
            w[1],
            w[0]
        );
    }

    shutdown(&client, server_thread);
}

/// Criterion 3: multiple positions are answered index-aligned — one chain per requested position,
/// in order. Two distinct cursors (on `a` and on `c`) each get their own chain rooted at the right
/// identifier.
#[test]
fn multiple_positions_are_index_aligned() {
    let p = base_project();
    let (server, client) = Connection::memory();
    let server_thread = std::thread::spawn(move || gd_server::serve(server));
    let src = "extends Node\n\nfunc run(a: int, b: int, c: int) -> void:\n\tprint(a + (b * c))\n";
    init_and_open(&p, &client, &[("multi.gd", src)]);
    let uri = file_uri(&p.root.join("multi.gd"));

    // `a` at column 7 (`\tprint(` = 7), `c` at column 16 (`\tprint(a + (b * ` = 16).
    let pos_a = Position {
        line: 3,
        character: 7,
    };
    let pos_c = Position {
        line: 3,
        character: 16,
    };
    let result = request_selection(&client, 10, &uri, vec![pos_a, pos_c]);
    assert_eq!(
        result.len(),
        2,
        "two positions → two chains, index-aligned; got {result:?}"
    );

    // result[0] innermost is `a` (cols 7..8); result[1] innermost is `c` (cols 16..17).
    assert_eq!(
        result[0].range,
        Range {
            start: Position {
                line: 3,
                character: 7
            },
            end: Position {
                line: 3,
                character: 8
            }
        },
        "result[0] should be the `a` identifier; got {:?}",
        result[0].range
    );
    assert_eq!(
        result[1].range,
        Range {
            start: Position {
                line: 3,
                character: 16
            },
            end: Position {
                line: 3,
                character: 17
            }
        },
        "result[1] should be the `c` identifier; got {:?}",
        result[1].range
    );

    // Both chains still satisfy strict containment.
    for sr in &result {
        for w in chain(sr).windows(2) {
            assert!(
                strictly_contains(&w[1], &w[0]),
                "parent {:?} must strictly contain child {:?}",
                w[1],
                w[0]
            );
        }
    }

    shutdown(&client, server_thread);
}

/// Criterion 4: a position over no node (well past end-of-input) and a malformed file both return a
/// (degenerate) range and never panic. The result stays index-aligned with the request.
#[test]
fn out_of_range_and_malformed_never_panic() {
    let p = base_project();
    let (server, client) = Connection::memory();
    let server_thread = std::thread::spawn(move || gd_server::serve(server));
    // A partial parse (unclosed call) plus a request far past the end.
    let broken = "extends Node\n\nfunc run():\n\tprint(\n";
    init_and_open(&p, &client, &[("broken.gd", broken)]);
    let uri = file_uri(&p.root.join("broken.gd"));

    // One in-bounds-ish position and one wildly out of range (line 999) — both must come back.
    let positions = vec![
        Position {
            line: 3,
            character: 2,
        },
        Position {
            line: 999,
            character: 999,
        },
    ];
    let result = request_selection(&client, 10, &uri, positions.clone());
    assert_eq!(
        result.len(),
        positions.len(),
        "result must be index-aligned with the requested positions even on partial input; got {result:?}"
    );
    // Every returned chain must still be strictly-increasing (degenerate single-node chains trivially
    // satisfy this — there are no windows).
    for sr in &result {
        for w in chain(sr).windows(2) {
            assert!(
                strictly_contains(&w[1], &w[0]),
                "parent {:?} must strictly contain child {:?}",
                w[1],
                w[0]
            );
        }
    }

    shutdown(&client, server_thread);
}
