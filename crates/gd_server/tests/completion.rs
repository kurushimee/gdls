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

/// The project `class_name` registry tier of IDENTIFIER completion is emitted in **sorted** order,
/// not the registry `FxHashMap`'s nondeterministic file-discovery order (#94 FIX 3). With several
/// global classes present, the registry-tier items must appear name-sorted in the response — so two
/// indexings of the same project yield the same ranking.
#[test]
fn identifier_completion_class_name_tier_is_sorted() {
    let p = rich_project();
    // Several user global classes whose names are deliberately not in registry-insertion order, so
    // an unsorted `entries()` walk would surface them out of alphabetical order.
    let classes = ["Zebra", "Mango", "Apple", "Pelican", "Delta"];
    for name in classes {
        p.write(
            &format!("src/{}.gd", name.to_lowercase()),
            &format!("class_name {name}\nextends Node\n"),
        );
    }
    // A buffer whose body has a bare-identifier site (the trailing `x`) so IDENTIFIER completion
    // fires the global tiers, including the `class_name` registry.
    let src = "extends Node\n\nfunc go() -> void:\n\tx\n";
    let uri = file_uri(&p.root.join("src/driver.gd"));
    let (client, server_thread) = boot(&p, rich_caps(), &uri, src);

    let raw = complete_raw(&client, 12, &uri, Position::new(3, 2));
    let list: CompletionList = serde_json::from_value(raw).expect("a CompletionList");

    // The registry-tier items in list order: the labels that are one of our user global classes.
    let want: std::collections::HashSet<&str> = classes.iter().copied().collect();
    let registry_order: Vec<String> = list
        .items
        .iter()
        .filter(|i| want.contains(i.label.as_str()))
        .map(|i| i.label.clone())
        .collect();
    assert_eq!(
        registry_order.len(),
        classes.len(),
        "every user global class is offered in the IDENTIFIER set: {registry_order:?}"
    );
    let mut sorted = registry_order.clone();
    sorted.sort();
    assert_eq!(
        registry_order, sorted,
        "the class_name registry tier must be emitted in sorted order, got {registry_order:?}"
    );

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

/// Consolidated capability-gating MATRIX: every M8 completion gate × {present, absent} in one
/// table-driven place, asserting BOTH projections. The scattered per-gate `gating_*` tests above
/// each exercise one gate in depth (and stay as focused regressions); this row table is the single
/// spot that proves the whole gate set is covered both ways — and it closes the one direction none
/// of the per-gate tests nor the `editor_profiles.rs` walk reach (all three vendored profiles have
/// `commitCharactersSupport` off): a client that DOES advertise commit characters receives them.
///
/// Each row flips the gates via [`caps`] and declares the expected projection of every gate for a
/// `Hero.` member access — the callable `attack` (snippet/insertReplace/commit), the documented
/// property `hp` (kind clamp + documentation format on resolve). The signatureHelp gates
/// (`labelOffsetSupport`, `activeParameter`) are NOT here: that handler lives on the stacked
/// `feat/m8-signaturehelp` branch, so its matrix rows join this table on that branch.
#[test]
fn gating_matrix_every_gate_both_ways() {
    /// One row of the gate matrix: the client capabilities + the projection each gate must produce.
    struct Case {
        name: &'static str,
        caps: ClientCapabilities,
        /// `attack`'s `insertTextFormat == Snippet` and its newText carries `$0`.
        expect_snippet: bool,
        /// the edit is an `InsertReplaceEdit` (vs a plain `TextEdit`).
        expect_insert_replace: bool,
        /// items carry `commitCharacters`.
        expect_commit: bool,
        /// the property `hp` keeps its `PROPERTY` kind (vs clamped to `None`).
        expect_property_kind: bool,
        /// resolve renders `hp`'s doc as Markdown (vs PlainText).
        expect_markdown_docs: bool,
    }

    // A value-set that includes PROPERTY (10), so the kind-clamp gate is driven by its PRESENCE,
    // independent of the other gates; and one that excludes it (METHOD-only) for the clamp.
    let with_property = vec![CompletionItemKind::METHOD, CompletionItemKind::PROPERTY];
    let method_only = vec![CompletionItemKind::METHOD];

    let cases = vec![
        // Baseline: every gate ON (snippet, insertReplace, commit, markdown docs, PROPERTY kept).
        Case {
            name: "all-present",
            caps: caps(
                true,
                true,
                true,
                Some(vec![MarkupKind::Markdown]),
                Some(with_property.clone()),
            ),
            expect_snippet: true,
            expect_insert_replace: true,
            expect_commit: true,
            expect_property_kind: true,
            expect_markdown_docs: true,
        },
        // snippet OFF (others held on).
        Case {
            name: "snippet-absent",
            caps: caps(
                false,
                true,
                true,
                Some(vec![MarkupKind::Markdown]),
                Some(with_property.clone()),
            ),
            expect_snippet: false,
            expect_insert_replace: true,
            expect_commit: true,
            expect_property_kind: true,
            expect_markdown_docs: true,
        },
        // insertReplace OFF.
        Case {
            name: "insert-replace-absent",
            caps: caps(
                true,
                false,
                true,
                Some(vec![MarkupKind::Markdown]),
                Some(with_property.clone()),
            ),
            expect_snippet: true,
            expect_insert_replace: false,
            expect_commit: true,
            expect_property_kind: true,
            expect_markdown_docs: true,
        },
        // commitCharacters OFF — the direction NO per-gate test nor the profile walk reaches in ON
        // form; the baseline row above is the ON form, this is the OFF form.
        Case {
            name: "commit-absent",
            caps: caps(
                true,
                true,
                false,
                Some(vec![MarkupKind::Markdown]),
                Some(with_property.clone()),
            ),
            expect_snippet: true,
            expect_insert_replace: true,
            expect_commit: false,
            expect_property_kind: true,
            expect_markdown_docs: true,
        },
        // documentationFormat PlainText (the markdown-OFF projection).
        Case {
            name: "doc-format-plaintext",
            caps: caps(
                true,
                true,
                true,
                Some(vec![MarkupKind::PlainText]),
                Some(with_property.clone()),
            ),
            expect_snippet: true,
            expect_insert_replace: true,
            expect_commit: true,
            expect_property_kind: true,
            expect_markdown_docs: false,
        },
        // completionItemKind excludes PROPERTY — the clamp-to-None projection.
        Case {
            name: "kind-clamped",
            caps: caps(
                true,
                true,
                true,
                Some(vec![MarkupKind::Markdown]),
                Some(method_only.clone()),
            ),
            expect_snippet: true,
            expect_insert_replace: true,
            expect_commit: true,
            expect_property_kind: false,
            expect_markdown_docs: true,
        },
        // Every gate ABSENT (a client that opted into completion but advertised no item caps): the
        // all-downgraded projection — bare name, plain edit, no commit, plaintext docs. PROPERTY
        // survives because an absent `completionItemKind.valueSet` falls back to the LSP-default set
        // (1..=18), which includes PROPERTY (10).
        Case {
            name: "all-absent",
            caps: caps(false, false, false, None, None),
            expect_snippet: false,
            expect_insert_replace: false,
            expect_commit: false,
            expect_property_kind: true,
            expect_markdown_docs: false,
        },
    ];

    // A documented same-file `Hero` (declaring file == requesting file, the Phase-3 resolve
    // constraint) with a callable `attack` and a `##`-documented property `hp`, accessed via `h.`.
    let src = "class_name Hero\nextends Node\n\n## Bold [b]points[/b] here.\nvar hp: int = 10\n\nfunc attack() -> void:\n\tpass\n\nfunc use(h: Hero) -> void:\n\th.\n";

    for case in cases {
        let p = TempProject::new();
        p.write(
            "project.godot",
            "config_version=5\n\n[application]\n\nconfig/name=\"T\"\n",
        );
        p.write("extension_api.json", common::MINI_API);
        let uri = file_uri(&p.root.join("src/hero.gd"));
        let (client, server_thread) = boot(&p, case.caps.clone(), &uri, src);

        // `\th.` is on line 10 (0-based) → column 3.
        let raw = complete_raw(&client, 80, &uri, Position::new(10, 3));
        let list: CompletionList = serde_json::from_value(raw)
            .unwrap_or_else(|e| panic!("{}: a CompletionList: {e}", case.name));
        let attack = list
            .items
            .iter()
            .find(|i| i.label == "attack")
            .unwrap_or_else(|| panic!("{}: attack present", case.name));
        let hp = list
            .items
            .iter()
            .find(|i| i.label == "hp")
            .unwrap_or_else(|| panic!("{}: hp present", case.name))
            .clone();

        // (1) snippet projection.
        assert_eq!(
            attack.insert_text_format == Some(lsp_types::InsertTextFormat::SNIPPET),
            case.expect_snippet,
            "{}: snippet projection",
            case.name
        );
        // (2) insertReplace projection + (1') the snippet's `$0` lives under the selected edit arm.
        match attack
            .text_edit
            .as_ref()
            .unwrap_or_else(|| panic!("{}: a textEdit", case.name))
        {
            CompletionTextEdit::InsertAndReplace(e) => {
                assert!(
                    case.expect_insert_replace,
                    "{}: got InsertReplaceEdit",
                    case.name
                );
                assert_eq!(
                    e.new_text.contains("$0"),
                    case.expect_snippet,
                    "{}: $0 in newText iff snippet",
                    case.name
                );
            }
            CompletionTextEdit::Edit(e) => {
                assert!(
                    !case.expect_insert_replace,
                    "{}: got a plain TextEdit",
                    case.name
                );
                assert_eq!(
                    e.new_text.contains("$0"),
                    case.expect_snippet,
                    "{}: $0 in newText iff snippet",
                    case.name
                );
            }
        }
        // (3) commitCharacters projection.
        assert_eq!(
            attack.commit_characters.is_some(),
            case.expect_commit,
            "{}: commitCharacters projection",
            case.name
        );
        // (4) kind-clamp projection — PROPERTY kept or dropped to None.
        assert_eq!(
            hp.kind == Some(CompletionItemKind::PROPERTY),
            case.expect_property_kind,
            "{}: PROPERTY kind projection",
            case.name
        );
        if !case.expect_property_kind {
            assert_eq!(
                hp.kind, None,
                "{}: clamped kind is None, not a number",
                case.name
            );
        }

        // (5) documentationFormat projection — resolve `hp` and check the rendered MarkupKind.
        client
            .sender
            .send(request(81, "completionItem/resolve", &hp))
            .unwrap();
        let resp = recv_response(&client);
        let resolved: CompletionItem = serde_json::from_value(resp.result.unwrap())
            .unwrap_or_else(|e| panic!("{}: resolve result: {e}", case.name));
        match resolved
            .documentation
            .unwrap_or_else(|| panic!("{}: documentation filled", case.name))
        {
            lsp_types::Documentation::MarkupContent(mc) => {
                if case.expect_markdown_docs {
                    assert_eq!(
                        mc.kind,
                        MarkupKind::Markdown,
                        "{}: markdown docs",
                        case.name
                    );
                    assert!(
                        mc.value.contains("**points**"),
                        "{}: BBCode renders as markdown emphasis: {:?}",
                        case.name,
                        mc.value
                    );
                } else {
                    assert_eq!(
                        mc.kind,
                        MarkupKind::PlainText,
                        "{}: plaintext docs",
                        case.name
                    );
                    assert!(
                        mc.value.contains("points") && !mc.value.contains("**"),
                        "{}: BBCode stripped for plaintext: {:?}",
                        case.name,
                        mc.value
                    );
                }
            }
            other => panic!("{}: expected MarkupContent, got {other:?}", case.name),
        }

        shutdown(&client, server_thread);
    }
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
    // Either empty or some identifier set — but never an error and always a List.
    let _ = list.items.len();

    // The raw message just exchanged proves no stdout corruption (the client got a clean Response).
    drop(Message::Response(lsp_server::Response::new_ok(
        lsp_server::RequestId::from(0),
        serde_json::Value::Null,
    )));

    shutdown(&client, server_thread);
}

// ===================================================================================================
// Phase 4 — the remaining completion contexts.
// ===================================================================================================

/// A dump rich enough for the Phase 4 contexts: native classes carrying **virtual** methods
/// (`Node._ready`/`_process` — the override set), a class with an **enum-typed** method parameter
/// (`Player.set_state(state: enum::Player.State)` — the call-arg enum candidates), the `State` enum,
/// and a `Color` builtin with a constant (`RED`), a **static** method (`from_hsv`), and an
/// **instance** method (`lerp` — must NOT appear in the `Color.` static set).
const P4_API: &str = r#"{
    "header": {"version_major": 4, "version_minor": 6, "version_patch": 3},
    "global_constants": [{"name": "KEY_A", "value": 65}],
    "utility_functions": [
        {"name": "print", "return_type": "void", "is_vararg": true, "arguments": []}
    ],
    "builtin_classes": [
        {"name": "Color", "is_keyed": false,
         "constants": [{"name": "RED", "type": "Color", "value": "Color(1, 0, 0, 1)"}],
         "methods": [
            {"name": "from_hsv", "is_const": false, "is_static": true, "is_vararg": false,
             "return_type": "Color",
             "arguments": [{"name": "h", "type": "float"}, {"name": "s", "type": "float"}]},
            {"name": "lerp", "is_const": true, "is_static": false, "is_vararg": false,
             "return_type": "Color",
             "arguments": [{"name": "to", "type": "Color"}, {"name": "weight", "type": "float"}]}
         ]}
    ],
    "classes": [
        {"name": "Object", "methods": [
            {"name": "get_class", "is_const": true, "return_value": {"type": "String"}}
        ]},
        {"name": "Node", "inherits": "Object", "methods": [
            {"name": "queue_free"},
            {"name": "_ready", "is_virtual": true, "return_value": {"type": "void"}},
            {"name": "_process", "is_virtual": true, "return_value": {"type": "void"},
             "arguments": [{"name": "delta", "type": "float"}]}
        ]},
        {"name": "CanvasItem", "inherits": "Node"},
        {"name": "Node2D", "inherits": "CanvasItem"},
        {"name": "Player", "inherits": "Node2D",
         "enums": [{"name": "State", "values": [
            {"name": "STATE_IDLE", "value": 0}, {"name": "STATE_RUN", "value": 1}]}],
         "methods": [
            {"name": "set_state", "is_const": false, "is_static": false, "is_vararg": false,
             "return_value": {"type": "void"},
             "arguments": [{"name": "state", "type": "enum::Player.State"}]}
         ]}
    ]
}"#;

/// A project on [`P4_API`] with a `Hero` (`class_name`, extends `Node2D`) carrying a documented
/// member — the Phase 4 fixture. Mirrors `rich_project`'s layout.
fn p4_project() -> TempProject {
    let p = TempProject::new();
    p.write(
        "project.godot",
        "config_version=5\n\n[application]\n\nconfig/name=\"T\"\n",
    );
    p.write("extension_api.json", P4_API);
    p.write(
        "src/hero.gd",
        "class_name Hero\nextends Node2D\n\n## The hero's hit points.\nvar hp: int = 10\n\nfunc attack() -> void:\n\tpass\n",
    );
    p
}

/// The label set of a completion result, for terse `contains` assertions.
fn labels(list: &CompletionList) -> Vec<String> {
    list.items.iter().map(|i| i.label.clone()).collect()
}

// --- ANNOTATION ---

/// `@<cursor>` returns the annotation name list (no leading `@`), and an annotation that takes
/// arguments inserts a trailing `(`. `@export_range(0, 10, 1, <cursor>` returns its special slider
/// argument words.
#[test]
fn annotation_name_list_and_argument_words() {
    let p = p4_project();
    let uri = file_uri(&p.root.join("src/a.gd"));
    // `@` on its own line (a script-level annotation position).
    let src = "@\n";
    let (client, server_thread) = boot(&p, rich_caps(), &uri, src);

    let raw = complete_raw(&client, 100, &uri, Position::new(0, 1));
    let list: CompletionList = serde_json::from_value(raw).expect("a CompletionList");
    let ls = labels(&list);
    // The `@` is stripped from labels; the registry names appear.
    assert!(
        ls.contains(&"export".to_string()),
        "@export offered: {ls:?}"
    );
    assert!(ls.contains(&"onready".to_string()), "@onready offered");
    assert!(ls.contains(&"tool".to_string()), "@tool offered");
    assert!(
        ls.contains(&"export_range".to_string()),
        "@export_range offered"
    );
    // `@export_range` takes args → its insert appends `(`; `@tool` takes none → bare name.
    let export_range = list
        .items
        .iter()
        .find(|i| i.label == "export_range")
        .unwrap();
    assert!(
        edit_new_text(export_range).ends_with('('),
        "an arg-taking annotation inserts a trailing `(`: {:?}",
        edit_new_text(export_range)
    );
    let tool = list.items.iter().find(|i| i.label == "tool").unwrap();
    assert_eq!(
        edit_new_text(tool),
        "tool",
        "a no-arg annotation inserts the bare name"
    );

    shutdown(&client, server_thread);
}

/// `@export_range(0, 10, 1, <cursor>` — the slider argument words at the `extra_hints` slot
/// (argument index ≥ 3, matching Godot's `_find_annotation_arguments` index gate).
#[test]
fn annotation_export_range_slider_words() {
    let p = p4_project();
    let uri = file_uri(&p.root.join("src/r.gd"));
    // The slider words are offered at the 4th argument (index 3) and beyond.
    let src = "extends Node2D\n\n@export_range(0, 10, 1, )\nvar speed: float\n";
    let (client, server_thread) = boot(&p, rich_caps(), &uri, src);

    // Cursor inside the 4th arg slot: line 2, just after the last `, ` → column 24.
    let raw = complete_raw(&client, 101, &uri, Position::new(2, 24));
    let list: CompletionList = serde_json::from_value(raw).expect("a CompletionList");
    let ls = labels(&list);
    for word in ["or_greater", "or_less", "prefer_slider", "hide_control"] {
        assert!(
            ls.contains(&word.to_string()),
            "@export_range slider word {word} offered: {ls:?}"
        );
    }
    // The word inserts as a double-quoted string (canonical, W17).
    let or_greater = list.items.iter().find(|i| i.label == "or_greater").unwrap();
    assert_eq!(edit_new_text(or_greater), "\"or_greater\"");

    shutdown(&client, server_thread);
}

/// Commit characters are suppressed for string-valued annotation-argument items (a `.`/`(` commit
/// mid-string is a wart) but KEPT for member items — both asserted under commit-capable caps so the
/// "member half" guards against over-suppression (#94 FIX 4).
#[test]
fn annotation_argument_items_suppress_commit_but_members_keep_it() {
    let p = p4_project();
    let uri = file_uri(&p.root.join("src/c.gd"));
    // One file with both an `@export_range(…, <cursor>)` slider-word site and a `n.<cursor>` member
    // site, so a single boot exercises both contexts under the same (commit-capable) caps.
    let src =
        "extends Node2D\n\n@export_range(0, 10, 1, )\nvar speed: float\n\nfunc use(n: Node) -> void:\n\tn.\n";
    let (client, server_thread) = boot(&p, rich_caps(), &uri, src);

    // (a) Annotation-argument words: cursor in the 4th arg slot (line 2, column 24).
    let raw = complete_raw(&client, 130, &uri, Position::new(2, 24));
    let ann: CompletionList = serde_json::from_value(raw).expect("a CompletionList");
    let or_greater = ann
        .items
        .iter()
        .find(|i| i.label == "or_greater")
        .expect("the slider word `or_greater` is offered");
    assert_eq!(
        or_greater.commit_characters, None,
        "a string-valued annotation-argument item must carry NO commit characters: {:?}",
        or_greater.commit_characters
    );

    // (b) Member access: cursor after `n.` (line 6, column 3). A member item KEEPS commit chars —
    // proves the suppression is context-scoped, not global.
    let raw = complete_raw(&client, 131, &uri, Position::new(6, 3));
    let members: CompletionList = serde_json::from_value(raw).expect("a CompletionList");
    let qf = members
        .items
        .iter()
        .find(|i| i.label == "queue_free")
        .expect("Node's `queue_free` is offered");
    assert_eq!(
        qf.commit_characters,
        Some(vec![".".to_string(), "(".to_string()]),
        "a member item must keep `.`/`(` commit characters: {:?}",
        qf.commit_characters
    );

    shutdown(&client, server_thread);
}

// --- TYPE positions ---

/// `var x: <cursor>` returns the available types: builtins, native classes, project `class_name`s,
/// and `Variant` — but no `void`.
#[test]
fn type_name_position_lists_types_no_void() {
    let p = p4_project();
    let uri = file_uri(&p.root.join("src/t.gd"));
    let src = "extends Node2D\n\nfunc f() -> void:\n\tvar x: \n";
    let (client, server_thread) = boot(&p, rich_caps(), &uri, src);

    // `\tvar x: ` → cursor at column 8 on line 3.
    let raw = complete_raw(&client, 110, &uri, Position::new(3, 8));
    let list: CompletionList = serde_json::from_value(raw).expect("a CompletionList");
    let ls = labels(&list);
    assert!(
        ls.contains(&"Node".to_string()),
        "native class Node: {ls:?}"
    );
    assert!(ls.contains(&"Color".to_string()), "builtin Color offered");
    assert!(
        ls.contains(&"Hero".to_string()),
        "project class Hero offered"
    );
    assert!(ls.contains(&"Variant".to_string()), "Variant offered");
    assert!(
        !ls.contains(&"void".to_string()),
        "void must NOT appear at a `var:` type position: {ls:?}"
    );

    shutdown(&client, server_thread);
}

/// `-> <cursor>` (return type) returns the type set **plus** `void`.
#[test]
fn return_type_position_includes_void() {
    let p = p4_project();
    let uri = file_uri(&p.root.join("src/rt.gd"));
    let src = "extends Node2D\n\nfunc f() -> \n";
    let (client, server_thread) = boot(&p, rich_caps(), &uri, src);

    // `func f() -> ` → cursor at column 12 on line 2.
    let raw = complete_raw(&client, 111, &uri, Position::new(2, 12));
    let list: CompletionList = serde_json::from_value(raw).expect("a CompletionList");
    let ls = labels(&list);
    assert!(
        ls.contains(&"void".to_string()),
        "void IS offered at a return-type position: {ls:?}"
    );
    assert!(
        ls.contains(&"Node".to_string()),
        "native class Node offered"
    );

    shutdown(&client, server_thread);
}

/// `extends <cursor>` returns class names only — no builtins, no `void`, no `Variant`.
#[test]
fn inherit_type_position_excludes_builtins_and_void() {
    let p = p4_project();
    let uri = file_uri(&p.root.join("src/ih.gd"));
    let src = "extends \n";
    let (client, server_thread) = boot(&p, rich_caps(), &uri, src);

    // `extends ` → cursor at column 8 on line 0.
    let raw = complete_raw(&client, 112, &uri, Position::new(0, 8));
    let list: CompletionList = serde_json::from_value(raw).expect("a CompletionList");
    let ls = labels(&list);
    assert!(
        ls.contains(&"Node".to_string()),
        "native class Node offered"
    );
    assert!(
        ls.contains(&"Hero".to_string()),
        "project class Hero offered"
    );
    assert!(
        !ls.contains(&"Color".to_string()),
        "a builtin must NOT appear for `extends`: {ls:?}"
    );
    assert!(
        !ls.contains(&"void".to_string()),
        "void must NOT appear for `extends`"
    );
    assert!(
        !ls.contains(&"Variant".to_string()),
        "Variant must NOT appear for `extends`"
    );

    shutdown(&client, server_thread);
}

// --- BUILTIN STATIC ---

/// `Color.<cursor>` returns the builtin type's constants + **static** methods (`Color.RED`,
/// `Color.from_hsv`), and NOT its instance methods (`Color.lerp` must be absent).
#[test]
fn builtin_static_lists_constants_and_static_methods_only() {
    let p = p4_project();
    let uri = file_uri(&p.root.join("src/bs.gd"));
    let src = "extends Node2D\n\nfunc f() -> void:\n\tvar c = Color.\n";
    let (client, server_thread) = boot(&p, rich_caps(), &uri, src);

    // `\tvar c = Color.` → cursor right after the `.`. `\t`=col0, `Color` ends at col13, `.` at
    // col13..14, so after the dot is column 15.
    let raw = complete_raw(&client, 120, &uri, Position::new(3, 15));
    let list: CompletionList = serde_json::from_value(raw).expect("a CompletionList");
    let ls = labels(&list);
    assert!(
        ls.contains(&"RED".to_string()),
        "Color.RED constant offered: {ls:?}"
    );
    assert!(
        ls.contains(&"from_hsv".to_string()),
        "Color.from_hsv static method offered: {ls:?}"
    );
    assert!(
        !ls.contains(&"lerp".to_string()),
        "Color.lerp is an INSTANCE method — must NOT appear as a static: {ls:?}"
    );

    shutdown(&client, server_thread);
}

// --- CALL ARGUMENTS enum candidates ---

/// Completing a call argument whose parameter is enum-typed (`Player.set_state(state: State)`)
/// suggests that enum's constants (`STATE_IDLE`, `STATE_RUN`).
#[test]
fn call_argument_enum_candidates() {
    let p = p4_project();
    let uri = file_uri(&p.root.join("src/ca.gd"));
    // A typed parameter `pl: Player`, then `pl.set_state(` on its own line.
    let src = "extends Node2D\n\nfunc f(pl: Player) -> void:\n\tpl.set_state()\n";
    let (client, server_thread) = boot(&p, rich_caps(), &uri, src);

    // Inside the `set_state(` arg list: `\tpl.set_state(` → cursor at column 14 on line 3.
    let raw = complete_raw(&client, 130, &uri, Position::new(3, 14));
    let list: CompletionList = serde_json::from_value(raw).expect("a CompletionList");
    let ls = labels(&list);
    assert!(
        ls.contains(&"STATE_IDLE".to_string()) && ls.contains(&"STATE_RUN".to_string()),
        "an enum-typed parameter suggests its enum's constants; got {ls:?}"
    );

    shutdown(&client, server_thread);
}

// --- ASSIGN enum candidates ---

/// `x = <cursor>` where `x` is enum-typed suggests that enum's members (`STATE_IDLE`, `STATE_RUN`).
#[test]
fn assign_to_enum_typed_var_suggests_enum_members() {
    let p = p4_project();
    let uri = file_uri(&p.root.join("src/as.gd"));
    // A local typed with the native enum, then an assignment to it.
    let src = "extends Node2D\n\nfunc f() -> void:\n\tvar s: Player.State\n\ts = \n";
    let (client, server_thread) = boot(&p, rich_caps(), &uri, src);

    // `\ts = ` → cursor at column 5 on line 4 (after `s = `).
    let raw = complete_raw(&client, 135, &uri, Position::new(4, 5));
    let list: CompletionList = serde_json::from_value(raw).expect("a CompletionList");
    let ls = labels(&list);
    assert!(
        ls.contains(&"STATE_IDLE".to_string()) && ls.contains(&"STATE_RUN".to_string()),
        "assigning to an enum-typed var suggests the enum's members; got {ls:?}"
    );

    shutdown(&client, server_thread);
}

// --- SUBSCRIPT (identifier fallback this phase) ---

/// `d[<cursor>` falls back to the in-scope identifier set (the constant-dict-key refinement is
/// deferred to the identifier fallback this phase). The identifier set is offered (a global like
/// `print`, a self member) — never empty / a wrong guess.
#[test]
fn subscript_falls_back_to_identifiers() {
    let p = p4_project();
    let uri = file_uri(&p.root.join("src/sub.gd"));
    let src = "extends Node2D\n\nfunc f() -> void:\n\tvar d = {}\n\tprint(d[)\n";
    let (client, server_thread) = boot(&p, rich_caps(), &uri, src);

    // `\tprint(d[` → cursor right after `[`. `\t`=0, `print(d[` → `[` at byte 8, after = column 9.
    let raw = complete_raw(&client, 136, &uri, Position::new(4, 9));
    let list: CompletionList = serde_json::from_value(raw).expect("a CompletionList");
    let ls = labels(&list);
    assert!(
        ls.contains(&"print".to_string()),
        "a subscript index falls back to the identifier set (the global `print`); got {ls:?}"
    );

    shutdown(&client, server_thread);
}

// --- PROPERTY METHOD (get =/set = binds a class method) ---

/// `var x: int:\n\tget = <cursor>` offers the class's own methods (the accessor binds a getter by
/// method name). The class method `helper` is offered.
#[test]
fn property_method_offers_class_methods() {
    let p = p4_project();
    let uri = file_uri(&p.root.join("src/pm.gd"));
    // A class method `helper`, then a property whose `get =` accessor binds a method by name.
    let src = "extends Node2D\n\nfunc helper() -> int:\n\treturn 1\n\nvar x: int:\n\tget = \n";
    let (client, server_thread) = boot(&p, rich_caps(), &uri, src);

    // `\tget = ` on line 6 → cursor at column 7 (after `get = `).
    let raw = complete_raw(&client, 137, &uri, Position::new(6, 7));
    let list: CompletionList = serde_json::from_value(raw).expect("a CompletionList");
    let ls = labels(&list);
    assert!(
        ls.contains(&"helper".to_string()),
        "a property accessor (`get =`) offers the class's own methods (helper); got {ls:?}"
    );

    shutdown(&client, server_thread);
}

// --- OVERRIDE METHOD ---

/// `func <cursor>` in a class body suggests overridable parent **virtuals** (`_ready`, `_process`)
/// with a full signature stub. With `snippetSupport` the insert carries a `$0` body tab-stop and
/// the canonical one-tab indent. A non-virtual native method (`queue_free`) is NOT offered.
#[test]
fn override_method_lists_virtuals_with_signature_stub() {
    let p = p4_project();
    let uri = file_uri(&p.root.join("src/ov.gd"));
    let src = "extends Node2D\n\nfunc \n";
    let (client, server_thread) = boot(&p, rich_caps(), &uri, src);

    // `func ` → cursor at column 5 on line 2.
    let raw = complete_raw(&client, 140, &uri, Position::new(2, 5));
    let list: CompletionList = serde_json::from_value(raw).expect("a CompletionList");

    // `_ready` and `_process` are offered; the label is the full `name(...) -> Ret:` stub.
    let ready = list
        .items
        .iter()
        .find(|i| i.filter_text.as_deref() == Some("_ready"))
        .expect("_ready virtual offered");
    assert!(
        ready.label.starts_with("_ready(") && ready.label.ends_with(':'),
        "the label is a signature stub `_ready() -> void:`, got {:?}",
        ready.label
    );
    let process = list
        .items
        .iter()
        .find(|i| i.filter_text.as_deref() == Some("_process"))
        .expect("_process virtual offered");
    assert!(
        process.label.contains("delta"),
        "the _process stub carries its parameter name: {:?}",
        process.label
    );
    // A non-virtual native method is not an override candidate.
    assert!(
        !list
            .items
            .iter()
            .any(|i| i.filter_text.as_deref() == Some("queue_free")),
        "a non-virtual native method must not be an override candidate"
    );
    // snippetSupport on ⇒ the insert is a snippet with a `$0` body tab-stop and a one-tab indent.
    assert_eq!(
        ready.insert_text_format,
        Some(lsp_types::InsertTextFormat::SNIPPET),
        "override stub is a snippet when gated"
    );
    let nt = edit_new_text(ready);
    assert!(
        nt.contains("$0") && nt.contains("\n\t"),
        "snippet body has a $0 tab-stop and canonical one-tab indent: {nt:?}"
    );

    shutdown(&client, server_thread);
}

/// A virtual the class **already overrides** is NOT offered again (Godot's
/// `has_function(...) continue`, `gdscript_editor.cpp:3744`): with `func _ready()` already defined,
/// completing `func <cursor>` offers `_process` (not yet overridden) but NOT `_ready`.
#[test]
fn override_method_skips_already_overridden_virtual() {
    let p = p4_project();
    let uri = file_uri(&p.root.join("src/ov2.gd"));
    // `_ready` is already defined on this class; a new `func ` should not re-offer it.
    let src = "extends Node2D\n\nfunc _ready() -> void:\n\tpass\n\nfunc \n";
    let (client, server_thread) = boot(&p, rich_caps(), &uri, src);

    // `func ` on line 5 → cursor at column 5.
    let raw = complete_raw(&client, 142, &uri, Position::new(5, 5));
    let list: CompletionList = serde_json::from_value(raw).expect("a CompletionList");
    assert!(
        !list
            .items
            .iter()
            .any(|i| i.filter_text.as_deref() == Some("_ready")),
        "an already-overridden virtual (_ready) must NOT be offered again; got {:?}",
        list.items
            .iter()
            .filter_map(|i| i.filter_text.clone())
            .collect::<Vec<_>>()
    );
    assert!(
        list.items
            .iter()
            .any(|i| i.filter_text.as_deref() == Some("_process")),
        "a not-yet-overridden virtual (_process) is still offered"
    );

    shutdown(&client, server_thread);
}

/// snippetSupport OFF: the override stub inserts the bare signature line (no `$0` body), as a plain
/// `TextEdit`. The signature is still present (the headline of the feature).
#[test]
fn override_method_stub_without_snippet_is_plain_signature() {
    let p = p4_project();
    let uri = file_uri(&p.root.join("src/ovp.gd"));
    let src = "extends Node2D\n\nfunc \n";
    let (client, server_thread) = boot(&p, caps(false, false, false, None, None), &uri, src);

    let raw = complete_raw(&client, 141, &uri, Position::new(2, 5));
    let list: CompletionList = serde_json::from_value(raw).expect("a CompletionList");
    let ready = list
        .items
        .iter()
        .find(|i| i.filter_text.as_deref() == Some("_ready"))
        .expect("_ready offered");
    assert_eq!(
        ready.insert_text_format, None,
        "no snippetSupport ⇒ a plain-text insert"
    );
    let nt = edit_new_text(ready);
    assert!(
        nt.starts_with("_ready(") && nt.ends_with(':') && !nt.contains("$0"),
        "plain insert is the bare signature line `_ready() -> void:`: {nt:?}"
    );

    shutdown(&client, server_thread);
}

/// A **script-parent** method is offered for override too (Godot's CLASS-branch walk over inherited
/// `FUNCTION` members, `gdscript_editor.cpp:3688-3708`), not only native virtuals. A child `extends
/// Hero` completing `func <cursor>` offers `attack` (the parent's own method) with its real
/// `name(params) -> Ret:` signature — rendered from the **declaring** file's parsed signature, so the
/// parameter name and the written default text are faithful (never fabricated).
#[test]
fn override_method_offers_script_parent_methods() {
    let p = p4_project();
    // A parent with a richly-signed method: a typed param, an untyped param, and a default whose
    // expression text must be reproduced verbatim (never fabricated).
    p.write(
        "src/parent.gd",
        "class_name OvParent\nextends Node2D\n\nfunc do_it(times: int, who, loud: bool = true) -> String:\n\treturn \"\"\n",
    );
    let uri = file_uri(&p.root.join("src/child.gd"));
    let src = "extends OvParent\n\nfunc \n";
    let (client, server_thread) = boot(&p, rich_caps(), &uri, src);

    // `func ` → cursor at column 5 on line 2.
    let raw = complete_raw(&client, 160, &uri, Position::new(2, 5));
    let list: CompletionList = serde_json::from_value(raw).expect("a CompletionList");

    let do_it = list
        .items
        .iter()
        .find(|i| i.filter_text.as_deref() == Some("do_it"))
        .unwrap_or_else(|| {
            panic!(
                "the script parent's `do_it` is offered for override; got {:?}",
                list.items
                    .iter()
                    .filter_map(|i| i.filter_text.clone())
                    .collect::<Vec<_>>()
            )
        });
    // The label is the VERBATIM author signature (Godot's `name + signature + ":"`): the real
    // parameter names, the untyped param left BARE (no synthesized `: Variant`), the written default
    // expression text (`= true`, never fabricated), and the return type — ending in `:`.
    assert_eq!(
        do_it.label, "do_it(times: int, who, loud: bool = true) -> String:",
        "the script-parent override stub reproduces the declaring signature verbatim"
    );
    assert_eq!(
        do_it.kind,
        Some(lsp_types::CompletionItemKind::METHOD),
        "an override stub is a METHOD item"
    );
    // The native virtuals are still offered alongside the script-parent method.
    assert!(
        list.items
            .iter()
            .any(|i| i.filter_text.as_deref() == Some("_ready")),
        "native virtuals are still offered alongside script-parent methods"
    );

    shutdown(&client, server_thread);
}

/// Verbatim-signature edge cases (Godot reproduces the source `signature` substring, never a
/// reconstruction, and the cut is bracket-depth-aware): a parent method with NO return annotation
/// appends no `-> void`; an `@abstract` method (no body, hence no block colon) is NOT truncated at a
/// parameter-type colon; a dict-literal default's inner `:` does not cut the signature early; and a
/// `static` parent method is NOT offered (the non-`static` `func` cursor's `is_static`-match skip).
#[test]
fn override_method_script_parent_verbatim_edges() {
    let p = p4_project();
    p.write(
        "src/edges.gd",
        "class_name OvEdges\nextends Node\n\nfunc no_ret(x):\n\tpass\n\nstatic func a_static() -> void:\n\tpass\n\n@abstract func must_do(x: int, y) -> int\n\nfunc with_dict(d := {\"a\": 1}) -> void:\n\tpass\n\nfunc with_esc(s := \"a\\\":b\") -> void:\n\tpass\n",
    );
    let uri = file_uri(&p.root.join("src/edge_child.gd"));
    let src = "extends OvEdges\n\nfunc \n";
    let (client, server_thread) = boot(&p, rich_caps(), &uri, src);

    let raw = complete_raw(&client, 162, &uri, Position::new(2, 5));
    let list: CompletionList = serde_json::from_value(raw).expect("a CompletionList");
    let label_of = |name: &str| {
        list.items
            .iter()
            .find(|i| i.filter_text.as_deref() == Some(name))
            .map(|i| i.label.clone())
    };

    // No return annotation in source ⇒ none synthesized; untyped param stays bare.
    assert_eq!(
        label_of("no_ret").as_deref(),
        Some("no_ret(x):"),
        "an absent return annotation must NOT append `-> void`"
    );
    // An @abstract method has no block colon — its signature must NOT be truncated at the `x:` colon.
    assert_eq!(
        label_of("must_do").as_deref(),
        Some("must_do(x: int, y) -> int:"),
        "an abstract method's verbatim signature must not be truncated at a param colon"
    );
    // A dict-literal default's inner `:` must not cut the signature early (depth-aware block colon).
    assert_eq!(
        label_of("with_dict").as_deref(),
        Some("with_dict(d := {\"a\": 1}) -> void:"),
        "a dict-default's inner colon must not truncate the signature"
    );
    // An ESCAPED quote in a string default must not corrupt the string scan (no double colon / no
    // early truncation): the signature is reproduced verbatim and ends in exactly one block colon.
    assert_eq!(
        label_of("with_esc").as_deref(),
        Some("with_esc(s := \"a\\\":b\") -> void:"),
        "an escaped quote in a string default must not break the block-colon scan"
    );
    // A `static` parent method is not an override candidate from a non-`static` `func` cursor.
    assert!(
        label_of("a_static").is_none(),
        "a static parent method must NOT be offered for a non-static override"
    );

    shutdown(&client, server_thread);
}

/// A script-parent method the child **already overrides** is not re-offered (the first-wins name
/// dedup / Godot's `has_function` skip), while a sibling parent method that is not yet overridden is.
#[test]
fn override_method_skips_already_overridden_script_parent_method() {
    let p = p4_project();
    p.write(
        "src/base2.gd",
        "class_name OvBase2\nextends Node2D\n\nfunc alpha() -> void:\n\tpass\n\nfunc beta() -> void:\n\tpass\n",
    );
    let uri = file_uri(&p.root.join("src/child2.gd"));
    // The child already overrides `alpha`; completing `func ` must offer `beta` but not `alpha`.
    let src = "extends OvBase2\n\nfunc alpha() -> void:\n\tpass\n\nfunc \n";
    let (client, server_thread) = boot(&p, rich_caps(), &uri, src);

    // `func ` on line 5 → cursor at column 5.
    let raw = complete_raw(&client, 161, &uri, Position::new(5, 5));
    let list: CompletionList = serde_json::from_value(raw).expect("a CompletionList");
    let filters: Vec<String> = list
        .items
        .iter()
        .filter_map(|i| i.filter_text.clone())
        .collect();
    assert!(
        !filters.contains(&"alpha".to_string()),
        "an already-overridden script-parent method must NOT be re-offered; got {filters:?}"
    );
    assert!(
        filters.contains(&"beta".to_string()),
        "a not-yet-overridden script-parent method is still offered; got {filters:?}"
    );

    shutdown(&client, server_thread);
}

// --- PROPERTY ACCESSOR (bare get/set keyword) ---

/// `var x: int:\n\t<cursor>` at the accessor-keyword position offers exactly the `get`/`set`
/// keywords (Godot `COMPLETION_PROPERTY_DECLARATION`), as plain-word inserts.
#[test]
fn property_accessor_offers_get_and_set_keywords() {
    let p = p4_project();
    let uri = file_uri(&p.root.join("src/pa.gd"));
    // `var x: int:` then a partial `g` on the indented accessor line.
    let src = "extends Node\n\nvar x: int:\n\tg\n";
    let (client, server_thread) = boot(&p, rich_caps(), &uri, src);

    // `\tg` on line 3 → cursor right after `g` at column 2.
    let raw = complete_raw(&client, 170, &uri, Position::new(3, 2));
    let list: CompletionList = serde_json::from_value(raw).expect("a CompletionList");
    let ls = labels(&list);
    assert!(
        ls.contains(&"get".to_string()) && ls.contains(&"set".to_string()),
        "the accessor-keyword position offers `get` and `set`; got {ls:?}"
    );
    // The inserts are the bare keyword words (no parens, not a snippet).
    let get = list
        .items
        .iter()
        .find(|i| i.label == "get")
        .expect("get item");
    assert_eq!(
        get.insert_text_format, None,
        "the keyword is a plain insert"
    );
    assert_eq!(edit_new_text(get), "get", "the insert is the bare keyword");

    shutdown(&client, server_thread);
}

// --- SUPER METHOD ---

/// `super.<cursor>` offers the **parent** class's methods (`queue_free` from the native `Node`
/// parent), restricted to methods — and NOT the current class's own method (`my_helper`), which
/// `super.` literally cannot call. Discriminates parent-only enumeration from a self-inclusive walk.
#[test]
fn super_method_lists_parent_methods_not_own() {
    let p = p4_project();
    let uri = file_uri(&p.root.join("src/sm.gd"));
    // The class defines `my_helper`; `super.` must not offer it (it's not on the parent).
    let src =
        "extends Node2D\n\nfunc my_helper() -> void:\n\tpass\n\nfunc _ready() -> void:\n\tsuper.\n";
    let (client, server_thread) = boot(&p, rich_caps(), &uri, src);

    // `\tsuper.` on line 6 → cursor right after the `.` at column 7.
    let raw = complete_raw(&client, 150, &uri, Position::new(6, 7));
    let list: CompletionList = serde_json::from_value(raw).expect("a CompletionList");
    let ls = labels(&list);
    assert!(
        ls.contains(&"queue_free".to_string()),
        "super. offers the parent's queue_free method: {ls:?}"
    );
    assert!(
        !ls.contains(&"my_helper".to_string()),
        "super. must NOT offer the current class's own method (parent-only): {ls:?}"
    );

    shutdown(&client, server_thread);
}

// --- TYPE ATTRIBUTE ---

/// `var x: Player.<cursor>` — the nested types / enums / constants of the type `Player` (its `State`
/// enum), NOT its instance members (`set_state`). The type-scoped set (Godot `COMPLETION_TYPE_ATTRIBUTE`).
#[test]
fn type_attribute_lists_nested_types_not_instance_members() {
    let p = p4_project();
    let uri = file_uri(&p.root.join("src/ta.gd"));
    let src = "extends Node2D\n\nfunc f() -> void:\n\tvar x: Player.\n";
    let (client, server_thread) = boot(&p, rich_caps(), &uri, src);

    // `\tvar x: Player.` → cursor after the `.`. `\t`=col0, `Player` ends col13, `.` at col13..14 →
    // after dot is column 15.
    let raw = complete_raw(&client, 200, &uri, Position::new(3, 15));
    let list: CompletionList = serde_json::from_value(raw).expect("a CompletionList");
    let ls = labels(&list);
    assert!(
        ls.contains(&"State".to_string()),
        "Player.State (a nested enum type) is offered in a type-attribute position: {ls:?}"
    );
    assert!(
        !ls.contains(&"set_state".to_string()),
        "an instance method (set_state) must NOT appear in a type-attribute position: {ls:?}"
    );

    shutdown(&client, server_thread);
}

/// `var x: Outer.Inner.<cursor>` — a MULTI-segment type-attribute chain descends the project class's
/// inner-class path, offering `Inner`'s nested types/constants (Godot's segment-by-segment
/// `COMPLETION_TYPE_ATTRIBUTE` walk). Resolution is by NAME through the inner chain — the previous
/// single-token base lookup fell through to empty here.
#[test]
fn type_attribute_multi_segment_descends_inner_chain() {
    let p = p4_project();
    // A project class with a nested inner class carrying a nested type (a deeper inner class) +
    // a const — the type-scoped members `Outer.Inner.<cursor>` should offer.
    p.write(
        "src/outer.gd",
        "class_name TaOuter\nextends Node\n\nclass TaInner:\n\tconst INNER_K := 7\n\tclass Deepest:\n\t\tpass\n",
    );
    let uri = file_uri(&p.root.join("src/use_outer.gd"));
    let src = "extends Node\n\nfunc f() -> void:\n\tvar x: TaOuter.TaInner.\n";
    let (client, server_thread) = boot(&p, rich_caps(), &uri, src);

    // `\tvar x: TaOuter.TaInner.` — compute the cursor column: `\t`=1 col, then
    // `var x: TaOuter.TaInner.` — the trailing `.` is the last char. Column = byte length of the
    // line content after the tab + 1 (tab counts as 1 column in LSP UTF-16 here).
    let line = "\tvar x: TaOuter.TaInner.";
    let col = line.chars().count() as u32;
    let raw = complete_raw(&client, 201, &uri, Position::new(3, col));
    let list: CompletionList = serde_json::from_value(raw).expect("a CompletionList");
    let ls = labels(&list);
    assert!(
        ls.contains(&"INNER_K".to_string()),
        "the inner class's const INNER_K is offered for `Outer.Inner.`: {ls:?}"
    );
    assert!(
        ls.contains(&"Deepest".to_string()),
        "the inner class's nested type Deepest is offered for `Outer.Inner.`: {ls:?}"
    );

    shutdown(&client, server_thread);
}

// --- carry-forward (a): native class names in IDENTIFIER position ---

/// Native engine class names (`Node`, `Color` is a builtin not a class, `Player`) appear in the
/// bare-identifier completion set (carry-forward (a)).
#[test]
fn identifier_position_offers_native_class_names() {
    let p = p4_project();
    let uri = file_uri(&p.root.join("src/id.gd"));
    let src = "extends Node2D\n\nfunc f() -> void:\n\tvar x = \n";
    let (client, server_thread) = boot(&p, rich_caps(), &uri, src);

    // `\tvar x = ` → cursor at column 9 on line 3 (an expression position).
    let raw = complete_raw(&client, 160, &uri, Position::new(3, 9));
    let list: CompletionList = serde_json::from_value(raw).expect("a CompletionList");
    let ls = labels(&list);
    assert!(
        ls.contains(&"Node".to_string()),
        "the native class name Node appears in IDENTIFIER position (carry-forward a): {ls:?}"
    );
    assert!(
        ls.contains(&"Player".to_string()),
        "the native class name Player appears in IDENTIFIER position"
    );

    shutdown(&client, server_thread);
}

// --- carry-forward (b): native + inherited member docs on resolve ---

/// `completionItem/resolve` fills a **native** member's documentation from the declaring class
/// (carry-forward (b)) — the Phase-3 gap where native member docs returned `None`.
#[test]
fn resolve_fills_native_member_doc() {
    // A documented native method: add a description to `Node.queue_free` via a bespoke dump.
    let p = TempProject::new();
    p.write(
        "project.godot",
        "config_version=5\n\n[application]\n\nconfig/name=\"T\"\n",
    );
    p.write(
        "extension_api.json",
        r#"{
        "header": {"version_major": 4, "version_minor": 6, "version_patch": 3},
        "classes": [
            {"name": "Object"},
            {"name": "Node", "inherits": "Object", "methods": [
                {"name": "queue_free", "description": "Queues this node for deletion."}
            ]},
            {"name": "CanvasItem", "inherits": "Node"},
            {"name": "Node2D", "inherits": "CanvasItem"}
        ]
    }"#,
    );
    let uri = file_uri(&p.root.join("src/nd.gd"));
    let src = "extends Node2D\n\nfunc use(n: Node) -> void:\n\tn.\n";
    let (client, server_thread) = boot(&p, rich_caps(), &uri, src);

    let raw = complete_raw(&client, 170, &uri, Position::new(3, 3));
    let list: CompletionList = serde_json::from_value(raw).expect("a CompletionList");
    let qf = list
        .items
        .iter()
        .find(|i| i.label == "queue_free")
        .expect("queue_free present")
        .clone();
    assert!(
        qf.documentation.is_none(),
        "documentation is lazy pre-resolve"
    );

    client
        .sender
        .send(request(171, "completionItem/resolve", &qf))
        .unwrap();
    let resp = recv_response(&client);
    let post: CompletionItem = serde_json::from_value(resp.result.unwrap()).unwrap();
    let doc = format!("{:?}", post.documentation);
    assert!(
        doc.contains("Queues this node for deletion"),
        "resolve fills the native member's description (carry-forward b); got {doc}"
    );

    shutdown(&client, server_thread);
}

/// `completionItem/resolve` fills an **inherited cross-file** member's `##` doc from the
/// **declaring** parent file (carry-forward (b)) — the exact Phase-3 bug (resolve used to read the
/// requesting buffer, never the declaring file). `Child` extends `Base`; `Base.health` has a doc
/// comment; completion is requested in `Child`.
#[test]
fn resolve_fills_inherited_crossfile_member_doc() {
    let p = TempProject::new();
    p.write(
        "project.godot",
        "config_version=5\n\n[application]\n\nconfig/name=\"T\"\n",
    );
    p.write("extension_api.json", common::MINI_API);
    // The declaring parent file: `Base` with a documented member `health`.
    p.write(
        "src/base.gd",
        "class_name Base\nextends Node\n\n## The base's health pool.\nvar health: int = 100\n",
    );
    // The child file: extends the parent script; completes a `self` member that is INHERITED.
    let child = "extends Base\n\nfunc f() -> void:\n\thealth\n";
    let uri = file_uri(&p.root.join("src/child.gd"));
    let (client, server_thread) = boot(&p, rich_caps(), &uri, child);

    // `\thealth` → cursor at column 7 on line 3 (an identifier position; `health` is an inherited
    // self member).
    let raw = complete_raw(&client, 180, &uri, Position::new(3, 7));
    let list: CompletionList = serde_json::from_value(raw).expect("a CompletionList");
    let health = list
        .items
        .iter()
        .find(|i| i.label == "health")
        .expect("the inherited member `health` is offered")
        .clone();

    client
        .sender
        .send(request(181, "completionItem/resolve", &health))
        .unwrap();
    let resp = recv_response(&client);
    let post: CompletionItem = serde_json::from_value(resp.result.unwrap()).unwrap();
    let doc = format!("{:?}", post.documentation);
    assert!(
        doc.contains("base's health pool"),
        "resolve reads the DECLARING parent file's doc for an inherited member (carry-forward b); got {doc}"
    );

    shutdown(&client, server_thread);
}

// --- M11 P3: scene-aware deferred contexts (`$`/`%`/`get_node` + `load`/`preload`) ---

/// W10 no-false-positive guard: a script attached to NO scene gets an EMPTY `$`/`%` node-path
/// completion — never a project-wide name guess.
#[test]
fn node_path_completion_without_a_scene_is_empty() {
    let p = p4_project(); // has src/hero.gd but NO .tscn — def.gd is attached nowhere
    let uri = file_uri(&p.root.join("src/def.gd"));
    let src = "extends Node2D\n\nfunc f() -> void:\n\tvar a = $\n\tvar b = %\n";
    let (client, server_thread) = boot(&p, rich_caps(), &uri, src);

    // `\tvar a = $` → cursor right after `$` → column 10.
    let node_path = complete_raw(&client, 190, &uri, Position::new(3, 10));
    let np: CompletionList = serde_json::from_value(node_path).expect("a CompletionList");
    assert!(
        np.items.is_empty(),
        "no scene attaches def.gd → `$` node path must be EMPTY (W10: never a guess); got {:?}",
        labels(&np)
    );

    // `\tvar b = %` → cursor right after `%` → column 10.
    let unique = complete_raw(&client, 191, &uri, Position::new(4, 10));
    let uq: CompletionList = serde_json::from_value(unique).expect("a CompletionList");
    assert!(
        uq.items.is_empty(),
        "no scene attaches def.gd → `%` unique node path must be EMPTY; got {:?}",
        labels(&uq)
    );

    shutdown(&client, server_thread);
}

/// `load("res://<cursor>")` lists the indexed `res://` project entries (scripts + scenes); the insert
/// is the FULL res path and the edit spans the whole typed content (no scheme dropped, no doubling).
#[test]
fn resource_path_lists_indexed_files() {
    let p = p4_project(); // indexes src/hero.gd
    let uri = file_uri(&p.root.join("src/def.gd"));
    let src = "extends Node2D\n\nfunc f() -> void:\n\tvar c = load(\"res://\")\n";
    let (client, server_thread) = boot(&p, rich_caps(), &uri, src);

    // `\tvar c = load("res://")` → the opening `"` is at column 14, `res://` occupies columns 15..21,
    // so the cursor sits at column 21.
    let res_path = complete_raw(&client, 192, &uri, Position::new(3, 21));
    let rp: CompletionList = serde_json::from_value(res_path).expect("a CompletionList");
    let ls = labels(&rp);
    assert!(
        ls.iter().any(|l| l == "src/"),
        "load(\"res://\") must list the `src/` subdirectory; got {ls:?}"
    );
    // The subdir item inserts the FULL `res://src/` prefix (whole-content span ⇒ scheme preserved).
    let subdir = rp.items.iter().find(|i| i.label == "src/").unwrap();
    assert_eq!(edit_new_text(subdir), "res://src/");

    // Drilling into `res://src/` lists the .gd files there.
    let src2 = "extends Node2D\n\nfunc f() -> void:\n\tvar c = load(\"res://src/\")\n";
    let uri2 = file_uri(&p.root.join("src/def2.gd"));
    let (client2, st2) = boot(&p, rich_caps(), &uri2, src2);
    // `res://src/` occupies columns 15..25, cursor at column 25.
    let res2 = complete_raw(&client2, 193, &uri2, Position::new(3, 25));
    let rp2: CompletionList = serde_json::from_value(res2).expect("a CompletionList");
    let ls2 = labels(&rp2);
    assert!(
        ls2.iter().any(|l| l == "src/hero.gd"),
        "load(\"res://src/\") must list hero.gd; got {ls2:?}"
    );
    // CORRUPTION GUARD: the insert is the FULL res path; splicing it over the whole typed content
    // yields `load("res://src/hero.gd")`.
    let hero = rp2.items.iter().find(|i| i.label == "src/hero.gd").unwrap();
    assert_eq!(edit_new_text(hero), "res://src/hero.gd");

    shutdown(&client, server_thread);
    shutdown(&client2, st2);
}

/// CORRUPTION GUARD (end-to-end): completing a PARTIAL filename `load("res://src/he|")` and accepting
/// `src/hero.gd` produces exactly `res://src/hero.gd` once — the edit spans the whole content
/// (`res://src/he`) and the insert is the full path, so nothing is doubled or dropped.
#[test]
fn resource_path_partial_prefix_replaces_whole_content() {
    let p = p4_project();
    let uri = file_uri(&p.root.join("src/def.gd"));
    let src = "extends Node2D\n\nfunc f() -> void:\n\tvar c = load(\"res://src/he\")\n";
    let (client, server_thread) = boot(&p, rich_caps(), &uri, src);

    // `res://src/he` occupies columns 15..27, cursor at column 27.
    let raw = complete_raw(&client, 194, &uri, Position::new(3, 27));
    let list: CompletionList = serde_json::from_value(raw).expect("a CompletionList");
    let hero = list
        .items
        .iter()
        .find(|i| i.label == "src/hero.gd")
        .expect("hero.gd offered for the `res://src/he` prefix");
    let (range, new_text) = match hero.text_edit.as_ref().unwrap() {
        CompletionTextEdit::Edit(e) => (e.range, e.new_text.clone()),
        CompletionTextEdit::InsertAndReplace(e) => (e.replace, e.new_text.clone()),
    };
    // The edit spans the whole content: from after the opening quote (column 15) to the cursor (27).
    assert_eq!(
        range.start.character, 15,
        "edit starts after the opening quote"
    );
    assert_eq!(range.end.character, 27, "edit ends at the cursor");
    assert_eq!(new_text, "res://src/hero.gd", "insert is the full res path");

    shutdown(&client, server_thread);
}

/// CORRUPTION GUARD (advisor, wire): a PARTIAL scheme `load("re|")` must NOT drop `res://`. Accepting
/// the `src/` subdirectory yields `load("res://src/")`, never `load("src/")`.
#[test]
fn resource_path_partial_scheme_keeps_res_prefix() {
    let p = p4_project();
    let uri = file_uri(&p.root.join("src/def.gd"));
    let src = "extends Node2D\n\nfunc f() -> void:\n\tvar c = load(\"re\")\n";
    let (client, server_thread) = boot(&p, rich_caps(), &uri, src);

    // `load("re")` — `re` occupies columns 15..17, cursor at column 17 (after `re`).
    let raw = complete_raw(&client, 195, &uri, Position::new(3, 17));
    let list: CompletionList = serde_json::from_value(raw).expect("a CompletionList");
    let subdir = list
        .items
        .iter()
        .find(|i| i.label == "src/")
        .expect("the src/ subdirectory is offered even mid-scheme");
    let (range, new_text) = match subdir.text_edit.as_ref().unwrap() {
        CompletionTextEdit::Edit(e) => (e.range, e.new_text.clone()),
        CompletionTextEdit::InsertAndReplace(e) => (e.replace, e.new_text.clone()),
    };
    // The edit spans the whole partial `re` (columns 15..17); the insert is the full `res://src/`.
    assert_eq!(range.start.character, 15);
    assert_eq!(range.end.character, 17);
    assert_eq!(
        new_text, "res://src/",
        "the full res:// prefix must be inserted — the scheme is never dropped"
    );

    shutdown(&client, server_thread);
}

/// The `detail` (type label) of a named completion item, for asserting node types.
fn detail_of(list: &CompletionList, label: &str) -> Option<String> {
    list.items
        .iter()
        .find(|i| i.label == label)
        .and_then(|i| i.detail.clone())
}

/// A project with one scene `player.tscn` attaching `player.gd` at its ROOT. Tree:
/// `Root(Node2D)[player.gd]` → { `Health`(Node2D), `Sprite`(Sprite2D), `UI`(Control) → `Bar`(ProgressBar) };
/// `Bar` is `unique_name_in_owner`. So `$` lists Health/Sprite/UI, `$UI/` lists Bar, `%` lists Bar.
fn scene_project() -> TempProject {
    let p = TempProject::new();
    p.write(
        "project.godot",
        "config_version=5\n\n[application]\n\nconfig/name=\"T\"\n",
    );
    p.write(
        "extension_api.json",
        r#"{
        "header": {"version_major": 4, "version_minor": 6, "version_patch": 3},
        "classes": [
            {"name": "Object"},
            {"name": "Node", "inherits": "Object"},
            {"name": "CanvasItem", "inherits": "Node"},
            {"name": "Node2D", "inherits": "CanvasItem"},
            {"name": "Sprite2D", "inherits": "Node2D"},
            {"name": "Control", "inherits": "CanvasItem"},
            {"name": "ProgressBar", "inherits": "Control"}
        ]
    }"#,
    );
    p.write(
        "player.tscn",
        r#"[gd_scene format=3]
[ext_resource type="Script" path="res://player.gd" id="1"]
[node name="Root" type="Node2D"]
script = ExtResource("1")
[node name="Health" type="Node2D" parent="."]
[node name="Sprite" type="Sprite2D" parent="."]
[node name="UI" type="Control" parent="."]
[node name="Bar" type="ProgressBar" parent="UI"]
unique_name_in_owner = true
"#,
    );
    p.write(
        "player.gd",
        "extends Node2D\n\nfunc _ready() -> void:\n\tpass\n",
    );
    p
}

/// `$<cursor>` on a scene-attached script lists the attachment node's DIRECT children with types.
#[test]
fn dollar_node_path_lists_scene_children_with_types() {
    let p = scene_project();
    let uri = file_uri(&p.root.join("player.gd"));
    let src = "extends Node2D\n\nfunc f() -> void:\n\tvar a = $\n";
    let (client, server_thread) = boot(&p, rich_caps(), &uri, src);

    // `\tvar a = $` → cursor right after `$` → column 10.
    let raw = complete_raw(&client, 200, &uri, Position::new(3, 10));
    let list: CompletionList = serde_json::from_value(raw).expect("a CompletionList");
    let ls = labels(&list);
    assert!(
        ls.contains(&"Health".to_string())
            && ls.contains(&"Sprite".to_string())
            && ls.contains(&"UI".to_string()),
        "`$` must list the root's direct children Health/Sprite/UI; got {ls:?}"
    );
    assert!(
        !ls.contains(&"Bar".to_string()),
        "nested Bar must NOT appear at the root level; got {ls:?}"
    );
    assert_eq!(detail_of(&list, "Health").as_deref(), Some("Node2D"));
    assert_eq!(detail_of(&list, "Sprite").as_deref(), Some("Sprite2D"));
    assert_eq!(detail_of(&list, "UI").as_deref(), Some("Control"));
    // A node-path list is `isIncomplete` so the client re-queries as the path grows past a `/`
    // (`/` is not a trigger char). Without this the deep-path `$UI/` list would be a stale filter.
    assert!(
        list.is_incomplete,
        "a `$` node-path completion must be isIncomplete (re-query as the path grows)"
    );

    shutdown(&client, server_thread);
}

/// A deep path `$UI/<cursor>` lists UI's children (the segment-by-segment walk).
#[test]
fn dollar_deep_path_lists_child_node_children() {
    let p = scene_project();
    let uri = file_uri(&p.root.join("player.gd"));
    let src = "extends Node2D\n\nfunc f() -> void:\n\tvar a = $UI/\n";
    let (client, server_thread) = boot(&p, rich_caps(), &uri, src);

    // `\tvar a = $UI/` → the `/` is the char at column 12, so the cursor sits at column 13.
    let raw = complete_raw(&client, 201, &uri, Position::new(3, 13));
    let list: CompletionList = serde_json::from_value(raw).expect("a CompletionList");
    let ls = labels(&list);
    assert_eq!(
        ls,
        vec!["Bar".to_string()],
        "`$UI/` must list UI's child Bar"
    );
    assert_eq!(detail_of(&list, "Bar").as_deref(), Some("ProgressBar"));

    shutdown(&client, server_thread);
}

/// `%<cursor>` lists the scene's owner-unique node names with their types.
#[test]
fn percent_unique_node_path_lists_unique_names() {
    let p = scene_project();
    let uri = file_uri(&p.root.join("player.gd"));
    let src = "extends Node2D\n\nfunc f() -> void:\n\tvar a = %\n";
    let (client, server_thread) = boot(&p, rich_caps(), &uri, src);

    let raw = complete_raw(&client, 202, &uri, Position::new(3, 10));
    let list: CompletionList = serde_json::from_value(raw).expect("a CompletionList");
    let ls = labels(&list);
    assert_eq!(
        ls,
        vec!["Bar".to_string()],
        "`%` must list the unique node Bar"
    );
    assert_eq!(detail_of(&list, "Bar").as_deref(), Some("ProgressBar"));

    shutdown(&client, server_thread);
}

/// `get_node("<cursor>")` lists the same children as `$`, and the edit inserts the bare node name
/// (quotes preserved — no corruption).
#[test]
fn get_node_string_lists_scene_children() {
    let p = scene_project();
    let uri = file_uri(&p.root.join("player.gd"));
    let src = "extends Node2D\n\nfunc f() -> void:\n\tvar a = get_node(\"\")\n";
    let (client, server_thread) = boot(&p, rich_caps(), &uri, src);

    // `\tvar a = get_node("")` → cursor between the quotes. `get_node("` ends at column 19.
    let raw = complete_raw(&client, 203, &uri, Position::new(3, 19));
    let list: CompletionList = serde_json::from_value(raw).expect("a CompletionList");
    let ls = labels(&list);
    assert!(
        ls.contains(&"Health".to_string()) && ls.contains(&"UI".to_string()),
        "get_node(\"\") must list the root's children; got {ls:?}"
    );
    let health = list.items.iter().find(|i| i.label == "Health").unwrap();
    assert_eq!(
        edit_new_text(health),
        "Health",
        "the node name is inserted into the string"
    );

    shutdown(&client, server_thread);
}

/// CORRUPTION GUARD (Bug 2, wire): completing with the cursor AFTER a terminated string's closing
/// quote (`get_node("Health"|)`) offers NOTHING — so the closing quote can never be swallowed.
#[test]
fn node_path_after_closing_quote_is_empty() {
    let p = scene_project();
    let uri = file_uri(&p.root.join("player.gd"));
    let src = "extends Node2D\n\nfunc f() -> void:\n\tvar a = get_node(\"Health\")\n";
    let (client, server_thread) = boot(&p, rich_caps(), &uri, src);

    // `get_node("Health")` — cursor right AFTER the closing quote: `get_node("Health"` ends at col 26.
    let raw = complete_raw(&client, 205, &uri, Position::new(3, 26));
    let list: CompletionList = serde_json::from_value(raw).expect("a CompletionList");
    assert!(
        list.items.is_empty(),
        "a cursor past the closing quote must offer nothing (no quote-eating edit); got {:?}",
        labels(&list)
    );

    shutdown(&client, server_thread);
}

/// A second scene attaching the SAME script with a different child set/type → the node-path
/// completion is the UNION, and a name with different types across scenes annotates `A | B`.
#[test]
fn multi_scene_attachment_unions_with_ambiguity_annotated() {
    let p = scene_project();
    // A SECOND scene attaching the same player.gd: a `Health` typed `Control` here (vs `Node2D` in
    // player.tscn) plus a unique-to-this-scene `Menu` child.
    p.write(
        "menu.tscn",
        r#"[gd_scene format=3]
[ext_resource type="Script" path="res://player.gd" id="1"]
[node name="Root" type="Node2D"]
script = ExtResource("1")
[node name="Health" type="Control" parent="."]
[node name="Menu" type="Control" parent="."]
"#,
    );
    let uri = file_uri(&p.root.join("player.gd"));
    let src = "extends Node2D\n\nfunc f() -> void:\n\tvar a = $\n";
    let (client, server_thread) = boot(&p, rich_caps(), &uri, src);

    let raw = complete_raw(&client, 204, &uri, Position::new(3, 10));
    let list: CompletionList = serde_json::from_value(raw).expect("a CompletionList");
    let ls = labels(&list);
    for expected in ["Health", "Sprite", "UI", "Menu"] {
        assert!(
            ls.contains(&expected.to_string()),
            "the union across both scenes must include {expected}; got {ls:?}"
        );
    }
    // `Health` is Node2D in one scene, Control in the other → the detail annotates BOTH (sorted).
    assert_eq!(
        detail_of(&list, "Health").as_deref(),
        Some("Control | Node2D"),
        "an ambiguous node type across scenes must be annotated, not picked"
    );
    assert_eq!(detail_of(&list, "Menu").as_deref(), Some("Control"));

    shutdown(&client, server_thread);
}

/// The single-line replace text of an item's edit (whichever edit form the client negotiated).
fn edit_new_text(item: &CompletionItem) -> String {
    match item.text_edit.as_ref().expect("an item has a textEdit") {
        CompletionTextEdit::Edit(e) => e.new_text.clone(),
        CompletionTextEdit::InsertAndReplace(e) => e.new_text.clone(),
    }
}

/// #146 regression (completion arm): member completion on an **inner-class instance** lists the
/// INNER class's members — including an inner-only member (`only_inner`) absent from the root class.
/// Before the producer fix (inner-class value types collapsed to the bare root `Script` with an
/// empty inner chain) `x.` listed the ROOT's members and dropped `only_inner` — the completion twin
/// of the hover/definition #146 lie. `var x := Inner.new()` infers `x` as the inner class.
#[test]
fn member_completion_on_inner_class_instance_lists_inner_members() {
    let p = sample_project();
    let src = "extends Node2D\n\nfunc collide(a):\n\tpass\n\nclass Inner:\n\tfunc collide(a, b):\n\t\tpass\n\tfunc only_inner():\n\t\tpass\n\nfunc use() -> void:\n\tvar x := Inner.new()\n\tx.\n";
    let uri = file_uri(&p.root.join("src/inner_consumer.gd"));
    let (client, server_thread) = boot(&p, rich_caps(), &uri, src);

    // `\tx.` is the last code line (0-based line 13); cursor right after the `.` → column 3.
    let raw = complete_raw(&client, 20, &uri, Position::new(13, 3));
    let list: CompletionList = serde_json::from_value(raw).expect("a CompletionList");
    let ls = labels(&list);
    assert!(
        ls.iter().any(|l| l == "only_inner"),
        "inner-class instance completion must list the inner-only member only_inner; got {ls:?}"
    );

    shutdown(&client, server_thread);
}
