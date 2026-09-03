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
        {"name": "Sprite2D", "inherits": "Node2D",
         "properties": [{"name": "flip_h", "type": "bool",
                         "setter": "set_flip_h", "getter": "is_flip_h"}],
         "methods": [{"name": "set_flip_h", "is_const": false, "is_static": false,
                      "is_vararg": false, "is_virtual": false, "hash": 1,
                      "arguments": [{"name": "enable", "type": "bool"}]}]},
        {"name": "Control", "inherits": "CanvasItem",
         "properties": [{"name": "tooltip_text", "type": "String",
                         "setter": "set_tooltip_text", "getter": "get_tooltip_text"}]}
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
                       \t$/root/Elsewhere\n\
                       \n\
                       @onready var sp := $Sprite\n\
                       @onready var lb := %Special\n\
                       @onready var h := $Health\n\
                       var typed: Node2D = $Sprite\n\
                       \n\
                       func g():\n\
                       \tsp.flip_h\n\
                       \tlb.tooltip_text\n\
                       \th.hp\n\
                       \ttyped.flip_h\n\
                       \tvar l = get_node(\"Sprite\")\n\
                       \tl.flip_h\n\
                       \n\
                       func shadowed(sp: Node2D):\n\
                       \tsp.flip_h\n";

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
    p.write(
        "health.gd",
        "class_name Health\nextends Node2D\nvar hp: int = 3\n",
    );
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

// ===================================================================================================
// #349 — the projection carries PAST the dot.
// ===================================================================================================

/// The completion labels at `pos`.
fn completion_labels(client: &Connection, id: i32, uri: &Uri, pos: Position) -> Vec<String> {
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
    let raw = resp.result.expect("completion result");
    let items = raw
        .get("items")
        .cloned()
        .unwrap_or(raw)
        .as_array()
        .cloned()
        .unwrap_or_default();
    items
        .iter()
        .filter_map(|i| i.get("label")?.as_str().map(str::to_string))
        .collect()
}

/// `$Sprite.` enumerates `Sprite2D`'s members. Before #349 the base fell back to the analyzer's
/// hard bare `Node`, so the one class the user had just been told the node was is the one class
/// whose members never showed up.
#[test]
fn completion_after_a_relative_access_offers_the_scene_classs_members() {
    let p = scene_project();
    let src = "extends Node2D\n\nfunc f():\n\t$Sprite.\n";
    let uri = file_uri(&p.root.join("main.gd"));
    let (client, server_thread) = boot(&p, &uri, src);

    let labels = completion_labels(&client, 20, &uri, Position::new(3, 9));
    assert!(
        labels.iter().any(|l| l == "flip_h"),
        "expected Sprite2D's members, got {labels:?}"
    );

    shutdown(&client, server_thread);
}

/// `%Special.` resolves through the owner-scoped unique-name table, same as the node hover does.
#[test]
fn completion_after_a_unique_name_access_offers_the_scene_classs_members() {
    let p = scene_project();
    let src = "extends Node2D\n\nfunc f():\n\t%Special.\n";
    let uri = file_uri(&p.root.join("main.gd"));
    let (client, server_thread) = boot(&p, &uri, src);

    let labels = completion_labels(&client, 21, &uri, Position::new(3, 10));
    assert!(
        labels.iter().any(|l| l == "tooltip_text"),
        "expected Control's members, got {labels:?}"
    );

    shutdown(&client, server_thread);
}

/// `get_node("Sprite").` is the same access spelled out, and gets the same answer.
#[test]
fn completion_after_a_get_node_call_offers_the_scene_classs_members() {
    let p = scene_project();
    let src = "extends Node2D\n\nfunc f():\n\tget_node(\"Sprite\").\n";
    let uri = file_uri(&p.root.join("main.gd"));
    let (client, server_thread) = boot(&p, &uri, src);

    let labels = completion_labels(&client, 22, &uri, Position::new(3, 20));
    assert!(
        labels.iter().any(|l| l == "flip_h"),
        "expected Sprite2D's members, got {labels:?}"
    );

    shutdown(&client, server_thread);
}

/// A node running a script answers with the SCRIPT's members — the projection hands back whatever
/// `scene_nav` resolved, so the script arm of the member walk takes over from there.
#[test]
fn completion_after_a_scripted_node_offers_the_scripts_members() {
    let p = scene_project();
    let src = "extends Node2D\n\nfunc f():\n\t$Health.\n";
    let uri = file_uri(&p.root.join("main.gd"));
    let (client, server_thread) = boot(&p, &uri, src);

    let labels = completion_labels(&client, 23, &uri, Position::new(3, 9));
    assert!(
        labels.iter().any(|l| l == "hp"),
        "expected health.gd's members, got {labels:?}"
    );

    shutdown(&client, server_thread);
}

/// Hover on the MEMBER read off a scene access names the precise declaring class, not `Variant`.
#[test]
fn hover_on_a_member_read_off_a_scene_access_names_the_precise_class() {
    let p = scene_project();
    let src = "extends Node2D\n\nfunc f():\n\t$Sprite.flip_h = true\n";
    let uri = file_uri(&p.root.join("main.gd"));
    let (client, server_thread) = boot(&p, &uri, src);

    let md = hover_md(&client, 24, &uri, Position::new(3, 13));
    assert!(
        md.contains("Sprite2D") && md.contains("flip_h"),
        "expected the Sprite2D property, got {md:?}"
    );

    shutdown(&client, server_thread);
}

/// Hover on a METHOD called off a scene access resolves the same way.
#[test]
fn hover_on_a_method_called_off_a_scene_access_names_the_precise_class() {
    let p = scene_project();
    let src = "extends Node2D\n\nfunc f():\n\t$Sprite.set_flip_h(true)\n";
    let uri = file_uri(&p.root.join("main.gd"));
    let (client, server_thread) = boot(&p, &uri, src);

    let md = hover_md(&client, 25, &uri, Position::new(3, 13));
    assert!(
        md.contains("Sprite2D") && md.contains("set_flip_h"),
        "expected the Sprite2D method, got {md:?}"
    );

    shutdown(&client, server_thread);
}

/// `definition` on the member jumps to the declaring class's stub, not nowhere.
#[test]
fn definition_on_a_member_read_off_a_scene_access_reaches_the_declaring_class() {
    let p = scene_project();
    let src = "extends Node2D\n\nfunc f():\n\t$Sprite.flip_h = true\n";
    let uri = file_uri(&p.root.join("main.gd"));
    let (client, server_thread) = boot(&p, &uri, src);

    let raw = def_raw(&client, 26, &uri, Position::new(3, 13));
    assert!(!raw.is_null(), "expected a definition for `$Sprite.flip_h`");
    let loc: GotoDefinitionResponse = serde_json::from_value(raw).expect("response deserializes");
    let GotoDefinitionResponse::Scalar(loc) = loc else {
        panic!("expected a single location");
    };
    assert!(
        loc.uri.as_str().ends_with("Sprite2D.gd"),
        "expected the Sprite2D stub, got {}",
        loc.uri.as_str()
    );

    shutdown(&client, server_thread);
}

/// The no-false-positives half: a script no scene attaches has no precise target, so the member
/// surface stays on the analyzer's bare `Node` rather than guessing a class.
#[test]
fn completion_in_a_scene_less_script_stays_on_bare_node() {
    let p = scene_project();
    let src = "extends Node2D\n\nfunc f():\n\t$Sprite.\n";
    p.write("orphan3.gd", src);
    let uri = file_uri(&p.root.join("orphan3.gd"));
    let (client, server_thread) = boot(&p, &uri, src);

    let labels = completion_labels(&client, 27, &uri, Position::new(3, 9));
    assert!(
        !labels.iter().any(|l| l == "flip_h"),
        "a scene-less script must not guess a class, got {labels:?}"
    );

    shutdown(&client, server_thread);
}

/// A `$X` buried INSIDE a larger base expression is not the base — the end-anchored match must not
/// let it hijack the enclosing expression's own type.
#[test]
fn a_nested_scene_access_does_not_hijack_the_enclosing_base() {
    let p = scene_project();
    let src = "extends Node2D\n\nfunc f():\n\tvar a := [$Sprite]\n\ta.\n";
    let uri = file_uri(&p.root.join("main.gd"));
    let (client, server_thread) = boot(&p, &uri, src);

    let labels = completion_labels(&client, 28, &uri, Position::new(4, 3));
    assert!(
        !labels.iter().any(|l| l == "flip_h"),
        "an Array base must not answer with Sprite2D's members, got {labels:?}"
    );

    shutdown(&client, server_thread);
}

// ===================================================================================================
// #458 — one hop through a variable that holds the access.
// ===================================================================================================

/// The line index of the single line containing `needle` in [`MAIN_GD`], and the column just past
/// the first character of `needle`'s member. Keeps these tests off hard-coded line numbers, which
/// the fixture has drifted under before.
fn at(needle: &str, member: &str) -> Position {
    at_nth(needle, member, false)
}

/// [`at`] against the LAST line matching `needle` — `\tsp.flip_h` appears in both `g()` and the
/// shadowing function, and the two must answer differently.
fn at_last(needle: &str, member: &str) -> Position {
    at_nth(needle, member, true)
}

fn at_nth(needle: &str, member: &str, last: bool) -> Position {
    let matches: Vec<usize> = MAIN_GD
        .lines()
        .enumerate()
        .filter(|(_, l)| l.contains(needle))
        .map(|(i, _)| i)
        .collect();
    let line = *(if last {
        matches.last()
    } else {
        matches.first()
    })
    .unwrap_or_else(|| panic!("{needle} in MAIN_GD"));
    let text = MAIN_GD.lines().nth(line).unwrap();
    let character = text.find(member).expect("member on that line") as u32 + 1;
    Position {
        line: line as u32,
        character,
    }
}

/// `@onready var sp := $Sprite` then `sp.flip_h` — the single most common Godot idiom. The member
/// read off `sp` hovers as `Sprite2D`'s property, where it used to answer bare `Node`'s surface.
#[test]
fn hover_past_a_variable_holding_an_access_uses_the_scene_type() {
    let p = scene_project();
    let uri = file_uri(&p.root.join("main.gd"));
    let (client, server_thread) = boot(&p, &uri, MAIN_GD);

    let md = hover_md(&client, 10, &uri, at("\tsp.flip_h", "flip_h"));
    assert!(
        md.contains("Sprite2D") && md.contains("flip_h"),
        "expected the Sprite2D property hover, got: {md}"
    );

    // The unique-name spelling reaches the same seam.
    let md = hover_md(&client, 11, &uri, at("lb.tooltip_text", "tooltip_text"));
    assert!(
        md.contains("Control") && md.contains("tooltip_text"),
        "expected the Control property hover, got: {md}"
    );

    shutdown(&client, server_thread);
}

/// The same hop through a LOCAL, written with the `get_node("…")` call form.
#[test]
fn hover_past_a_local_holding_a_get_node_call() {
    let p = scene_project();
    let uri = file_uri(&p.root.join("main.gd"));
    let (client, server_thread) = boot(&p, &uri, MAIN_GD);

    let md = hover_md(&client, 10, &uri, at("\tl.flip_h", "flip_h"));
    assert!(
        md.contains("Sprite2D") && md.contains("flip_h"),
        "expected the Sprite2D property hover, got: {md}"
    );

    shutdown(&client, server_thread);
}

/// A node carrying a script resolves to that script, so a member the script declares jumps into its
/// file rather than answering nothing.
#[test]
fn definition_past_a_variable_holding_a_scripted_node() {
    let p = scene_project();
    let uri = file_uri(&p.root.join("main.gd"));
    let (client, server_thread) = boot(&p, &uri, MAIN_GD);

    let raw = def_raw(&client, 10, &uri, at("\th.hp", "hp"));
    let text = raw.to_string();
    assert!(
        text.contains("health.gd"),
        "expected a jump into health.gd, got: {text}"
    );

    shutdown(&client, server_thread);
}

/// The two refusals, both of which must fall back rather than answer. An explicitly annotated
/// declaration keeps the author's type ("Annotated type takes precedence"), and a parameter that
/// shadows the member is a different symbol entirely.
#[test]
fn an_annotation_or_a_shadow_falls_back_to_the_analyzer() {
    let p = scene_project();
    let uri = file_uri(&p.root.join("main.gd"));
    let (client, server_thread) = boot(&p, &uri, MAIN_GD);

    // `var typed: Node2D = $Sprite` — `flip_h` is a Sprite2D member, so the annotated Node2D has
    // nothing to answer with, and no scene type may rescue it.
    let raw = def_raw(&client, 10, &uri, at("typed.flip_h", "flip_h"));
    assert!(
        raw.is_null() || !raw.to_string().contains("Sprite2D"),
        "an annotated declaration must not pick up the scene type, got: {raw}"
    );

    // `func shadowed(sp: Node2D)` — the parameter shadows the member of the same name, so the
    // SECOND `sp.flip_h` (inside that function) must not answer the way the first one does.
    let raw = def_raw(&client, 11, &uri, at_last("\tsp.flip_h", "flip_h"));
    let text = raw.to_string();
    assert!(
        !text.contains("Sprite2D"),
        "a shadowing parameter must not pick up the member's scene type, got: {text}"
    );

    shutdown(&client, server_thread);
}

/// Completion after `sp.` offers `Sprite2D`'s surface. This is the payoff line of the whole hop:
/// bare `Node` has hundreds of members and none of them are the one the author wants.
#[test]
fn completion_past_a_variable_holding_an_access_offers_the_scene_surface() {
    let p = scene_project();
    let uri = file_uri(&p.root.join("main.gd"));
    let (client, server_thread) = boot(&p, &uri, MAIN_GD);

    let pos = at("\tsp.flip_h", "flip_h");
    client
        .sender
        .send(request(
            10,
            "textDocument/completion",
            serde_json::json!({
                "textDocument": { "uri": uri.as_str() },
                "position": { "line": pos.line, "character": pos.character - 1 },
            }),
        ))
        .unwrap();
    let resp = recv_response(&client);
    assert!(resp.error.is_none(), "completion errored: {:?}", resp.error);
    let raw = resp.result.expect("completion result");
    let labels: Vec<String> = raw
        .get("items")
        .unwrap_or(&raw)
        .as_array()
        .expect("completion items")
        .iter()
        .filter_map(|i| i.get("label")?.as_str().map(str::to_owned))
        .collect();
    assert!(
        labels.iter().any(|l| l == "flip_h"),
        "Sprite2D's own property must be offered; got {labels:?}"
    );

    shutdown(&client, server_thread);
}

/// signatureHelp on a method that only `Sprite2D` declares. The hop feeds the same seam, so the
/// call's signature is found where bare `Node` had nothing to offer.
#[test]
fn signature_help_past_a_variable_holding_an_access() {
    let p = scene_project();
    let uri = file_uri(&p.root.join("main.gd"));
    let src = format!("{MAIN_GD}\nfunc h():\n\tsp.set_flip_h(\n");
    let (client, server_thread) = boot(&p, &uri, &src);

    let line = src.lines().count() as u32 - 1;
    let character = src.lines().last().unwrap().len() as u32;
    client
        .sender
        .send(request(
            10,
            "textDocument/signatureHelp",
            serde_json::json!({
                "textDocument": { "uri": uri.as_str() },
                "position": { "line": line, "character": character },
            }),
        ))
        .unwrap();
    let resp = recv_response(&client);
    assert!(
        resp.error.is_none(),
        "signatureHelp errored: {:?}",
        resp.error
    );
    let raw = resp.result.expect("signatureHelp result");
    assert!(
        raw.to_string().contains("set_flip_h"),
        "expected Sprite2D's set_flip_h signature, got: {raw}"
    );

    shutdown(&client, server_thread);
}

// ===================================================================================================
// #589 — the projection reaches the BINDING itself.
// ===================================================================================================

/// [`at`] without the `+1` — a one-letter binding's `find` is already its last character, so the
/// +1 would land past the identifier (where the cursor gate correctly declines).
fn at_name(needle: &str, name: &str) -> Position {
    let mut pos = at(needle, name);
    pos.character -= 1;
    pos
}

/// Hover on the member declaration `@onready var sp := $Sprite` names the scene type, where it
/// used to render the analyzer's bare `var sp: Node` — contradicting the precise card the access
/// itself shows and the members `sp.` offers one character later.
#[test]
fn hover_on_a_binding_declared_from_an_access_shows_the_scene_type() {
    let p = scene_project();
    let uri = file_uri(&p.root.join("main.gd"));
    let (client, server_thread) = boot(&p, &uri, MAIN_GD);

    let md = hover_md(&client, 10, &uri, at("@onready var sp := $Sprite", "sp"));
    assert!(
        md.contains("Sprite2D"),
        "the binding must carry the access's scene type, got {md:?}"
    );

    shutdown(&client, server_thread);
}

/// Hover on a bare USE of such a binding — the base of `h.hp` — answers with the SCRIPT class the
/// access reaches, matching what definition on `h.hp` already jumps to.
#[test]
fn hover_on_a_bare_use_of_a_scene_typed_binding() {
    let p = scene_project();
    let uri = file_uri(&p.root.join("main.gd"));
    let (client, server_thread) = boot(&p, &uri, MAIN_GD);

    let md = hover_md(&client, 10, &uri, at_name("\th.hp", "h"));
    assert!(
        md.contains("Health"),
        "expected the attached script's class on the base, got {md:?}"
    );

    shutdown(&client, server_thread);
}

/// The hop works for a LOCAL declared from a `get_node("…")` call, on the declaration name itself.
#[test]
fn hover_on_a_local_declared_from_a_get_node_call() {
    let p = scene_project();
    let uri = file_uri(&p.root.join("main.gd"));
    let (client, server_thread) = boot(&p, &uri, MAIN_GD);

    let md = hover_md(
        &client,
        10,
        &uri,
        at_name("var l = get_node(\"Sprite\")", "l"),
    );
    assert!(
        md.contains("Sprite2D"),
        "the local's declaration must carry the call's scene type, got {md:?}"
    );

    shutdown(&client, server_thread);
}

/// typeDefinition on the binding jumps to the scene type's declaration — the Sprite2D stub for
/// `sp` — instead of bare `Node`'s.
#[test]
fn type_definition_on_a_scene_typed_binding_reaches_the_scene_class() {
    let p = scene_project();
    let uri = file_uri(&p.root.join("main.gd"));
    let (client, server_thread) = boot(&p, &uri, MAIN_GD);

    let raw = type_def_raw(&client, 10, &uri, at("@onready var sp := $Sprite", "sp"));
    assert!(!raw.is_null(), "expected a typeDefinition for `sp`");
    let loc: GotoDefinitionResponse = serde_json::from_value(raw).expect("response deserializes");
    let GotoDefinitionResponse::Scalar(loc) = loc else {
        panic!("expected a single location");
    };
    assert!(
        loc.uri.as_str().ends_with("Sprite2D.gd"),
        "expected the Sprite2D stub, got {}",
        loc.uri.as_str()
    );

    shutdown(&client, server_thread);
}

/// typeDefinition through the scripted-node binding reaches the attached script's file.
#[test]
fn type_definition_on_a_scripted_binding_reaches_the_script() {
    let p = scene_project();
    let uri = file_uri(&p.root.join("main.gd"));
    let (client, server_thread) = boot(&p, &uri, MAIN_GD);

    let raw = type_def_raw(&client, 10, &uri, at_name("@onready var h := $Health", "h"));
    let text = raw.to_string();
    assert!(
        text.contains("health.gd"),
        "expected a jump into health.gd, got: {text}"
    );

    shutdown(&client, server_thread);
}

/// The two refusals: an explicitly annotated declaration keeps the author's type (no scene
/// rescue), and a binding in a scene-less script stays bare `Node` (no false positive).
#[test]
fn an_annotated_or_sceneless_binding_stays_on_the_analyzer() {
    let p = scene_project();
    let uri = file_uri(&p.root.join("main.gd"));
    let (client, server_thread) = boot(&p, &uri, MAIN_GD);

    // `var typed: Node2D = $Sprite` — the annotation outranks the scene, so the hover must not
    // name the node's Sprite2D.
    let md = hover_md(
        &client,
        10,
        &uri,
        at("var typed: Node2D = $Sprite", "typed"),
    );
    assert!(
        !md.contains("Sprite2D"),
        "an annotated binding must not pick up the scene type, got {md:?}"
    );

    shutdown(&client, server_thread);

    // Scene-less script: the hop has no scene to ask, so the answer stays the analyzer's Node.
    let src = "extends Node2D\n\n@onready var sp := $Sprite\nfunc g():\n\tsp.flip_h\n";
    p.write("orphan4.gd", src);
    let uri2 = file_uri(&p.root.join("orphan4.gd"));
    let (client2, server_thread2) = boot(&p, &uri2, src);
    let md = hover_md(&client2, 10, &uri2, Position::new(2, 13));
    assert!(
        !md.contains("Sprite2D"),
        "a scene-less binding must stay bare, got {md:?}"
    );

    shutdown(&client2, server_thread2);
}
