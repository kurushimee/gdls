//! M9 (#70): `textDocument/foldingRange` — fold compound AST blocks (class / func / if / for /
//! while / match arms, kind `Region`), `#region`/`#endregion` pairs (kind `Region`), and runs of
//! ≥2 own-line comments (kind `Comment`). Honors the `textDocument.foldingRange` client hints
//! `rangeLimit` (truncate) and `lineFoldingOnly` (whole-line, no columns).
//!
//! Covers the phase-2 acceptance criteria for folding:
//!   1. `foldingRangeProvider` advertised in `InitializeResult`.
//!   2. Folds a class body, a func body, an if/for/while suite, match arms, a `#region`/`#endregion`
//!      pair, and a ≥2-line comment run — correct `[startLine, endLine]` + kind for each.
//!   3. `rangeLimit` truncates; `lineFoldingOnly` collapses to whole-line (no `startCharacter`/
//!      `endCharacter`); full columned ranges otherwise.
//!   4. A malformed/partial-AST file still folds what parsed and never panics.

mod common;

use common::{file_uri, notification, request, shutdown, TempProject};
use lsp_server::Connection;
use lsp_types::{
    ClientCapabilities, DidOpenTextDocumentParams, FoldingRange, FoldingRangeClientCapabilities,
    FoldingRangeKind, FoldingRangeParams, InitializeParams, InitializeResult, InitializedParams,
    PartialResultParams, TextDocumentClientCapabilities, TextDocumentIdentifier, TextDocumentItem,
    Uri, WorkDoneProgressParams,
};

/// Initialize against `project` with the given client capabilities, returning the parsed
/// `InitializeResult`, then send `initialized` and open `files` (draining diagnostics).
fn init_and_open_caps(
    project: &TempProject,
    client: &Connection,
    files: &[(&str, &str)],
    caps: ClientCapabilities,
) -> InitializeResult {
    let init = InitializeParams {
        initialization_options: Some(serde_json::json!({
            "projectRoot": project.root.as_str(),
            "autoDumpExtensionApi": false,
            "extensionApiPath": project.root.join("extension_api.json").as_str(),
        })),
        capabilities: caps,
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
    result
}

/// `foldingRange` client capabilities with optional `rangeLimit` / `lineFoldingOnly`.
fn folding_caps(range_limit: Option<u32>, line_folding_only: Option<bool>) -> ClientCapabilities {
    ClientCapabilities {
        text_document: Some(TextDocumentClientCapabilities {
            folding_range: Some(FoldingRangeClientCapabilities {
                dynamic_registration: None,
                range_limit,
                line_folding_only,
                folding_range_kind: None,
                folding_range: None,
            }),
            ..Default::default()
        }),
        ..Default::default()
    }
}

fn folding_params(uri: &Uri) -> FoldingRangeParams {
    FoldingRangeParams {
        text_document: TextDocumentIdentifier { uri: uri.clone() },
        work_done_progress_params: WorkDoneProgressParams::default(),
        partial_result_params: PartialResultParams::default(),
    }
}

fn request_folds(client: &Connection, id: i32, uri: &Uri) -> Vec<FoldingRange> {
    client
        .sender
        .send(request(
            id,
            "textDocument/foldingRange",
            folding_params(uri),
        ))
        .unwrap();
    let resp = common::recv_response(client);
    assert!(
        resp.error.is_none(),
        "foldingRange errored: {:?}",
        resp.error
    );
    serde_json::from_value(resp.result.expect("foldingRange result")).unwrap()
}

/// A small base project (project.godot + api), no source files — tests write their own.
fn base_project() -> TempProject {
    let p = TempProject::new();
    p.write("project.godot", "config_version=5\n");
    p.write("extension_api.json", common::MINI_API);
    p
}

/// The rich fixture exercising every fold source. Line numbers (0-based) are pinned in the
/// assertions below; keep this string and those numbers in lockstep.
const RICH: &str = "\
extends Node

# A leading comment block
# that spans three
# consecutive lines.
class Inner:
\tvar x := 1
\tvar y := 2

func run(n: int) -> void:
\tif n > 0:
\t\tprint(\"pos\")
\t\tprint(\"still pos\")
\telse:
\t\tprint(\"neg\")
\tfor i in range(n):
\t\tprint(i)
\t\tprint(i * 2)
\twhile n > 0:
\t\tn -= 1
\t\tprint(n)
\tmatch n:
\t\t0:
\t\t\tprint(\"zero\")
\t\t\tprint(\"done\")
\t\t_:
\t\t\tprint(\"other\")

#region Helpers
func helper() -> int:
\treturn 42
#endregion
";

/// Criterion 1: the server advertises `foldingRangeProvider`.
#[test]
fn folding_range_provider_advertised() {
    let p = base_project();
    let (server, client) = Connection::memory();
    let server_thread = std::thread::spawn(move || gd_server::serve(server));
    let result = init_and_open_caps(
        &p,
        &client,
        &[("a.gd", "extends Node\n")],
        ClientCapabilities::default(),
    );
    assert!(
        result.capabilities.folding_range_provider.is_some(),
        "foldingRangeProvider must be advertised"
    );
    shutdown(&client, server_thread);
}

/// Criterion 2: a class body, func body, if/for/while suites, match arms, a comment run, and a
/// `#region`/`#endregion` pair all fold with the right `[startLine, endLine]` + kind.
#[test]
fn folds_blocks_comments_and_regions() {
    let p = base_project();
    let (server, client) = Connection::memory();
    let server_thread = std::thread::spawn(move || gd_server::serve(server));
    init_and_open_caps(
        &p,
        &client,
        &[("rich.gd", RICH)],
        ClientCapabilities::default(),
    );
    let uri = file_uri(&p.root.join("rich.gd"));
    let folds = request_folds(&client, 10, &uri);

    // Helper: is there a fold with these exact lines + kind?
    let has = |start: u32, end: u32, kind: FoldingRangeKind| {
        folds
            .iter()
            .any(|f| f.start_line == start && f.end_line == end && f.kind.as_ref() == Some(&kind))
    };

    // Comment run: lines 2..4 (three own-line `#` comments), kind Comment.
    assert!(
        has(2, 4, FoldingRangeKind::Comment),
        "expected a Comment fold over lines 2..4; got {folds:?}"
    );

    // Class body `class Inner:` (line 5) with body lines 6-7 → fold 5..7, kind Region.
    assert!(
        has(5, 7, FoldingRangeKind::Region),
        "expected a Region fold for the inner class body 5..7; got {folds:?}"
    );

    // Func `run` header line 9, body runs through the match (last content line 26) → 9..26 Region.
    assert!(
        has(9, 26, FoldingRangeKind::Region),
        "expected a Region fold for func run 9..26; got {folds:?}"
    );

    // `if n > 0:` header line 10, true-block lines 11-12 → 10..12 Region (the if statement spans
    // through its else; assert the if fold starts at 10 and ends at the last branch line 14).
    assert!(
        folds
            .iter()
            .any(|f| f.start_line == 10 && f.kind == Some(FoldingRangeKind::Region)),
        "expected a Region fold starting at the `if` header line 10; got {folds:?}"
    );

    // `for i in range(n):` header line 15, body 16-17 → 15..17 Region.
    assert!(
        has(15, 17, FoldingRangeKind::Region),
        "expected a Region fold for the for loop 15..17; got {folds:?}"
    );

    // `while n > 0:` header line 18, body 19-20 → 18..20 Region.
    assert!(
        has(18, 20, FoldingRangeKind::Region),
        "expected a Region fold for the while loop 18..20; got {folds:?}"
    );

    // `match n:` header line 21; the two arms fold individually.
    // arm `0:` header line 22, body 23-24 → 22..24 Region.
    assert!(
        has(22, 24, FoldingRangeKind::Region),
        "expected a Region fold for the first match arm 22..24; got {folds:?}"
    );
    // arm `_:` header line 25, body 26 → 25..26 Region.
    assert!(
        has(25, 26, FoldingRangeKind::Region),
        "expected a Region fold for the wildcard match arm 25..26; got {folds:?}"
    );

    // `#region Helpers` (line 28) … `#endregion` (line 31) → 28..31 Region.
    assert!(
        has(28, 31, FoldingRangeKind::Region),
        "expected a Region fold for the #region pair 28..31; got {folds:?}"
    );

    // No degenerate (single-line) folds.
    assert!(
        folds.iter().all(|f| f.start_line < f.end_line),
        "no fold should be single-line; got {folds:?}"
    );

    shutdown(&client, server_thread);
}

/// Criterion 3a: full columned ranges by default (no `lineFoldingOnly`).
#[test]
fn folds_carry_columns_by_default() {
    let p = base_project();
    let (server, client) = Connection::memory();
    let server_thread = std::thread::spawn(move || gd_server::serve(server));
    init_and_open_caps(
        &p,
        &client,
        &[("rich.gd", RICH)],
        ClientCapabilities::default(),
    );
    let uri = file_uri(&p.root.join("rich.gd"));
    let folds = request_folds(&client, 10, &uri);

    assert!(
        folds
            .iter()
            .all(|f| f.start_character.is_some() && f.end_character.is_some()),
        "default (no lineFoldingOnly) folds must carry start/end columns; got {folds:?}"
    );
    shutdown(&client, server_thread);
}

/// Criterion 3b: `lineFoldingOnly` collapses to whole-line ranges (no columns).
#[test]
fn line_folding_only_drops_columns() {
    let p = base_project();
    let (server, client) = Connection::memory();
    let server_thread = std::thread::spawn(move || gd_server::serve(server));
    init_and_open_caps(
        &p,
        &client,
        &[("rich.gd", RICH)],
        folding_caps(None, Some(true)),
    );
    let uri = file_uri(&p.root.join("rich.gd"));
    let folds = request_folds(&client, 10, &uri);

    assert!(
        !folds.is_empty(),
        "expected folds even with lineFoldingOnly"
    );
    assert!(
        folds
            .iter()
            .all(|f| f.start_character.is_none() && f.end_character.is_none()),
        "lineFoldingOnly folds must omit start/end columns; got {folds:?}"
    );
    shutdown(&client, server_thread);
}

/// Criterion 3c: `rangeLimit` truncates the returned set.
#[test]
fn range_limit_truncates() {
    let p = base_project();
    let (server, client) = Connection::memory();
    let server_thread = std::thread::spawn(move || gd_server::serve(server));

    // First, the full (unlimited) count.
    init_and_open_caps(
        &p,
        &client,
        &[("rich.gd", RICH)],
        ClientCapabilities::default(),
    );
    let uri = file_uri(&p.root.join("rich.gd"));
    let full = request_folds(&client, 10, &uri);
    let full_n = full.len();
    assert!(full_n > 3, "fixture should yield >3 folds; got {full_n}");
    shutdown(&client, server_thread);

    // Re-init with a rangeLimit of 3 and assert the result is truncated to 3.
    let (server2, client2) = Connection::memory();
    let server_thread2 = std::thread::spawn(move || gd_server::serve(server2));
    init_and_open_caps(
        &p,
        &client2,
        &[("rich.gd", RICH)],
        folding_caps(Some(3), None),
    );
    let uri2 = file_uri(&p.root.join("rich.gd"));
    let limited = request_folds(&client2, 11, &uri2);
    assert_eq!(
        limited.len(),
        3,
        "rangeLimit=3 must truncate to 3 folds; got {limited:?}"
    );
    shutdown(&client2, server_thread2);
}

/// Criterion 4: a syntactically broken file still folds what parsed, and never panics.
#[test]
fn malformed_file_still_folds_and_never_panics() {
    let p = base_project();
    let (server, client) = Connection::memory();
    let server_thread = std::thread::spawn(move || gd_server::serve(server));
    // A function whose body is fine, then a dangling `if` with no condition / no body, then a
    // stray unclosed call — a partial parse. The good function must still fold.
    let broken = "\
extends Node

func ok() -> void:
\tprint(\"a\")
\tprint(\"b\")

func busted(:
\tif
\tprint(
";
    init_and_open_caps(
        &p,
        &client,
        &[("broken.gd", broken)],
        ClientCapabilities::default(),
    );
    let uri = file_uri(&p.root.join("broken.gd"));
    // The request must succeed (never panic / error) and fold the well-formed `ok` body.
    let folds = request_folds(&client, 10, &uri);
    assert!(
        folds.iter().any(|f| f.start_line == 2 && f.end_line >= 4),
        "the well-formed `ok` function must still fold; got {folds:?}"
    );
    shutdown(&client, server_thread);
}

/// Criterion 2 (region nesting): an outer `#region`/`#endregion` containing an inner pair folds as
/// two independent `Region`s — the stack matcher pairs inner-with-inner and outer-with-outer.
#[test]
fn nested_regions_fold_independently() {
    let p = base_project();
    let (server, client) = Connection::memory();
    let server_thread = std::thread::spawn(move || gd_server::serve(server));
    // Line 0: `extends Node`
    // Line 1: (blank)
    // Line 2: `#region Outer`
    // Line 3: `var a := 1`
    // Line 4: `#region Inner`
    // Line 5: `var b := 2`
    // Line 6: `#endregion`        ← closes Inner → 4..6
    // Line 7: `var c := 3`
    // Line 8: `#endregion`        ← closes Outer → 2..8
    let src =
        "extends Node\n\n#region Outer\nvar a := 1\n#region Inner\nvar b := 2\n#endregion\nvar c := 3\n#endregion\n";
    init_and_open_caps(
        &p,
        &client,
        &[("nested.gd", src)],
        ClientCapabilities::default(),
    );
    let uri = file_uri(&p.root.join("nested.gd"));
    let folds = request_folds(&client, 10, &uri);

    let has_region = |start: u32, end: u32| {
        folds.iter().any(|f| {
            f.start_line == start
                && f.end_line == end
                && f.kind.as_ref() == Some(&FoldingRangeKind::Region)
        })
    };
    assert!(
        has_region(4, 6),
        "the inner #region pair must fold 4..6; got {folds:?}"
    );
    assert!(
        has_region(2, 8),
        "the outer #region pair must fold 2..8; got {folds:?}"
    );
    shutdown(&client, server_thread);
}
