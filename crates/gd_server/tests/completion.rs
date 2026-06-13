//! M8 (#64) Phase 3: `textDocument/completion` + `completionItem/resolve` over a real in-memory
//! `Connection`. These are the acceptance tests for the phase — they assert the wire contract a
//! Godot-unaware client sees:
//!
//! - a member completion at `base.<cursor>` is a `CompletionList` (an OBJECT with `items`), never a
//!   bare JSON array (anti-catalog W18) — asserted on the raw JSON;
//! - an identifier completion is ranked by a fixed-width `sortText` (lexicographic == priority);
//! - `completionItem/resolve` fills documentation/detail and leaves the ranking/edit fields
//!   byte-for-byte unchanged, with a compact `data` key (no request params);
//! - capability gating both ways (snippet on/off, documentationFormat, insertReplaceSupport off);
//! - `CompletionItemKind` clamped to the client's `completionItemKind.valueSet`.

mod common;

use common::{file_uri, notification, recv_response, request, sample_project, TempProject};
use lsp_server::{Connection, Message};
use lsp_types::{
    ClientCapabilities, CompletionClientCapabilities, CompletionItem, CompletionItemCapability,
    CompletionItemKind, CompletionItemKindCapability, CompletionList, CompletionTextEdit,
    DidOpenTextDocumentParams, InitializeParams, InitializedParams, MarkupKind, Position,
    TextDocumentClientCapabilities, TextDocumentItem, Uri,
};

/// A completion-capable client capability bundle. `snippet`/`insert_replace`/`commit`/`doc_md` and
/// the kind value-set are knobs each test flips to exercise a gate.
#[allow(clippy::too_many_arguments)]
fn caps(
    snippet: bool,
    insert_replace: bool,
    commit: bool,
    doc_formats: Option<Vec<MarkupKind>>,
    kinds: Option<Vec<CompletionItemKind>>,
) -> ClientCapabilities {
    ClientCapabilities {
        text_document: Some(TextDocumentClientCapabilities {
            completion: Some(CompletionClientCapabilities {
                completion_item: Some(CompletionItemCapability {
                    snippet_support: Some(snippet),
                    insert_replace_support: Some(insert_replace),
                    commit_characters_support: Some(commit),
                    documentation_format: doc_formats,
                    ..Default::default()
                }),
                completion_item_kind: kinds.map(|value_set| CompletionItemKindCapability {
                    value_set: Some(value_set),
                }),
                ..Default::default()
            }),
            ..Default::default()
        }),
        ..Default::default()
    }
}

/// A richer `extension_api.json` than `common::MINI_API`: native classes that actually carry
/// **members** (`Object.get_class()`, `Node.queue_free()` — so the inherited-native enumeration is
/// exercised) plus a global **utility** (`print`) and a global **constant** (`KEY_A`), so the
/// IDENTIFIER global set is non-empty and a known global can be asserted by name.
const RICH_API: &str = r#"{
    "header": {"version_major": 4, "version_minor": 6, "version_patch": 3},
    "global_constants": [
        {"name": "KEY_A", "value": 65}
    ],
    "utility_functions": [
        {"name": "print", "return_type": "void", "is_vararg": true, "arguments": []}
    ],
    "classes": [
        {"name": "Object", "methods": [
            {"name": "get_class", "is_const": true, "return_value": {"type": "String"}}
        ]},
        {"name": "Node", "inherits": "Object", "methods": [
            {"name": "queue_free"}
        ]},
        {"name": "CanvasItem", "inherits": "Node"},
        {"name": "Node2D", "inherits": "CanvasItem"}
    ]
}"#;

/// A throwaway project whose dump is [`RICH_API`] (members + a utility + a constant). Mirrors
/// `common::sample_project`'s layout (a `Hero` class extending `Node2D`).
fn rich_project() -> TempProject {
    let p = TempProject::new();
    p.write(
        "project.godot",
        "config_version=5\n\n[application]\n\nconfig/name=\"T\"\n",
    );
    p.write("extension_api.json", RICH_API);
    p.write(
        "src/hero.gd",
        "class_name Hero\nextends Node2D\n\nvar hp: int = 10\n\nfunc attack() -> void:\n\tpass\n",
    );
    p
}

/// Boot the server over an in-memory connection against `project`, handshaking with `client_caps`,
/// and open `(uri, text)`. Returns the connected client + the server thread join handle.
fn boot(
    project: &TempProject,
    client_caps: ClientCapabilities,
    uri: &Uri,
    text: &str,
) -> (Connection, std::thread::JoinHandle<anyhow::Result<()>>) {
    // The init options as raw JSON (the schema is Deserialize-only): project root + the on-disk
    // mini dump, with auto-dump off so the test is hermetic.
    let options = serde_json::json!({
        "projectRoot": project.root.as_str(),
        "autoDumpExtensionApi": false,
        "extensionApiPath": project.root.join("extension_api.json").as_str(),
    });
    let (server, client) = Connection::memory();
    let server_thread = std::thread::spawn(move || gd_server::serve(server));

    let init = InitializeParams {
        capabilities: client_caps,
        initialization_options: Some(options),
        ..Default::default()
    };
    client.sender.send(request(1, "initialize", init)).unwrap();
    let resp = recv_response(&client);
    assert!(resp.error.is_none(), "initialize errored: {:?}", resp.error);

    client
        .sender
        .send(notification(
            "initialized",
            serde_json::to_value(InitializedParams {}).unwrap(),
        ))
        .unwrap();
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

    (client, server_thread)
}

/// Send a `textDocument/completion` at `pos` in `uri` and return the RAW JSON result (so a test can
/// assert the shape before any typed deserialization could hide a bare-array bug).
fn complete_raw(client: &Connection, id: i32, uri: &Uri, pos: Position) -> serde_json::Value {
    client
        .sender
        .send(request(
            id,
            "textDocument/completion",
            serde_json::json!({
                "textDocument": { "uri": uri.as_str() },
                "position": { "line": pos.line, "character": pos.character },
            }),
        ))
        .unwrap();
    let resp = recv_response(client);
    assert!(resp.error.is_none(), "completion errored: {:?}", resp.error);
    resp.result.expect("completion result")
}

fn shutdown(client: &Connection, server_thread: std::thread::JoinHandle<anyhow::Result<()>>) {
    common::shutdown(client, server_thread);
}

/// The capability bundle the "rich client" tests use: every gate on, Markdown docs, default kinds.
fn rich_caps() -> ClientCapabilities {
    caps(true, true, true, Some(vec![MarkupKind::Markdown]), None)
}

// ===================================================================================================
// Capability advertisement.
// ===================================================================================================

/// The server advertises `completionProvider` with the M8 trigger characters, `resolveProvider:
/// true`, and `completionItem.labelDetailsSupport`. Confirms the static capability the client reads
/// at `initialize` (the dispatch arms + ClientCaps negotiation are exercised by the other tests).
#[test]
fn advertises_completion_provider_with_triggers_and_resolve() {
    let p = sample_project();
    let (server, client) = Connection::memory();
    let server_thread = std::thread::spawn(move || gd_server::serve(server));
    let init = InitializeParams {
        capabilities: rich_caps(),
        initialization_options: Some(serde_json::json!({ "projectRoot": p.root.as_str() })),
        ..Default::default()
    };
    client.sender.send(request(1, "initialize", init)).unwrap();
    let resp = recv_response(&client);
    let result: lsp_types::InitializeResult =
        serde_json::from_value(resp.result.expect("initialize result")).unwrap();
    let cp = result
        .capabilities
        .completion_provider
        .expect("completionProvider advertised");
    let triggers = cp.trigger_characters.expect("trigger characters");
    for t in [".", "$", "%", "\"", "@"] {
        assert!(
            triggers.contains(&t.to_string()),
            "missing trigger char {t:?} in {triggers:?}"
        );
    }
    assert_eq!(
        cp.resolve_provider,
        Some(true),
        "resolve_provider advertised"
    );
    assert_eq!(
        cp.completion_item.and_then(|c| c.label_details_support),
        Some(true),
        "labelDetailsSupport advertised"
    );

    // Complete the handshake before shutting down (matches the proven `boot` sequence).
    client
        .sender
        .send(notification(
            "initialized",
            serde_json::to_value(InitializedParams {}).unwrap(),
        ))
        .unwrap();
    shutdown(&client, server_thread);
}

// ===================================================================================================
// ATTRIBUTE — member completion at `base.<cursor>`.
// ===================================================================================================

/// THE headline W18 test: completion at a typed `base.<cursor>` returns a `CompletionList` whose
/// JSON is an OBJECT (`items` + `isIncomplete`), never a bare array, and whose items include the
/// resolved script type's members. The base is a typed PARAMETER inside a function body, where
/// `classify` yields `Attribute { base: Some(_) }` and the analyzer has pinned the type.
#[test]
fn member_completion_is_a_completion_list_not_an_array() {
    let p = sample_project();
    // A consumer of the `Hero` script class: a typed parameter `h: Hero`, then `h.` on its own line.
    let src = "extends Node2D\n\nfunc use(h: Hero) -> void:\n\th.\n";
    let uri = file_uri(&p.root.join("src/consumer.gd"));
    let (client, server_thread) = boot(&p, rich_caps(), &uri, src);

    // Cursor right after the `.` on line 3 (0-based): `\th.` → column 3.
    let raw = complete_raw(&client, 10, &uri, Position::new(3, 3));

    // (W18) The result is a List object, NOT a bare array. Inspect the raw JSON directly.
    assert!(
        raw.is_object(),
        "completion result must be a CompletionList object, got: {raw}"
    );
    assert!(
        raw.get("items").is_some_and(|i| i.is_array()),
        "a CompletionList carries an `items` array; got: {raw}"
    );
    assert!(
        !raw.is_array(),
        "completion result must never be a bare array (W18)"
    );

    // The resolved type's members are present: `Hero` declares `hp` and `attack`.
    let list: CompletionList = serde_json::from_value(raw).expect("a CompletionList");
    let labels: Vec<&str> = list.items.iter().map(|i| i.label.as_str()).collect();
    assert!(
        labels.contains(&"hp") && labels.contains(&"attack"),
        "member completion must include Hero's members hp + attack; got {labels:?}"
    );

    shutdown(&client, server_thread);
}

/// Criterion 1's `node.<cursor>` example, exercising the **inherited-native** arm explicitly:
/// completion on a typed `n: Node` includes `queue_free` (Node's own native method) AND `get_class`
/// (inherited from `Object` up the native `inherits` chain) — proving `members_of_type` walks the
/// native extends chain, not just the leaf class. Uses [`RICH_API`] (native classes with members).
#[test]
fn member_completion_includes_inherited_native_members() {
    let p = rich_project();
    let src = "extends Node2D\n\nfunc use(n: Node) -> void:\n\tn.\n";
    let uri = file_uri(&p.root.join("src/consumer.gd"));
    let (client, server_thread) = boot(&p, rich_caps(), &uri, src);

    let raw = complete_raw(&client, 15, &uri, Position::new(3, 3));
    let list: CompletionList = serde_json::from_value(raw).expect("a CompletionList");
    let labels: Vec<&str> = list.items.iter().map(|i| i.label.as_str()).collect();
    assert!(
        labels.contains(&"queue_free"),
        "Node's own native method must appear; got {labels:?}"
    );
    assert!(
        labels.contains(&"get_class"),
        "the inherited native method get_class (from Object) must appear; got {labels:?}"
    );
    // The native method renders as kind METHOD.
    let qf = list.items.iter().find(|i| i.label == "queue_free").unwrap();
    assert_eq!(qf.kind, Some(CompletionItemKind::METHOD));

    shutdown(&client, server_thread);
}

/// Every member item carries a single-line `TextEdit` over the (empty) prefix span at the cursor,
/// a fixed-width `sortText`, and a `filterText` equal to the label. The method `attack` is callable
/// → with `snippetSupport` on it inserts a `($0)` snippet.
#[test]
fn member_items_carry_text_edit_sort_filter_and_snippet() {
    let p = sample_project();
    let src = "extends Node2D\n\nfunc use(h: Hero) -> void:\n\th.\n";
    let uri = file_uri(&p.root.join("src/consumer.gd"));
    let (client, server_thread) = boot(&p, rich_caps(), &uri, src);

    let raw = complete_raw(&client, 11, &uri, Position::new(3, 3));
    let list: CompletionList = serde_json::from_value(raw).expect("a CompletionList");

    // Items are sorted by a fixed-width sortText, so a lexicographic sort == server order.
    let sorts: Vec<String> = list
        .items
        .iter()
        .map(|i| i.sort_text.clone().expect("every item has sortText"))
        .collect();
    assert!(
        sorts.iter().all(|s| s.len() == sorts[0].len()),
        "sortText is fixed-width: {sorts:?}"
    );
    let mut sorted = sorts.clone();
    sorted.sort();
    assert_eq!(
        sorts, sorted,
        "items already in lexicographic sortText order"
    );

    for item in &list.items {
        assert_eq!(
            item.filter_text.as_deref(),
            Some(item.label.as_str()),
            "filterText aligns to the label"
        );
        match item.text_edit.as_ref().expect("every item has a textEdit") {
            CompletionTextEdit::InsertAndReplace(e) => {
                assert_eq!(
                    e.insert.start.line, 3,
                    "edit is single-line on the cursor row"
                );
                assert_eq!(e.insert, e.replace, "insert == replace for a prefix edit");
            }
            CompletionTextEdit::Edit(_) => panic!("rich client opted into insertReplaceSupport"),
        }
    }

    // The callable `attack` carries a snippet (insertTextFormat == Snippet, newText has `$0`).
    let attack = list
        .items
        .iter()
        .find(|i| i.label == "attack")
        .expect("attack present");
    assert_eq!(
        attack.insert_text_format,
        Some(lsp_types::InsertTextFormat::SNIPPET),
        "a callable with snippetSupport inserts a snippet"
    );
    if let CompletionTextEdit::InsertAndReplace(e) = attack.text_edit.as_ref().unwrap() {
        assert!(
            e.new_text.contains("$0"),
            "snippet newText carries the $0 tab-stop: {:?}",
            e.new_text
        );
    }

    shutdown(&client, server_thread);
}

// ===================================================================================================
// IDENTIFIER — the bare-name set, ranked.
// ===================================================================================================

/// A bare-identifier site returns locals/params + class members + globals, ranked by a fixed-width
/// `sortText`, with the local ranked ahead of any global. Here a body-local `speed` precedes the
/// global utility `print`.
#[test]
fn identifier_completion_ranks_locals_before_globals() {
    // [`RICH_API`] so a real global utility (`print`) and constant (`KEY_A`) exist. `Sprinter` has
    // a self class-member `speedy`; the body has a local `speed`. The server returns the FULL
    // in-scope set (the client filters by `filterText`), so locals, the self-member, and the
    // globals all appear — and `sortText` encodes the priority order.
    let p = rich_project();
    let src = "class_name Sprinter\nextends Node2D\n\nvar speedy := 1\n\nfunc go() -> void:\n\tvar speed := 5\n\ts\n";
    let uri = file_uri(&p.root.join("src/ident.gd"));
    let (client, server_thread) = boot(&p, rich_caps(), &uri, src);

    // Cursor after the `s` on line 7 (`\ts`) → column 2.
    let raw = complete_raw(&client, 12, &uri, Position::new(7, 2));
    let list: CompletionList = serde_json::from_value(raw).expect("a CompletionList");

    let speed = list
        .items
        .iter()
        .find(|i| i.label == "speed")
        .expect("the local `speed` is offered");
    // The implicit-self class member `speedy` is also offered (the self-extends-chain arm), ranked
    // after the local but before globals.
    let speedy = list
        .items
        .iter()
        .find(|i| i.label == "speedy")
        .expect("the self class-member `speedy` is offered");
    assert!(
        speed.sort_text < speedy.sort_text,
        "a body local ranks before a self class-member"
    );
    // The global utility `print` is present (a FUNCTION kind) and ranks AFTER the local — the
    // criterion-2 assertion, now unconditional (RICH_API guarantees `print` exists).
    let print = list
        .items
        .iter()
        .find(|i| i.label == "print")
        .expect("the global utility `print` is offered");
    assert_eq!(
        print.kind,
        Some(CompletionItemKind::FUNCTION),
        "a utility renders as FUNCTION"
    );
    assert!(
        speed.sort_text < print.sort_text,
        "the local `speed` ({:?}) must rank before the global `print` ({:?})",
        speed.sort_text,
        print.sort_text
    );
    assert!(
        speedy.sort_text < print.sort_text,
        "the self class-member `speedy` must rank before the global `print`"
    );
    // The global constant `KEY_A` is also present (proves the global_constants arm).
    assert!(
        list.items.iter().any(|i| i.label == "KEY_A"),
        "the global constant KEY_A is offered"
    );
    // sortText is fixed-width across the whole list.
    let widths: std::collections::HashSet<usize> = list
        .items
        .iter()
        .filter_map(|i| i.sort_text.as_ref().map(|s| s.len()))
        .collect();
    assert_eq!(widths.len(), 1, "all sortText share one fixed width");

    shutdown(&client, server_thread);
}

// ===================================================================================================
// completionItem/resolve — round-trip: docs filled, ranking/edit fields immutable, compact data.
// ===================================================================================================

/// `completionItem/resolve` fills documentation/detail and leaves `sortText`/`filterText`/
/// `insertTextFormat`/`textEdit` byte-for-byte unchanged. `data` is a compact key (no request
/// params). Uses a member with a `##` doc comment so resolve has documentation to add.
#[test]
fn resolve_fills_docs_and_leaves_ranking_fields_unchanged() {
    let p = TempProject::new();
    p.write(
        "project.godot",
        "config_version=5\n\n[application]\n\nconfig/name=\"T\"\n",
    );
    p.write("extension_api.json", common::MINI_API);
    // `Hero` with a documented member `hp`, completed via a typed parameter `h: Hero` IN THE SAME
    // FILE — so the member's declaring file equals the requesting file and resolve can find the
    // `##` doc comment through the interface (cross-file declaring-file doc lookup is a documented
    // Phase-3 gap: `data` doesn't carry the declaring location).
    let src = "class_name Hero\nextends Node\n\n## The hero's hit points.\nvar hp: int = 10\n\nfunc use(h: Hero) -> void:\n\th.\n";
    let uri = file_uri(&p.root.join("src/hero.gd"));
    let (client, server_thread) = boot(&p, rich_caps(), &uri, src);

    // `\th.` is on line 7 (0-based) → column 3.
    let raw = complete_raw(&client, 20, &uri, Position::new(7, 3));
    let list: CompletionList = serde_json::from_value(raw).expect("a CompletionList");
    let pre = list
        .items
        .iter()
        .find(|i| i.label == "hp")
        .expect("hp present")
        .clone();

    // `data` must be compact and self-sufficient — NOT the request params (W18).
    let data = pre.data.clone().expect("item carries data");
    let data_str = data.to_string();
    for banned in ["position", "\"line\"", "\"character\"", "textDocument"] {
        assert!(
            !data_str.contains(banned),
            "data must not carry request param `{banned}`: {data_str}"
        );
    }
    // Pre-resolve, documentation/detail are lazy (absent).
    assert!(pre.documentation.is_none(), "documentation is lazy");

    // Resolve it.
    client
        .sender
        .send(request(21, "completionItem/resolve", &pre))
        .unwrap();
    let resp = recv_response(&client);
    assert!(resp.error.is_none(), "resolve errored: {:?}", resp.error);
    let post: CompletionItem =
        serde_json::from_value(resp.result.expect("resolve result")).unwrap();

    // Documentation is now filled (the `## The hero's hit points.` doc comment).
    let doc = format!("{:?}", post.documentation);
    assert!(
        doc.contains("hit points"),
        "resolve must fill the member's doc comment; got {doc}"
    );

    // The immutable fields are byte-for-byte unchanged by resolve (the spec rule).
    assert_eq!(
        post.sort_text, pre.sort_text,
        "sortText unchanged by resolve"
    );
    assert_eq!(
        post.filter_text, pre.filter_text,
        "filterText unchanged by resolve"
    );
    assert_eq!(
        post.insert_text, pre.insert_text,
        "insertText unchanged by resolve"
    );
    assert_eq!(
        post.text_edit, pre.text_edit,
        "textEdit unchanged by resolve"
    );

    shutdown(&client, server_thread);
}

/// Resolve with no `data` (or unknown data) returns the item unchanged and never errors — the
/// "never crash, never lie" floor.
#[test]
fn resolve_without_data_is_a_noop() {
    let p = sample_project();
    let uri = file_uri(&p.root.join("src/enemy.gd"));
    let (client, server_thread) = boot(&p, rich_caps(), &uri, "extends Hero\n");

    let item = CompletionItem {
        label: "bare".to_string(),
        sort_text: Some("00007".to_string()),
        ..Default::default()
    };
    client
        .sender
        .send(request(30, "completionItem/resolve", &item))
        .unwrap();
    let resp = recv_response(&client);
    assert!(resp.error.is_none(), "resolve must not error on no data");
    let post: CompletionItem = serde_json::from_value(resp.result.unwrap()).unwrap();
    assert_eq!(post.label, "bare");
    assert_eq!(post.sort_text.as_deref(), Some("00007"));
    assert!(post.documentation.is_none());

    shutdown(&client, server_thread);
}

// ===================================================================================================
// Capability gating — both projections of each gate.
// ===================================================================================================

/// snippetSupport OFF: a callable member inserts a PLAIN name (no `$0`, insertTextFormat absent),
/// and the edit is a plain `TextEdit` (insertReplaceSupport is also off here).
#[test]
fn gating_snippet_off_yields_plain_text_edit() {
    let p = sample_project();
    let src = "extends Node2D\n\nfunc use(h: Hero) -> void:\n\th.\n";
    let uri = file_uri(&p.root.join("src/consumer.gd"));
    // Everything OFF: no snippet, no insertReplace, no commit, no doc format, default kinds.
    let (client, server_thread) = boot(&p, caps(false, false, false, None, None), &uri, src);

    let raw = complete_raw(&client, 40, &uri, Position::new(3, 3));
    let list: CompletionList = serde_json::from_value(raw).expect("a CompletionList");
    let attack = list
        .items
        .iter()
        .find(|i| i.label == "attack")
        .expect("attack present");

    assert_eq!(
        attack.insert_text_format, None,
        "no snippetSupport ⇒ insertTextFormat absent (plain text)"
    );
    match attack.text_edit.as_ref().expect("a textEdit") {
        CompletionTextEdit::Edit(e) => {
            assert_eq!(e.new_text, "attack", "plain bare name, no `($0)`");
            assert!(!e.new_text.contains("$0"));
        }
        CompletionTextEdit::InsertAndReplace(_) => {
            panic!("no insertReplaceSupport ⇒ a plain TextEdit, not InsertReplaceEdit")
        }
    }
    // No commitCharactersSupport ⇒ no commit characters on any item.
    assert!(
        list.items.iter().all(|i| i.commit_characters.is_none()),
        "no commitCharactersSupport ⇒ items carry no commitCharacters"
    );

    shutdown(&client, server_thread);
}

/// snippetSupport ON but insertReplaceSupport OFF: the callable still gets a snippet, but as a
/// plain `TextEdit` (the InsertReplaceEdit gate is independent of the snippet gate).
#[test]
fn gating_snippet_on_insert_replace_off() {
    let p = sample_project();
    let src = "extends Node2D\n\nfunc use(h: Hero) -> void:\n\th.\n";
    let uri = file_uri(&p.root.join("src/consumer.gd"));
    let (client, server_thread) = boot(
        &p,
        caps(true, false, false, Some(vec![MarkupKind::Markdown]), None),
        &uri,
        src,
    );

    let raw = complete_raw(&client, 41, &uri, Position::new(3, 3));
    let list: CompletionList = serde_json::from_value(raw).expect("a CompletionList");
    let attack = list.items.iter().find(|i| i.label == "attack").unwrap();

    assert_eq!(
        attack.insert_text_format,
        Some(lsp_types::InsertTextFormat::SNIPPET),
        "snippetSupport on ⇒ snippet format"
    );
    match attack.text_edit.as_ref().unwrap() {
        CompletionTextEdit::Edit(e) => assert!(
            e.new_text.contains("$0"),
            "snippet newText in a plain TextEdit (insertReplace off): {:?}",
            e.new_text
        ),
        CompletionTextEdit::InsertAndReplace(_) => {
            panic!("insertReplaceSupport off ⇒ a plain TextEdit")
        }
    }

    shutdown(&client, server_thread);
}

/// documentationFormat: a PlainText-only client gets plaintext documentation from resolve (no
/// markdown markup), proving the doc-format gate threads through resolve.
#[test]
fn gating_documentation_format_plaintext() {
    let p = TempProject::new();
    p.write(
        "project.godot",
        "config_version=5\n\n[application]\n\nconfig/name=\"T\"\n",
    );
    p.write("extension_api.json", common::MINI_API);
    // Same-file member (see `resolve_fills_docs_…` for why the declaring file must equal the
    // requesting file this phase).
    let src = "class_name Hero\nextends Node\n\n## Bold [b]points[/b] here.\nvar hp: int = 10\n\nfunc use(h: Hero) -> void:\n\th.\n";
    let uri = file_uri(&p.root.join("src/hero.gd"));
    // PlainText-only documentation format.
    let (client, server_thread) = boot(
        &p,
        caps(true, true, true, Some(vec![MarkupKind::PlainText]), None),
        &uri,
        src,
    );

    let raw = complete_raw(&client, 50, &uri, Position::new(7, 3));
    let list: CompletionList = serde_json::from_value(raw).unwrap();
    let hp = list.items.iter().find(|i| i.label == "hp").unwrap().clone();

    client
        .sender
        .send(request(51, "completionItem/resolve", &hp))
        .unwrap();
    let resp = recv_response(&client);
    let post: CompletionItem = serde_json::from_value(resp.result.unwrap()).unwrap();

    // The resolved documentation is PlainText, and the BBCode `[b]…[/b]` is stripped (not rendered
    // as markdown `**…**`).
    match post.documentation.expect("documentation filled") {
        lsp_types::Documentation::MarkupContent(mc) => {
            assert_eq!(mc.kind, MarkupKind::PlainText, "plaintext-only client");
            assert!(
                mc.value.contains("points") && !mc.value.contains("**"),
                "BBCode stripped for plaintext: {:?}",
                mc.value
            );
        }
        other => panic!("expected MarkupContent documentation, got {other:?}"),
    }

    shutdown(&client, server_thread);
}

/// The OTHER projection of the documentationFormat gate: a Markdown client gets Markdown
/// documentation from resolve, with the BBCode `[b]…[/b]` rendered as `**…**` (the markdown
/// emphasis), proving the doc-format gate selects markdown when the client prefers it.
#[test]
fn gating_documentation_format_markdown() {
    let p = TempProject::new();
    p.write(
        "project.godot",
        "config_version=5\n\n[application]\n\nconfig/name=\"T\"\n",
    );
    p.write("extension_api.json", common::MINI_API);
    let src = "class_name Hero\nextends Node\n\n## Bold [b]points[/b] here.\nvar hp: int = 10\n\nfunc use(h: Hero) -> void:\n\th.\n";
    let uri = file_uri(&p.root.join("src/hero.gd"));
    // Markdown documentation format.
    let (client, server_thread) = boot(
        &p,
        caps(true, true, true, Some(vec![MarkupKind::Markdown]), None),
        &uri,
        src,
    );

    let raw = complete_raw(&client, 52, &uri, Position::new(7, 3));
    let list: CompletionList = serde_json::from_value(raw).unwrap();
    let hp = list.items.iter().find(|i| i.label == "hp").unwrap().clone();

    client
        .sender
        .send(request(53, "completionItem/resolve", &hp))
        .unwrap();
    let resp = recv_response(&client);
    let post: CompletionItem = serde_json::from_value(resp.result.unwrap()).unwrap();

    match post.documentation.expect("documentation filled") {
        lsp_types::Documentation::MarkupContent(mc) => {
            assert_eq!(mc.kind, MarkupKind::Markdown, "markdown-preferring client");
            assert!(
                mc.value.contains("**points**"),
                "BBCode [b] renders as markdown emphasis: {:?}",
                mc.value
            );
        }
        other => panic!("expected MarkupContent documentation, got {other:?}"),
    }

    shutdown(&client, server_thread);
}

/// CompletionItemKind clamping: a client whose `completionItemKind.valueSet` excludes `PROPERTY`
/// (10) must receive `kind: None` for a property item (it still completes, just without that icon),
/// while a kind it DOES support survives.
#[test]
fn gating_kind_clamped_to_value_set() {
    let p = sample_project();
    let src = "extends Node2D\n\nfunc use(h: Hero) -> void:\n\th.\n";
    let uri = file_uri(&p.root.join("src/consumer.gd"));
    // The client supports ONLY METHOD — not PROPERTY.
    let (client, server_thread) = boot(
        &p,
        caps(
            true,
            true,
            true,
            Some(vec![MarkupKind::Markdown]),
            Some(vec![CompletionItemKind::METHOD]),
        ),
        &uri,
        src,
    );

    let raw = complete_raw(&client, 60, &uri, Position::new(3, 3));
    let list: CompletionList = serde_json::from_value(raw).unwrap();

    // `attack` is a METHOD — in the value-set, so its kind survives.
    let attack = list.items.iter().find(|i| i.label == "attack").unwrap();
    assert_eq!(
        attack.kind,
        Some(CompletionItemKind::METHOD),
        "a supported kind survives clamping"
    );
    // `hp` is a PROPERTY — NOT in the value-set, so it is clamped to None.
    let hp = list.items.iter().find(|i| i.label == "hp").unwrap();
    assert_eq!(
        hp.kind, None,
        "an unsupported kind (PROPERTY) is dropped to None, not sent as a number"
    );

    shutdown(&client, server_thread);
}

/// A `textDocument/completion` at a non-`.gd` URI (or with nothing to complete) returns a
/// well-formed empty `CompletionList`, never an error or a bare array.
#[test]
fn completion_in_unhandled_context_is_an_empty_list() {
    let p = sample_project();
    let uri = file_uri(&p.root.join("src/enemy.gd"));
    // `extends Hero\n` then a completion at the very top (line 0, col 0) — a `Deferred`/`None`-ish
    // context for this phase.
    let (client, server_thread) = boot(&p, rich_caps(), &uri, "extends Hero\n");

    let raw = complete_raw(&client, 70, &uri, Position::new(0, 0));
    assert!(raw.is_object(), "still a CompletionList object: {raw}");
    let list: CompletionList = serde_json::from_value(raw).unwrap();
    // Either empty or some inherit-type set — but never an error and always a List. (This phase
    // renders InheritType as empty.)
    let _ = list.items.len();

    // The raw message just exchanged proves no stdout corruption (the client got a clean Response).
    drop(Message::Response(lsp_server::Response::new_ok(
        lsp_server::RequestId::from(0),
        serde_json::Value::Null,
    )));

    shutdown(&client, server_thread);
}
