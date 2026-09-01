//! #521, end to end: an `:=` member whose initializer is a `preload` of a non-script resource, or a
//! global float constant, must carry its real type across a file boundary. Both used to arrive at
//! the reader as `Variant`, so hover said nothing and a strict session reported an unsafe access on
//! a member whose type was perfectly knowable.

mod common;

use common::{file_uri, notification, recv, recv_response, request, shutdown, TempProject};
use lsp_server::{Connection, Message};
use lsp_types::{
    DidOpenTextDocumentParams, Hover, HoverContents, HoverParams, InitializeParams,
    InitializedParams, Position, PublishDiagnosticsParams, TextDocumentIdentifier,
    TextDocumentItem, TextDocumentPositionParams, WorkDoneProgressParams,
};

const LIB_GD: &str = "\
class_name PreloadLib
extends Node
var scene := preload(\"res://thing.tscn\")
var glob := PI
var typed: Node = null
var opaque = untyped()
func untyped():
\treturn 1
";

const READER_GD: &str = "\
extends Node
func use(l: PreloadLib) -> void:
\tprint(l.scene)
\tprint(l.glob)
";

/// The same two members, each used in a way its real type forbids. As `Variant` both lines were
/// silent.
const BAD_READER_GD: &str = "\
extends Node
func use(l: PreloadLib) -> void:
\tl.scene.nosuchmethod()
\tvar s: String = l.glob\n\tprint(s)
";

fn project() -> TempProject {
    let p = TempProject::new();
    p.write(
        "project.godot",
        "config_version=5\n\n[application]\n\nconfig/name=\"Test\"\nconfig/features=PackedStringArray(\"4.6\")\n",
    );
    p.write("extension_api.json", common::MINI_API);
    p.write(
        "thing.tscn",
        "[gd_scene load_steps=1 format=3]\n\n[node name=\"Root\" type=\"Node\"]\n",
    );
    p.write("lib.gd", LIB_GD);
    p.write("reader.gd", READER_GD);
    p.write("bad_reader.gd", BAD_READER_GD);
    p.write("heir.gd", HEIR_GD);
    p
}

/// A subclass reading the same members BARE — the other hover path, which resolves through the
/// binding rather than through a base expression.
const HEIR_GD: &str = "\
extends PreloadLib
func use() -> void:
\tprint(scene)
\tprint(typed)
\tprint(opaque)
";

fn boot(p: &TempProject) -> (Connection, std::thread::JoinHandle<anyhow::Result<()>>) {
    let (server, client) = Connection::memory();
    let handle = std::thread::spawn(move || gd_server::serve(server));
    let init = InitializeParams {
        initialization_options: Some(serde_json::json!({
            "projectRoot": p.root.as_str(),
            "autoDumpExtensionApi": false,
            "extensionApiPath": p.root.join("extension_api.json").as_str(),
            "strict": { "profile": "strict" },
        })),
        capabilities: lsp_types::ClientCapabilities {
            general: Some(lsp_types::GeneralClientCapabilities {
                position_encodings: Some(vec![lsp_types::PositionEncodingKind::UTF8]),
                ..Default::default()
            }),
            ..Default::default()
        },
        ..Default::default()
    };
    client.sender.send(request(1, "initialize", init)).unwrap();
    let _ = recv(&client);
    client
        .sender
        .send(notification("initialized", InitializedParams {}))
        .unwrap();
    (client, handle)
}

fn recv_publish(client: &Connection) -> PublishDiagnosticsParams {
    loop {
        let msg = recv(client);
        let Message::Notification(notif) = msg else {
            panic!("expected a publishDiagnostics notification, got {msg:?}");
        };
        if notif.method == "textDocument/publishDiagnostics" {
            return serde_json::from_value(notif.params).expect("valid PublishDiagnosticsParams");
        }
    }
}

fn open(client: &Connection, uri: &lsp_types::Uri, text: &str) -> PublishDiagnosticsParams {
    client
        .sender
        .send(notification(
            "textDocument/didOpen",
            DidOpenTextDocumentParams {
                text_document: TextDocumentItem {
                    uri: uri.clone(),
                    language_id: "gdscript".to_string(),
                    version: 1,
                    text: text.to_string(),
                },
            },
        ))
        .unwrap();
    recv_publish(client)
}

fn hover_at(client: &Connection, uri: &lsp_types::Uri, position: Position) -> Option<Hover> {
    let params = HoverParams {
        text_document_position_params: TextDocumentPositionParams {
            text_document: TextDocumentIdentifier { uri: uri.clone() },
            position,
        },
        work_done_progress_params: WorkDoneProgressParams::default(),
    };
    client
        .sender
        .send(request(10, "textDocument/hover", params))
        .unwrap();
    let resp = recv_response(client);
    let value = resp.result.expect("hover result always present");
    serde_json::from_value(value).expect("valid Option<Hover>")
}

fn hover_markdown(hover: &Hover) -> &str {
    match &hover.contents {
        HoverContents::Markup(m) => m.value.as_str(),
        other => panic!("expected markup hover contents, got {other:?}"),
    }
}

/// #526: the card and the diagnostics have to agree about what a member is. The strict profile
/// checks `l.scene` against `PackedScene`, so hover must say `PackedScene` too — it used to render
/// the bare `var scene`, because the shallow interface has no type for a `preload` and the card
/// read only that.
#[test]
fn a_cross_file_member_hovers_with_the_type_the_analyzer_resolved() {
    let p = project();
    let (client, server_thread) = boot(&p);
    let uri = file_uri(&p.root.join("reader.gd"));
    let _ = open(&client, &uri, READER_GD);

    // `l.scene` on line 2, at the `s` of `scene`.
    let scene = hover_at(&client, &uri, Position::new(2, 10)).expect("hover on l.scene");
    assert!(
        hover_markdown(&scene).contains("var scene: PackedScene"),
        "the card must name what the diagnostics checked: {}",
        hover_markdown(&scene)
    );

    // `l.glob` on line 3, at the `g` of `glob`.
    let glob = hover_at(&client, &uri, Position::new(3, 10)).expect("hover on l.glob");
    assert!(
        hover_markdown(&glob).contains("var glob: float"),
        "a member inferred from PI reads as float: {}",
        hover_markdown(&glob)
    );

    shutdown(&client, server_thread);
}

/// The bare-read path reaches the same answer, and the two things that must NOT change: an
/// annotated member keeps the author's own spelling, and a member the analyzer could only call
/// `Variant` stays bare rather than being handed a type gdls never read.
#[test]
fn a_bare_read_of_an_inherited_member_agrees_and_the_negatives_hold() {
    let p = project();
    let (client, server_thread) = boot(&p);
    let uri = file_uri(&p.root.join("heir.gd"));
    let _ = open(&client, &uri, HEIR_GD);

    let scene = hover_at(&client, &uri, Position::new(2, 9)).expect("hover on scene");
    assert!(
        hover_markdown(&scene).contains("var scene: PackedScene"),
        "{}",
        hover_markdown(&scene)
    );

    let typed = hover_at(&client, &uri, Position::new(3, 9)).expect("hover on typed");
    assert!(
        hover_markdown(&typed).contains("var typed: Node"),
        "an annotation is the author's own spelling: {}",
        hover_markdown(&typed)
    );

    let opaque = hover_at(&client, &uri, Position::new(4, 9)).expect("hover on opaque");
    assert!(
        !hover_markdown(&opaque).contains("var opaque:"),
        "nothing better than Variant means nothing appended: {}",
        hover_markdown(&opaque)
    );

    shutdown(&client, server_thread);
}

#[test]
fn a_preloaded_scene_and_a_global_constant_cross_the_file_boundary() {
    let p = project();
    let (client, server_thread) = boot(&p);

    let uri = file_uri(&p.root.join("reader.gd"));
    let clean = open(&client, &uri, READER_GD);
    assert!(
        clean.diagnostics.is_empty(),
        "reading either member is well typed; got: {:?}",
        clean.diagnostics
    );

    let bad_uri = file_uri(&p.root.join("bad_reader.gd"));
    let bad = open(&client, &bad_uri, BAD_READER_GD);
    let messages: Vec<String> = bad.diagnostics.iter().map(|d| d.message.clone()).collect();
    assert_eq!(messages.len(), 2, "one row per misuse; got: {messages:?}");
    assert!(
        messages[0].contains("PackedScene"),
        "the `.tscn` preload must reach the reader as PackedScene, not Variant; got: {}",
        messages[0]
    );
    assert!(
        messages[1].contains("float"),
        "a member inferred from PI must reach the reader as float, not Variant; got: {}",
        messages[1]
    );

    shutdown(&client, server_thread);
}
