//! #504: `textDocument/references` returns the whole override group, from any anchor in it.
//!
//! GDScript overriding is purely name-based, so a method override chain is ONE symbol — the
//! position `rename` already takes, because renaming one link without the others silently
//! un-overrides the method. `references` filtered on a single `(file, class_path)` identity, which
//! made the answer depend on which end of the chain the cursor landed on: two disjoint halves,
//! each presented as the complete answer. These pin the union, and pin what must NOT join it.

mod common;

use common::{file_uri, notification, recv, request, shutdown, TempProject};
use lsp_server::Connection;
use lsp_types::{
    DidOpenTextDocumentParams, InitializeParams, InitializedParams, Location, Position,
    ReferenceContext, ReferenceParams, RenameParams, TextDocumentIdentifier, TextDocumentItem,
    TextDocumentPositionParams, WorkDoneProgressParams, WorkspaceEdit,
};

/// A dump with a real `Node` method, so a native-rooted override has an engine declaration to
/// root on. `MINI_API`'s classes carry no methods.
const NODE_API: &str = r#"{
    "header": { "version_major": 4, "version_minor": 6, "version_patch": 3 },
    "classes": [
        {"name": "Object", "is_instantiable": true},
        {"name": "Node", "inherits": "Object", "is_instantiable": true,
         "methods": [{"name": "_ready", "is_const": false, "is_static": false,
                      "is_vararg": false, "is_virtual": true, "hash": 1, "arguments": []}]},
        {"name": "CanvasItem", "inherits": "Node", "is_instantiable": true},
        {"name": "Node2D", "inherits": "CanvasItem", "is_instantiable": true}
    ]
}"#;

fn boot(
    project: &TempProject,
    files: &[&str],
) -> (Connection, std::thread::JoinHandle<anyhow::Result<()>>) {
    let (server, client) = Connection::memory();
    let handle = std::thread::spawn(move || gd_server::serve(server));
    let init = InitializeParams {
        initialization_options: Some(serde_json::json!({
            "projectRoot": project.root.as_str(),
            "autoDumpExtensionApi": false,
            "extensionApiPath": project.root.join("extension_api.json").as_str(),
        })),
        ..Default::default()
    };
    client.sender.send(request(1, "initialize", init)).unwrap();
    let _ = recv(&client);
    client
        .sender
        .send(notification("initialized", InitializedParams {}))
        .unwrap();
    for (i, rel) in files.iter().enumerate() {
        let abs = project.root.join(rel);
        let text = std::fs::read_to_string(abs.as_std_path()).expect("read fixture");
        client
            .sender
            .send(notification(
                "textDocument/didOpen",
                DidOpenTextDocumentParams {
                    text_document: TextDocumentItem {
                        uri: file_uri(&abs),
                        language_id: "gdscript".to_string(),
                        version: (i + 2) as i32,
                        text,
                    },
                },
            ))
            .unwrap();
    }
    while common::try_recv(&client, std::time::Duration::from_millis(300)).is_some() {}
    (client, handle)
}

fn pos(uri: &lsp_types::Uri, line: u32, character: u32) -> TextDocumentPositionParams {
    TextDocumentPositionParams {
        text_document: TextDocumentIdentifier { uri: uri.clone() },
        position: Position { line, character },
    }
}

fn sites(locs: &[Location]) -> Vec<String> {
    let mut out: Vec<String> = locs
        .iter()
        .map(|l| {
            format!(
                "{}:{}:{}",
                l.uri.as_str(),
                l.range.start.line,
                l.range.start.character
            )
        })
        .collect();
    out.sort();
    out
}

fn ref_sites(
    client: &Connection,
    id: i32,
    p: TextDocumentPositionParams,
    include_declaration: bool,
) -> Vec<String> {
    client
        .sender
        .send(request(
            id,
            "textDocument/references",
            ReferenceParams {
                text_document_position: p,
                context: ReferenceContext {
                    include_declaration,
                },
                work_done_progress_params: WorkDoneProgressParams::default(),
                partial_result_params: Default::default(),
            },
        ))
        .unwrap();
    let resp = common::recv_response(client);
    assert!(resp.error.is_none(), "references errored: {:?}", resp.error);
    let locs: Vec<Location> =
        serde_json::from_value(resp.result.expect("references result")).unwrap();
    sites(&locs)
}

fn rename_sites(
    client: &Connection,
    id: i32,
    p: TextDocumentPositionParams,
    new: &str,
) -> Vec<String> {
    client
        .sender
        .send(request(
            id,
            "textDocument/rename",
            RenameParams {
                text_document_position: p,
                new_name: new.to_string(),
                work_done_progress_params: WorkDoneProgressParams::default(),
            },
        ))
        .unwrap();
    let resp = common::recv_response(client);
    assert!(resp.error.is_none(), "rename errored: {:?}", resp.error);
    let edit: WorkspaceEdit =
        serde_json::from_value(resp.result.expect("rename result")).expect("workspace edit");
    let mut out: Vec<String> = Vec::new();
    for (uri, edits) in edit.changes.unwrap_or_default() {
        for e in edits {
            out.push(format!(
                "{}:{}:{}",
                uri.as_str(),
                e.range.start.line,
                e.range.start.character
            ));
        }
    }
    out.sort();
    out
}

/// Base ← Mid ← Leaf, each declaring `hit`, plus callers at every static receiver type and one
/// unrelated class with a same-named method that must never join the group.
fn chain_project() -> TempProject {
    let p = TempProject::new();
    p.write(
        "project.godot",
        "config_version=5\n\n[application]\n\nconfig/name=\"T\"\nconfig/features=PackedStringArray(\"4.6\")\n",
    );
    p.write("extension_api.json", NODE_API);
    // base.gd line 3: `func `(0-4) `hit`(5).
    p.write(
        "src/base.gd",
        "class_name Base\nextends Node2D\n\nfunc hit(n: int) -> void:\n\tprint(n)\n",
    );
    // mid.gd line 3 decl, line 4 super call: tab(0) `super.`(1-6) `hit`(7).
    p.write(
        "src/mid.gd",
        "class_name Mid\nextends Base\n\nfunc hit(n: int) -> void:\n\tsuper.hit(n)\n",
    );
    // leaf.gd line 3 decl, line 4 super call.
    p.write(
        "src/leaf.gd",
        "class_name Leaf\nextends Mid\n\nfunc hit(n: int) -> void:\n\tsuper.hit(n)\n",
    );
    // caller.gd lines 3/4/5: tab(0) `b`/`m`/`l`(1) `.`(2) `hit`(3).
    p.write(
        "src/caller.gd",
        "extends Node\n\nfunc run(b: Base, m: Mid, l: Leaf) -> void:\n\tb.hit(1)\n\tm.hit(2)\n\tl.hit(3)\n",
    );
    // An unrelated class with the same method name, and a caller for it. Neither may ever be
    // collected: the group walk is chain-membership-checked, never name-only.
    p.write(
        "src/shrine.gd",
        "class_name Shrine\nextends Node\n\nfunc hit(n: int) -> void:\n\tprint(n)\n",
    );
    p.write(
        "src/shrine_user.gd",
        "extends Node\n\nfunc run(s: Shrine) -> void:\n\ts.hit(9)\n",
    );
    p
}

const CHAIN_FILES: [&str; 6] = [
    "src/base.gd",
    "src/mid.gd",
    "src/leaf.gd",
    "src/caller.gd",
    "src/shrine.gd",
    "src/shrine_user.gd",
];

/// The whole group, from any anchor in it: three declarations, two `super` sites, three call sites.
fn want_chain_union(p: &TempProject) -> Vec<String> {
    let base = file_uri(&p.root.join("src/base.gd"));
    let mid = file_uri(&p.root.join("src/mid.gd"));
    let leaf = file_uri(&p.root.join("src/leaf.gd"));
    let caller = file_uri(&p.root.join("src/caller.gd"));
    let mut want = vec![
        format!("{}:3:5", base.as_str()),
        format!("{}:3:5", mid.as_str()),
        format!("{}:4:7", mid.as_str()),
        format!("{}:3:5", leaf.as_str()),
        format!("{}:4:7", leaf.as_str()),
        format!("{}:3:3", caller.as_str()),
        format!("{}:4:3", caller.as_str()),
        format!("{}:5:3", caller.as_str()),
    ];
    want.sort();
    want
}

#[test]
fn every_anchor_in_a_three_level_chain_returns_the_same_union() {
    let p = chain_project();
    let (client, server) = boot(&p, &CHAIN_FILES);
    let base = file_uri(&p.root.join("src/base.gd"));
    let leaf = file_uri(&p.root.join("src/leaf.gd"));
    let mid = file_uri(&p.root.join("src/mid.gd"));
    let caller = file_uri(&p.root.join("src/caller.gd"));
    let want = want_chain_union(&p);

    for (id, anchor, what) in [
        (10, pos(&base, 3, 5), "the root declaration"),
        (11, pos(&mid, 3, 5), "a middle override's declaration"),
        (12, pos(&leaf, 3, 5), "the leaf override's declaration"),
        (13, pos(&mid, 4, 7), "a `super.hit` callee"),
        (14, pos(&caller, 3, 3), "a Base-typed call site"),
        (15, pos(&caller, 5, 3), "a Leaf-typed call site"),
    ] {
        assert_eq!(
            ref_sites(&client, id, anchor, true),
            want,
            "references from {what} must return the whole override group"
        );
    }
    shutdown(&client, server);
}

/// The invariant `rename`'s own doc comment promises: the edited set equals what `references`
/// returns for the same symbol. It was false for every override group.
#[test]
fn the_union_equals_the_rename_edit_set() {
    let p = chain_project();
    let (client, server) = boot(&p, &CHAIN_FILES);
    let base = file_uri(&p.root.join("src/base.gd"));

    let refs = ref_sites(&client, 20, pos(&base, 3, 5), true);
    let edits = rename_sites(&client, 21, pos(&base, 3, 5), "strike");
    assert_eq!(
        refs, edits,
        "the reference list and the edit set are the same list for one symbol"
    );
    shutdown(&client, server);
}

/// `includeDeclaration: false` drops EVERY declaration in the group, not just the anchor's.
#[test]
fn excluding_declarations_drops_all_three_of_them() {
    let p = chain_project();
    let (client, server) = boot(&p, &CHAIN_FILES);
    let base = file_uri(&p.root.join("src/base.gd"));
    let mid = file_uri(&p.root.join("src/mid.gd"));
    let leaf = file_uri(&p.root.join("src/leaf.gd"));
    let caller = file_uri(&p.root.join("src/caller.gd"));

    let got = ref_sites(&client, 30, pos(&base, 3, 5), false);
    let mut want = vec![
        format!("{}:4:7", mid.as_str()),
        format!("{}:4:7", leaf.as_str()),
        format!("{}:3:3", caller.as_str()),
        format!("{}:4:3", caller.as_str()),
        format!("{}:5:3", caller.as_str()),
    ];
    want.sort();
    assert_eq!(
        got, want,
        "the call sites survive, all three declarations go"
    );
    shutdown(&client, server);
}

/// The group is chain-membership-checked, never name-only: an unrelated class declaring the same
/// method, and its caller, stay out of every answer.
#[test]
fn an_unrelated_same_named_method_never_joins_the_group() {
    let p = chain_project();
    let (client, server) = boot(&p, &CHAIN_FILES);
    let base = file_uri(&p.root.join("src/base.gd"));
    let shrine = file_uri(&p.root.join("src/shrine.gd"));
    let shrine_user = file_uri(&p.root.join("src/shrine_user.gd"));

    let got = ref_sites(&client, 40, pos(&base, 3, 5), true);
    assert!(
        !got.iter().any(|s| s.starts_with(shrine.as_str())),
        "Shrine.hit is a different symbol: {got:?}"
    );
    assert!(
        !got.iter().any(|s| s.starts_with(shrine_user.as_str())),
        "a call to Shrine.hit is a different symbol: {got:?}"
    );

    // And from the other side: Shrine's own reference list is its declaration plus its one caller.
    let from_shrine = ref_sites(&client, 41, pos(&shrine, 3, 5), true);
    let mut want = vec![
        format!("{}:3:5", shrine.as_str()),
        format!("{}:3:3", shrine_user.as_str()),
    ];
    want.sort();
    assert_eq!(from_shrine, want);
    shutdown(&client, server);
}

/// A method no one overrides is untouched — the `Single` path, which is most of a project.
#[test]
fn a_method_no_one_overrides_is_unchanged() {
    let p = TempProject::new();
    p.write(
        "project.godot",
        "config_version=5\n\n[application]\n\nconfig/name=\"T\"\nconfig/features=PackedStringArray(\"4.6\")\n",
    );
    p.write("extension_api.json", NODE_API);
    p.write(
        "src/lone.gd",
        "class_name Lone\nextends Node\n\nfunc solo() -> void:\n\tpass\n",
    );
    p.write(
        "src/lone_user.gd",
        "extends Node\n\nfunc run(l: Lone) -> void:\n\tl.solo()\n",
    );
    let files = ["src/lone.gd", "src/lone_user.gd"];
    let (client, server) = boot(&p, &files);
    let lone = file_uri(&p.root.join("src/lone.gd"));
    let user = file_uri(&p.root.join("src/lone_user.gd"));

    let mut want = vec![
        format!("{}:3:5", lone.as_str()),
        format!("{}:3:3", user.as_str()),
    ];
    want.sort();
    assert_eq!(ref_sites(&client, 50, pos(&lone, 3, 5), true), want);
    shutdown(&client, server);
}

/// A native-rooted override (`_ready` and friends) stays NARROW. That group is effectively every
/// script in the project, and a `_ready` reference list spanning the whole tree answers nothing —
/// the same arm `rename` refuses on. Two unrelated scripts each overriding `_ready` must not
/// appear in each other's answers.
#[test]
fn a_native_rooted_override_stays_narrow() {
    let p = TempProject::new();
    p.write(
        "project.godot",
        "config_version=5\n\n[application]\n\nconfig/name=\"T\"\nconfig/features=PackedStringArray(\"4.6\")\n",
    );
    p.write("extension_api.json", NODE_API);
    p.write(
        "src/one.gd",
        "extends Node\n\nfunc _ready() -> void:\n\tpass\n",
    );
    p.write(
        "src/two.gd",
        "extends Node\n\nfunc _ready() -> void:\n\tpass\n",
    );
    let files = ["src/one.gd", "src/two.gd"];
    let (client, server) = boot(&p, &files);
    let one = file_uri(&p.root.join("src/one.gd"));
    let two = file_uri(&p.root.join("src/two.gd"));

    let got = ref_sites(&client, 60, pos(&one, 2, 5), true);
    assert!(
        !got.iter().any(|s| s.starts_with(two.as_str())),
        "a `_ready` override must not fan out across the project: {got:?}"
    );
    shutdown(&client, server);
}
