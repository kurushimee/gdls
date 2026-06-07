//! M6-E gate: `textDocument/references` finds callers through body-local typed vars.
//!
//! When `lib.gd` defines `func helper()` and callers reach it through a body-local typed var
//! (`var l: Lib = Lib.new(); l.helper()`), find-references on `helper`'s declaration must
//! include all cross-file call sites — even files whose interface does not mention `Lib`.

mod common;

use common::{file_uri, notification, recv, request, shutdown, TempProject};
use lsp_server::{Connection, Message};
use lsp_types::{
    DidOpenTextDocumentParams, InitializeParams, InitializedParams, Location, Position,
    ReferenceContext, ReferenceParams, TextDocumentIdentifier, TextDocumentItem,
    TextDocumentPositionParams, WorkDoneProgressParams,
};

fn init_and_open(project: &TempProject, client: &Connection, files: &[&str]) {
    let init = InitializeParams {
        initialization_options: Some(serde_json::json!({
            "projectRoot": project.root.as_str(),
            "extensionApiPath": project.root.join("extension_api.json").as_str(),
        })),
        ..Default::default()
    };
    client.sender.send(request(1, "initialize", init)).unwrap();
    let _ = recv(client);
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
}

/// M6-E: references on `helper` in `lib.gd` must return the call sites in `a.gd` and `b.gd`,
/// including when one caller reaches the method through a body-local typed var — not a typed
/// parameter — so `Lib` does NOT appear in that caller's interface. This exercises the project-
/// wide text-scan path that supersedes `name_referencers` for method/signal targets.
#[test]
fn references_finds_cross_file_method_calls() {
    let p = TempProject::new();
    p.write("project.godot", "config_version=5\n");
    p.write("extension_api.json", common::MINI_API);

    // lib.gd defines `class_name Lib` and `func helper()`.
    // Line 0: `class_name Lib`
    // Line 1: `extends Node`
    // Line 3: `func helper():`
    // Line 4: `\tpass`
    // `helper` identifier at line 3, col 5..11.
    p.write(
        "lib.gd",
        "class_name Lib\nextends Node\n\nfunc helper():\n\tpass\n",
    );

    // a.gd calls helper() via a typed parameter — `Lib` appears in its interface.
    // Line 0: `extends Node`
    // Line 2: `func test(l: Lib):`
    // Line 3: `\tl.helper()`  — `helper` at col 3..9
    p.write("a.gd", "extends Node\n\nfunc test(l: Lib):\n\tl.helper()\n");

    // b.gd calls helper() through a BODY-LOCAL typed var — `Lib` does NOT appear in b.gd's
    // interface (the interface pass only records types from parameters/return/annotations, not
    // local variable declarations). This is the seam the M6-E fix must close: b.gd is NOT in
    // `name_referencers("Lib")` or `name_referencers("helper")`, so it was previously missed.
    // Line 0: `extends Node`
    // Line 2: `func run():`
    // Line 3: `\tvar l: Lib = Lib.new()`
    // Line 4: `\tl.helper()`  — `helper` at col 3..9
    p.write(
        "b.gd",
        "extends Node\n\nfunc run():\n\tvar l: Lib = Lib.new()\n\tl.helper()\n",
    );

    let (server, client) = Connection::memory();
    let server_thread = std::thread::spawn(move || gd_server::serve(server));
    init_and_open(&p, &client, &["lib.gd", "a.gd", "b.gd"]);

    let lib_uri = file_uri(&p.root.join("lib.gd"));
    let params = ReferenceParams {
        text_document_position: TextDocumentPositionParams {
            text_document: TextDocumentIdentifier {
                uri: lib_uri.clone(),
            },
            // Click on `helper` at line 3, col 7.
            position: Position {
                line: 3,
                character: 7,
            },
        },
        context: ReferenceContext {
            include_declaration: false,
        },
        work_done_progress_params: WorkDoneProgressParams::default(),
        partial_result_params: Default::default(),
    };
    client
        .sender
        .send(request(10, "textDocument/references", params))
        .unwrap();
    let Message::Response(resp) = recv(&client) else {
        panic!("expected references response");
    };
    assert!(resp.error.is_none(), "references errored: {:?}", resp.error);
    let locs: Vec<Location> =
        serde_json::from_value(resp.result.expect("references result")).unwrap();

    // Must include call sites from BOTH a.gd (typed-parameter caller) AND b.gd (body-local
    // typed-var caller). The b.gd assertion is the critical M6-E seam.
    let a_uri = file_uri(&p.root.join("a.gd"));
    let b_uri = file_uri(&p.root.join("b.gd"));
    let has_a = locs.iter().any(|l| l.uri == a_uri);
    let has_b = locs.iter().any(|l| l.uri == b_uri);
    assert!(
        has_a,
        "references must include call site in a.gd (typed-param caller); got: {locs:?}"
    );
    assert!(
        has_b,
        "references must include call site in b.gd (body-local var caller — M6-E seam); \
         got: {locs:?}"
    );

    // The call sites must be the `helper` identifier range (narrow), not the whole call expression.
    for loc in locs.iter().filter(|l| l.uri == a_uri || l.uri == b_uri) {
        assert_eq!(
            loc.range.start.character, 3,
            "call site range should start at `helper` identifier col 3, got {loc:?}"
        );
    }

    shutdown(&client, server_thread);
}

/// M6-E false-positive gate: `textDocument/references` for `Lib::helper` must NOT include
/// occurrences of an unrelated same-named method in `other.gd` (`class_name Other` with its own
/// `func helper()`). The callee_file-filtered mechanism must distinguish between the two.
#[test]
fn references_excludes_unrelated_same_named_method() {
    let p = TempProject::new();
    p.write("project.godot", "config_version=5\n");
    p.write("extension_api.json", common::MINI_API);

    // lib.gd defines `class_name Lib` and `func helper()`.
    p.write(
        "lib.gd",
        "class_name Lib\nextends Node\n\nfunc helper():\n\tpass\n",
    );

    // other.gd is an unrelated class with its own `func helper()`.
    // It does NOT extend Lib, does NOT call Lib.helper — it just happens to have the same name.
    // Line 0: `class_name Other`
    // Line 1: `extends Node`
    // Line 3: `func helper():`  — `helper` at col 5..11
    // Line 4: `\tpass`
    p.write(
        "other.gd",
        "class_name Other\nextends Node\n\nfunc helper():\n\tpass\n",
    );

    // caller.gd calls Lib.helper() — it's a genuine reference.
    // Line 0: `extends Node`
    // Line 2: `func run():`
    // Line 3: `\tvar l: Lib = Lib.new()`
    // Line 4: `\tl.helper()`  — `helper` at col 3..9
    p.write(
        "caller.gd",
        "extends Node\n\nfunc run():\n\tvar l: Lib = Lib.new()\n\tl.helper()\n",
    );

    let (server, client) = Connection::memory();
    let server_thread = std::thread::spawn(move || gd_server::serve(server));
    init_and_open(&p, &client, &["lib.gd", "other.gd", "caller.gd"]);

    let lib_uri = file_uri(&p.root.join("lib.gd"));
    let params = ReferenceParams {
        text_document_position: TextDocumentPositionParams {
            text_document: TextDocumentIdentifier { uri: lib_uri },
            // Click on `helper` at line 3, col 7 (declaration site in lib.gd).
            position: Position {
                line: 3,
                character: 7,
            },
        },
        context: ReferenceContext {
            include_declaration: false,
        },
        work_done_progress_params: WorkDoneProgressParams::default(),
        partial_result_params: Default::default(),
    };
    client
        .sender
        .send(request(20, "textDocument/references", params))
        .unwrap();
    let Message::Response(resp) = recv(&client) else {
        panic!("expected references response");
    };
    assert!(resp.error.is_none(), "references errored: {:?}", resp.error);
    let locs: Vec<Location> =
        serde_json::from_value(resp.result.expect("references result")).unwrap();

    let caller_uri = file_uri(&p.root.join("caller.gd"));
    let other_uri = file_uri(&p.root.join("other.gd"));

    // caller.gd's `l.helper()` call IS a genuine reference — must be included.
    assert!(
        locs.iter().any(|l| l.uri == caller_uri),
        "references must include genuine call site in caller.gd; got: {locs:?}"
    );

    // other.gd's `func helper():` declaration is unrelated — must NOT appear.
    assert!(
        !locs.iter().any(|l| l.uri == other_uri),
        "references must NOT include other.gd's unrelated helper (false positive); got: {locs:?}"
    );

    shutdown(&client, server_thread);
}
