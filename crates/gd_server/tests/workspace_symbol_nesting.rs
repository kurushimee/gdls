//! `workspace/symbol` reaches every symbol the file outline shows (#305).
//!
//! The candidate walk used to stop at file scope, so inner classes, their members, and named-enum
//! values were all in `textDocument/documentSymbol` and none of them in `workspace/symbol` — a cut
//! exactly at the nesting boundary, for information already sitting in the interface. An autoload
//! singleton whose script carries no `class_name` was missing for a different reason: it is a
//! project-wide global that no `class_name` registry entry covers.

mod common;

use std::time::Duration;

use common::{file_uri, notification, request, try_recv, TempProject};
use lsp_server::{Connection, Message};
use lsp_types::{InitializeParams, InitializedParams, SymbolKind, WorkspaceSymbolResponse};

const INVENTORY_GD: &str = "\
class_name Inventory
extends Node

signal cleared

const MAX_SLOTS := 8

enum Slot { WEAPON, ARMOR, TRINKET }

class Entry:
\tvar item: String
\tvar count: int

\tfunc total_weight() -> int:
\t\treturn count

\tclass Tag:
\t\tvar label: String

var entries: Array[Entry] = []

func slot_name(s: Slot) -> String:
\treturn str(s)
";

/// A project with one `class_name` script carrying nested everything, plus a `class_name`-less
/// autoload singleton.
fn project() -> TempProject {
    let p = TempProject::new();
    p.write(
        "project.godot",
        "config_version=5\n\n[application]\n\nconfig/name=\"T\"\n\n[autoload]\n\nGlobal=\"*res://autoload/global.gd\"\nPlain=\"res://autoload/plain.gd\"\n",
    );
    p.write("extension_api.json", common::MINI_API);
    p.write("src/inventory.gd", INVENTORY_GD);
    p.write("autoload/global.gd", "extends Node\n\nvar score := 0\n");
    p.write("autoload/plain.gd", "extends Node\n\nvar unused := 0\n");
    p
}

fn boot(p: &TempProject) -> (Connection, std::thread::JoinHandle<anyhow::Result<()>>) {
    let (server, client) = Connection::memory();
    let server_thread = std::thread::spawn(move || gd_server::serve(server));
    let init = InitializeParams {
        initialization_options: Some(serde_json::json!({
            "projectRoot": p.root.as_str(),
            "autoDumpExtensionApi": false,
            "extensionApiPath": p.root.join("extension_api.json").as_str(),
        })),
        ..Default::default()
    };
    client.sender.send(request(1, "initialize", init)).unwrap();
    loop {
        if let Message::Response(r) = common::recv(&client) {
            assert!(r.error.is_none(), "initialize errored: {:?}", r.error);
            break;
        }
    }
    client
        .sender
        .send(notification("initialized", InitializedParams {}))
        .unwrap();
    while try_recv(&client, Duration::from_millis(300)).is_some() {}
    (client, server_thread)
}

/// `(name, kind, container)` triples for one query.
fn symbols(client: &Connection, id: i32, query: &str) -> Vec<(String, SymbolKind, Option<String>)> {
    client
        .sender
        .send(request(
            id,
            "workspace/symbol",
            serde_json::json!({ "query": query }),
        ))
        .unwrap();
    let resp = common::recv_response(client);
    assert!(
        resp.error.is_none(),
        "workspace/symbol errored: {:?}",
        resp.error
    );
    let parsed: Option<WorkspaceSymbolResponse> =
        serde_json::from_value(resp.result.expect("a result")).expect("a symbol response");
    match parsed {
        Some(WorkspaceSymbolResponse::Flat(v)) => v
            .into_iter()
            .map(|s| (s.name, s.kind, s.container_name))
            .collect(),
        Some(WorkspaceSymbolResponse::Nested(v)) => v
            .into_iter()
            .map(|s| (s.name, s.kind, s.container_name))
            .collect(),
        None => Vec::new(),
    }
}

fn find<'a>(
    rows: &'a [(String, SymbolKind, Option<String>)],
    name: &str,
) -> &'a (String, SymbolKind, Option<String>) {
    rows.iter()
        .find(|(n, _, _)| n == name)
        .unwrap_or_else(|| panic!("no `{name}` in {rows:?}"))
}

#[test]
fn an_inner_class_is_a_workspace_symbol_under_its_outer_class() {
    let p = project();
    let (client, server) = boot(&p);

    let entry = find(&symbols(&client, 2, "Entry"), "Entry").clone();
    assert_eq!(entry.1, SymbolKind::CLASS);
    assert_eq!(entry.2.as_deref(), Some("Inventory"));

    // Nesting is recursive, and the container is the dotted path, not just the immediate parent.
    let tag = find(&symbols(&client, 3, "Tag"), "Tag").clone();
    assert_eq!(tag.1, SymbolKind::CLASS);
    assert_eq!(tag.2.as_deref(), Some("Inventory.Entry"));

    common::shutdown(&client, server);
}

#[test]
fn an_inner_classes_members_are_workspace_symbols() {
    let p = project();
    let (client, server) = boot(&p);

    let weight = find(&symbols(&client, 2, "total_weight"), "total_weight").clone();
    assert_eq!(weight.1, SymbolKind::METHOD);
    assert_eq!(weight.2.as_deref(), Some("Inventory.Entry"));

    let label = find(&symbols(&client, 3, "label"), "label").clone();
    assert_eq!(label.2.as_deref(), Some("Inventory.Entry.Tag"));

    common::shutdown(&client, server);
}

#[test]
fn named_enum_values_are_workspace_symbols_under_their_enum() {
    let p = project();
    let (client, server) = boot(&p);

    let rows = symbols(&client, 2, "TRINKET");
    let trinket = find(&rows, "TRINKET");
    assert_eq!(trinket.1, SymbolKind::ENUM_MEMBER);
    assert_eq!(trinket.2.as_deref(), Some("Inventory.Slot"));

    // The enum itself is still a member of the class, not of itself.
    let slot = find(&symbols(&client, 3, "Slot"), "Slot").clone();
    assert_eq!(slot.1, SymbolKind::ENUM);
    assert_eq!(slot.2.as_deref(), Some("Inventory"));

    common::shutdown(&client, server);
}

/// An enum value reports its OWN declaration line, not the enum's. All three values here sit on
/// the enum's line, so the check that matters is that the recorded span is the value's identifier:
/// resolve validates it against the live text and would fall back to a zero-width anchor otherwise.
#[test]
fn an_enum_value_resolves_to_its_own_name_range() {
    let p = project();
    p.write(
        "src/spread.gd",
        "class_name Spread\nextends Node\n\nenum Phase {\n\tSTART,\n\tMIDDLE,\n\tEND,\n}\n",
    );
    let (client, server) = boot(&p);

    client
        .sender
        .send(request(
            2,
            "workspace/symbol",
            serde_json::json!({ "query": "MIDDLE" }),
        ))
        .unwrap();
    let resp = common::recv_response(&client);
    let result = resp.result.expect("a result");
    let row = &result.as_array().expect("an array")[0];
    // `MIDDLE` is on line 6 (1-based), i.e. LSP line 5.
    assert_eq!(
        row["location"]["range"]["start"]["line"], 5,
        "the value anchors at its own line: {row}"
    );
    assert_eq!(
        row["location"]["range"]["start"]["character"], 1,
        "…and at its own name token, past the leading tab: {row}"
    );

    common::shutdown(&client, server);
}

/// An autoload singleton is a project-wide global that `definition` and `hover` both resolve, so
/// it belongs in the symbol list even when its script declares no `class_name`. A non-singleton
/// autoload is not a name in scope in Godot, and stays out.
#[test]
fn a_class_name_less_autoload_singleton_is_a_workspace_symbol() {
    let p = project();
    let (client, server) = boot(&p);

    let global = find(&symbols(&client, 2, "Global"), "Global").clone();
    assert_eq!(global.1, SymbolKind::CLASS);

    assert!(
        !symbols(&client, 3, "Plain")
            .iter()
            .any(|(n, _, _)| n == "Plain"),
        "a non-`*` autoload is registered but not a global singleton"
    );

    common::shutdown(&client, server);
}

/// The head class is still one row, not two — the registry entry and the interface walk must not
/// both claim it.
#[test]
fn the_head_class_is_not_reported_twice() {
    let p = project();
    let (client, server) = boot(&p);

    let rows = symbols(&client, 2, "Inventory");
    let hits: Vec<_> = rows.iter().filter(|(n, _, _)| n == "Inventory").collect();
    assert_eq!(hits.len(), 1, "one `Inventory` row: {rows:?}");

    common::shutdown(&client, server);
}

/// `documentSymbol` and `workspace/symbol` now agree on what exists: every named symbol in the
/// outline is findable project-wide.
#[test]
fn every_document_symbol_name_is_findable_project_wide() {
    let p = project();
    let (client, server) = boot(&p);
    let uri = file_uri(&p.root.join("src/inventory.gd"));

    for (i, name) in [
        "Inventory",
        "cleared",
        "MAX_SLOTS",
        "Slot",
        "WEAPON",
        "ARMOR",
        "TRINKET",
        "Entry",
        "item",
        "count",
        "total_weight",
        "Tag",
        "label",
        "entries",
        "slot_name",
    ]
    .iter()
    .enumerate()
    {
        let rows = symbols(&client, 100 + i as i32, name);
        assert!(
            rows.iter().any(|(n, _, _)| n == name),
            "`{name}` is in the outline of {uri:?} but not in workspace/symbol: {rows:?}"
        );
    }

    common::shutdown(&client, server);
}
