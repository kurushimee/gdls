//! M11 follow-up (#125) — precise `$`/`%` typing on the NAVIGATION surfaces.
//!
//! `hover` / `definition` / `typeDefinition` answer a `$Path` / `%Name` / `get_node("…")` access
//! with the scene-precise node type: the engine class of the node the access reaches, or the
//! `class_name` of the script attached to it. That comes from the scene index, NOT from the
//! analyzer — `reduce_get_node` still types the access as bare `NATIVE Node` (faithful to Godot,
//! `docs/02` §11), which is what keeps the Godot-tolerated sibling downcasts free of false
//! positives. `tests/scene_typing.rs` guards that other half.
//!
//! The bar here is no-false-positives: every shape the scene index cannot resolve unanimously —
//! an absolute path, a script no scene attaches, two scenes disagreeing — must fall BACK to the
//! bare `Node` answer rather than guess.

mod common;

use common::{file_uri, notification, recv_response, request, TempProject};
use lsp_server::Connection;
use lsp_types::{
    ClientCapabilities, DidOpenTextDocumentParams, GotoDefinitionResponse, Hover, HoverContents,
    InitializeParams, InitializedParams, MarkupContent, Position, TextDocumentItem, Uri,
};

/// `Object ← Node ← CanvasItem ← {Node2D ← Sprite2D, Control}` — a hierarchy with a sibling branch,
/// so a precise answer is distinguishable from both `Node` and its neighbours.
const API: &str = r#"{
    "header": {"version_major": 4, "version_minor": 6, "version_patch": 3},
    "classes": [
        {"name": "Object"},
        {"name": "Node", "inherits": "Object"},
        {"name": "CanvasItem", "inherits": "Node"},
        {"name": "Node2D", "inherits": "CanvasItem"},
        {"name": "Sprite2D", "inherits": "Node2D"},
        {"name": "Control", "inherits": "CanvasItem"}
    ]
}"#;

/// The consumer script, attached to `main.tscn`'s root. One access per line so a test can point at
/// a line and get exactly one shape.
const MAIN_GD: &str = "extends Node2D\n\n\
                       func f():\n\
                       \t$Sprite\n\
                       \t$Health\n\
                       \t%Special\n\
                       \tget_node(\"Sprite\")\n\
                       \t$/root/Elsewhere\n";

/// A project whose `main.tscn` attaches `main.gd` at the root, with a native-only child (`Sprite`, a
/// `Sprite2D`), a script-carrying child (`Health`, running `health.gd` = `class_name Health`), and a
/// unique-named `Control` (`%Special`).
fn scene_project() -> TempProject {
    let p = TempProject::new();
    p.write(
        "project.godot",
        "config_version=5\n\n[application]\nconfig/features=PackedStringArray(\"4.6\")\n",
    );
    p.write("extension_api.json", API);
    p.write("health.gd", "class_name Health\nextends Node2D\n");
    p.write("main.gd", MAIN_GD);
    p.write(
        "main.tscn",
        "[gd_scene format=3]\n\
         [ext_resource type=\"Script\" path=\"res://main.gd\" id=\"1\"]\n\
         [ext_resource type=\"Script\" path=\"res://health.gd\" id=\"2\"]\n\
         [node name=\"Root\" type=\"Node2D\"]\n\
         script = ExtResource(\"1\")\n\
         [node name=\"Sprite\" type=\"Sprite2D\" parent=\".\"]\n\
         [node name=\"Health\" type=\"Node2D\" parent=\".\"]\n\
         script = ExtResource(\"2\")\n\
         [node name=\"Special\" type=\"Control\" parent=\".\"]\n\
         unique_name_in_owner = true\n",
    );
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

    let init = InitializeParams {
        capabilities: ClientCapabilities::default(),
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

/// The hover markdown at `pos` (asserting a non-null hover came back).
fn hover_md(client: &Connection, id: i32, uri: &Uri, pos: Position) -> String {
    client
        .sender
        .send(request(
            id,
            "textDocument/hover",
            serde_json::json!({
                "textDocument": { "uri": uri.as_str() },
                "position": { "line": pos.line, "character": pos.character },
            }),
        ))
        .unwrap();
    let resp = recv_response(client);
    assert!(resp.error.is_none(), "hover errored: {:?}", resp.error);
    let raw = resp.result.expect("hover result");
    assert!(!raw.is_null(), "expected a hover at {pos:?}, got null");
    let hover: Hover = serde_json::from_value(raw).expect("Hover deserializes");
    match hover.contents {
        HoverContents::Markup(MarkupContent { value, .. }) => value,
        other => panic!("expected markup hover contents, got {other:?}"),
    }
}

/// The raw `textDocument/definition` result at `pos`.
fn def_raw(client: &Connection, id: i32, uri: &Uri, pos: Position) -> serde_json::Value {
    client
        .sender
        .send(request(
            id,
            "textDocument/definition",
            serde_json::json!({
                "textDocument": { "uri": uri.as_str() },
                "position": { "line": pos.line, "character": pos.character },
            }),
        ))
        .unwrap();
    let resp = recv_response(client);
    assert!(resp.error.is_none(), "definition errored: {:?}", resp.error);
    resp.result.expect("definition result")
}

fn type_def_raw(client: &Connection, id: i32, uri: &Uri, pos: Position) -> serde_json::Value {
    client
        .sender
        .send(request(
            id,
            "textDocument/typeDefinition",
            serde_json::json!({
                "textDocument": { "uri": uri.as_str() },
                "position": { "line": pos.line, "character": pos.character },
            }),
        ))
        .unwrap();
    let resp = recv_response(client);
    assert!(
        resp.error.is_none(),
        "typeDefinition errored: {:?}",
        resp.error
    );
    resp.result.expect("typeDefinition result")
}

fn shutdown(client: &Connection, server_thread: std::thread::JoinHandle<anyhow::Result<()>>) {
    common::shutdown(client, server_thread);
}

// ===================================================================================================
// hover — the precise answers.
// ===================================================================================================

/// `$Sprite` hovers as `Sprite2D` (the scene child's engine class), not the analyzer's bare `Node`.
#[test]
fn hover_on_relative_access_shows_the_scene_node_class() {
    let p = scene_project();
    let uri = file_uri(&p.root.join("main.gd"));
    let (client, server_thread) = boot(&p, &uri, MAIN_GD);

    // `\t$Sprite` is line 3; the cursor sits on the path text.
    let md = hover_md(&client, 10, &uri, Position::new(3, 4));
    assert!(
        md.contains("Sprite2D"),
        "expected the scene-precise class, got {md:?}"
    );

    shutdown(&client, server_thread);
}

/// A node carrying a script answers with the SCRIPT's `class_name` — the more precise of the two
/// (the node is a `Node2D` running `class_name Health`).
#[test]
fn hover_on_scripted_node_shows_the_attached_scripts_class_name() {
    let p = scene_project();
    let uri = file_uri(&p.root.join("main.gd"));
    let (client, server_thread) = boot(&p, &uri, MAIN_GD);

    // `\t$Health` is line 4.
    let md = hover_md(&client, 11, &uri, Position::new(4, 4));
    assert!(
        md.contains("Health"),
        "expected the attached script's class_name, got {md:?}"
    );

    shutdown(&client, server_thread);
}

/// `%Special` resolves through the scene's owner-scoped unique-name table.
#[test]
fn hover_on_unique_name_access_resolves_through_the_owner_table() {
    let p = scene_project();
    let uri = file_uri(&p.root.join("main.gd"));
    let (client, server_thread) = boot(&p, &uri, MAIN_GD);

    // `\t%Special` is line 5.
    let md = hover_md(&client, 12, &uri, Position::new(5, 4));
    assert!(
        md.contains("Control"),
        "expected the unique node's class, got {md:?}"
    );

    shutdown(&client, server_thread);
}

/// `get_node("Sprite")` is the same access spelled long-hand, and resolves identically.
#[test]
fn hover_on_get_node_literal_resolves_like_the_dollar_form() {
    let p = scene_project();
    let uri = file_uri(&p.root.join("main.gd"));
    let (client, server_thread) = boot(&p, &uri, MAIN_GD);

    // `\tget_node("Sprite")` is line 6; the cursor sits inside the literal.
    let md = hover_md(&client, 13, &uri, Position::new(6, 12));
    assert!(
        md.contains("Sprite2D"),
        "expected the scene-precise class, got {md:?}"
    );

    shutdown(&client, server_thread);
}

// ===================================================================================================
// hover — the refusals (no false positives).
// ===================================================================================================

/// An ABSOLUTE `$/root/…` path is resolved against the RUNNING scene tree, which a parsed `.tscn`
/// cannot stand in for — so the answer stays the analyzer's bare `Node`.
#[test]
fn hover_on_absolute_path_falls_back_to_bare_node() {
    let p = scene_project();
    let uri = file_uri(&p.root.join("main.gd"));
    let (client, server_thread) = boot(&p, &uri, MAIN_GD);

    // `\t$/root/Elsewhere` is line 7.
    let md = hover_md(&client, 14, &uri, Position::new(7, 4));
    assert!(
        md.contains("Node") && !md.contains("Node2D") && !md.contains("Sprite2D"),
        "an absolute path must stay bare `Node`, got {md:?}"
    );

    shutdown(&client, server_thread);
}

/// A script NO scene attaches has no node to resolve against (W10: never a project-wide guess), so
/// the access stays bare `Node`.
#[test]
fn hover_in_a_scene_less_script_falls_back_to_bare_node() {
    let p = scene_project();
    let src = "extends Node2D\n\nfunc f():\n\t$Sprite\n";
    p.write("orphan.gd", src);
    let uri = file_uri(&p.root.join("orphan.gd"));
    let (client, server_thread) = boot(&p, &uri, src);

    let md = hover_md(&client, 15, &uri, Position::new(3, 4));
    assert!(
        md.contains("Node") && !md.contains("Sprite2D"),
        "a scene-less script must stay bare `Node`, got {md:?}"
    );

    shutdown(&client, server_thread);
}

/// Two scenes attach the same script and resolve `$Thing` to DIFFERENT classes: the answer must be
/// the bare `Node` both scenes agree on, never one scene's guess. This is the property that keeps a
/// precise hover honest for a shared script.
#[test]
fn hover_with_disagreeing_scenes_falls_back_to_bare_node() {
    let p = scene_project();
    let src = "extends Node2D\n\nfunc f():\n\t$Thing\n";
    p.write("shared.gd", src);
    p.write(
        "a.tscn",
        "[gd_scene format=3]\n\
         [ext_resource type=\"Script\" path=\"res://shared.gd\" id=\"1\"]\n\
         [node name=\"Root\" type=\"Node2D\"]\n\
         script = ExtResource(\"1\")\n\
         [node name=\"Thing\" type=\"Sprite2D\" parent=\".\"]\n",
    );
    p.write(
        "b.tscn",
        "[gd_scene format=3]\n\
         [ext_resource type=\"Script\" path=\"res://shared.gd\" id=\"1\"]\n\
         [node name=\"Root\" type=\"Node2D\"]\n\
         script = ExtResource(\"1\")\n\
         [node name=\"Thing\" type=\"Control\" parent=\".\"]\n",
    );
    let uri = file_uri(&p.root.join("shared.gd"));
    let (client, server_thread) = boot(&p, &uri, src);

    let md = hover_md(&client, 16, &uri, Position::new(3, 4));
    assert!(
        !md.contains("Sprite2D") && !md.contains("Control"),
        "disagreeing scenes must not pick a side, got {md:?}"
    );

    shutdown(&client, server_thread);
}

// ===================================================================================================
// definition / typeDefinition.
// ===================================================================================================

/// `definition` on `$Health` jumps to the attached script's `class_name` site — the access carries
/// no identifier, so before #125 this walk ended in `null`.
#[test]
fn definition_on_scripted_node_jumps_to_the_declaring_script() {
    let p = scene_project();
    let uri = file_uri(&p.root.join("main.gd"));
    let (client, server_thread) = boot(&p, &uri, MAIN_GD);

    let raw = def_raw(&client, 17, &uri, Position::new(4, 4));
    assert!(!raw.is_null(), "expected a definition for `$Health`");
    let loc: GotoDefinitionResponse =
        serde_json::from_value(raw).expect("GotoDefinitionResponse deserializes");
    let GotoDefinitionResponse::Scalar(loc) = loc else {
        panic!("expected a single location");
    };
    assert!(
        loc.uri.as_str().ends_with("health.gd"),
        "expected a jump into health.gd, got {}",
        loc.uri.as_str()
    );
    // `class_name Health` is line 0; the anchor is the class-name identifier.
    assert_eq!(loc.range.start.line, 0);

    shutdown(&client, server_thread);
}

/// `typeDefinition` answers the same access with the same target (the access IS its type).
#[test]
fn type_definition_on_scripted_node_matches_definition() {
    let p = scene_project();
    let uri = file_uri(&p.root.join("main.gd"));
    let (client, server_thread) = boot(&p, &uri, MAIN_GD);

    let raw = type_def_raw(&client, 18, &uri, Position::new(4, 4));
    assert!(!raw.is_null(), "expected a typeDefinition for `$Health`");
    let loc: GotoDefinitionResponse = serde_json::from_value(raw).expect("response deserializes");
    let GotoDefinitionResponse::Scalar(loc) = loc else {
        panic!("expected a single location");
    };
    assert!(loc.uri.as_str().ends_with("health.gd"));

    shutdown(&client, server_thread);
}

/// A scene-less script's `$Sprite` has no precise target, so `definition` stays `null` — the arm
/// must not fall back to jumping somewhere plausible.
#[test]
fn definition_in_a_scene_less_script_is_null() {
    let p = scene_project();
    let src = "extends Node2D\n\nfunc f():\n\t$Sprite\n";
    p.write("orphan2.gd", src);
    let uri = file_uri(&p.root.join("orphan2.gd"));
    let (client, server_thread) = boot(&p, &uri, src);

    let raw = def_raw(&client, 19, &uri, Position::new(3, 4));
    assert!(raw.is_null(), "expected null, got {raw}");

    shutdown(&client, server_thread);
}
