//! The expression-position and class-meta completion surface: what `Class.<cursor>`, a bare
//! identifier, and a declaration's rendered type actually contain (#306, #307, #308, #318).
//!
//! These are wire-level acceptance tests, not unit tests of the item builders: every assertion is
//! made on the `CompletionList` / `Hover` / `DocumentSymbol` a Godot-unaware client receives, since
//! all four defects were invisible from inside the crate and only showed up against a live server.
//!
//! - `Inventory.<cursor>` offered the instance surface (every method, every property, every signal)
//!   and omitted `new` — the one member a class-meta completion exists to offer (#306).
//! - `Array[Entry]` and `Dictionary[String, int]` lost their element types at the declaration, in
//!   hover and in `documentSymbol` alike (#307).
//! - Expression position listed no builtin type names, so `Vector2` never completed (#308).
//! - …and none of Godot's fourteen keyword constants, so `PI`, `self`, and `return` never did
//!   either (#318).

mod common;

use common::{file_uri, notification, recv_response, request, TempProject};
use lsp_server::Connection;
use lsp_types::{
    ClientCapabilities, CompletionClientCapabilities, CompletionItem, CompletionItemCapability,
    CompletionItemKind, CompletionList, DidOpenTextDocumentParams, DocumentSymbolResponse, Hover,
    HoverContents, InitializeParams, InitializedParams, MarkupKind, Position,
    TextDocumentClientCapabilities, TextDocumentItem, Uri,
};

/// A dump with the builtin Variant types the expression-position tier is expected to surface, plus
/// enough of a class chain for a `class_name` script to extend.
const API: &str = r#"{
    "header": {"version_major": 4, "version_minor": 6, "version_patch": 3},
    "utility_functions": [
        {"name": "print", "return_type": "void", "is_vararg": true, "arguments": []}
    ],
    "builtin_classes": [
        {"name": "Vector2"},
        {"name": "Vector3"},
        {"name": "Transform2D"},
        {"name": "PackedStringArray"}
    ],
    "classes": [
        {"name": "Object", "is_instantiable": true, "methods": [
            {"name": "get_class", "is_const": true, "return_value": {"type": "String"}}
        ]},
        {"name": "Node", "inherits": "Object", "is_instantiable": true, "methods": [{"name": "queue_free"}]},
        {"name": "CanvasItem", "inherits": "Node", "is_instantiable": true},
        {"name": "Node2D", "inherits": "CanvasItem", "is_instantiable": true}
    ]
}"#;

/// `Inventory`: a `class_name` script carrying one of everything the meta/instance split turns on —
/// a constant, an inner class, a named enum, a static method, an instance method, a property, and a
/// signal — plus the two typed collections #307 was about.
const INVENTORY_GD: &str = "class_name Inventory\nextends Node\n\nsignal cleared\n\nconst MAX_SLOTS := 8\n\nenum Slot { WEAPON, ARMOR }\n\nclass Entry:\n\tvar name: String\n\nvar entries: Array[Entry] = []\nvar counts: Dictionary[String, int] = {}\n\nstatic func make() -> Inventory:\n\treturn Inventory.new()\n\nfunc add_item(n: String) -> void:\n\tprint(n)\n";

fn caps() -> ClientCapabilities {
    ClientCapabilities {
        text_document: Some(TextDocumentClientCapabilities {
            completion: Some(CompletionClientCapabilities {
                completion_item: Some(CompletionItemCapability {
                    snippet_support: Some(true),
                    documentation_format: Some(vec![MarkupKind::Markdown]),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            document_symbol: Some(lsp_types::DocumentSymbolClientCapabilities {
                hierarchical_document_symbol_support: Some(true),
                ..Default::default()
            }),
            ..Default::default()
        }),
        ..Default::default()
    }
}

fn project() -> TempProject {
    let p = TempProject::new();
    p.write(
        "project.godot",
        "config_version=5\n\n[application]\n\nconfig/name=\"T\"\n",
    );
    p.write("extension_api.json", API);
    p.write("src/inventory.gd", INVENTORY_GD);
    p
}

fn boot(
    project: &TempProject,
    uri: &Uri,
    text: &str,
) -> (Connection, std::thread::JoinHandle<anyhow::Result<()>>) {
    let options = serde_json::json!({
        "projectRoot": project.root.as_str(),
        "autoDumpExtensionApi": false,
        "extensionApiPath": project.root.join("extension_api.json").as_str(),
    });
    let (server, client) = Connection::memory();
    let server_thread = std::thread::spawn(move || gd_server::serve(server));
    client
        .sender
        .send(request(
            1,
            "initialize",
            InitializeParams {
                capabilities: caps(),
                initialization_options: Some(options),
                ..Default::default()
            },
        ))
        .unwrap();
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

fn complete(client: &Connection, id: i32, uri: &Uri, pos: Position) -> Vec<CompletionItem> {
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
    let list: CompletionList =
        serde_json::from_value(resp.result.expect("completion result")).expect("a CompletionList");
    list.items
}

fn labels(items: &[CompletionItem]) -> Vec<&str> {
    items.iter().map(|i| i.label.as_str()).collect()
}

// ===================================================================================================
// #306 — `Inventory.<cursor>` is the class-meta surface, not the instance surface.
// ===================================================================================================

/// A project class's meta type offers what you can actually write after the class name: the
/// constructor, constants, inner classes, named enums, and `static` functions. Instance methods,
/// instance properties, and signals are NOT reachable through the class name and must not appear —
/// offering them is how the completion turned into an 89-item list of things that all fail to
/// compile.
#[test]
fn class_meta_completion_offers_new_statics_and_types_only() {
    let p = project();
    let uri = file_uri(&p.root.join("src/use.gd"));
    let text = "extends Node\n\nfunc f() -> void:\n\tInventory.\n";
    let (client, server) = boot(&p, &uri, text);

    let items = complete(&client, 2, &uri, Position::new(3, 11));
    let got = labels(&items);
    for want in ["new", "MAX_SLOTS", "Slot", "Entry", "make"] {
        assert!(got.contains(&want), "missing {want:?} in {got:?}");
    }
    for unwanted in ["add_item", "entries", "counts", "cleared"] {
        assert!(
            !got.contains(&unwanted),
            "{unwanted:?} is not reachable through the class name, but was offered: {got:?}"
        );
    }
    shutdown(&client, server);
}

/// The synthesized `new` is a static constructor, and it is synthesized only when the chain does
/// not declare one — a script that writes its own `new` keeps its own.
#[test]
fn the_synthesized_constructor_does_not_shadow_a_declared_new() {
    let p = project();
    p.write(
        "src/own_new.gd",
        "class_name OwnNew\nextends Node\n\nstatic func new(a: int) -> OwnNew:\n\treturn null\n",
    );
    let uri = file_uri(&p.root.join("src/use.gd"));
    let text = "extends Node\n\nfunc f() -> void:\n\tOwnNew.\n";
    let (client, server) = boot(&p, &uri, text);

    let items = complete(&client, 2, &uri, Position::new(3, 8));
    let news: Vec<&CompletionItem> = items.iter().filter(|i| i.label == "new").collect();
    assert_eq!(news.len(), 1, "exactly one `new`: {:?}", labels(&items));
    assert_ne!(
        news[0].detail.as_deref(),
        Some("new() -> Object"),
        "the declared `new` wins over the synthesized one"
    );
    shutdown(&client, server);
}

/// The instance surface is unchanged — the meta filter must not leak into `inv.<cursor>`.
#[test]
fn the_instance_surface_still_offers_instance_members() {
    let p = project();
    let uri = file_uri(&p.root.join("src/use.gd"));
    let text = "extends Node\n\nfunc f() -> void:\n\tvar inv := Inventory.new()\n\tinv.\n";
    p.write("src/use.gd", text);
    let (client, server) = boot(&p, &uri, text);

    let items = complete(&client, 2, &uri, Position::new(4, 5));
    let got = labels(&items);
    for want in ["add_item", "entries", "counts", "cleared", "MAX_SLOTS"] {
        assert!(got.contains(&want), "missing {want:?} in {got:?}");
    }
    shutdown(&client, server);
}

// ===================================================================================================
// #307 — a typed collection keeps its element types at the declaration.
// ===================================================================================================

/// `var entries: Array[Entry]` hovers as `Array[Entry]`, not bare `Array`. The element type is
/// written right there in the source, so dropping it makes hover *less* informative than the line
/// the cursor is on.
#[test]
fn a_typed_collection_keeps_its_element_types_in_hover() {
    let p = project();
    let uri = file_uri(&p.root.join("src/inventory.gd"));
    let (client, server) = boot(&p, &uri, INVENTORY_GD);

    let entries_line = INVENTORY_GD
        .lines()
        .position(|l| l.starts_with("var entries"))
        .expect("the entries declaration") as u32;
    let counts_line = INVENTORY_GD
        .lines()
        .position(|l| l.starts_with("var counts"))
        .expect("the counts declaration") as u32;

    for (id, line, want) in [
        (2, entries_line, "Array[Entry]"),
        (3, counts_line, "Dictionary[String, int]"),
    ] {
        client
            .sender
            .send(request(
                id,
                "textDocument/hover",
                serde_json::json!({
                    "textDocument": { "uri": uri.as_str() },
                    "position": { "line": line, "character": 5 },
                }),
            ))
            .unwrap();
        let resp = recv_response(&client);
        assert!(resp.error.is_none(), "hover errored: {:?}", resp.error);
        let hover: Hover =
            serde_json::from_value(resp.result.expect("hover result")).expect("a Hover");
        let HoverContents::Markup(markup) = hover.contents else {
            panic!("hover is markup content");
        };
        assert!(
            markup.value.contains(want),
            "line {line} hover must render {want:?}: {}",
            markup.value
        );
    }
    shutdown(&client, server);
}

/// The same rendering feeds `documentSymbol`'s detail, so the outline agrees with the hover.
#[test]
fn a_typed_collection_keeps_its_element_types_in_document_symbol() {
    let p = project();
    let uri = file_uri(&p.root.join("src/inventory.gd"));
    let (client, server) = boot(&p, &uri, INVENTORY_GD);

    client
        .sender
        .send(request(
            2,
            "textDocument/documentSymbol",
            serde_json::json!({ "textDocument": { "uri": uri.as_str() } }),
        ))
        .unwrap();
    let resp = recv_response(&client);
    assert!(
        resp.error.is_none(),
        "documentSymbol errored: {:?}",
        resp.error
    );
    let symbols: DocumentSymbolResponse =
        serde_json::from_value(resp.result.expect("documentSymbol result")).expect("a response");
    let DocumentSymbolResponse::Nested(symbols) = symbols else {
        panic!("gdls returns the nested (DocumentSymbol) form");
    };
    // The outline nests members under the file's implicit class, so walk it.
    fn flatten(out: &mut Vec<(String, String)>, symbols: &[lsp_types::DocumentSymbol]) {
        for s in symbols {
            out.push((s.name.clone(), s.detail.clone().unwrap_or_default()));
            #[allow(
                deprecated,
                reason = "DocumentSymbol::children is not the deprecated field"
            )]
            if let Some(children) = &s.children {
                flatten(out, children);
            }
        }
    }
    let mut flat = Vec::new();
    flatten(&mut flat, &symbols);
    let detail_of = |name: &str| -> String {
        flat.iter()
            .find(|(n, _)| n == name)
            .unwrap_or_else(|| panic!("no `{name}` symbol in {flat:?}"))
            .1
            .clone()
    };
    assert!(
        detail_of("entries").contains("Array[Entry]"),
        "entries detail: {:?}",
        detail_of("entries")
    );
    assert!(
        detail_of("counts").contains("Dictionary[String, int]"),
        "counts detail: {:?}",
        detail_of("counts")
    );
    shutdown(&client, server);
}

// ===================================================================================================
// #308 / #318 — expression position carries builtin type names and Godot's keyword constants.
// ===================================================================================================

/// A bare identifier in expression position can be a builtin Variant type (`Vector2.ONE`,
/// `Transform2D()`), so the builtin names belong in the list. `Nil` is not spellable in GDScript
/// and stays out.
#[test]
fn expression_position_offers_builtin_type_names() {
    let p = project();
    let uri = file_uri(&p.root.join("src/use.gd"));
    let text = "extends Node\n\nfunc f() -> void:\n\tvar x = \n";
    let (client, server) = boot(&p, &uri, text);

    let items = complete(&client, 2, &uri, Position::new(3, 9));
    let got = labels(&items);
    for want in ["Vector2", "Vector3", "Transform2D", "PackedStringArray"] {
        assert!(got.contains(&want), "missing builtin {want:?} in {got:?}");
    }
    assert!(
        !got.contains(&"Nil"),
        "`Nil` is not spellable in GDScript: {got:?}"
    );
    shutdown(&client, server);
}

/// Godot's `COMPLETION_IDENTIFIER` seeds a fixed list of fourteen keyword constants
/// (`gdscript_editor.cpp`'s `_add_keywords`). All fourteen must be offered, as `KEYWORD` items.
#[test]
fn expression_position_offers_all_fourteen_godot_keywords() {
    let p = project();
    let uri = file_uri(&p.root.join("src/use.gd"));
    let text = "extends Node\n\nfunc f() -> void:\n\tvar x = \n";
    let (client, server) = boot(&p, &uri, text);

    let items = complete(&client, 2, &uri, Position::new(3, 9));
    for want in [
        "true",
        "false",
        "PI",
        "TAU",
        "INF",
        "NAN",
        "null",
        "self",
        "super",
        "break",
        "breakpoint",
        "continue",
        "pass",
        "return",
    ] {
        let item = items
            .iter()
            .find(|i| i.label == want)
            .unwrap_or_else(|| panic!("missing keyword {want:?} in {:?}", labels(&items)));
        assert_eq!(
            item.kind,
            Some(CompletionItemKind::KEYWORD),
            "{want:?} is a keyword item"
        );
    }
    shutdown(&client, server);
}

/// The new tiers rank BELOW the ones that were already there — a local still beats `Vector2`, and
/// `Vector2` still beats `return`. Ranking is `sortText`, so lexicographic order is priority order.
#[test]
fn the_new_tiers_rank_below_locals_and_project_classes() {
    let p = project();
    let uri = file_uri(&p.root.join("src/use.gd"));
    let text = "extends Node\n\nfunc f() -> void:\n\tvar velocity := 1\n\tvar x = \n";
    let (client, server) = boot(&p, &uri, text);

    let items = complete(&client, 2, &uri, Position::new(4, 9));
    let sort_of = |name: &str| -> String {
        items
            .iter()
            .find(|i| i.label == name)
            .unwrap_or_else(|| panic!("no `{name}` item in {:?}", labels(&items)))
            .sort_text
            .clone()
            .expect("every item carries a sortText")
    };
    assert!(
        sort_of("velocity") < sort_of("Vector2"),
        "a local outranks a builtin type name"
    );
    assert!(
        sort_of("Vector2") < sort_of("return"),
        "a builtin type name outranks a keyword"
    );
    shutdown(&client, server);
}

fn shutdown(client: &Connection, server_thread: std::thread::JoinHandle<anyhow::Result<()>>) {
    common::shutdown(client, server_thread);
}
