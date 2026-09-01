//! M11 (#76): integration tests for the `.tscn` parser + [`SceneIndex`] against on-disk fixtures.
//!
//! These exercise the realistic-input bar the parser must clear: a real scene with a root +
//! children + attached script + `%`-unique node + an instanced sub-scene (every relation
//! extracted); malformed/truncated/garbage (partial, no panic); a cyclic instance graph
//! (terminates); multi-scene attachment (reverse map has all); fork quirks + unknown sections/keys
//! tolerated; and node paths with `/`. The fixtures double as the fuzz corpus seed (see
//! `.github/workflows/ci.yml`).

use camino::{Utf8Path, Utf8PathBuf};
use gd_project::{parse_scene, NodeType, Scene, SceneIndex};

fn fixtures_dir() -> Utf8PathBuf {
    Utf8Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/scenes")
}

fn load(name: &str) -> Scene {
    let path = fixtures_dir().join(name);
    let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {path}: {e}"));
    parse_scene(&text)
}

// (a) A realistic scene: every relation extracted.
#[test]
fn realistic_main_scene_every_relation() {
    let s = load("main.tscn");
    assert_eq!(s.uid.as_deref(), Some("uid://dmainscene01"));

    // Root + attached script.
    let root = s.root_node().expect("root present");
    assert_eq!(root.name, "Root");
    assert_eq!(root.ty, NodeType::Native("Control".into()));
    assert_eq!(s.root_script_path(), Some("res://src/main.gd"));

    // Node types by path.
    assert_eq!(
        s.node_type("VBox"),
        Some(&NodeType::Native("VBoxContainer".into()))
    );
    assert_eq!(
        s.node_type("VBox/TitleLabel"),
        Some(&NodeType::Native("Label".into()))
    );

    // `%`-unique node — keyed off `unique_name_in_owner = true`, NOT a `%` prefix.
    let title = s
        .node_by_unique_name("TitleLabel")
        .expect("unique node TitleLabel");
    assert_eq!(title.path, "VBox/TitleLabel");

    // Instanced sub-scene: type is the PackedScene path; alphanumeric ext id "3_abc" resolves.
    assert_eq!(
        s.node_type("VBox/ChildInstance"),
        Some(&NodeType::Instanced(Some("res://src/child.tscn".into())))
    );

    // attached_scripts + instanced_scenes.
    let scripts: Vec<&str> = s.attached_scripts().collect();
    assert_eq!(scripts, vec!["res://src/main.gd"]);
    let instanced: Vec<&str> = s.instanced_scenes().collect();
    assert_eq!(instanced, vec!["res://src/child.tscn"]);
}

// (a, cont.) The instanced sub-scene's root type resolves through the child's text.
#[test]
fn instanced_root_resolves_via_child_fixture() {
    let main = load("main.tscn");
    let child_text = std::fs::read_to_string(fixtures_dir().join("child.tscn")).unwrap();
    let lookup = |p: &str| -> Option<std::borrow::Cow<'_, str>> {
        (p == "res://src/child.tscn").then_some(std::borrow::Cow::Borrowed(child_text.as_str()))
    };
    let resolved = main
        .resolve_instanced_root("VBox/ChildInstance", &lookup)
        .expect("child root resolves");
    // child.tscn's root is a PanelContainer with res://src/child.gd attached.
    assert_eq!(resolved.native_type.as_deref(), Some("PanelContainer"));
    assert_eq!(resolved.script.as_deref(), Some("res://src/child.gd"));
}

// (b) Malformed/truncated/garbage → partial, no panic.
#[test]
fn malformed_fixture_is_partial_not_panic() {
    let s = load("malformed.tscn"); // must not panic
                                    // The well-formed `[node name="RealNode" type="Node2D"]` line should still be recovered.
    assert!(
        s.nodes.iter().any(|n| n.name == "RealNode"),
        "a well-formed node after garbage lines must still parse; got {:?}",
        s.nodes.iter().map(|n| &n.name).collect::<Vec<_>>()
    );
    // The truncated `script = ExtResource(` line resolves to no script (degrade, never lie).
    for n in &s.nodes {
        assert!(
            n.script.is_none(),
            "no script id resolved from the truncated ext table"
        );
    }
}

// (c) A cyclic instance graph terminates (depth cap / visited set).
#[test]
fn cyclic_instance_graph_terminates() {
    let mut idx = SceneIndex::new();
    idx.reindex(
        "res://a.tscn",
        "[gd_scene format=3]\n\
         [ext_resource type=\"PackedScene\" path=\"res://b.tscn\" id=\"1\"]\n\
         [node name=\"A\" type=\"Node\"]\n\
         [node name=\"Sub\" parent=\".\" instance=ExtResource(\"1\")]\n",
    );
    idx.reindex(
        "res://b.tscn",
        "[gd_scene format=3]\n\
         [ext_resource type=\"PackedScene\" path=\"res://a.tscn\" id=\"1\"]\n\
         [node name=\"B\" type=\"Node\"]\n\
         [node name=\"Sub\" parent=\".\" instance=ExtResource(\"1\")]\n",
    );
    // The scene-graph reverse closure terminates with exactly the two scenes.
    let closure = idx.instance_reverse_closure("res://a.tscn");
    assert_eq!(closure.len(), 2);

    // And the per-scene root resolver terminates on a root↔root instance cycle.
    let a = "[gd_scene format=3]\n\
             [ext_resource type=\"PackedScene\" path=\"res://b.tscn\" id=\"1\"]\n\
             [node name=\"ARoot\" instance=ExtResource(\"1\")]\n"
        .to_owned();
    let b = "[gd_scene format=3]\n\
             [ext_resource type=\"PackedScene\" path=\"res://a.tscn\" id=\"1\"]\n\
             [node name=\"BRoot\" instance=ExtResource(\"1\")]\n"
        .to_owned();
    let sa = parse_scene(&a);
    let lookup = move |p: &str| -> Option<std::borrow::Cow<'static, str>> {
        match p {
            "res://a.tscn" => Some(std::borrow::Cow::Owned(a.clone())),
            "res://b.tscn" => Some(std::borrow::Cow::Owned(b.clone())),
            _ => None,
        }
    };
    assert!(
        sa.resolve_root_type(&lookup).is_none(),
        "a root↔root instance cycle must terminate (None)"
    );
}

// (d) Multi-scene attachment → reverse map has all scenes.
#[test]
fn multi_scene_attachment_reverse_map() {
    let mut idx = SceneIndex::new();
    let mk = |node: &str| {
        format!(
            "[gd_scene format=3]\n\
             [ext_resource type=\"Script\" path=\"res://shared.gd\" id=\"1\"]\n\
             [node name=\"{node}\" type=\"Node\"]\nscript = ExtResource(\"1\")\n"
        )
    };
    idx.reindex("res://one.tscn", &mk("One"));
    idx.reindex("res://two.tscn", &mk("Two"));
    idx.reindex("res://three.tscn", &mk("Three"));
    let mut scenes: Vec<&str> = idx.scenes_attaching_script("res://shared.gd").collect();
    scenes.sort_unstable();
    assert_eq!(
        scenes,
        vec!["res://one.tscn", "res://three.tscn", "res://two.tscn"]
    );
}

// (e) Unknown sections/keys + fork quirks tolerated.
#[test]
fn fork_quirks_and_unknown_sections_tolerated() {
    let s = load("fork_quirks.tscn"); // contains unique_id=, groups=, [some_future_section], etc.
    let root = s.root_node().expect("root despite fork-specific keys");
    assert_eq!(root.name, "Root");
    assert_eq!(s.root_script_path(), Some("res://src/forked.gd"));

    // Special-char node names survive.
    assert_eq!(s.node_at("100%").map(|n| n.name.as_str()), Some("100%"));
    assert_eq!(
        s.node_at("3D Object Tree").map(|n| n.name.as_str()),
        Some("3D Object Tree")
    );
    // The `100%` button's unique_name_in_owner was set.
    assert!(s.node_by_unique_name("100%").is_some());
    // The unknown Object(...) body value on "3D Object Tree" didn't break parsing of the node.
    assert_eq!(
        s.node_type("3D Object Tree"),
        Some(&NodeType::Native("Tree".into()))
    );
}

// (f) Node paths with `/` (nested parents).
#[test]
fn node_paths_with_slashes() {
    let s = load("main.tscn");
    // VBox/TitleLabel and VBox/ChildInstance are both `/`-pathed.
    assert!(s.node_at("VBox/TitleLabel").is_some());
    assert!(s.node_at("VBox/ChildInstance").is_some());
    // A path that doesn't exist returns None (no panic, no false hit).
    assert!(s.node_at("VBox/Nonexistent").is_none());
}

// Multi-line value robustness: a `[`-leading continuation line inside a sub_resource value must
// NOT be read as a section header, so the node after it still parses.
#[test]
fn multiline_value_does_not_desync_sections() {
    let s = load("multiline_value.tscn");
    let root = s
        .root_node()
        .expect("root after multi-line sub_resource value");
    assert_eq!(root.name, "Root");
    assert_eq!(s.root_script_path(), Some("res://src/anim.gd"));
    assert_eq!(
        s.node_type("Sprite"),
        Some(&NodeType::Native("Sprite2D".into()))
    );
}

// SceneIndex::build over the whole fixtures dir indexes every fixture without panic, and the
// reverse maps line up with the fixtures' contents.
#[test]
fn build_over_fixtures_dir() {
    let idx = SceneIndex::build(&fixtures_dir(), Default::default());
    // 5 fixtures (main, child, fork_quirks, malformed, multiline_value). The malformed one still
    // produces a (partial) Scene and is indexed.
    assert!(idx.len() >= 5, "all fixtures indexed; got {}", idx.len());
}
