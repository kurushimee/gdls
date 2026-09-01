//! Static members on a builtin type (#591): `Vector2.from_angle(...)` in callee position.
//!
//! `reduce_call` short-circuits a builtin type name in a subscript callee's base into a
//! synthesized meta type without ever reducing that base node (`gdscript_analyzer.cpp:3597-3603`),
//! so the base identifier carries no type in the result. That is faithful — Godot's own AST has
//! the same hole — but every navigation surface reads the base's type, so `definition`, `hover`,
//! `signatureHelp`, and `completion` all used to miss the static. The server reconstructs the meta
//! type at the protocol boundary; nothing here reaches an `AnalysisResult`.

mod common;

use std::time::Duration;

use common::{file_uri, notification, recv, recv_response, request, shutdown, try_recv};
use lsp_server::Connection;
use lsp_types::{
    CompletionResponse, DidOpenTextDocumentParams, GotoDefinitionResponse, Hover, HoverContents,
    InitializeParams, InitializedParams, Location, Position, SignatureHelp, TextDocumentIdentifier,
    TextDocumentItem, TextDocumentPositionParams, Uri,
};

/// A dump whose `Vector2` carries one static constructor, one instance method, and a constant —
/// the three shapes the meta/instance split has to keep apart — beside a native class chain so the
/// `Node2D.new()` control routes through the native metatype path instead.
const API: &str = r#"{
    "header": { "version_major": 4, "version_minor": 6, "version_patch": 3 },
    "builtin_classes": [
        {"name": "Vector2",
         "members": [{"name": "x", "type": "float"}, {"name": "y", "type": "float"}],
         "constants": [{"name": "ONE", "type": "Vector2", "value": "Vector2(1, 1)"}],
         "methods": [
            {"name": "from_angle", "return_type": "Vector2", "is_vararg": false,
             "is_const": false, "is_static": true, "hash": 1,
             "arguments": [{"name": "angle", "type": "float"}]},
            {"name": "normalized", "return_type": "Vector2", "is_vararg": false,
             "is_const": true, "is_static": false, "hash": 2}
         ]},
        {"name": "String",
         "methods": [
            {"name": "num", "return_type": "String", "is_vararg": false,
             "is_const": false, "is_static": true, "hash": 3,
             "arguments": [{"name": "number", "type": "float"},
                           {"name": "decimals", "type": "int", "default_value": "-1"}]}
         ]}
    ],
    "classes": [
        {"name": "Object", "is_instantiable": true},
        {"name": "Node", "inherits": "Object", "is_instantiable": true},
        {"name": "CanvasItem", "inherits": "Node", "is_instantiable": true},
        {"name": "Node2D", "inherits": "CanvasItem", "is_instantiable": true}
    ]
}"#;

struct Fixture {
    _dir: tempfile::TempDir,
    uri: Uri,
    stub_cache: std::path::PathBuf,
}

fn boot(client: &Connection, src: &str) -> Fixture {
    let dir = tempfile::tempdir().expect("create fixture dir");
    let root = dir.path();
    std::fs::write(root.join("project.godot"), "").unwrap();
    let api_path = root.join("extension_api.json");
    std::fs::write(&api_path, API).unwrap();
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

fn ask(
    client: &Connection,
    id: i32,
    method: &str,
    pos: &TextDocumentPositionParams,
) -> serde_json::Value {
    client
        .sender
        .send(request(
            id,
            method,
            serde_json::json!({
                "textDocument": pos.text_document,
                "position": pos.position,
            }),
        ))
        .unwrap();
    let resp = recv_response(client);
    assert!(resp.error.is_none(), "{method} errored: {:?}", resp.error);
    resp.result.unwrap()
}

fn definition(client: &Connection, id: i32, pos: TextDocumentPositionParams) -> Option<Location> {
    let got: Option<GotoDefinitionResponse> =
        serde_json::from_value(ask(client, id, "textDocument/definition", &pos)).unwrap();
    match got? {
        GotoDefinitionResponse::Scalar(l) => Some(l),
        GotoDefinitionResponse::Array(v) => v.into_iter().next(),
        GotoDefinitionResponse::Link(_) => panic!("definition must answer Locations, not Links"),
    }
}

fn hover(client: &Connection, id: i32, pos: TextDocumentPositionParams) -> Option<String> {
    let got: Option<Hover> =
        serde_json::from_value(ask(client, id, "textDocument/hover", &pos)).unwrap();
    match got.map(|h| h.contents) {
        Some(HoverContents::Markup(m)) => Some(m.value),
        None => None,
        other => panic!("hover must answer markup content; got {other:?}"),
    }
}

fn signature_labels(client: &Connection, id: i32, pos: TextDocumentPositionParams) -> Vec<String> {
    let got: Option<SignatureHelp> =
        serde_json::from_value(ask(client, id, "textDocument/signatureHelp", &pos)).unwrap();
    got.map(|h| h.signatures.into_iter().map(|s| s.label).collect())
        .unwrap_or_default()
}

fn completion_labels(client: &Connection, id: i32, pos: TextDocumentPositionParams) -> Vec<String> {
    let got: Option<CompletionResponse> =
        serde_json::from_value(ask(client, id, "textDocument/completion", &pos)).unwrap();
    match got {
        Some(CompletionResponse::Array(v)) => v.into_iter().map(|i| i.label).collect(),
        Some(CompletionResponse::List(l)) => l.items.into_iter().map(|i| i.label).collect(),
        None => Vec::new(),
    }
}

/// The line a location points at, plus the text it selects.
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

// 0 extends Node2D
// 1
// 2 func go() -> void:
// 3     var a := Vector2.from_angle(1.0)
// 4     var b := a.normalized()
// 5     var c := Vector2.ONE.normalized()
// 6     var d := String.num(1.0, 2)
// 7     var n := Node2D.new()
// 8     print(a, b, c, d, n)
const SRC: &str = concat!(
    "extends Node2D\n",
    "\n",
    "func go() -> void:\n",
    "\tvar a := Vector2.from_angle(1.0)\n",
    "\tvar b := a.normalized()\n",
    "\tvar c := Vector2.ONE.normalized()\n",
    "\tvar d := String.num(1.0, 2)\n",
    "\tvar n := Node2D.new()\n",
    "\tprint(a, b, c, d, n)\n",
);

#[test]
fn a_static_on_a_builtin_type_anchors_on_that_type_s_page() {
    let (server, client) = Connection::memory();
    let server_thread = std::thread::spawn(move || gd_server::serve(server));
    let fixture = boot(&client, SRC);

    for (id, line, col, page, name) in [
        (10, 3, 22, "Vector2.gd", "from_angle"),
        (11, 6, 18, "String.gd", "num"),
    ] {
        let loc = definition(&client, id, at(&fixture, line, col))
            .unwrap_or_else(|| panic!("{name} must resolve"));
        let path = gd_server::uri::uri_to_path(&loc.uri).unwrap();
        assert!(
            path.as_std_path().starts_with(&fixture.stub_cache),
            "{name} anchors under the stub cache; got {path:?}"
        );
        assert_eq!(
            path.file_name(),
            Some(page),
            "a builtin static lives on its own type's page; got {path:?}"
        );
        let (decl, selected) = anchored(&loc);
        assert_eq!(selected, name, "the range selects the name in {decl:?}");
        assert!(
            decl.contains(&format!("{name}(")),
            "the anchored line declares {name}; got {decl:?}"
        );
    }

    shutdown(&client, server_thread);
}

#[test]
fn hover_on_a_builtin_static_renders_its_signature() {
    let (server, client) = Connection::memory();
    let server_thread = std::thread::spawn(move || gd_server::serve(server));
    let fixture = boot(&client, SRC);

    let md = hover(&client, 10, at(&fixture, 3, 22)).expect("from_angle must hover");
    assert!(
        md.contains("from_angle(angle: float)") && md.contains("Vector2"),
        "hover renders the static's own signature, not the bare base type; got {md:?}"
    );

    shutdown(&client, server_thread);
}

#[test]
fn signature_help_fires_inside_a_builtin_static_call() {
    let (server, client) = Connection::memory();
    let server_thread = std::thread::spawn(move || gd_server::serve(server));
    let fixture = boot(&client, SRC);

    let labels = signature_labels(&client, 10, at(&fixture, 3, 29));
    assert!(
        labels
            .iter()
            .any(|l| l.contains("from_angle(angle: float)")),
        "signatureHelp answers for Vector2.from_angle(; got {labels:?}"
    );

    let labels = signature_labels(&client, 11, at(&fixture, 6, 22));
    assert!(
        labels
            .iter()
            .any(|l| l.contains("num(number: float, decimals: int")),
        "a defaulted parameter still renders; got {labels:?}"
    );

    shutdown(&client, server_thread);
}

#[test]
fn completion_after_the_dot_of_a_builtin_static_call_offers_that_type_s_members() {
    let (server, client) = Connection::memory();
    let server_thread = std::thread::spawn(move || gd_server::serve(server));
    let fixture = boot(&client, SRC);

    let labels = completion_labels(&client, 10, at(&fixture, 3, 18));
    assert!(
        labels.iter().any(|l| l == "from_angle"),
        "the static itself is offered; got {labels:?}"
    );
    assert!(
        labels.iter().any(|l| l == "ONE"),
        "so are the type's constants; got {labels:?}"
    );
    assert!(
        !labels.iter().any(|l| l == "normalized"),
        "a meta type offers no instance method; got {labels:?}"
    );

    shutdown(&client, server_thread);
}

#[test]
fn the_instance_and_native_paths_are_untouched() {
    let (server, client) = Connection::memory();
    let server_thread = std::thread::spawn(move || gd_server::serve(server));
    let fixture = boot(&client, SRC);

    // A value of a builtin type, and a constant read off one — both already typed by the
    // analyzer, so the reconstruction must not shadow them.
    for (id, line, col, page, name) in [
        (10, 4, 13, "Vector2.gd", "normalized"),
        (11, 5, 24, "Vector2.gd", "normalized"),
    ] {
        let loc = definition(&client, id, at(&fixture, line, col))
            .unwrap_or_else(|| panic!("{name} at {line}:{col} must resolve"));
        let path = gd_server::uri::uri_to_path(&loc.uri).unwrap();
        assert_eq!(path.file_name(), Some(page), "got {path:?}");
        assert_eq!(anchored(&loc).1, name);
    }

    // A native class name in the same position keeps routing through the native metatype path:
    // the name itself still anchors on its own page, and its constructor still signature-helps.
    let loc = definition(&client, 12, at(&fixture, 7, 12)).expect("Node2D must resolve");
    let path = gd_server::uri::uri_to_path(&loc.uri).unwrap();
    assert_eq!(
        path.file_name(),
        Some("Node2D.gd"),
        "a native class name stays on the native page; got {path:?}"
    );
    let labels = signature_labels(&client, 13, at(&fixture, 7, 21));
    assert!(
        labels.iter().any(|l| l.starts_with("Node2D(")),
        "the native constructor still answers; got {labels:?}"
    );

    shutdown(&client, server_thread);
}
