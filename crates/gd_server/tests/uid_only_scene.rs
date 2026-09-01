//! #484: a scene whose `[ext_resource]` names its script only by `uid://` resolves end to end.
//!
//! Godot drops the `path` from an ext-resource entry once the resource has a `.uid` sidecar, so a
//! scene saved by a recent editor can name its script purely by uid. The [`SceneIndex`] resolves
//! those at insert time off the project's uid map, and every event that can change that map — a
//! sidecar appearing, changing, or being deleted — re-reads the scenes that depend on it.

mod common;

use gd_server::config::InitializationOptions;
use gd_server::Workspace;

fn options(root: &camino::Utf8Path) -> InitializationOptions {
    InitializationOptions::parse(Some(&serde_json::json!({
        "projectRoot": root.as_str(),
        "autoDumpExtensionApi": false,
    })))
}

fn project(with_sidecar: bool) -> common::TempProject {
    let p = common::TempProject::new();
    p.write(
        "project.godot",
        "config_version=5\n\n[application]\nconfig/features=PackedStringArray(\"4.7\")\n",
    );
    p.write("src/hero.gd", "class_name Hero\nextends Node2D\n");
    if with_sidecar {
        p.write("src/hero.gd.uid", "uid://c484hero\n");
    }
    p.write(
        "scenes/host.tscn",
        "[gd_scene load_steps=2 format=3 uid=\"uid://c484host\"]\n\n\
         [ext_resource type=\"Script\" uid=\"uid://c484hero\" id=\"1\"]\n\n\
         [node name=\"Root\" type=\"Node2D\"]\n\
         script = ExtResource(\"1\")\n\n\
         [node name=\"Child\" type=\"Label\" parent=\".\"]\n",
    );
    p
}

/// Warm cold-load: the sidecar is on disk before the workspace is built, so the uid-only entry
/// resolves to the script and the reverse map finds the scene from the script side.
#[test]
fn cold_load_resolves_a_uid_only_script_entry() {
    let p = project(true);
    let ws = Workspace::load(&p.root, &options(&p.root));

    let scene = ws
        .scenes()
        .scene("res://scenes/host.tscn")
        .expect("indexed");
    assert_eq!(
        scene.root_script_path(),
        Some("res://src/hero.gd"),
        "the uid-only ext_resource resolves to the script it names"
    );
    let attaching: Vec<&str> = ws
        .scenes()
        .scenes_attaching_script("res://src/hero.gd")
        .collect();
    assert_eq!(attaching, vec!["res://scenes/host.tscn"]);
}

/// No sidecar means no mapping, so the link is simply absent — never the raw `uid://` leaking out
/// where a `res://` path belongs.
#[test]
fn a_missing_sidecar_leaves_the_link_absent_not_wrong() {
    let p = project(false);
    let ws = Workspace::load(&p.root, &options(&p.root));

    let scene = ws
        .scenes()
        .scene("res://scenes/host.tscn")
        .expect("indexed");
    assert_eq!(scene.root_script_path(), None);
    assert!(
        scene.node_at("Child").is_some(),
        "the node tree survives an unresolvable uid"
    );
}

/// A sidecar written mid-session re-resolves the scenes that named its uid, with no restart.
#[test]
fn a_sidecar_appearing_re_resolves_the_scene() {
    let p = project(false);
    let mut ws = Workspace::load(&p.root, &options(&p.root));
    assert_eq!(
        ws.scenes()
            .scene("res://scenes/host.tscn")
            .and_then(|s| s.root_script_path()),
        None
    );

    p.write("src/hero.gd.uid", "uid://c484hero\n");
    ws.sync_uid_declaration(&p.root.join("src/hero.gd.uid"));

    assert_eq!(
        ws.scenes()
            .scene("res://scenes/host.tscn")
            .and_then(|s| s.root_script_path()),
        Some("res://src/hero.gd"),
        "the scene picks up the script the moment its uid becomes resolvable"
    );
}

/// Deleting the sidecar takes the link away again rather than leaving a stale target behind.
#[test]
fn deleting_the_sidecar_unresolves_the_scene() {
    let p = project(true);
    let mut ws = Workspace::load(&p.root, &options(&p.root));
    assert_eq!(
        ws.scenes()
            .scene("res://scenes/host.tscn")
            .and_then(|s| s.root_script_path()),
        Some("res://src/hero.gd")
    );

    let sidecar = p.root.join("src/hero.gd.uid");
    p.remove("src/hero.gd.uid");
    ws.drop_uid_declaration(&sidecar);

    assert_eq!(
        ws.scenes()
            .scene("res://scenes/host.tscn")
            .and_then(|s| s.root_script_path()),
        None,
        "the link goes away with the mapping, no stale script"
    );
    assert_eq!(
        ws.scenes()
            .scenes_attaching_script("res://src/hero.gd")
            .count(),
        0
    );
}

/// Re-pointing a sidecar at a different script moves the scene's link with it. Both the old and the
/// new uid have to be re-read: the scene keyed under the old one no longer belongs there.
#[test]
fn re_pointing_a_sidecar_moves_the_link() {
    let p = project(true);
    p.write("src/villain.gd", "class_name Villain\nextends Node2D\n");
    p.write("src/villain.gd.uid", "uid://c484other\n");
    let mut ws = Workspace::load(&p.root, &options(&p.root));

    // The host names `uid://c484hero`; hand that uid to villain.gd instead.
    p.remove("src/hero.gd.uid");
    ws.drop_uid_declaration(&p.root.join("src/hero.gd.uid"));
    p.write("src/villain.gd.uid", "uid://c484hero\n");
    ws.sync_uid_declaration(&p.root.join("src/villain.gd.uid"));

    assert_eq!(
        ws.scenes()
            .scene("res://scenes/host.tscn")
            .and_then(|s| s.root_script_path()),
        Some("res://src/villain.gd"),
        "the scene follows the uid to its new owner"
    );
    assert_eq!(
        ws.scenes()
            .scenes_attaching_script("res://src/hero.gd")
            .count(),
        0,
        "and stops pointing at the old one"
    );
}

/// Warm start: the cache stores scenes already canonicalized against the PREVIOUS session's uid
/// map, and a sidecar edited while gdls was off leaves the `.tscn` byte-identical, so no stat diff
/// catches it. The load re-reads every scene that names a `path`-less uid, which is what keeps the
/// warm session from serving the old target.
#[test]
fn warm_start_re_resolves_after_an_offline_sidecar_change() {
    let p = project(true);
    let cold = Workspace::load(&p.root, &options(&p.root));
    assert_eq!(
        cold.scenes()
            .scene("res://scenes/host.tscn")
            .and_then(|s| s.root_script_path()),
        Some("res://src/hero.gd")
    );
    cold.save_cache();

    // gdls is off: the sidecar moves to a different script. The scene file never changes.
    p.remove("src/hero.gd.uid");
    p.write("src/villain.gd", "class_name Villain\nextends Node2D\n");
    p.write("src/villain.gd.uid", "uid://c484hero\n");

    let warm = Workspace::load(&p.root, &options(&p.root));
    assert_eq!(
        warm.scenes()
            .scene("res://scenes/host.tscn")
            .and_then(|s| s.root_script_path()),
        Some("res://src/villain.gd"),
        "the warm load re-resolves the uid rather than trusting the cached target"
    );
}
