//! The scene index: every parsed `.tscn` in the project, keyed by `res://` path, plus the
//! script↔scene and scene→scene(instance) relations Phase-2 scene typing and the dependency-graph
//! invalidation need.
//!
//! This is a **standalone structure parallel to the script [`Index`](crate::index::Index)** — it is
//! deliberately NOT folded into `Index`. Scenes aren't `FileId`s and don't participate in the
//! `class_name` registry, the `extends` resolution, or the `txn`/`verify`/quarantine machinery the
//! script index is built around; mixing them would force every script-index invariant to grow
//! scene-awareness for no benefit. The two indexes live side by side in the workspace.
//!
//! **Source-of-truth + rebuilt inverses** (mirrors [`IndexCache`](crate::index::IndexCache)). The
//! serialized form stores only the parsed [`Scene`]s keyed by res path; the reverse maps
//! (`script → scenes`, `scene → scenes-that-instance-it`) are derived in [`SceneIndex::reindex`]
//! and rebuilt on [`SceneIndex::from_cache`]. So a warm-loaded scene index is identical to a
//! cold-built one by construction.
//!
//! **Scope (M11 Phase 1).** Nothing consumes this yet — `$`/`%` typing stays the permissive
//! `gd_analyze` deferred-node seam until Phase 2. This module builds the index + a query API on it.

use camino::{Utf8Path, Utf8PathBuf};
use rustc_hash::{FxHashMap, FxHashSet};
use serde::{Deserialize, Serialize};
use walkdir::WalkDir;

use crate::scene::{self, Scene};

/// All parsed scenes in a project, keyed by normalized `res://` path, with the reverse relations a
/// `.tscn` edit needs to invalidate the right scripts (directly and transitively through instanced
/// sub-scenes).
#[derive(Clone, Debug, Default)]
pub struct SceneIndex {
    /// `res://….tscn` (normalized) → its parsed [`Scene`].
    scenes: FxHashMap<String, Scene>,
    /// `res://….gd` (a script) → the set of scene res paths that attach it to one of their nodes.
    /// Conservative reverse map: a script is in the set of every scene that references it. Rebuilt
    /// from `scenes` on every [`Self::reindex`] / [`Self::from_cache`].
    script_to_scenes: FxHashMap<String, FxHashSet<String>>,
    /// `res://….tscn` (a sub-scene) → the set of scene res paths that *instance* it (via a node's
    /// `instance=ExtResource(id)`). The scene→scene(instance) edge, used for transitive
    /// invalidation. Rebuilt alongside `script_to_scenes`.
    instanced_by: FxHashMap<String, FxHashSet<String>>,
}

impl SceneIndex {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Cold-build the scene index by scanning every `.tscn` under `root` from disk, sharing
    /// the script index's exclusion set ([`crate::exclude::is_excluded`] — `.godot/`, `.import/`,
    /// `.git/`, `target/`, `node_modules/`, `.gdls/`, editor temp suffixes) so the two indexes agree
    /// on what enters them. A file that can't be read or isn't under `root` is skipped (degrade,
    /// never fail), and walk/UTF-8 errors are logged at `warn` — matching `gd_files`' discipline.
    /// Scenes are keyed by their `res://` path.
    #[must_use]
    pub fn build(root: &Utf8Path) -> Self {
        let mut idx = SceneIndex::new();
        for entry_result in WalkDir::new(root).into_iter().filter_entry(|e| {
            Utf8Path::from_path(e.path()).is_none_or(|p| !crate::exclude::is_excluded(p, root))
        }) {
            let entry = match entry_result {
                Ok(e) => e,
                Err(e) => {
                    log::warn!("scene index: walk error: {e}");
                    continue;
                }
            };
            let Some(p) = Utf8Path::from_path(entry.path()) else {
                log::warn!("scene index: skipping non-UTF-8 path under {root}");
                continue;
            };
            if !scene::is_scene_path(p) {
                continue;
            }
            let Some(res) = crate::paths::path_to_res(root, p) else {
                continue; // not under root (shouldn't happen post-walk) — skip rather than mis-key
            };
            match std::fs::read_to_string(p) {
                Ok(text) => idx.reindex(&res, &text),
                Err(e) => log::warn!("scene index: skipping unreadable {p}: {e}"),
            }
        }
        idx
    }

    /// Number of indexed scenes.
    #[must_use]
    pub fn len(&self) -> usize {
        self.scenes.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.scenes.is_empty()
    }

    // --- Building / incremental update ----------------------------------------------------------

    /// Parse `text` as the scene at `res_path` (a `res://….tscn`) and record it, then rebuild the
    /// reverse maps for the affected relations. Replaces any prior scene at that path. The key is
    /// normalized via [`scene::normalize_res`] so a `\`-spelled path and a `/`-spelled one collapse.
    pub fn reindex(&mut self, res_path: &str, text: &str) {
        let key = scene::normalize_res(res_path);
        let scene = scene::parse_scene(text);
        self.insert_scene(key, scene);
    }

    /// Record an already-parsed [`Scene`] at `res_path`. (Used by warm-load and tests.)
    pub fn insert_scene(&mut self, res_path: impl Into<String>, scene: Scene) {
        let key = scene::normalize_res(&res_path.into());
        // Drop the old scene's reverse entries before inserting the new one.
        self.remove_reverse(&key);
        self.add_reverse(&key, &scene);
        self.scenes.insert(key, scene);
    }

    /// Drop the scene at `res_path` (a deleted `.tscn`) and its reverse entries.
    pub fn remove(&mut self, res_path: &str) {
        let key = scene::normalize_res(res_path);
        self.remove_reverse(&key);
        self.scenes.remove(&key);
    }

    // --- Queries --------------------------------------------------------------------------------

    /// The parsed scene at `res_path`, if indexed.
    #[must_use]
    pub fn scene(&self, res_path: &str) -> Option<&Scene> {
        self.scenes.get(&scene::normalize_res(res_path))
    }

    /// Iterate every `(res_path, &Scene)` pair currently held.
    #[must_use = "iterators are lazy and do nothing unless consumed"]
    pub fn iter(&self) -> impl Iterator<Item = (&str, &Scene)> + '_ {
        self.scenes.iter().map(|(k, v)| (k.as_str(), v))
    }

    /// The scenes that attach `script_res` (a `res://….gd`) to one of their nodes. Empty iterator
    /// if no scene references it. This is the direct script↔scene reverse map.
    #[must_use = "iterators are lazy and do nothing unless consumed"]
    pub fn scenes_attaching_script<'a>(
        &'a self,
        script_res: &str,
    ) -> impl Iterator<Item = &'a str> + 'a {
        let key = scene::normalize_res(script_res);
        self.script_to_scenes
            .get(&key)
            .into_iter()
            .flat_map(|set| set.iter().map(String::as_str))
    }

    /// The scenes that *instance* `scene_res` (a `res://….tscn` used as a sub-scene). Empty if none.
    #[must_use = "iterators are lazy and do nothing unless consumed"]
    pub fn scenes_instancing<'a>(&'a self, scene_res: &str) -> impl Iterator<Item = &'a str> + 'a {
        let key = scene::normalize_res(scene_res);
        self.instanced_by
            .get(&key)
            .into_iter()
            .flat_map(|set| set.iter().map(String::as_str))
    }

    /// The transitive closure of scenes affected when `scene_res` changes: `scene_res` itself plus
    /// every scene that instances it, directly or through a chain of sub-scene instancing. Cycle-safe
    /// (a `seen` set bounds the walk), mirroring [`DepGraph::reverse_closure`](crate::DepGraph). The
    /// returned set INCLUDES `scene_res` (the edited scene is itself affected).
    #[must_use]
    pub fn instance_reverse_closure(&self, scene_res: &str) -> FxHashSet<String> {
        let start = scene::normalize_res(scene_res);
        let mut seen = FxHashSet::default();
        let mut stack = vec![start.clone()];
        seen.insert(start);
        while let Some(cur) = stack.pop() {
            if let Some(parents) = self.instanced_by.get(&cur) {
                for p in parents {
                    if seen.insert(p.clone()) {
                        stack.push(p.clone());
                    }
                }
            }
        }
        seen
    }

    /// Every `res://….gd` script that a `.tscn` edit to `scene_res` should cause to re-diagnose:
    /// the union of the scripts attached by `scene_res` and by every scene that transitively
    /// instances it. This is the scene→script invalidation set Phase-2 / the dep graph consumes
    /// (the analyzer is inert on scenes until Phase 2, so today it is computed but not yet wired to
    /// dirty-marking — see [`crate::depgraph`] integration).
    #[must_use]
    pub fn affected_scripts(&self, scene_res: &str) -> FxHashSet<String> {
        let mut scripts = FxHashSet::default();
        for affected_scene in self.instance_reverse_closure(scene_res) {
            if let Some(scene) = self.scenes.get(&affected_scene) {
                for s in scene.attached_scripts() {
                    scripts.insert(scene::normalize_res(s));
                }
            }
        }
        scripts
    }

    // --- Reverse-map maintenance ----------------------------------------------------------------

    /// Add `scene`'s attached-script and instanced-sub-scene reverse entries under key `scene_key`.
    fn add_reverse(&mut self, scene_key: &str, scene: &Scene) {
        for script in scene.attached_scripts() {
            self.script_to_scenes
                .entry(scene::normalize_res(script))
                .or_default()
                .insert(scene_key.to_owned());
        }
        for sub in scene.instanced_scenes() {
            self.instanced_by
                .entry(scene::normalize_res(sub))
                .or_default()
                .insert(scene_key.to_owned());
        }
    }

    /// Remove every reverse entry pointing *from* the scene currently stored at `scene_key` (called
    /// before replacing or deleting it). Prunes emptied sets so removed targets don't accumulate.
    fn remove_reverse(&mut self, scene_key: &str) {
        let Some(old) = self.scenes.get(scene_key) else {
            return;
        };
        // Collect keys first to avoid borrowing `self.scenes` while mutating the reverse maps.
        let old_scripts: Vec<String> = old.attached_scripts().map(scene::normalize_res).collect();
        let old_subs: Vec<String> = old.instanced_scenes().map(scene::normalize_res).collect();
        for script in old_scripts {
            if let Some(set) = self.script_to_scenes.get_mut(&script) {
                set.remove(scene_key);
                if set.is_empty() {
                    self.script_to_scenes.remove(&script);
                }
            }
        }
        for sub in old_subs {
            if let Some(set) = self.instanced_by.get_mut(&sub) {
                set.remove(scene_key);
                if set.is_empty() {
                    self.instanced_by.remove(&sub);
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Cache integration — store scenes only; rebuild reverse maps on load.
// ---------------------------------------------------------------------------

/// A serializable snapshot of a [`SceneIndex`]. Stores only the parsed scenes (the source of
/// truth); the reverse maps are rebuilt on [`SceneIndex::from_cache`] to avoid storing two copies
/// that could drift — exactly the [`IndexCache`](crate::index::IndexCache) discipline.
#[derive(Serialize, Deserialize, Default)]
pub struct SceneIndexCache {
    /// `res://….tscn` (normalized) → parsed scene, as a sorted vec for deterministic output.
    scenes: Vec<(String, Scene)>,
}

impl SceneIndex {
    /// Produce a serializable snapshot (scenes only; reverse maps omitted — rebuilt on load).
    #[must_use]
    pub fn to_cache(&self) -> SceneIndexCache {
        let mut scenes: Vec<(String, Scene)> = self
            .scenes
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        scenes.sort_by(|a, b| a.0.cmp(&b.0));
        SceneIndexCache { scenes }
    }

    /// Reconstruct a [`SceneIndex`] from a snapshot, rebuilding the reverse maps from the stored
    /// scenes via [`Self::insert_scene`] (which routes derived state through the same finalize
    /// chokepoint a fresh parse uses).
    #[must_use]
    pub fn from_cache(cache: SceneIndexCache) -> Self {
        let mut idx = SceneIndex::new();
        for (key, scene) in cache.scenes {
            idx.insert_scene(key, scene);
        }
        idx
    }
}

/// Normalize a project-absolute path to its `res://` form for scene-index keys, given the project
/// root. A thin wrapper over [`crate::paths::path_to_res`] kept here so the scene-index call sites
/// read coherently. `None` if `path` is not under `root`.
#[must_use]
pub fn path_to_res(root: &Utf8Path, path: &Utf8Path) -> Option<String> {
    crate::paths::path_to_res(root, path)
}

/// Resolve a `res://` scene path to its absolute filesystem path under `root` (no existence check),
/// mirroring [`crate::paths::res_to_path`]. Kept beside the scene index for call-site clarity.
#[must_use]
pub fn res_to_path(root: &Utf8Path, res: &str) -> Option<Utf8PathBuf> {
    crate::paths::res_to_path(root, res)
}

#[cfg(test)]
mod tests {
    use super::*;

    const PARENT: &str = r#"[gd_scene format=3 uid="uid://parent"]
[ext_resource type="Script" path="res://main.gd" id="1"]
[ext_resource type="PackedScene" path="res://child.tscn" id="2"]
[node name="Root" type="Control"]
script = ExtResource("1")
[node name="Sub" parent="." instance=ExtResource("2")]
"#;

    const CHILD: &str = r#"[gd_scene format=3 uid="uid://child"]
[ext_resource type="Script" path="res://child.gd" id="1"]
[node name="ChildRoot" type="Panel"]
script = ExtResource("1")
"#;

    fn build() -> SceneIndex {
        let mut idx = SceneIndex::new();
        idx.reindex("res://parent.tscn", PARENT);
        idx.reindex("res://child.tscn", CHILD);
        idx
    }

    #[test]
    fn indexes_scenes_and_reverse_maps() {
        let idx = build();
        assert_eq!(idx.len(), 2);
        assert!(idx.scene("res://parent.tscn").is_some());

        // script → scenes reverse map: main.gd is attached only by parent.
        let main_scenes: Vec<&str> = idx.scenes_attaching_script("res://main.gd").collect();
        assert_eq!(main_scenes, vec!["res://parent.tscn"]);
        let child_scenes: Vec<&str> = idx.scenes_attaching_script("res://child.gd").collect();
        assert_eq!(child_scenes, vec!["res://child.tscn"]);

        // scene → scenes-instancing: child.tscn is instanced by parent.
        let inst: Vec<&str> = idx.scenes_instancing("res://child.tscn").collect();
        assert_eq!(inst, vec!["res://parent.tscn"]);
    }

    #[test]
    fn transitive_instance_invalidation() {
        // Editing child.tscn must affect parent.tscn (which instances it) AND child's own
        // consumers — so affected_scripts(child) includes BOTH child.gd and main.gd.
        let idx = build();
        let affected = idx.affected_scripts("res://child.tscn");
        assert!(affected.contains("res://child.gd"), "child's own script");
        assert!(
            affected.contains("res://main.gd"),
            "the parent scene that instances child must re-diagnose its script"
        );

        // The reverse closure includes the edited scene + its instancers.
        let closure = idx.instance_reverse_closure("res://child.tscn");
        assert!(closure.contains("res://child.tscn"));
        assert!(closure.contains("res://parent.tscn"));
    }

    #[test]
    fn multi_scene_attachment_reverse_map_has_all() {
        // A shared script attached by two scenes: both must appear in the reverse map.
        let mut idx = SceneIndex::new();
        let a = "[gd_scene format=3]\n\
                 [ext_resource type=\"Script\" path=\"res://shared.gd\" id=\"1\"]\n\
                 [node name=\"A\" type=\"Node\"]\nscript = ExtResource(\"1\")\n";
        let b = "[gd_scene format=3]\n\
                 [ext_resource type=\"Script\" path=\"res://shared.gd\" id=\"1\"]\n\
                 [node name=\"B\" type=\"Node\"]\nscript = ExtResource(\"1\")\n";
        idx.reindex("res://a.tscn", a);
        idx.reindex("res://b.tscn", b);
        let mut scenes: Vec<&str> = idx.scenes_attaching_script("res://shared.gd").collect();
        scenes.sort_unstable();
        assert_eq!(scenes, vec!["res://a.tscn", "res://b.tscn"]);
    }

    #[test]
    fn reindex_replaces_stale_reverse_edges() {
        let mut idx = SceneIndex::new();
        idx.reindex(
            "res://a.tscn",
            "[gd_scene format=3]\n\
             [ext_resource type=\"Script\" path=\"res://old.gd\" id=\"1\"]\n\
             [node name=\"A\" type=\"Node\"]\nscript = ExtResource(\"1\")\n",
        );
        assert_eq!(idx.scenes_attaching_script("res://old.gd").count(), 1);
        // Re-index the same scene now attaching a different script: the stale edge must be gone.
        idx.reindex(
            "res://a.tscn",
            "[gd_scene format=3]\n\
             [ext_resource type=\"Script\" path=\"res://new.gd\" id=\"1\"]\n\
             [node name=\"A\" type=\"Node\"]\nscript = ExtResource(\"1\")\n",
        );
        assert_eq!(idx.scenes_attaching_script("res://old.gd").count(), 0);
        assert_eq!(idx.scenes_attaching_script("res://new.gd").count(), 1);
    }

    #[test]
    fn remove_drops_scene_and_reverse() {
        let mut idx = build();
        idx.remove("res://parent.tscn");
        assert!(idx.scene("res://parent.tscn").is_none());
        // parent attached main.gd and instanced child.tscn — both reverse edges gone.
        assert_eq!(idx.scenes_attaching_script("res://main.gd").count(), 0);
        assert_eq!(idx.scenes_instancing("res://child.tscn").count(), 0);
    }

    #[test]
    fn cyclic_instance_closure_terminates() {
        // a.tscn instances b.tscn; b.tscn instances a.tscn. The closure walk must terminate.
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
        let closure = idx.instance_reverse_closure("res://a.tscn");
        assert!(closure.contains("res://a.tscn"));
        assert!(closure.contains("res://b.tscn"));
        // Terminated with exactly the two scenes.
        assert_eq!(closure.len(), 2);
    }

    #[test]
    fn cache_round_trips_through_query_api() {
        let idx = build();
        let snapshot = idx.to_cache();
        let json = serde_json::to_string(&snapshot).unwrap();
        let restored_snapshot: SceneIndexCache = serde_json::from_str(&json).unwrap();
        let restored = SceneIndex::from_cache(restored_snapshot);

        assert_eq!(restored.len(), idx.len());
        // Reverse maps rebuilt (not stored) — prove through the query API.
        let main_scenes: Vec<&str> = restored.scenes_attaching_script("res://main.gd").collect();
        assert_eq!(main_scenes, vec!["res://parent.tscn"]);
        let inst: Vec<&str> = restored.scenes_instancing("res://child.tscn").collect();
        assert_eq!(inst, vec!["res://parent.tscn"]);
        // And the parsed scene's own query API still works after round-trip.
        let parent = restored.scene("res://parent.tscn").unwrap();
        assert_eq!(parent.root_script_path(), Some("res://main.gd"));
        assert_eq!(
            parent.node_type("Sub"),
            Some(&scene::NodeType::Instanced(Some("res://child.tscn".into())))
        );
    }
}
