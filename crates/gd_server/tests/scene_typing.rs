//! M11 Phase 2 — **no-false-positive regression guard** for `$`/`%` typing.
//!
//! `reduce_get_node` types a valid `$`/`%` access as a hard `NATIVE Node` — exactly what Godot's
//! frontend produces (`gdscript_analyzer.cpp:3882-3886`). Godot never reads the `.tscn` for the
//! analyzer's *type*, so a `$`/`%` access is the bare `Node` base, NOT the scene-precise node class,
//! and Godot TOLERATES sibling/subtype downcasts off it that a precise type would reject. Precise
//! scene-derived node types are a navigation-only goal (precise HOVER/DEFINITION/COMPLETION) kept
//! OUT of the diagnostic path; the scene-resolution seam (`scene_node_facts`) serves those
//! navigation surfaces alone — `tests/scene_nav.rs` is their guard (`docs/02` §11).
//!
//! This guard is **direction-independent**: assigning/casting a scene-attached `$Health` (a `Node2D`
//! node) to an incompatible Node-derived sibling (`Control`) must produce NO error — a `Node` →
//! `Control` downcast Godot tolerates. It FAILS LOUDLY if anyone re-wires precise scene typing into
//! the diagnostic path through that navigation seam (which would make these Godot-tolerated downcasts
//! into `Invalid cast` / `Cannot assign` false positives). Keeping the real scene setup is what
//! gives the guard teeth against re-introduction.

mod common;

use common::TempProject;
use gd_server::config::InitializationOptions;
use gd_server::uri::{path_to_file_uri, CanonicalKey};
use gd_server::Workspace;

/// `Object ← Node ← CanvasItem ← Node2D`, plus a sibling `Control` under `CanvasItem` so a
/// `Control`-typed var is a genuine cross-hierarchy mismatch with a `Node2D` node — the exact shape
/// Godot tolerates for a `$` access (which it types bare `Node`) but a precise `Node2D` type rejects.
const API: &str = r#"{
    "header": {"version_major": 4, "version_minor": 6, "version_patch": 3},
    "classes": [
        {"name": "Object"},
        {"name": "Node", "inherits": "Object"},
        {"name": "CanvasItem", "inherits": "Node"},
        {"name": "Node2D", "inherits": "CanvasItem"},
        {"name": "Control", "inherits": "CanvasItem"}
    ]
}"#;

fn options(p: &TempProject) -> InitializationOptions {
    InitializationOptions::parse(Some(&serde_json::json!({
        "projectRoot": p.root.as_str(),
        "autoDumpExtensionApi": false,
        "extensionApiPath": p.root.join("extension_api.json").as_str(),
    })))
}

fn key_for(p: &TempProject, rel: &str) -> CanonicalKey {
    let uri = path_to_file_uri(&p.root.join(rel)).expect("valid file uri");
    CanonicalKey::for_uri(&uri)
}

fn diags_of(ws: &mut Workspace, p: &TempProject, rel: &str, src: &str) -> Vec<String> {
    let key = key_for(p, rel);
    let path = p.root.join(rel);
    let parsed = gd_syntax::parse(src);
    ws.analyze(&key, &path, &parsed.tree, src)
        .diagnostics
        .iter()
        .map(|d| d.message().to_owned())
        .collect()
}

/// A `.gd` attached to a scene root whose child `Health` is a `Node2D`: assigning `$Health` to a
/// `Control` var, and passing it to a `Control` parameter, must NOT error — Godot tolerates these
/// (it types `$Health` bare `Node`). Re-wiring precise `Node2D` typing would make both fail; this
/// guard catches that regression.
#[test]
fn dollar_access_does_not_false_positive_on_sibling_downcast() {
    let p = TempProject::new();
    p.write("project.godot", "config_version=5\n");
    p.write("extension_api.json", API);
    p.write(
        "player.tscn",
        r#"[gd_scene format=3]
[ext_resource type="Script" path="res://player.gd" id="1"]
[node name="Root" type="Node2D"]
script = ExtResource("1")
[node name="Health" type="Node2D" parent="."]
[node name="Special" type="Node2D" parent="."]
unique_name_in_owner = true
"#,
    );
    // Sibling-downcast assignment + typed-arg pass, for both `$relative` and `%unique`. Godot: clean.
    let src = "\
extends Node2D

func wants(_c: Control) -> void:
\tpass

func f():
\tvar c: Control = $Health
\tvar u: Control = %Special
\twants($Health)
\tprint(c, u)
";
    p.write("player.gd", src);

    let mut ws = Workspace::load(&p.root, &options(&p));
    let diags = diags_of(&mut ws, &p, "player.gd", src);
    let false_positives: Vec<&String> = diags
        .iter()
        .filter(|m| {
            m.contains("Cannot assign a value of type Node2D")
                || (m.contains("argument 1 should be") && m.contains("Node2D"))
        })
        .collect();
    assert!(
        false_positives.is_empty(),
        "a scene-attached `$Health`/`%Special` (Node2D) assigned/passed to a sibling `Control` must \
         NOT error — Godot tolerates the downcast (it types `$` bare Node). Re-introducing precise \
         scene typing into the diagnostic path is a false positive (release blocker). Got: {diags:?}"
    );
}

/// `$Health as Control` (an explicit cast to a sibling type) must NOT error, even with the real
/// scene present: `$Health` is bare `Node`, so `Node as Control` is a valid downcast Godot accepts.
/// A precise `Node2D` type would make `Node2D as Control` an `Invalid cast` — the same regression the
/// downcast guard catches, here on the cast path.
#[test]
fn dollar_cast_to_sibling_does_not_false_positive() {
    let p = TempProject::new();
    p.write("project.godot", "config_version=5\n");
    p.write("extension_api.json", API);
    p.write(
        "player.tscn",
        r#"[gd_scene format=3]
[ext_resource type="Script" path="res://player.gd" id="1"]
[node name="Root" type="Node2D"]
script = ExtResource("1")
[node name="Health" type="Node2D" parent="."]
"#,
    );
    let src = "\
extends Node2D

func f():
\tvar c = $Health as Control
\tprint(c)
";
    p.write("player.gd", src);

    let mut ws = Workspace::load(&p.root, &options(&p));
    let diags = diags_of(&mut ws, &p, "player.gd", src);
    let false_positives: Vec<&String> = diags
        .iter()
        .filter(|m| m.contains("Invalid cast"))
        .collect();
    assert!(
        false_positives.is_empty(),
        "`$Health as Control` (a `Node` → `Control` downcast) must NOT error — Godot tolerates it. \
         Re-introducing precise scene typing would make this `Invalid cast` (release blocker). \
         Got: {diags:?}"
    );
}
