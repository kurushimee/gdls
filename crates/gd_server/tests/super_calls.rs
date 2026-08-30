//! `super` navigation (#333): `super.method()` and bare `super()` name the PARENT's method, never
//! the override doing the calling.
//!
//! Before this, the reducer pre-reduced a super callee as an ordinary identifier in the CURRENT
//! scope (Godot never does — `gdscript_analyzer.cpp:3269`/`:3731` gate every such site on
//! `!p_call->is_super`), which for an override resolved to the override itself. That produced a
//! self-referential `definition`, a `Callable` hover, and — because `rename` is `references` with
//! the declaration included — an edit set that rewrote `super.describe()` into a call on a method
//! that does not exist on the parent.
//!
//! The invariant these tests pin: a `super.X()` site is edited exactly when the declaration its
//! call binding resolves to is edited, and never when some other declaration is.

mod common;

use common::{file_uri, notification, recv, request, shutdown, TempProject};
use lsp_server::Connection;
use lsp_types::{
    DidOpenTextDocumentParams, GotoDefinitionResponse, Hover, HoverContents, InitializeParams,
    InitializedParams, Location, MarkupContent, Position, ReferenceContext, ReferenceParams,
    RenameParams, TextDocumentIdentifier, TextDocumentItem, TextDocumentPositionParams,
    WorkDoneProgressParams, WorkspaceEdit,
};

/// A native dump with one real `Node` method, so a super call through a NATIVE parent has
/// something to resolve to. `MINI_API`'s classes carry no methods.
const NODE_API: &str = r#"{
    "header": { "version_major": 4, "version_minor": 6, "version_patch": 3 },
    "classes": [
        {"name": "Object", "is_instantiable": true},
        {"name": "Node", "inherits": "Object", "is_instantiable": true,
         "methods": [{"name": "queue_free", "is_const": false, "is_static": false,
                      "is_vararg": false, "is_virtual": false, "hash": 1, "arguments": []}]},
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
        let text = std::fs::read_to_string(abs.as_std_path()).expect("read file");
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

/// `(uri-as-string, line, character)` of every location in a definition response, sorted.
fn def_sites(
    client: &Connection,
    id: i32,
    method: &str,
    p: TextDocumentPositionParams,
) -> Vec<String> {
    client
        .sender
        .send(request(
            id,
            method,
            lsp_types::GotoDefinitionParams {
                text_document_position_params: p,
                work_done_progress_params: WorkDoneProgressParams::default(),
                partial_result_params: Default::default(),
            },
        ))
        .unwrap();
    let resp = common::recv_response(client);
    assert!(resp.error.is_none(), "{method} errored: {:?}", resp.error);
    let Some(v) = resp.result.filter(|v| !v.is_null()) else {
        return Vec::new();
    };
    let got: GotoDefinitionResponse = serde_json::from_value(v).unwrap();
    let locs: Vec<Location> = match got {
        GotoDefinitionResponse::Scalar(l) => vec![l],
        GotoDefinitionResponse::Array(v) => v,
        GotoDefinitionResponse::Link(v) => v
            .into_iter()
            .map(|l| Location {
                uri: l.target_uri,
                range: l.target_selection_range,
            })
            .collect(),
    };
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

fn ref_sites(client: &Connection, id: i32, p: TextDocumentPositionParams) -> Vec<String> {
    client
        .sender
        .send(request(
            id,
            "textDocument/references",
            ReferenceParams {
                text_document_position: p,
                context: ReferenceContext {
                    include_declaration: true,
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
        serde_json::from_value(resp.result.expect("a WorkspaceEdit")).unwrap();
    let mut out = Vec::new();
    if let Some(lsp_types::DocumentChanges::Edits(edits)) = &edit.document_changes {
        for tde in edits {
            for e in &tde.edits {
                let lsp_types::OneOf::Left(te) = e else {
                    continue;
                };
                out.push(format!(
                    "{}:{}:{}",
                    tde.text_document.uri.as_str(),
                    te.range.start.line,
                    te.range.start.character
                ));
            }
        }
    }
    for (uri, edits) in edit.changes.iter().flatten() {
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

fn hover_md(client: &Connection, id: i32, p: TextDocumentPositionParams) -> String {
    client
        .sender
        .send(request(
            id,
            "textDocument/hover",
            lsp_types::HoverParams {
                text_document_position_params: p,
                work_done_progress_params: WorkDoneProgressParams::default(),
            },
        ))
        .unwrap();
    let resp = common::recv_response(client);
    assert!(resp.error.is_none(), "hover errored: {:?}", resp.error);
    let Some(v) = resp.result.filter(|v| !v.is_null()) else {
        return String::new();
    };
    let hover: Hover = serde_json::from_value(v).unwrap();
    match hover.contents {
        HoverContents::Markup(MarkupContent { value, .. }) => value,
        other => panic!("unexpected hover shape: {other:?}"),
    }
}

/// `actor.gd` declares `_ready` and `describe`; `player.gd` overrides both and super-calls each;
/// `game.gd` calls `describe` on a Player-typed and an Actor-typed value.
///
/// ```text
/// actor.gd  0 class_name Actor          player.gd 0 class_name Player
///           1 extends Node2D                      1 extends Actor
///           2                                     2
///           3 func _ready() -> void:              3 func _ready() -> void:
///           4 \tpass                              4 \tsuper()
///           5                                     5
///           6 func describe() -> String:          6 func describe() -> String:
///           7 \treturn "actor"                    7 \treturn "P:" + super.describe()
/// ```
fn override_project() -> TempProject {
    let p = TempProject::new();
    p.write(
        "project.godot",
        "config_version=5\n\n[application]\n\nconfig/name=\"T\"\nconfig/features=PackedStringArray(\"4.6\")\n",
    );
    p.write("extension_api.json", NODE_API);
    p.write(
        "src/actor.gd",
        "class_name Actor\nextends Node2D\n\nfunc _ready() -> void:\n\tpass\n\nfunc describe() -> String:\n\treturn \"actor\"\n",
    );
    p.write(
        "src/player.gd",
        "class_name Player\nextends Actor\n\nfunc _ready() -> void:\n\tsuper()\n\nfunc describe() -> String:\n\treturn \"P:\" + super.describe()\n",
    );
    p.write(
        "src/game.gd",
        "extends Node\n\nfunc run(a: Actor) -> void:\n\tvar p := Player.new()\n\tprint(p.describe())\n\tprint(a.describe())\n",
    );
    p
}

const FILES: [&str; 3] = ["src/actor.gd", "src/player.gd", "src/game.gd"];

/// `super.describe` at player.gd line 7: tab(0) `return "P:" + `(1-14) `super`(15-19) `.`(20)
/// `describe`(21).
const SUPER_DESCRIBE: (u32, u32) = (7, 21);

#[test]
fn definition_on_a_super_callee_resolves_to_the_parent_not_the_override() {
    let p = override_project();
    let (client, server) = boot(&p, &FILES);
    let player = file_uri(&p.root.join("src/player.gd"));
    let actor = file_uri(&p.root.join("src/actor.gd"));
    let want = vec![format!("{}:6:5", actor.as_str())];
    assert_eq!(
        def_sites(
            &client,
            10,
            "textDocument/definition",
            pos(&player, SUPER_DESCRIBE.0, SUPER_DESCRIBE.1)
        ),
        want,
        "`super.describe()` names Actor's `describe`, never the Player override that calls it"
    );
    assert_eq!(
        def_sites(
            &client,
            11,
            "textDocument/declaration",
            pos(&player, SUPER_DESCRIBE.0, SUPER_DESCRIBE.1)
        ),
        want,
        "`declaration` must agree with `definition`"
    );
    shutdown(&client, server);
}

#[test]
fn definition_on_a_bare_super_call_resolves_to_the_parents_same_named_method() {
    // `super()` carries NO callee at all — the parser fills `function_name` with the enclosing
    // function's name (gdscript_parser.cpp:3487-3499), so the cursor sits on a keyword and the
    // whole answer has to come from the call's binding.
    let p = override_project();
    let (client, server) = boot(&p, &FILES);
    let player = file_uri(&p.root.join("src/player.gd"));
    let actor = file_uri(&p.root.join("src/actor.gd"));
    // player.gd line 4: tab(0) `super`(1).
    assert_eq!(
        def_sites(&client, 12, "textDocument/definition", pos(&player, 4, 1)),
        vec![format!("{}:3:5", actor.as_str())],
        "bare `super()` in `_ready` resolves to Actor's `_ready`"
    );
    shutdown(&client, server);
}

#[test]
fn hover_on_a_super_callee_shows_the_parents_signature() {
    let p = override_project();
    let (client, server) = boot(&p, &FILES);
    let player = file_uri(&p.root.join("src/player.gd"));
    let md = hover_md(
        &client,
        13,
        pos(&player, SUPER_DESCRIBE.0, SUPER_DESCRIBE.1),
    );
    assert!(
        md.contains("func describe() -> String"),
        "hover on a super callee must render the method's signature, not the callee expression's \
         `Callable` value type; got {md:?}"
    );
    shutdown(&client, server);
}

#[test]
fn hover_on_a_bare_call_to_an_own_method_shows_its_signature() {
    // The `super` sibling (#334): a bare `helper()` callee has no subscript base either, so it fell
    // through to the same `Callable` type label. Both now project the call binding.
    let p = TempProject::new();
    p.write(
        "project.godot",
        "config_version=5\n\n[application]\n\nconfig/name=\"T\"\nconfig/features=PackedStringArray(\"4.6\")\n",
    );
    p.write("extension_api.json", NODE_API);
    p.write(
        "src/bare.gd",
        "extends Node\n\nfunc helper(n: int) -> String:\n\treturn str(n)\n\nfunc run() -> void:\n\tprint(helper(1))\n",
    );
    let (client, server) = boot(&p, &["src/bare.gd"]);
    let uri = file_uri(&p.root.join("src/bare.gd"));
    // line 6: tab(0) `print(`(1-6) `helper`(7).
    let md = hover_md(&client, 14, pos(&uri, 6, 7));
    assert!(
        md.contains("func helper(n: int) -> String"),
        "hover on a bare call to an own method must render its signature; got {md:?}"
    );
    shutdown(&client, server);
}

#[test]
fn references_on_the_base_declaration_include_the_super_call_site() {
    let p = override_project();
    let (client, server) = boot(&p, &FILES);
    let actor = file_uri(&p.root.join("src/actor.gd"));
    let player = file_uri(&p.root.join("src/player.gd"));
    let game = file_uri(&p.root.join("src/game.gd"));
    // actor.gd line 6: `func `(0-4) `describe`(5).
    let from_decl = ref_sites(&client, 15, pos(&actor, 6, 5));
    assert_eq!(
        from_decl,
        {
            let mut want = vec![
                format!("{}:6:5", actor.as_str()),
                format!("{}:7:21", player.as_str()),
                format!("{}:5:9", game.as_str()),
            ];
            want.sort();
            want
        },
        "the declaration, the `super.describe()` site, and the Actor-typed call in game.gd"
    );
    // The same set from the super cursor: a super site names the base, so both cursors are the
    // same symbol.
    assert_eq!(
        ref_sites(
            &client,
            16,
            pos(&player, SUPER_DESCRIBE.0, SUPER_DESCRIBE.1)
        ),
        from_decl,
        "a super-callee cursor and the base declaration must resolve to the same symbol"
    );
    shutdown(&client, server);
}

#[test]
fn renaming_either_end_of_an_override_chain_edits_the_whole_group() {
    // THE CORRUPTION GUARD. GDScript overriding is purely name-based, so an override chain is ONE
    // symbol and a single-link rename corrupts in both directions: renaming only `Player.describe`
    // silently un-overrides it (`p.describe()` starts dispatching to `Actor.describe`, with no
    // diagnostic anywhere), and renaming only `Actor.describe` orphans the override and dangles
    // the `super.describe()` that targets it.
    //
    // So the edit set is the whole group, and it is the SAME set from every click site in it.
    let p = override_project();
    let (client, server) = boot(&p, &FILES);
    let actor = file_uri(&p.root.join("src/actor.gd"));
    let player = file_uri(&p.root.join("src/player.gd"));
    let game = file_uri(&p.root.join("src/game.gd"));

    let want = {
        let mut want = vec![
            // Actor's declaration, and Player's override of it.
            format!("{}:6:5", actor.as_str()),
            format!("{}:6:5", player.as_str()),
            // The `super.describe()` site, which targets Actor's.
            format!("{}:7:21", player.as_str()),
            // game.gd line 4/5: tab(0) `print(`(1-6) `p`/`a`(7) `.`(8) `describe`(9).
            format!("{}:4:9", game.as_str()),
            format!("{}:5:9", game.as_str()),
        ];
        want.sort();
        want
    };

    assert_eq!(
        rename_sites(&client, 17, pos(&player, 6, 5), "label"),
        want,
        "from the OVERRIDE's declaration"
    );
    assert_eq!(
        rename_sites(&client, 18, pos(&actor, 6, 5), "label"),
        want,
        "from the BASE's declaration"
    );
    assert_eq!(
        rename_sites(&client, 19, pos(&game, 4, 9), "label"),
        want,
        "from a Player-typed call site"
    );
    assert_eq!(
        rename_sites(&client, 20, pos(&game, 5, 9), "label"),
        want,
        "from an Actor-typed call site"
    );
    assert_eq!(
        rename_sites(
            &client,
            21,
            pos(&player, SUPER_DESCRIBE.0, SUPER_DESCRIBE.1),
            "label"
        ),
        want,
        "from the `super.describe()` site itself"
    );
    shutdown(&client, server);
}

/// The invariant, restated as the thing that makes the group safe: a `super.X()` site is edited
/// exactly when the declaration its call binding resolves to is edited. Because the edit set is
/// always a whole group containing EVERY declaration in the dispatch chain, the super site and its
/// target move together or not at all — there is no rename that separates them.
#[test]
fn a_super_site_never_moves_without_the_declaration_it_targets() {
    let p = override_project();
    let (client, server) = boot(&p, &FILES);
    let actor = file_uri(&p.root.join("src/actor.gd"));
    let player = file_uri(&p.root.join("src/player.gd"));
    let super_site = format!("{}:7:21", player.as_str());
    let base_decl = format!("{}:6:5", actor.as_str());

    let edits = rename_sites(&client, 22, pos(&player, 6, 5), "label");
    assert_eq!(
        edits.contains(&super_site),
        edits.contains(&base_decl),
        "the super site and Actor's declaration must ride together: {edits:?}"
    );
    shutdown(&client, server);
}

/// A method with no overrides anywhere is a group of one and renames as it always did — the group
/// expansion must not widen an ordinary rename.
#[test]
fn a_method_no_one_overrides_still_renames_alone() {
    let p = override_project();
    p.write(
        "src/actor.gd",
        "class_name Actor\nextends Node2D\n\nfunc _ready() -> void:\n\tpass\n\nfunc describe() -> String:\n\treturn \"actor\"\n\nfunc only_here() -> int:\n\treturn 1\n",
    );
    let (client, server) = boot(&p, &FILES);
    let actor = file_uri(&p.root.join("src/actor.gd"));
    assert_eq!(
        rename_sites(&client, 23, pos(&actor, 9, 5), "solo"),
        vec![format!("{}:9:5", actor.as_str())],
        "an unoverridden method renames only its own declaration"
    );
    shutdown(&client, server);
}

/// A chain whose ROOT is native is unrenamable: `_ready` overrides `Node._ready`, and the group
/// reaches into engine code no edit can touch. Refuse at every cursor in the group rather than
/// rewrite half of it — the rename-side twin of the `NATIVE_METHOD_OVERRIDE` warning.
#[test]
fn renaming_a_native_rooted_override_refuses_at_every_cursor() {
    let p = TempProject::new();
    p.write(
        "project.godot",
        "config_version=5\n\n[application]\n\nconfig/name=\"T\"\nconfig/features=PackedStringArray(\"4.6\")\n",
    );
    // `Node` here declares `_ready`, so the chain's native root owns the name.
    p.write(
        "extension_api.json",
        r#"{
    "header": { "version_major": 4, "version_minor": 6, "version_patch": 3 },
    "classes": [
        {"name": "Object", "is_instantiable": true},
        {"name": "Node", "inherits": "Object", "is_instantiable": true,
         "methods": [{"name": "_ready", "is_const": false, "is_static": false,
                      "is_vararg": false, "is_virtual": true, "hash": 1, "arguments": []}]},
        {"name": "CanvasItem", "inherits": "Node", "is_instantiable": true},
        {"name": "Node2D", "inherits": "CanvasItem", "is_instantiable": true}
    ]
}"#,
    );
    p.write(
        "src/actor.gd",
        "class_name Actor\nextends Node2D\n\nfunc _ready() -> void:\n\tpass\n",
    );
    p.write(
        "src/player.gd",
        "class_name Player\nextends Actor\n\nfunc _ready() -> void:\n\tsuper()\n",
    );
    let (client, server) = boot(&p, &["src/actor.gd", "src/player.gd"]);
    for (id, uri) in [
        (24, file_uri(&p.root.join("src/actor.gd"))),
        (25, file_uri(&p.root.join("src/player.gd"))),
    ] {
        client
            .sender
            .send(request(
                id,
                "textDocument/rename",
                RenameParams {
                    text_document_position: pos(&uri, 3, 5),
                    new_name: "started".to_string(),
                    work_done_progress_params: WorkDoneProgressParams::default(),
                },
            ))
            .unwrap();
        let resp = common::recv_response(&client);
        let err = resp.error.expect("a refusal, not an edit");
        assert_eq!(err.code, -32803, "RequestFailed: {err:?}");
        assert!(
            err.message.contains("_ready") && err.message.contains("Node"),
            "the refusal names the native method it overrides: {}",
            err.message
        );
    }
    shutdown(&client, server);
}

#[test]
fn a_super_call_through_a_native_parent_resolves_into_the_api_stub() {
    let p = TempProject::new();
    p.write(
        "project.godot",
        "config_version=5\n\n[application]\n\nconfig/name=\"T\"\nconfig/features=PackedStringArray(\"4.6\")\n",
    );
    p.write("extension_api.json", NODE_API);
    p.write(
        "src/n.gd",
        "extends Node\n\nfunc queue_free() -> void:\n\tsuper.queue_free()\n",
    );
    let (client, server) = boot(&p, &["src/n.gd"]);
    let uri = file_uri(&p.root.join("src/n.gd"));
    // line 3: tab(0) `super`(1-5) `.`(6) `queue_free`(7).
    let sites = def_sites(&client, 19, "textDocument/definition", pos(&uri, 3, 7));
    assert_eq!(sites.len(), 1, "one definition, got {sites:?}");
    assert!(
        sites[0].contains("Node.gd"),
        "a super call whose parent is native lands in the materialized `Node` stub, not the \
         overriding project method; got {sites:?}"
    );
    shutdown(&client, server);
}

#[test]
fn a_super_call_skips_a_level_that_does_not_declare_the_method() {
    // GrandBase declares `foo`; Mid does not; Leaf overrides it and super-calls. `super.foo()`
    // must resolve past Mid to GrandBase — `get_function_signature` walks the chain upward.
    let p = TempProject::new();
    p.write(
        "project.godot",
        "config_version=5\n\n[application]\n\nconfig/name=\"T\"\nconfig/features=PackedStringArray(\"4.6\")\n",
    );
    p.write("extension_api.json", NODE_API);
    p.write(
        "src/grand.gd",
        "class_name GrandBase\nextends Node\n\nfunc foo() -> int:\n\treturn 1\n",
    );
    p.write("src/mid.gd", "class_name Mid\nextends GrandBase\n");
    p.write(
        "src/leaf.gd",
        "class_name Leaf\nextends Mid\n\nfunc foo() -> int:\n\treturn super.foo() + 1\n",
    );
    let (client, server) = boot(&p, &["src/grand.gd", "src/mid.gd", "src/leaf.gd"]);
    let leaf = file_uri(&p.root.join("src/leaf.gd"));
    let grand = file_uri(&p.root.join("src/grand.gd"));
    // leaf.gd line 4: tab(0) `return `(1-7) `super`(8-12) `.`(13) `foo`(14).
    assert_eq!(
        def_sites(&client, 20, "textDocument/definition", pos(&leaf, 4, 14)),
        vec![format!("{}:3:5", grand.as_str())],
        "the chain walk skips Mid, which does not declare `foo`"
    );
    shutdown(&client, server);
}
