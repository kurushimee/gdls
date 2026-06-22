//! #127: end-to-end asset-index lifecycle through the [`Workspace`] — cold build from disk, live
//! re-index on an arbitrary-asset change (the same `reindex_asset`/`remove_asset` the watcher
//! `Reaction::Asset` path calls), warm-start cache round-trip, and reconcile drift recovery. Proves
//! the arbitrary project resources Godot's `_get_directory_contents` lists for `load`/`preload`
//! completion are indexed, kept live, and survive a warm restart — the same discipline scenes get.

mod common;

use gd_server::config::InitializationOptions;
use gd_server::Workspace;

fn options(root: &camino::Utf8Path) -> InitializationOptions {
    InitializationOptions::parse(Some(&serde_json::json!({
        "projectRoot": root.as_str(),
        "autoDumpExtensionApi": false,
    })))
}

#[test]
fn cold_load_builds_asset_index_from_disk() {
    let p = common::TempProject::new();
    p.write("project.godot", "config_version=5\n");
    // Scripts + scenes are indexed elsewhere; arbitrary assets are the asset index's job.
    p.write("src/hero.gd", "extends Node\n");
    p.write("scenes/main.tscn", "[gd_scene format=3]\n");
    p.write("art/icon.png", "PNG-PLACEHOLDER");
    p.write("data/config.tres", "[gd_resource type=\"Resource\"]\n");
    p.write("LICENSE", "MIT"); // a no-extension file is a project asset Godot lists too

    let ws = Workspace::load(&p.root, &options(&p.root));
    let assets = ws.assets();
    assert!(assets.contains("res://art/icon.png"), "png is an asset");
    assert!(
        assets.contains("res://data/config.tres"),
        "tres is an asset"
    );
    assert!(
        assets.contains("res://LICENSE"),
        "no-extension file is an asset"
    );
    // Scripts and scenes must NOT double-count here (indexed elsewhere; the consumer unions all three).
    assert!(
        !assets.contains("res://src/hero.gd"),
        ".gd indexed elsewhere"
    );
    assert!(
        !assets.contains("res://scenes/main.tscn"),
        ".tscn indexed elsewhere"
    );
}

#[test]
fn reindex_asset_keeps_index_live_on_add() {
    let p = common::TempProject::new();
    p.write("project.godot", "config_version=5\n");

    let mut ws = Workspace::load(&p.root, &options(&p.root));
    assert!(!ws.assets().contains("res://art/icon.png"));

    // Write a new asset on disk and drive the reaction the watcher's `Reaction::Asset` arm fires.
    p.write("art/icon.png", "PNG-PLACEHOLDER");
    let asset_path = p.root.join("art/icon.png");
    ws.reindex_asset(&asset_path);

    assert!(
        ws.assets().contains("res://art/icon.png"),
        "reindex_asset must make the new asset live for completion"
    );
}

#[test]
fn remove_asset_drops_it_from_index() {
    let p = common::TempProject::new();
    p.write("project.godot", "config_version=5\n");
    p.write("art/icon.png", "PNG-PLACEHOLDER");

    let mut ws = Workspace::load(&p.root, &options(&p.root));
    assert!(ws.assets().contains("res://art/icon.png"));

    // Delete on disk + drive the delete reaction.
    let asset_path = p.root.join("art/icon.png");
    p.remove("art/icon.png");
    ws.remove_asset(&asset_path);

    assert!(
        !ws.assets().contains("res://art/icon.png"),
        "remove_asset must drop the deleted asset"
    );
}

#[test]
fn warm_start_round_trips_asset_index() {
    let p = common::TempProject::new();
    p.write("project.godot", "config_version=5\n");
    p.write("art/icon.png", "PNG-PLACEHOLDER");
    p.write("data/config.tres", "[gd_resource type=\"Resource\"]\n");

    // Cold load → save cache.
    let cold = Workspace::load(&p.root, &options(&p.root));
    assert!(cold.assets().contains("res://art/icon.png"));
    assert!(cold.assets().contains("res://data/config.tres"));
    cold.save_cache();

    // Warm load from the cache: the asset index must be restored (v8 cache field, not defaulted).
    let warm = Workspace::load(&p.root, &options(&p.root));
    assert!(
        warm.assets().contains("res://art/icon.png"),
        "warm load restores the png asset"
    );
    assert!(
        warm.assets().contains("res://data/config.tres"),
        "warm load restores the tres asset"
    );
}

#[test]
fn reconcile_recovers_asset_drift() {
    // The watcher-overflow (`need_rescan`) and disabled-watcher liveness-tick recovery paths both
    // route through `Workspace::reconcile`. This proves reconcile stat-diffs arbitrary assets
    // (added/removed), so an asset drifted while the watcher was overflowed/off is recovered — the
    // same backstop scenes get.
    let p = common::TempProject::new();
    p.write("project.godot", "config_version=5\n");
    p.write("art/icon.png", "PNG-PLACEHOLDER");

    let mut ws = Workspace::load(&p.root, &options(&p.root));
    let no_open = std::collections::HashSet::default();
    assert!(ws.assets().contains("res://art/icon.png"));

    // (a) ADDED while off: a brand-new asset appears, reconcile picks it up.
    p.write("audio/blip.wav", "RIFF-PLACEHOLDER");
    ws.reconcile(&no_open);
    assert!(
        ws.assets().contains("res://audio/blip.wav"),
        "reconcile must discover an asset added while the watcher was off"
    );

    // (b) REMOVED while off: delete an asset, reconcile drops it from the index.
    p.remove("art/icon.png");
    ws.reconcile(&no_open);
    assert!(
        !ws.assets().contains("res://art/icon.png"),
        "reconcile must drop an asset deleted while the watcher was off"
    );
}
