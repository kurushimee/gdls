//! M7 (#62) — the documentation pipeline over the wire: `##` doc prose in hover (same-file
//! declaration, cross-file member), BBCode → GFM conversion (never raw BBCode on the wire,
//! anti-catalog W8), and the `hover.contentFormat` gate with its plaintext downgrade.

mod common;

use common::{file_uri, notification, recv, recv_response, request, sample_project};
use lsp_server::{Connection, Message, RequestId};
use lsp_types::{
    ClientCapabilities, DidOpenTextDocumentParams, HoverClientCapabilities, InitializeParams,
    InitializedParams, MarkupKind, Position, TextDocumentClientCapabilities,
    TextDocumentIdentifier, TextDocumentItem, TextDocumentPositionParams, Uri,
};

const HERO_SRC: &str = "\
class_name Hero
extends Node2D

var hp: int = 10

## Attacks the [b]nearest[/b] enemy. See [method take_damage].
func attack() -> void:
\tpass
";

fn boot(
    p: &common::TempProject,
    capabilities: ClientCapabilities,
) -> (Connection, std::thread::JoinHandle<anyhow::Result<()>>) {
    let (server, client) = Connection::memory();
    let server_thread = std::thread::spawn(move || gd_server::serve(server));
    let init = InitializeParams {
        capabilities,
        initialization_options: Some(serde_json::json!({
            "projectRoot": p.root.as_str(),
            "autoDumpExtensionApi": false,
            "extensionApiPath": p.root.join("extension_api.json").as_str(),
        })),
        ..Default::default()
    };
    client.sender.send(request(1, "initialize", init)).unwrap();
    loop {
        if let Message::Response(resp) = recv(&client) {
            assert!(resp.error.is_none());
            break;
        }
    }
    client
        .sender
        .send(notification("initialized", InitializedParams {}))
        .unwrap();
    (client, server_thread)
}

fn hover_caps(formats: Vec<MarkupKind>) -> ClientCapabilities {
    ClientCapabilities {
        text_document: Some(TextDocumentClientCapabilities {
            hover: Some(HoverClientCapabilities {
                content_format: Some(formats),
                ..Default::default()
            }),
            ..Default::default()
        }),
        ..Default::default()
    }
}

fn did_open(client: &Connection, uri: &Uri, text: &str) {
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
    // Drain the didOpen publish.
    loop {
        if let Message::Notification(n) = recv(client) {
            if n.method == "textDocument/publishDiagnostics" {
                break;
            }
        }
    }
}

/// `(kind, value)` of a hover at `(line, character)`.
fn hover_at(
    client: &Connection,
    id: i32,
    uri: &Uri,
    line: u32,
    character: u32,
) -> (String, String) {
    client
        .sender
        .send(request(
            id,
            "textDocument/hover",
            lsp_types::HoverParams {
                text_document_position_params: TextDocumentPositionParams {
                    text_document: TextDocumentIdentifier { uri: uri.clone() },
                    position: Position { line, character },
                },
                work_done_progress_params: Default::default(),
            },
        ))
        .unwrap();
    let resp = loop {
        let r = recv_response(client);
        if r.id == RequestId::from(id) {
            break r;
        }
    };
    assert!(resp.error.is_none(), "hover errored: {:?}", resp.error);
    let result = resp.result.expect("hover result");
    assert!(!result.is_null(), "hover returned null");
    (
        result["contents"]["kind"]
            .as_str()
            .unwrap_or("")
            .to_string(),
        result["contents"]["value"]
            .as_str()
            .unwrap_or("")
            .to_string(),
    )
}

/// The #62 acceptance bar: hover on a `##`-documented member shows the prose — converted to
/// GFM (bold, code-span cross-refs; no raw BBCode), below the rust-analyzer-style `---` rule —
/// both on the declaration itself and from a dependent file's call site.
#[test]
fn documented_member_hover_shows_converted_prose() {
    let p = sample_project();
    p.write("src/hero.gd", HERO_SRC);
    p.write(
        "src/enemy.gd",
        "extends Node\n\nfunc taunt(h: Hero) -> void:\n\th.attack()\n",
    );
    let (client, server_thread) = boot(&p, hover_caps(vec![MarkupKind::Markdown]));

    // Same-file: cursor on the declaration name.
    let hero_uri = file_uri(&p.root.join("src/hero.gd"));
    did_open(&client, &hero_uri, HERO_SRC);
    let (kind, value) = hover_at(&client, 2, &hero_uri, 6, 6); // `attack` in `func attack()`
    assert_eq!(kind, "markdown");
    assert!(value.contains("```gdscript"), "signature fence: {value}");
    assert!(
        value.contains("\n---\n"),
        "rust-analyzer separator: {value}"
    );
    assert!(
        value.contains("Attacks the **nearest** enemy. See `take_damage()`."),
        "converted GFM prose: {value}"
    );
    assert!(!value.contains("[b]"), "no raw BBCode on the wire: {value}");

    // Cross-file: cursor on the call site in the dependent.
    let enemy_uri = file_uri(&p.root.join("src/enemy.gd"));
    did_open(
        &client,
        &enemy_uri,
        "extends Node\n\nfunc taunt(h: Hero) -> void:\n\th.attack()\n",
    );
    let (_, value) = hover_at(&client, 3, &enemy_uri, 3, 4); // `attack` in `h.attack()`
    assert!(
        value.contains("Attacks the **nearest** enemy."),
        "cross-file hover carries the declaring file's doc prose: {value}"
    );

    common::shutdown(&client, server_thread);
}

/// A client whose `hover.contentFormat` prefers plaintext gets `kind: plaintext` with markup
/// stripped — no fences, no `**`, no backticks.
#[test]
fn plaintext_preferring_client_gets_stripped_hover() {
    let p = sample_project();
    p.write("src/hero.gd", HERO_SRC);
    let (client, server_thread) = boot(&p, hover_caps(vec![MarkupKind::PlainText]));
    let hero_uri = file_uri(&p.root.join("src/hero.gd"));
    did_open(&client, &hero_uri, HERO_SRC);

    let (kind, value) = hover_at(&client, 2, &hero_uri, 6, 6);
    assert_eq!(kind, "plaintext");
    assert!(
        value.contains("func attack() -> void"),
        "signature text survives: {value}"
    );
    assert!(
        value.contains("Attacks the nearest enemy. See take_damage()."),
        "prose survives with markup stripped: {value}"
    );
    assert!(!value.contains("```"), "no fences: {value}");
    assert!(!value.contains("**"), "no bold markers: {value}");

    common::shutdown(&client, server_thread);
}

/// #261: a client that advertised no `hover.contentFormat` has told the server nothing about
/// what it can render, so plaintext is the floor — sending markdown on that assumption puts raw
/// ``` fences and `**` into a popup that may not render them. Every captured editor profile asks
/// for markdown explicitly, so nothing real is downgraded by this.
#[test]
fn absent_content_format_defaults_to_plaintext() {
    let p = sample_project();
    p.write("src/hero.gd", HERO_SRC);
    let (client, server_thread) = boot(&p, ClientCapabilities::default());
    let hero_uri = file_uri(&p.root.join("src/hero.gd"));
    did_open(&client, &hero_uri, HERO_SRC);

    let (kind, value) = hover_at(&client, 2, &hero_uri, 6, 6);
    assert_eq!(kind, "plaintext");
    assert!(!value.contains("```"), "no fences: {value}");
    assert!(!value.contains("**"), "no bold markers: {value}");

    common::shutdown(&client, server_thread);
}

/// An EMPTY `contentFormat` list says just as little as an absent one, so it takes the same
/// floor rather than falling through to markdown.
#[test]
fn empty_content_format_also_takes_plaintext() {
    let p = sample_project();
    p.write("src/hero.gd", HERO_SRC);
    let (client, server_thread) = boot(&p, hover_caps(vec![]));
    let hero_uri = file_uri(&p.root.join("src/hero.gd"));
    did_open(&client, &hero_uri, HERO_SRC);

    let (kind, value) = hover_at(&client, 2, &hero_uri, 6, 6);
    assert_eq!(kind, "plaintext");
    assert!(!value.contains("```"), "no fences: {value}");

    common::shutdown(&client, server_thread);
}

/// A `##`-documented class shows its brief in hover via the warm interface; the doc-only edit
/// rule (no dependent invalidation) is pinned at the gd_project layer.
#[test]
fn documented_class_members_survive_a_doc_only_edit() {
    // The structural contract behind "docs stay fresh without reverse-dependency churn":
    // hover prose updates after a doc edit because hover reads the LIVE interface.
    let p = sample_project();
    p.write("src/hero.gd", HERO_SRC);
    let (client, server_thread) = boot(&p, hover_caps(vec![MarkupKind::Markdown]));
    let hero_uri = file_uri(&p.root.join("src/hero.gd"));
    did_open(&client, &hero_uri, HERO_SRC);

    // Edit ONLY the doc comment.
    let edited = HERO_SRC.replace(
        "Attacks the [b]nearest[/b] enemy",
        "Strikes the closest foe",
    );
    client
        .sender
        .send(notification(
            "textDocument/didChange",
            lsp_types::DidChangeTextDocumentParams {
                text_document: lsp_types::VersionedTextDocumentIdentifier {
                    uri: hero_uri.clone(),
                    version: 2,
                },
                content_changes: vec![lsp_types::TextDocumentContentChangeEvent {
                    range: None,
                    range_length: None,
                    text: edited,
                }],
            },
        ))
        .unwrap();
    loop {
        if let Message::Notification(n) = recv(&client) {
            if n.method == "textDocument/publishDiagnostics" {
                break;
            }
        }
    }

    let (_, value) = hover_at(&client, 4, &hero_uri, 6, 6);
    assert!(
        value.contains("Strikes the closest foe"),
        "doc edits surface immediately in hover: {value}"
    );

    common::shutdown(&client, server_thread);
}
