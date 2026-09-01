//! Global-scope utility navigation (#584): `definition` on a bare `print(`, `len(`, or `maxi(`
//! anchors on a materialized global-scope page the same way `queue_free()` anchors in `Node.gd`,
//! and `hover` renders a signature for every one of them — including the engine-compiled
//! GDScript-only family, which is in no dump and used to hover as its bare return type.

mod common;

use std::time::Duration;

use common::{file_uri, notification, recv, recv_response, request, shutdown, try_recv};
use lsp_server::Connection;
use lsp_types::{
    DidOpenTextDocumentParams, GotoDefinitionResponse, Hover, HoverContents, InitializeParams,
    InitializedParams, Location, Position, TextDocumentIdentifier, TextDocumentItem,
    TextDocumentPositionParams, Uri,
};

/// A Node-rooted dump carrying three utilities: a documented typed one, a vararg one, and one
/// whose name a project script can shadow.
const API: &str = r#"{
    "header": { "version_major": 4, "version_minor": 6, "version_patch": 3 },
    "classes": [
        {"name": "Object", "is_instantiable": true},
        {"name": "Node", "inherits": "Object", "is_instantiable": true}
    ],
    "utility_functions": [
        {"name": "maxi", "return_type": "int", "category": "math", "is_vararg": false,
         "hash": 1, "description": "Returns the maximum of two [int] values.",
         "arguments": [{"name": "a", "type": "int"}, {"name": "b", "type": "int"}]},
        {"name": "print", "category": "general", "is_vararg": true, "hash": 2,
         "arguments": [{"name": "arg1", "type": "Variant"}]}
    ]
}"#;

/// The same dump with the utility table removed — the stock shape for a project whose dump
/// predates the with-docs export, and the degrade case for the `@GlobalScope` page.
const API_NO_UTILITIES: &str = r#"{
    "header": { "version_major": 4, "version_minor": 6, "version_patch": 3 },
    "classes": [
        {"name": "Object", "is_instantiable": true},
        {"name": "Node", "inherits": "Object", "is_instantiable": true}
    ]
}"#;

struct Fixture {
    _dir: tempfile::TempDir,
    uri: Uri,
    stub_cache: std::path::PathBuf,
}

fn boot(client: &Connection, api: &str, src: &str) -> Fixture {
    let dir = tempfile::tempdir().expect("create fixture dir");
    let root = dir.path();
    std::fs::write(root.join("project.godot"), "").unwrap();
    let api_path = root.join("extension_api.json");
    std::fs::write(&api_path, api).unwrap();
    let stub_cache = root.join("stub-cache");
    let main_path = root.join("main.gd");
    std::fs::write(&main_path, src).unwrap();

    let init = InitializeParams {
        initialization_options: Some(serde_json::json!({
            "projectRoot": root.to_string_lossy().as_ref(),
            "extensionApiPath": api_path.to_string_lossy().as_ref(),
            "autoDumpExtensionApi": false,
            "stubCacheDir": stub_cache.to_string_lossy().as_ref(),
        })),
        ..Default::default()
    };
    client.sender.send(request(1, "initialize", init)).unwrap();
    let _ = recv(client);
    client
        .sender
        .send(notification("initialized", InitializedParams {}))
        .unwrap();

    let uri = file_uri(camino::Utf8Path::from_path(&main_path).expect("utf-8 fixture path"));
    client
        .sender
        .send(notification(
            "textDocument/didOpen",
            DidOpenTextDocumentParams {
                text_document: TextDocumentItem {
                    uri: uri.clone(),
                    language_id: "gdscript".to_string(),
                    version: 1,
                    text: src.to_string(),
                },
            },
        ))
        .unwrap();
    while try_recv(client, Duration::from_millis(500)).is_some() {}

    Fixture {
        _dir: dir,
        uri,
        stub_cache: stub_cache.to_path_buf(),
    }
}

fn at(fixture: &Fixture, line: u32, character: u32) -> TextDocumentPositionParams {
    TextDocumentPositionParams {
        text_document: TextDocumentIdentifier {
            uri: fixture.uri.clone(),
        },
        position: Position { line, character },
    }
}

fn definition(client: &Connection, id: i32, pos: TextDocumentPositionParams) -> Option<Location> {
    client
        .sender
        .send(request(
            id,
            "textDocument/definition",
            serde_json::json!({
                "textDocument": pos.text_document,
                "position": pos.position,
            }),
        ))
        .unwrap();
    let resp = recv_response(client);
    assert!(resp.error.is_none(), "definition errored: {:?}", resp.error);
    let got: Option<GotoDefinitionResponse> = serde_json::from_value(resp.result.unwrap()).unwrap();
    match got? {
        GotoDefinitionResponse::Scalar(l) => Some(l),
        GotoDefinitionResponse::Array(v) => v.into_iter().next(),
        GotoDefinitionResponse::Link(_) => panic!("definition must answer Locations, not Links"),
    }
}

fn hover(client: &Connection, id: i32, pos: TextDocumentPositionParams) -> String {
    client
        .sender
        .send(request(
            id,
            "textDocument/hover",
            serde_json::json!({
                "textDocument": pos.text_document,
                "position": pos.position,
            }),
        ))
        .unwrap();
    let resp = recv_response(client);
    assert!(resp.error.is_none(), "hover errored: {:?}", resp.error);
    let got: Option<Hover> = serde_json::from_value(resp.result.unwrap()).unwrap();
    match got.map(|h| h.contents) {
        Some(HoverContents::Markup(m)) => m.value,
        other => panic!("hover must answer markup content; got {other:?}"),
    }
}

/// The line a location points at, plus the text it selects — the pair that proves the anchor
/// lands on the name token rather than somewhere on the page.
fn anchored(loc: &Location) -> (String, String) {
    let path = gd_server::uri::uri_to_path(&loc.uri).expect("stub uri is a file path");
    let text = std::fs::read_to_string(path.as_std_path()).expect("page on disk");
    let line = text
        .lines()
        .nth(loc.range.start.line as usize)
        .expect("range line within the page")
        .to_owned();
    let selected =
        line[loc.range.start.character as usize..loc.range.end.character as usize].to_owned();
    (line, selected)
}

const SRC: &str = "extends Node\n\nfunc go() -> void:\n\tprint(\"x\")\n\tvar a := maxi(1, 2)\n\tvar b := len([1])\n\tvar c := range(3)\n\tprint(a, b, c)\n";

#[test]
fn a_dump_utility_anchors_on_the_global_scope_page() {
    let (server, client) = Connection::memory();
    let server_thread = std::thread::spawn(move || gd_server::serve(server));
    let fixture = boot(&client, API, SRC);

    for (id, line, col, name) in [(10, 3, 1, "print"), (11, 4, 10, "maxi")] {
        let loc = definition(&client, id, at(&fixture, line, col))
            .unwrap_or_else(|| panic!("{name} must resolve"));
        let path = gd_server::uri::uri_to_path(&loc.uri).unwrap();
        assert!(
            path.as_std_path().starts_with(&fixture.stub_cache),
            "{name} anchors under the stub cache; got {path:?}"
        );
        assert_eq!(
            path.file_name(),
            Some("@GlobalScope.gd"),
            "a dump utility lives on the global-scope page; got {path:?}"
        );
        let (decl, selected) = anchored(&loc);
        assert_eq!(
            selected, name,
            "the range selects the name token in {decl:?}"
        );
        assert!(
            decl.starts_with(&format!("func {name}(")),
            "the anchored line declares {name}; got {decl:?}"
        );
    }

    shutdown(&client, server_thread);
}

#[test]
fn an_engine_compiled_utility_anchors_on_the_gdscript_page() {
    let (server, client) = Connection::memory();
    let server_thread = std::thread::spawn(move || gd_server::serve(server));
    let fixture = boot(&client, API, SRC);

    for (id, line, col, name, decl) in [
        (10, 5, 10, "len", "func len(var: Variant) -> int"),
        (11, 6, 10, "range", "func range(...) -> Array"),
    ] {
        let loc = definition(&client, id, at(&fixture, line, col))
            .unwrap_or_else(|| panic!("{name} must resolve"));
        let path = gd_server::uri::uri_to_path(&loc.uri).unwrap();
        assert_eq!(
            path.file_name(),
            Some("@GDScript.gd"),
            "a GDScript-only utility lives on its own page; got {path:?}"
        );
        let (got, selected) = anchored(&loc);
        assert_eq!(got, decl);
        assert_eq!(selected, name);
    }

    shutdown(&client, server_thread);
}

#[test]
fn every_utility_hovers_as_a_signature() {
    let (server, client) = Connection::memory();
    let server_thread = std::thread::spawn(move || gd_server::serve(server));
    let fixture = boot(&client, API, SRC);

    // The dump-backed pair, one of them documented.
    let md = hover(&client, 10, at(&fixture, 4, 10));
    assert!(
        md.contains("func maxi(a: int, b: int) -> int"),
        "maxi hovers as its signature; got {md:?}"
    );
    assert!(
        md.contains("Returns the maximum of two"),
        "a with-docs dump's description reaches hover; got {md:?}"
    );
    assert!(
        !md.contains("[b]") && !md.contains("[codeblock]"),
        "presentational BBCode must never reach the wire; got {md:?}"
    );

    // The engine-compiled pair. `len` used to hover as `Variant` — its return type — because
    // the only utility arm consulted the dump.
    let md = hover(&client, 11, at(&fixture, 5, 10));
    assert!(
        md.contains("func len(var: Variant) -> int"),
        "len hovers as its signature; got {md:?}"
    );
    let md = hover(&client, 12, at(&fixture, 6, 10));
    assert!(
        md.contains("func range(...) -> Array"),
        "range hovers as its signature; got {md:?}"
    );

    shutdown(&client, server_thread);
}

#[test]
fn a_project_function_shadows_the_utility_of_the_same_name() {
    let src = "extends Node\n\nfunc print(what: String) -> void:\n\tpass\n\nfunc go() -> void:\n\tprint(\"x\")\n";
    let (server, client) = Connection::memory();
    let server_thread = std::thread::spawn(move || gd_server::serve(server));
    let fixture = boot(&client, API, src);

    let loc = definition(&client, 10, at(&fixture, 6, 1)).expect("the project function resolves");
    assert_eq!(
        loc.uri, fixture.uri,
        "a declared member wins over the global utility; got {loc:?}"
    );
    assert_eq!(loc.range.start.line, 2, "it points at the declaration");

    shutdown(&client, server_thread);
}

#[test]
fn a_dump_without_utilities_still_resolves_the_engine_compiled_family() {
    let (server, client) = Connection::memory();
    let server_thread = std::thread::spawn(move || gd_server::serve(server));
    let fixture = boot(&client, API_NO_UTILITIES, SRC);

    // Honest under-report: the dump is the only source for `print`, so with no table there is
    // nothing to point at.
    assert!(
        definition(&client, 10, at(&fixture, 3, 1)).is_none(),
        "print must answer null when the dump carries no utilities"
    );
    // `len` is compiled into the engine, so it resolves under any dump.
    let loc = definition(&client, 11, at(&fixture, 5, 10)).expect("len resolves without a dump");
    assert_eq!(
        gd_server::uri::uri_to_path(&loc.uri).unwrap().file_name(),
        Some("@GDScript.gd")
    );

    shutdown(&client, server_thread);
}

#[test]
fn an_opened_global_page_never_self_diagnoses() {
    let (server, client) = Connection::memory();
    let server_thread = std::thread::spawn(move || gd_server::serve(server));
    let fixture = boot(&client, API, SRC);

    let loc = definition(&client, 10, at(&fixture, 5, 10)).expect("len resolves");
    let path = gd_server::uri::uri_to_path(&loc.uri).unwrap();
    let text = std::fs::read_to_string(path.as_std_path()).unwrap();
    client
        .sender
        .send(notification(
            "textDocument/didOpen",
            DidOpenTextDocumentParams {
                text_document: TextDocumentItem {
                    uri: loc.uri.clone(),
                    language_id: "gdscript".to_string(),
                    version: 1,
                    text,
                },
            },
        ))
        .unwrap();
    // The page declares `func range(...)` and a parameter literally named `var`, neither of
    // which parses as GDScript. The stub gate is what keeps that off the wire.
    while let Some(msg) = try_recv(&client, Duration::from_millis(500)) {
        if let lsp_server::Message::Notification(n) = msg {
            if n.method == "textDocument/publishDiagnostics" {
                let p: lsp_types::PublishDiagnosticsParams =
                    serde_json::from_value(n.params).unwrap();
                if p.uri == loc.uri {
                    assert!(
                        p.diagnostics.is_empty(),
                        "an API page must publish nothing; got {:?}",
                        p.diagnostics
                    );
                }
            }
        }
    }

    shutdown(&client, server_thread);
}

/// The house one-builder rule: the line `definition` lands on is byte-for-byte the line `hover`
/// shows. Both families go through it, so neither page can drift from its own hover.
#[test]
fn the_page_line_and_the_hover_signature_are_one_string() {
    let (server, client) = Connection::memory();
    let server_thread = std::thread::spawn(move || gd_server::serve(server));
    let fixture = boot(&client, API, SRC);

    let mut id = 10;
    for (line, col) in [(3, 1), (4, 10), (5, 10), (6, 10)] {
        let loc = definition(&client, id, at(&fixture, line, col)).expect("resolves");
        let (decl, _) = anchored(&loc);
        let md = hover(&client, id + 1, at(&fixture, line, col));
        assert!(
            md.lines().any(|l| l == decl),
            "the page line {decl:?} must appear verbatim in the hover {md:?}"
        );
        id += 2;
    }

    shutdown(&client, server_thread);
}
