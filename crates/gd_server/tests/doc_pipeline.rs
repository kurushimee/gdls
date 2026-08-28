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

// ===================================================================================================
// #258 — the doc surfaces M7 left unwired: the file's own class, enums and enum values, non-`func`
// members at a cross-file USE site, and the `@deprecated` / `@experimental` markers.
// ===================================================================================================

/// Every declaration kind that can carry a `##` block, plus a `@deprecated` member and a
/// `@tutorial` link on the class.
const DOCUMENTED_SRC: &str = "\
## A documented widget.
##
## The [b]long[/b] form.
##
## @tutorial(Widgets): https://example.com/widgets
class_name DocWidget
extends Node

## The widget's width.
var width: int = 3

## The upper bound.
const LIMIT := 9

## What a widget can be.
enum Kind {
\t## The first one.
\tONE,
\tTWO,
}

## Fired on change.
signal changed(v: int)

## Grows the widget.
##
## @deprecated: Use resize() instead.
func grow(amount: int) -> void:
\twidth += amount

## A nested helper.
class Inner:
\t## The inner field.
\tvar q := 1
";

const DOCUMENTED_USE_SRC: &str = "\
extends Node

func use_it(w: DocWidget) -> void:
\tw.grow(1)
\tprint(w.width, DocWidget.LIMIT, DocWidget.Kind.ONE)
\tw.changed.connect(func(v): pass)
\tvar i := DocWidget.Inner.new()
\tprint(i.q)
";

/// Lay the pair down and open both, returning their URIs.
fn documented_project() -> (
    common::TempProject,
    Connection,
    std::thread::JoinHandle<anyhow::Result<()>>,
    Uri,
    Uri,
) {
    let p = common::sample_project();
    p.write("src/docw.gd", DOCUMENTED_SRC);
    p.write("src/docuse.gd", DOCUMENTED_USE_SRC);
    let (client, server_thread) = boot(&p, hover_caps(vec![MarkupKind::Markdown]));
    let decl = file_uri(&p.root.join("src/docw.gd"));
    let usage = file_uri(&p.root.join("src/docuse.gd"));
    did_open(&client, &decl, DOCUMENTED_SRC);
    did_open(&client, &usage, DOCUMENTED_USE_SRC);
    (p, client, server_thread, decl, usage)
}

/// Hovering the file's OWN `class_name` used to render a bare name. It now carries the head class's
/// `##` block — brief, long form, and the `@tutorial` links — and so does every cross-file use of
/// that class name, which reads the same `Interface`.
#[test]
fn the_files_own_class_name_hovers_with_its_doc() {
    let (_p, client, server_thread, decl, usage) = documented_project();

    let (_, value) = hover_at(&client, 10, &decl, 5, 12); // `DocWidget` in `class_name DocWidget`
    assert!(
        value.contains("A documented widget."),
        "brief on the declaration: {value}"
    );
    assert!(
        value.contains("The **long** form."),
        "long form, converted: {value}"
    );
    assert!(
        value.contains("[Widgets](https://example.com/widgets)"),
        "@tutorial link: {value}"
    );

    let (_, value) = hover_at(&client, 11, &usage, 6, 11); // `DocWidget` in `DocWidget.Inner.new()`
    assert!(
        value.contains("A documented widget."),
        "the same body at a cross-file use site: {value}"
    );

    common::shutdown(&client, server_thread);
}

/// A named enum and its values each carry their own `##` block. Neither reached hover before #258:
/// the declaration scan skipped `Member::Enum` outright, and a value's doc is keyed by
/// `(enum node, index)` rather than by declaration node.
#[test]
fn enums_and_enum_values_hover_with_their_docs() {
    let (_p, client, server_thread, decl, usage) = documented_project();

    let (_, value) = hover_at(&client, 12, &decl, 15, 6); // `Kind` in `enum Kind {`
    assert!(value.contains("enum Kind"), "declaration line: {value}");
    assert!(value.contains("What a widget can be."), "enum doc: {value}");

    let (_, value) = hover_at(&client, 13, &decl, 17, 2); // `ONE`
    assert!(value.contains("Kind.ONE"), "qualified value: {value}");
    assert!(value.contains("The first one."), "value doc: {value}");

    let (_, value) = hover_at(&client, 14, &usage, 4, 44); // `Kind` in `DocWidget.Kind.ONE`
    assert!(
        value.contains("What a widget can be."),
        "cross-file enum doc: {value}"
    );

    let (_, value) = hover_at(&client, 15, &usage, 4, 49); // `ONE` in `DocWidget.Kind.ONE`
    assert!(
        value.contains("The first one."),
        "cross-file enum-value doc: {value}"
    );

    common::shutdown(&client, server_thread);
}

/// M7 wired the doc through the CALL hover only, so a `var` / `const` / `signal` / inner `class`
/// referenced from another file rendered a bare signature. All four now carry the declaring file's
/// prose, like the method already did.
#[test]
fn non_func_members_carry_their_doc_at_a_cross_file_use_site() {
    let (_p, client, server_thread, _decl, usage) = documented_project();

    let (_, value) = hover_at(&client, 16, &usage, 4, 10); // `width` in `w.width`
    assert!(value.contains("The widget's width."), "var: {value}");

    let (_, value) = hover_at(&client, 17, &usage, 4, 27); // `LIMIT` in `DocWidget.LIMIT`
    assert!(value.contains("The upper bound."), "const: {value}");

    let (_, value) = hover_at(&client, 18, &usage, 5, 4); // `changed` in `w.changed.connect(…)`
    assert!(value.contains("Fired on change."), "signal: {value}");

    let (_, value) = hover_at(&client, 19, &usage, 6, 21); // `Inner` in `DocWidget.Inner.new()`
    assert!(value.contains("class Inner"), "inner class line: {value}");
    assert!(value.contains("A nested helper."), "inner class: {value}");

    let (_, value) = hover_at(&client, 20, &usage, 7, 9); // `q` in `i.q`
    assert!(value.contains("The inner field."), "inner field: {value}");

    common::shutdown(&client, server_thread);
}

/// `@deprecated: msg` renders as a banner ABOVE the prose, at the declaration and at every use.
#[test]
fn a_deprecated_member_renders_its_marker_in_hover() {
    let (_p, client, server_thread, decl, usage) = documented_project();

    let (_, value) = hover_at(&client, 21, &decl, 27, 6); // `grow` in `func grow(...)`
    assert!(
        value.contains("**Deprecated:** Use resize() instead."),
        "banner on the declaration: {value}"
    );
    let banner = value.find("**Deprecated:**").expect("banner");
    let prose = value.find("Grows the widget.").expect("prose");
    assert!(banner < prose, "the banner leads the prose: {value}");

    let (_, value) = hover_at(&client, 22, &usage, 3, 4); // `grow` in `w.grow(1)`
    assert!(
        value.contains("**Deprecated:** Use resize() instead."),
        "banner at the use site: {value}"
    );

    common::shutdown(&client, server_thread);
}

/// The plaintext downgrade covers the new prose too: no `**` on the banner, and the `@tutorial`
/// link flattens to `title (url)` — the same shape `[url=…]` already takes in plaintext.
#[test]
fn the_new_prose_survives_the_plaintext_downgrade() {
    let p = common::sample_project();
    p.write("src/docw.gd", DOCUMENTED_SRC);
    let (client, server_thread) = boot(&p, hover_caps(vec![MarkupKind::PlainText]));
    let decl = file_uri(&p.root.join("src/docw.gd"));
    did_open(&client, &decl, DOCUMENTED_SRC);

    let (kind, value) = hover_at(&client, 23, &decl, 5, 12); // `class_name DocWidget`
    assert_eq!(kind, "plaintext");
    assert!(
        value.contains("Widgets (https://example.com/widgets)"),
        "flattened tutorial link: {value}"
    );
    assert!(!value.contains("]("), "no markdown link syntax: {value}");
    assert!(!value.contains("**"), "no bold markers: {value}");

    let (kind, value) = hover_at(&client, 24, &decl, 27, 6); // `func grow(...)`
    assert_eq!(kind, "plaintext");
    assert!(
        value.contains("Deprecated: Use resize() instead."),
        "plain banner: {value}"
    );

    common::shutdown(&client, server_thread);
}

/// #277: an inner class named in a TYPE position (`func f(i: Outer.Inner)`) has no `class_name`
/// registry entry for its `Inner` segment, so the leaf-label hover arm never fired and the body was
/// a bare `DocWidget.Inner`. The analyzer pinned a Script type there — following it to the
/// declaring interface renders the same doc the expression position (`DocWidget.Inner.new()`)
/// already showed.
#[test]
fn an_inner_class_in_a_type_position_carries_its_doc() {
    let p = common::sample_project();
    p.write("src/docw.gd", DOCUMENTED_SRC);
    let use_src = "extends Node\n\nfunc take(i: DocWidget.Inner) -> void:\n\tprint(i)\n";
    p.write("src/typepos.gd", use_src);
    let (client, server_thread) = boot(&p, hover_caps(vec![MarkupKind::Markdown]));
    let decl = file_uri(&p.root.join("src/docw.gd"));
    let usage = file_uri(&p.root.join("src/typepos.gd"));
    did_open(&client, &decl, DOCUMENTED_SRC);
    did_open(&client, &usage, use_src);

    let (_, value) = hover_at(&client, 30, &usage, 2, 25); // `Inner` in `i: DocWidget.Inner`
    assert!(
        value.contains("A nested helper."),
        "the inner class's doc in a type annotation: {value}"
    );

    // The outer segment of the same annotation still renders the head class's doc.
    let (_, value) = hover_at(&client, 31, &usage, 2, 15); // `DocWidget`
    assert!(
        value.contains("A documented widget."),
        "the outer segment keeps the head-class doc: {value}"
    );

    common::shutdown(&client, server_thread);
}
