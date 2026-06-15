//! M11 (#76): end-to-end scene-index lifecycle through the [`Workspace`] — cold build from disk,
//! live re-index on a `.tscn` change (the same `reindex_scene`/`remove_scene` the watcher reaction
//! path calls), and warm-start cache round-trip. Proves the `**/*.tscn` watcher glob's events are
//! actually consumed (not delivered-then-ignored) and that the scene index survives a warm restart.

mod common;

use gd_server::config::InitializationOptions;
use gd_server::Workspace;

fn options(root: &camino::Utf8Path) -> InitializationOptions {
    InitializationOptions::parse(Some(&serde_json::json!({
        "projectRoot": root.as_str(),
        "autoDumpExtensionApi": false,
    })))
}

const MAIN_TSCN: &str = r#"[gd_scene format=3 uid="uid://main"]
[ext_resource type="Script" path="res://main.gd" id="1"]
[ext_resource type="PackedScene" path="res://child.tscn" id="2"]
[node name="Root" type="Control"]
script = ExtResource("1")
[node name="Sub" parent="." instance=ExtResource("2")]
"#;

const CHILD_TSCN: &str = r#"[gd_scene format=3 uid="uid://child"]
[ext_resource type="Script" path="res://child.gd" id="1"]
[node name="ChildRoot" type="Panel"]
script = ExtResource("1")
[node name="Special" type="Button" parent="."]
unique_name_in_owner = true
"#;

#[test]
fn cold_load_builds_scene_index_from_disk() {
    let p = common::TempProject::new();
    p.write("project.godot", "config_version=5\n");
    p.write("main.gd", "extends Control\n");
    p.write("child.gd", "extends Panel\n");
    p.write("main.tscn", MAIN_TSCN);
    p.write("child.tscn", CHILD_TSCN);

    let ws = Workspace::load(&p.root, &options(&p.root));
    let scenes = ws.scenes();
    assert_eq!(scenes.len(), 2, "both .tscn files indexed at cold build");

    // Relations extracted from main.tscn.
    let main = scenes.scene("res://main.tscn").expect("main.tscn indexed");
    assert_eq!(main.root_script_path(), Some("res://main.gd"));
    assert_eq!(
        main.node_type("Sub"),
        Some(&gd_project::NodeType::Instanced(Some(
            "res://child.tscn".into()
        )))
    );
    // Reverse maps: child.gd is attached by child.tscn; child.tscn is instanced by main.tscn.
    let child_scenes: Vec<&str> = scenes.scenes_attaching_script("res://child.gd").collect();
    assert_eq!(child_scenes, vec!["res://child.tscn"]);
    let instancers: Vec<&str> = scenes.scenes_instancing("res://child.tscn").collect();
    assert_eq!(instancers, vec!["res://main.tscn"]);

    // Transitive scene→script set: editing child.tscn affects child.gd AND main.gd (its instancer).
    let affected = scenes.affected_scripts("res://child.tscn");
    assert!(affected.contains("res://child.gd"));
    assert!(affected.contains("res://main.gd"));
}

#[test]
fn reindex_scene_keeps_index_live_on_change() {
    let p = common::TempProject::new();
    p.write("project.godot", "config_version=5\n");
    p.write("child.gd", "extends Panel\n");
    p.write("child.tscn", CHILD_TSCN);

    let mut ws = Workspace::load(&p.root, &options(&p.root));
    let child_path = p.root.join("child.tscn");

    // Initially the unique node "Special" is reachable.
    assert!(ws
        .scenes()
        .scene("res://child.tscn")
        .unwrap()
        .node_by_unique_name("Special")
        .is_some());

    // Rewrite the scene: rename the unique node, change the root type. This is the on-disk edit a
    // watcher delivers; `reindex_scene` is exactly what `apply_reaction(Reaction::Scene{..})` calls.
    let edited = r#"[gd_scene format=3 uid="uid://child"]
[node name="NewRoot" type="VBoxContainer"]
[node name="Renamed" type="Button" parent="."]
unique_name_in_owner = true
"#;
    p.write("child.tscn", edited);
    ws.reindex_scene(&child_path);

    let scene = ws.scenes().scene("res://child.tscn").unwrap();
    // The new content is live: old unique name gone, new one present, root type updated.
    assert!(
        scene.node_by_unique_name("Special").is_none(),
        "stale unique node must be gone after re-index"
    );
    assert!(scene.node_by_unique_name("Renamed").is_some());
    assert_eq!(
        scene.node_type(""),
        Some(&gd_project::NodeType::Native("VBoxContainer".into()))
    );
}

#[test]
fn remove_scene_drops_it_from_index() {
    let p = common::TempProject::new();
    p.write("project.godot", "config_version=5\n");
    p.write("child.gd", "extends Panel\n");
    p.write("child.tscn", CHILD_TSCN);

    let mut ws = Workspace::load(&p.root, &options(&p.root));
    assert!(ws.scenes().scene("res://child.tscn").is_some());

    // Delete on disk + drive the delete reaction.
    let child_path = p.root.join("child.tscn");
    p.remove("child.tscn");
    ws.remove_scene(&child_path);

    assert!(ws.scenes().scene("res://child.tscn").is_none());
    // The reverse edge from child.gd is gone too.
    assert_eq!(
        ws.scenes()
            .scenes_attaching_script("res://child.gd")
            .count(),
        0
    );
}

#[test]
fn warm_start_round_trips_scene_index() {
    let p = common::TempProject::new();
    p.write("project.godot", "config_version=5\n");
    p.write("main.gd", "extends Control\n");
    p.write("child.gd", "extends Panel\n");
    p.write("main.tscn", MAIN_TSCN);
    p.write("child.tscn", CHILD_TSCN);

    // Cold load → save cache.
    let cold = Workspace::load(&p.root, &options(&p.root));
    assert_eq!(cold.scenes().len(), 2);
    cold.save_cache();

    // Warm load from the cache: the scene index must be restored with working query API +
    // rebuilt reverse maps (which are NOT persisted — they're derived on load).
    let warm = Workspace::load(&p.root, &options(&p.root));
    assert_eq!(warm.scenes().len(), 2, "warm load restores both scenes");
    let main = warm.scenes().scene("res://main.tscn").expect("main warm");
    assert_eq!(main.root_script_path(), Some("res://main.gd"));
    assert_eq!(
        main.node_type("Sub"),
        Some(&gd_project::NodeType::Instanced(Some(
            "res://child.tscn".into()
        )))
    );
    // Rebuilt reverse map works after warm load.
    let instancers: Vec<&str> = warm
        .scenes()
        .scenes_instancing("res://child.tscn")
        .collect();
    assert_eq!(instancers, vec!["res://main.tscn"]);
    // Unique-name lookup survives the round-trip.
    assert!(warm
        .scenes()
        .scene("res://child.tscn")
        .unwrap()
        .node_by_unique_name("Special")
        .is_some());
}

#[test]
fn reconcile_recovers_scene_drift() {
    // Finding from the M11 fusion review: the watcher-overflow (`need_rescan`) and disabled-watcher
    // liveness-tick recovery paths both route through `Workspace::reconcile`. This proves reconcile
    // stat-diffs `.tscn` (added/modified/removed), so a scene drifted while the watcher was
    // overflowed/off is recovered — not left stale (scenes had no reconcile backstop before).
    let p = common::TempProject::new();
    p.write("project.godot", "config_version=5\n");
    p.write("child.gd", "extends Panel\n");
    p.write("child.tscn", CHILD_TSCN);

    let mut ws = Workspace::load(&p.root, &options(&p.root));
    let no_open = std::collections::HashSet::default();

    // (a) MODIFIED while "watcher was off": change the scene on disk, then reconcile (no per-event
    // reindex_scene called — only reconcile).
    let edited = r#"[gd_scene format=3 uid="uid://child"]
[node name="ReconciledRoot" type="VBoxContainer"]
"#;
    p.write("child.tscn", edited);
    ws.reconcile(&no_open);
    assert_eq!(
        ws.scenes().scene("res://child.tscn").unwrap().node_type(""),
        Some(&gd_project::NodeType::Native("VBoxContainer".into())),
        "reconcile must recover a scene modified while the watcher was off"
    );

    // (b) ADDED while off: a brand-new scene appears, reconcile picks it up.
    p.write(
        "extra.tscn",
        "[gd_scene format=3]\n[node name=\"Extra\" type=\"Node\"]\n",
    );
    ws.reconcile(&no_open);
    assert!(
        ws.scenes().scene("res://extra.tscn").is_some(),
        "reconcile must discover a scene added while the watcher was off"
    );

    // (c) REMOVED while off: delete a scene, reconcile drops it from the index.
    p.remove("child.tscn");
    ws.reconcile(&no_open);
    assert!(
        ws.scenes().scene("res://child.tscn").is_none(),
        "reconcile must drop a scene deleted while the watcher was off"
    );
}

#[test]
fn warm_start_reparses_scene_changed_while_offline() {
    // A scene edited while gdls was off must be re-parsed by the warm-start stat-diff (scene
    // freshness rides the FileStat table, since the CacheKey doesn't move on a .tscn edit).
    let p = common::TempProject::new();
    p.write("project.godot", "config_version=5\n");
    p.write("child.gd", "extends Panel\n");
    p.write("child.tscn", CHILD_TSCN);

    let cold = Workspace::load(&p.root, &options(&p.root));
    cold.save_cache();
    drop(cold);

    // Edit the scene on disk *after* the cache was saved, bumping its size so the stat-diff fires
    // regardless of mtime granularity.
    let edited = r#"[gd_scene format=3 uid="uid://child"]
[node name="OfflineEditedRoot" type="HBoxContainer"]
[node name="X" type="Label" parent="."]
"#;
    p.write("child.tscn", edited);

    let warm = Workspace::load(&p.root, &options(&p.root));
    let scene = warm.scenes().scene("res://child.tscn").unwrap();
    assert_eq!(
        scene.node_type(""),
        Some(&gd_project::NodeType::Native("HBoxContainer".into())),
        "warm-start stat-diff must re-parse the offline-edited scene, not serve the stale cache"
    );
}
