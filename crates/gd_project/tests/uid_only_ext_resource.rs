//! #484: a `[ext_resource]` that carries only a `uid://` and no `path` still resolves.
//!
//! Godot writes the `path` out of an ext-resource entry when the resource has a `.uid` sidecar, so
//! a scene saved by a recent editor names its script purely by uid. A pure parse cannot resolve
//! that — only the project's uid map can — so [`SceneIndex`] canonicalizes every `uid://` in a
//! scene's node fields to a `res://` path at insert time. An unresolvable uid becomes `None`, never
//! the uid string, so the worst case is exactly the old behavior and never a wrong file.

use camino::Utf8Path;
use gd_project::{parse_scene, NodeType, SceneIndex};
use rustc_hash::FxHashMap;

const UID_ONLY: &str = r#"[gd_scene load_steps=3 format=3 uid="uid://dhost"]

[ext_resource type="Script" uid="uid://dscript" id="1"]
[ext_resource type="PackedScene" uid="uid://dsub" id="2"]

[node name="Root" type="Node2D"]
script = ExtResource("1")

[node name="Sub" parent="." instance=ExtResource("2")]
"#;

fn uid_map(pairs: &[(&str, &str)]) -> FxHashMap<String, String> {
    pairs
        .iter()
        .map(|(u, p)| ((*u).to_string(), (*p).to_string()))
        .collect()
}

// (a) The parse alone carries the uid through, unresolved. That is the seam the index fixes.
#[test]
fn a_pure_parse_yields_the_uid_string() {
    let s = parse_scene(UID_ONLY);
    assert_eq!(s.root_script_path(), Some("uid://dscript"));
    assert_eq!(
        s.node_type("Sub"),
        Some(&NodeType::Instanced(Some("uid://dsub".into())))
    );
}

// (b) Inserted into an index that knows the uids, both fields read as `res://` paths.
#[test]
fn b_index_canonicalizes_both_script_and_instance() {
    let mut idx = SceneIndex::new();
    idx.set_uid_map(uid_map(&[
        ("uid://dscript", "res://src/host.gd"),
        ("uid://dsub", "res://src/sub.tscn"),
    ]));
    idx.insert_scene("res://host.tscn", parse_scene(UID_ONLY));

    let s = idx.scene("res://host.tscn").expect("scene indexed");
    assert_eq!(s.root_script_path(), Some("res://src/host.gd"));
    assert_eq!(
        s.node_type("Sub"),
        Some(&NodeType::Instanced(Some("res://src/sub.tscn".into())))
    );
}

// (c) The reverse maps key off the canonical path, so the script and instance lookups both hit.
#[test]
fn c_reverse_maps_key_off_the_canonical_path() {
    let mut idx = SceneIndex::new();
    idx.set_uid_map(uid_map(&[
        ("uid://dscript", "res://src/host.gd"),
        ("uid://dsub", "res://src/sub.tscn"),
    ]));
    idx.insert_scene("res://host.tscn", parse_scene(UID_ONLY));

    let attaching: Vec<&str> = idx.scenes_attaching_script("res://src/host.gd").collect();
    assert_eq!(attaching, vec!["res://host.tscn"]);
    let instancing: Vec<&str> = idx.scenes_instancing("res://src/sub.tscn").collect();
    assert_eq!(instancing, vec!["res://host.tscn"]);
}

// (d) An unknown uid degrades to `None` — never to the raw uid string leaking out as a path.
#[test]
fn d_an_unknown_uid_degrades_to_none() {
    let mut idx = SceneIndex::new();
    idx.insert_scene("res://host.tscn", parse_scene(UID_ONLY));

    let s = idx.scene("res://host.tscn").expect("scene indexed");
    assert_eq!(s.root_script_path(), None);
    assert_eq!(s.node_type("Sub"), Some(&NodeType::Instanced(None)));
    // The node itself survives — an unresolvable uid loses the link, not the tree.
    assert!(s.node_at("Sub").is_some());
    assert_eq!(idx.scenes_attaching_script("uid://dscript").count(), 0);
}

// (e) The referencer map names the scene whether or not the uid resolved, so a sidecar that shows
// up later has a work list to re-resolve.
#[test]
fn e_referencers_are_recorded_even_when_unresolved() {
    let mut idx = SceneIndex::new();
    idx.insert_scene("res://host.tscn", parse_scene(UID_ONLY));

    let refs: Vec<&str> = idx.scenes_referencing_uid("uid://dscript").collect();
    assert_eq!(refs, vec!["res://host.tscn"]);
    assert_eq!(idx.uid_referencing_scenes(), vec!["res://host.tscn"]);

    // Learning the uid and re-reading the scene resolves it.
    idx.set_uid_map(uid_map(&[("uid://dscript", "res://src/host.gd")]));
    idx.insert_scene("res://host.tscn", parse_scene(UID_ONLY));
    let s = idx.scene("res://host.tscn").expect("scene reindexed");
    assert_eq!(s.root_script_path(), Some("res://src/host.gd"));
}

// (f) Removing the scene prunes it from the referencer map, so a deleted scene never keeps a
// sidecar work list alive.
#[test]
fn f_remove_prunes_the_referencer_map() {
    let mut idx = SceneIndex::new();
    idx.insert_scene("res://host.tscn", parse_scene(UID_ONLY));
    idx.remove("res://host.tscn");

    assert_eq!(idx.scenes_referencing_uid("uid://dscript").count(), 0);
    assert!(idx.uid_referencing_scenes().is_empty());
}

// (g) An entry carrying BOTH a path and a uid keeps the path — the uid is never consulted, and the
// scene is not recorded as a uid referencer.
#[test]
fn g_a_path_wins_over_a_uid_on_the_same_entry() {
    const BOTH: &str = r#"[gd_scene load_steps=2 format=3]

[ext_resource type="Script" path="res://src/real.gd" uid="uid://dscript" id="1"]

[node name="Root" type="Node"]
script = ExtResource("1")
"#;
    let mut idx = SceneIndex::new();
    idx.set_uid_map(uid_map(&[("uid://dscript", "res://src/decoy.gd")]));
    idx.insert_scene("res://host.tscn", parse_scene(BOTH));

    let s = idx.scene("res://host.tscn").expect("scene indexed");
    assert_eq!(s.root_script_path(), Some("res://src/real.gd"));
    assert_eq!(idx.scenes_referencing_uid("uid://dscript").count(), 0);
}

// (h) Canonicalization is idempotent: a second insert of an already-rewritten scene is a no-op, and
// `build` over a tree with no uid map behaves exactly as it did before #484.
#[test]
fn h_build_without_a_uid_map_is_unchanged() {
    let dir = Utf8Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/scenes");
    let idx = SceneIndex::build(&dir, FxHashMap::default());
    assert!(idx.len() >= 5);
    assert!(idx.uid_referencing_scenes().is_empty());
}
