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
//! **Scope.** The diagnostic path does NOT consume this — a valid `$`/`%` types as bare `NATIVE
//! Node` (`gd_analyze`, faithful to Godot), independent of the scene. This index + its node-path
//! resolution are the substrate the precise NAVIGATION surfaces read (hover / definition /
//! completion, via `gd_server`); this module builds the index + a query API on it.

use camino::{Utf8Path, Utf8PathBuf};
use rustc_hash::{FxHashMap, FxHashSet};
use serde::{Deserialize, Serialize};
use walkdir::WalkDir;

use crate::scene::{self, NodeType, ResolvedRoot, Scene, SceneNode, MAX_INSTANCE_DEPTH};

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
    /// `uid://…` → `res://…`, a copy of [`crate::ProjectModel::uids`]. The SAME map
    /// [`crate::Index::set_uid_map`] receives, never a second resolver. #484.
    uids: FxHashMap<String, String>,
    /// `uid://…` → the scenes whose ext-resource table names that uid with no `path`, resolved or
    /// not. Derived from the RAW table, so it survives canonicalization and answers "which scenes
    /// must be re-read when this sidecar appears, changes, or is deleted?".
    uid_referencers: FxHashMap<String, FxHashSet<String>>,
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
    pub fn build(root: &Utf8Path, uids: FxHashMap<String, String>) -> Self {
        let mut idx = SceneIndex::new();
        // Before the first `reindex`, so every scene canonicalizes against a populated map.
        idx.set_uid_map(uids);
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
        let mut scene = scene;
        // Drop the old scene's reverse entries before inserting the new one.
        self.remove_reverse(&key);
        // The uid referencers come off the RAW ext table, which canonicalization never touches;
        // the script/instance maps come off the node fields, which it rewrites. So: referencers
        // first, then rewrite, then the rest — otherwise the forward maps would key by `uid://`.
        self.add_uid_referencers(&key, &scene);
        self.canonicalize(&mut scene);
        self.add_reverse(&key, &scene);
        self.scenes.insert(key, scene);
    }

    /// Rewrite every `uid://` a pure parse left in this scene's node fields to the `res://` path
    /// the project's sidecars declare. #484.
    ///
    /// An unresolvable uid becomes `None`, never the uid string: that is exactly the behavior
    /// before this existed — no script, no instance, the node still present with its native type —
    /// and it keeps a `uid://` out of every consumer that compares these fields as paths or shows
    /// them to a user. Idempotent, since no `uid://` survives the pass.
    fn canonicalize(&self, scene: &mut Scene) {
        let deref = |s: &String| -> Option<String> {
            if s.starts_with("uid://") {
                self.uids.get(s).cloned()
            } else {
                Some(s.clone())
            }
        };
        for node in &mut scene.nodes {
            if let Some(script) = &node.script {
                node.script = deref(script);
            }
            if let scene::NodeType::Instanced(Some(sub)) = &node.ty {
                node.ty = scene::NodeType::Instanced(deref(sub));
            }
        }
    }

    /// Replace the uid map. Stored scenes are NOT retro-fixed here — the caller re-reads the
    /// scenes [`Self::scenes_referencing_uid`] names, which is the only set that can change.
    pub fn set_uid_map(&mut self, uids: FxHashMap<String, String>) {
        self.uids = uids;
    }

    /// The scenes whose ext-resource table names `uid` with no `path`. The work list for a sidecar
    /// that appeared, changed, or was deleted.
    #[must_use = "iterators are lazy and do nothing unless consumed"]
    pub fn scenes_referencing_uid(&self, uid: &str) -> impl Iterator<Item = &str> + '_ {
        self.uid_referencers
            .get(uid)
            .into_iter()
            .flat_map(|set| set.iter().map(String::as_str))
    }

    /// Every scene carrying at least one `path`-less uid reference. The warm-start work list: a
    /// sidecar changed while gdls was off leaves an unchanged `.tscn`, which no stat diff catches.
    #[must_use]
    pub fn uid_referencing_scenes(&self) -> Vec<String> {
        let mut out: FxHashSet<&String> = FxHashSet::default();
        for set in self.uid_referencers.values() {
            out.extend(set.iter());
        }
        let mut out: Vec<String> = out.into_iter().cloned().collect();
        out.sort();
        out
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

    // --- Phase-2 node-path resolution -----------------------------------------------------------
    //
    // These resolve a `$RelPath` / `%UniqueName` access made by a script ATTACHED to a node in
    // `scene_res` into the concrete [`ResolvedRoot`] (native class and/or attached script) of the
    // target node, following instanced sub-scenes through the index's own parsed scenes (anti-catalog
    // W16: parsed text only, never an engine instantiation). They are the gd_project half of M11
    // Phase-2 scene typing — the analyzer's `CrossFileQuery::scene_node_facts` (in `gd_server`) maps
    // the returned `ResolvedRoot` to the analyzer's fact type. Every uncertainty (missing scene,
    // absent node, unresolvable instance, cycle/depth cap) degrades to `None` (stay permissive),
    // never a wrong type — the no-false-positive bar.

    /// Resolve `rel_path` (a root-relative `$A/B`-style path with NO leading `/` and NO `%`) as seen
    /// by a script attached at `attachment_path` within `scene_res`. The access target's
    /// root-relative path is `attachment_path` joined with `rel_path` (Godot resolves `$Rel` relative
    /// to the node owning the script). `None` if the scene, the joined node, or the node's type chain
    /// can't be resolved.
    #[must_use]
    pub fn resolve_relative_from(
        &self,
        scene_res: &str,
        attachment_path: &str,
        rel_path: &str,
    ) -> Option<ResolvedRoot> {
        let scene = self.scene(scene_res)?;
        let target_path = join_node_path(attachment_path, rel_path)?;
        let node = scene.node_at(&target_path)?;
        self.resolve_node_root_via_index(node, &mut FxHashSet::default(), 0)
    }

    /// Resolve `%unique_name` (owner-scoped, single-segment) as seen by a script attached anywhere in
    /// `scene_res`. Unique names are looked up in the scene's owner-wide table, not relative to the
    /// attachment node. `None` if the scene has no such unique node or its type chain can't resolve.
    #[must_use]
    pub fn resolve_unique_in(&self, scene_res: &str, unique_name: &str) -> Option<ResolvedRoot> {
        let scene = self.scene(scene_res)?;
        let node = scene.node_by_unique_name(unique_name)?;
        self.resolve_node_root_via_index(node, &mut FxHashSet::default(), 0)
    }

    /// The direct children of the node reached by `$rel_path` as seen by a script attached at
    /// `attachment_path` within `scene_res` — the candidate set for `$RelPath/<cursor>` completion
    /// (M11 Phase 3). The base node's root-relative path is `attachment_path` joined with `rel_path`
    /// (lexically, via [`join_node_path`], so `.`/`..` resolve and an escape above the root yields
    /// nothing); each returned `(name, ResolvedRoot)` is a direct child's name and its resolved root
    /// (native type / attached script, instanced sub-scenes followed for the type). Names are sorted
    /// for deterministic ranking. Empty when the scene, the base node, or its children don't resolve.
    ///
    /// SAME-SCENE only: a base that lands ON an instanced sub-scene resolves its TYPE (via
    /// `ResolvedRoot`) but its sub-tree children are not enumerated here — that cross-scene walk is a
    /// documented Phase-3 deferral; this lists children declared directly in `scene_res`.
    #[must_use]
    pub fn children_relative_from(
        &self,
        scene_res: &str,
        attachment_path: &str,
        rel_path: &str,
    ) -> Vec<(String, ResolvedRoot)> {
        let Some(scene) = self.scene(scene_res) else {
            return Vec::new();
        };
        let Some(base_path) = join_node_path(attachment_path, rel_path) else {
            return Vec::new();
        };
        // The base must name an actual node (so `$Bogus/` lists nothing, not the root's children).
        if scene.node_at(&base_path).is_none() {
            return Vec::new();
        }
        let mut out: Vec<(String, ResolvedRoot)> = scene
            .nodes
            .iter()
            .filter(|n| !n.name.is_empty() && is_direct_child(&base_path, &n.path))
            .filter_map(|n| {
                let resolved = self.resolve_node_root_via_index(n, &mut FxHashSet::default(), 0)?;
                Some((n.name.clone(), resolved))
            })
            .collect();
        out.sort_by(|a, b| a.0.cmp(&b.0));
        out
    }

    /// Every `unique_name_in_owner` node of `scene_res`, as `(unique_name, ResolvedRoot)` — the
    /// candidate set for `%<cursor>` completion (M11 Phase 3). Owner-scoped (attachment-independent),
    /// mirroring [`Self::resolve_unique_in`]. Names sorted; empty if the scene isn't indexed.
    #[must_use]
    pub fn unique_nodes_in(&self, scene_res: &str) -> Vec<(String, ResolvedRoot)> {
        let Some(scene) = self.scene(scene_res) else {
            return Vec::new();
        };
        let mut out: Vec<(String, ResolvedRoot)> = scene
            .unique_names
            .keys()
            .filter_map(|name| {
                let node = scene.node_by_unique_name(name)?;
                let resolved =
                    self.resolve_node_root_via_index(node, &mut FxHashSet::default(), 0)?;
                Some((name.clone(), resolved))
            })
            .collect();
        out.sort_by(|a, b| a.0.cmp(&b.0));
        out
    }

    /// Index-backed analogue of [`scene::resolve_node_root`](crate::scene): resolve one node's
    /// contributed root, recursing through instanced sub-scenes by looking the PackedScene up in this
    /// index (the parsed scene, no disk re-read), bounded by a `visited` cycle set and
    /// [`MAX_INSTANCE_DEPTH`]. Mirrors the text-based resolver's three behaviours exactly: a Native
    /// node yields its native type plus any local `script=`; an Instanced node recurses into the
    /// sub-scene root with the instancing node's local `script=` overriding the packed root's script;
    /// an unresolvable / typeless node yields just its local script.
    fn resolve_node_root_via_index(
        &self,
        node: &SceneNode,
        visited: &mut FxHashSet<String>,
        depth: usize,
    ) -> Option<ResolvedRoot> {
        match &node.ty {
            NodeType::Native(ty) => Some(ResolvedRoot {
                native_type: Some(ty.clone()),
                script: node.script.clone(),
            }),
            NodeType::Instanced(Some(sub_path)) => {
                if depth >= MAX_INSTANCE_DEPTH {
                    return None; // depth backstop — degrade, never recurse unbounded
                }
                let key = scene::normalize_res(sub_path);
                // Insert BEFORE recursing so a self-/mutually-instancing graph terminates.
                if !visited.insert(key.clone()) {
                    return None; // cycle — already on the current resolution path
                }
                let sub = self.scenes.get(&key)?; // sub-scene not indexed → degrade
                let sub_root = sub.root_node()?;
                let local_script = node.script.clone();
                let mut resolved =
                    self.resolve_node_root_via_index(sub_root, visited, depth + 1)?;
                // The local `script=` on the instancing node overrides the packed root's script.
                if local_script.is_some() {
                    resolved.script = local_script;
                }
                Some(resolved)
            }
            NodeType::Instanced(None) | NodeType::Unknown => Some(ResolvedRoot {
                native_type: None,
                script: node.script.clone(),
            }),
        }
    }

    // --- Reverse-map maintenance ----------------------------------------------------------------

    /// Add `scene`'s attached-script and instanced-sub-scene reverse entries under key `scene_key`.
    /// Record this scene under every `path`-less uid its ext table names. Reads the raw table, so
    /// it is independent of whether the uid resolved.
    fn add_uid_referencers(&mut self, scene_key: &str, scene: &Scene) {
        for ext in scene.ext_resources.values() {
            if ext.path.is_some() {
                continue;
            }
            if let Some(uid) = &ext.uid {
                self.uid_referencers
                    .entry(uid.clone())
                    .or_default()
                    .insert(scene_key.to_owned());
            }
        }
    }

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
        let old_uids: Vec<String> = old
            .ext_resources
            .values()
            .filter(|e| e.path.is_none())
            .filter_map(|e| e.uid.clone())
            .collect();
        for uid in old_uids {
            if let Some(set) = self.uid_referencers.get_mut(&uid) {
                set.remove(scene_key);
                if set.is_empty() {
                    self.uid_referencers.remove(&uid);
                }
            }
        }
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

/// Join a node's root-relative `attachment` path with a `$rel` access path (no leading `/`, no `%`),
/// resolving `.`/`..` segments lexically against the scene tree. Returns the target node's
/// root-relative path, or `None` if a `..` would escape ABOVE the scene root (popping an empty
/// stack): such a path names no node in the scene, and `None` is the safe permissive outcome (a
/// resolved-but-escaped path string could spuriously match an unrelated node). An attachment of `""`
/// is the scene root, so `join_node_path("", "A/B")` is `Some("A/B")`.
fn join_node_path(attachment: &str, rel: &str) -> Option<String> {
    let mut parts: Vec<&str> = if attachment.is_empty() {
        Vec::new()
    } else {
        attachment.split('/').collect()
    };
    for seg in rel.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                // A `..` above the root escapes the scene tree — refuse rather than silently
                // resolving to a wrong (or root) node.
                parts.pop()?;
            }
            s => parts.push(s),
        }
    }
    Some(parts.join("/"))
}

/// Whether `child_path` is a DIRECT child of the node at `parent_path` (both root-relative). The
/// root is `""`, so its direct children are the single-segment paths (`"A"`, `"B"` — no `/`); a node
/// `"A"`'s direct children are `"A/<name>"` with exactly one more segment. Used by
/// [`SceneIndex::children_relative_from`] to list a path prefix's immediate children.
fn is_direct_child(parent_path: &str, child_path: &str) -> bool {
    if parent_path.is_empty() {
        // Root's direct children: non-empty single-segment paths (the root itself is "").
        !child_path.is_empty() && !child_path.contains('/')
    } else {
        match child_path.strip_prefix(parent_path) {
            // `parent/<seg>` with exactly one trailing segment (no further `/`).
            Some(rest) => rest.strip_prefix('/').is_some_and(|seg| !seg.contains('/')),
            None => false,
        }
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
    fn resolve_relative_from_root_attachment() {
        // main.gd is attached at the parent scene's ROOT (path ""). `$Sub` from the root resolves to
        // the instanced child.tscn → child's Panel root + child.gd script (script wins).
        let idx = build();
        let r = idx
            .resolve_relative_from("res://parent.tscn", "", "Sub")
            .expect("Sub resolves through the instanced child scene");
        assert_eq!(r.native_type.as_deref(), Some("Panel"));
        assert_eq!(r.script.as_deref(), Some("res://child.gd"));
    }

    #[test]
    fn resolve_relative_native_node() {
        let mut idx = SceneIndex::new();
        idx.reindex(
            "res://s.tscn",
            "[gd_scene format=3]\n\
             [ext_resource type=\"Script\" path=\"res://s.gd\" id=\"1\"]\n\
             [node name=\"Root\" type=\"Node\"]\nscript = ExtResource(\"1\")\n\
             [node name=\"Child\" type=\"Sprite2D\" parent=\".\"]\n",
        );
        let r = idx
            .resolve_relative_from("res://s.tscn", "", "Child")
            .unwrap();
        assert_eq!(r.native_type.as_deref(), Some("Sprite2D"));
        assert_eq!(r.script, None);
    }

    #[test]
    fn children_relative_lists_direct_children_with_types() {
        // Root has two children (Health: Node2D, UI: Control); UI has a Button. `$` (from the root
        // attachment) lists Root's DIRECT children only; `$UI` lists UI's child.
        let mut idx = SceneIndex::new();
        idx.reindex(
            "res://s.tscn",
            "[gd_scene format=3]\n\
             [ext_resource type=\"Script\" path=\"res://s.gd\" id=\"1\"]\n\
             [node name=\"Root\" type=\"Node2D\"]\nscript = ExtResource(\"1\")\n\
             [node name=\"Health\" type=\"Node2D\" parent=\".\"]\n\
             [node name=\"UI\" type=\"Control\" parent=\".\"]\n\
             [node name=\"Button\" type=\"Button\" parent=\"UI\"]\n",
        );
        // `$` from the root: direct children Health + UI, NOT the nested Button. Sorted by name.
        let kids = idx.children_relative_from("res://s.tscn", "", "");
        let names: Vec<&str> = kids.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names, vec!["Health", "UI"], "direct children only, sorted");
        assert_eq!(kids[0].1.native_type.as_deref(), Some("Node2D")); // Health
        assert_eq!(kids[1].1.native_type.as_deref(), Some("Control")); // UI

        // `$UI/` → UI's direct child Button.
        let ui_kids = idx.children_relative_from("res://s.tscn", "", "UI");
        let ui_names: Vec<&str> = ui_kids.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(ui_names, vec!["Button"]);
        assert_eq!(ui_kids[0].1.native_type.as_deref(), Some("Button"));
    }

    #[test]
    fn children_relative_from_non_root_attachment() {
        // A script attached at `Wrap` accessing `$` lists Wrap's children (attachment-relative).
        let mut idx = SceneIndex::new();
        idx.reindex(
            "res://s.tscn",
            "[gd_scene format=3]\n\
             [node name=\"Root\" type=\"Node\"]\n\
             [node name=\"Wrap\" type=\"Control\" parent=\".\"]\n\
             [node name=\"Leaf\" type=\"Button\" parent=\"Wrap\"]\n",
        );
        let kids = idx.children_relative_from("res://s.tscn", "Wrap", "");
        let names: Vec<&str> = kids.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names, vec!["Leaf"]);
    }

    #[test]
    fn children_relative_unknown_base_is_empty() {
        // `$Bogus/` (a base that names no node) lists NOTHING — never falls back to the root.
        let mut idx = SceneIndex::new();
        idx.reindex(
            "res://s.tscn",
            "[gd_scene format=3]\n\
             [node name=\"Root\" type=\"Node\"]\n\
             [node name=\"A\" type=\"Node\" parent=\".\"]\n",
        );
        assert!(idx
            .children_relative_from("res://s.tscn", "", "Bogus")
            .is_empty());
        // A missing scene → empty.
        assert!(idx
            .children_relative_from("res://nope.tscn", "", "")
            .is_empty());
    }

    #[test]
    fn children_relative_resolves_instanced_child_type() {
        // A child that is an instanced sub-scene resolves its TYPE through the instance (script-first
        // via the sub-scene root), even though we don't enumerate ITS children here.
        let mut idx = SceneIndex::new();
        idx.reindex("res://child.tscn", CHILD); // root Panel + child.gd
        idx.reindex(
            "res://parent.tscn",
            "[gd_scene format=3]\n\
             [ext_resource type=\"PackedScene\" path=\"res://child.tscn\" id=\"2\"]\n\
             [node name=\"Root\" type=\"Node\"]\n\
             [node name=\"Sub\" parent=\".\" instance=ExtResource(\"2\")]\n",
        );
        let kids = idx.children_relative_from("res://parent.tscn", "", "");
        assert_eq!(kids.len(), 1);
        assert_eq!(kids[0].0, "Sub");
        // Sub's type comes from child.tscn's root (Panel) + its script (script-first at the caller).
        assert_eq!(kids[0].1.native_type.as_deref(), Some("Panel"));
        assert_eq!(kids[0].1.script.as_deref(), Some("res://child.gd"));
    }

    #[test]
    fn unique_nodes_lists_owner_unique_names() {
        let mut idx = SceneIndex::new();
        idx.reindex(
            "res://s.tscn",
            "[gd_scene format=3]\n\
             [node name=\"Root\" type=\"Node\"]\n\
             [node name=\"Wrap\" type=\"Control\" parent=\".\"]\n\
             [node name=\"Special\" type=\"Label\" parent=\"Wrap\"]\nunique_name_in_owner = true\n\
             [node name=\"Bar\" type=\"ProgressBar\" parent=\".\"]\nunique_name_in_owner = true\n",
        );
        let uniques = idx.unique_nodes_in("res://s.tscn");
        let names: Vec<&str> = uniques.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names, vec!["Bar", "Special"], "owner-unique names, sorted");
        assert_eq!(uniques[0].1.native_type.as_deref(), Some("ProgressBar")); // Bar
        assert_eq!(uniques[1].1.native_type.as_deref(), Some("Label")); // Special
    }

    #[test]
    fn is_direct_child_root_and_nested() {
        // Root ("") direct children: single-segment paths.
        assert!(is_direct_child("", "A"));
        assert!(!is_direct_child("", "A/B")); // nested, not direct
        assert!(!is_direct_child("", "")); // the root itself
                                           // "A"'s direct children: "A/<seg>".
        assert!(is_direct_child("A", "A/B"));
        assert!(!is_direct_child("A", "A/B/C")); // grandchild
        assert!(!is_direct_child("A", "AB")); // a sibling whose name starts with A
        assert!(!is_direct_child("A", "A")); // the node itself
    }

    #[test]
    fn resolve_unique_in_owner_scope() {
        let mut idx = SceneIndex::new();
        idx.reindex(
            "res://s.tscn",
            "[gd_scene format=3]\n\
             [node name=\"Root\" type=\"Node\"]\n\
             [node name=\"Wrap\" type=\"Control\" parent=\".\"]\n\
             [node name=\"Special\" type=\"Label\" parent=\"Wrap\"]\nunique_name_in_owner = true\n",
        );
        // `%Special` is owner-scoped: resolvable regardless of the attachment node's path.
        let r = idx.resolve_unique_in("res://s.tscn", "Special").unwrap();
        assert_eq!(r.native_type.as_deref(), Some("Label"));
        // A non-unique name yields None.
        assert!(idx.resolve_unique_in("res://s.tscn", "Wrap").is_none());
    }

    #[test]
    fn resolve_relative_missing_node_is_none() {
        let idx = build();
        assert!(idx
            .resolve_relative_from("res://parent.tscn", "", "Nonexistent")
            .is_none());
        // Missing scene → None.
        assert!(idx
            .resolve_relative_from("res://nope.tscn", "", "X")
            .is_none());
    }

    #[test]
    fn resolve_relative_from_non_root_attachment() {
        // A script attached at a non-root node `Wrap` accessing `$Leaf` resolves Wrap/Leaf.
        let mut idx = SceneIndex::new();
        idx.reindex(
            "res://s.tscn",
            "[gd_scene format=3]\n\
             [ext_resource type=\"Script\" path=\"res://wrap.gd\" id=\"1\"]\n\
             [node name=\"Root\" type=\"Node\"]\n\
             [node name=\"Wrap\" type=\"Control\" parent=\".\"]\nscript = ExtResource(\"1\")\n\
             [node name=\"Leaf\" type=\"Button\" parent=\"Wrap\"]\n",
        );
        let r = idx
            .resolve_relative_from("res://s.tscn", "Wrap", "Leaf")
            .unwrap();
        assert_eq!(r.native_type.as_deref(), Some("Button"));
    }

    #[test]
    fn resolve_relative_parent_segment() {
        // `$../Sibling` from Wrap resolves to a sibling of Wrap under the root.
        let mut idx = SceneIndex::new();
        idx.reindex(
            "res://s.tscn",
            "[gd_scene format=3]\n\
             [node name=\"Root\" type=\"Node\"]\n\
             [node name=\"Wrap\" type=\"Control\" parent=\".\"]\n\
             [node name=\"Sibling\" type=\"Timer\" parent=\".\"]\n",
        );
        let r = idx
            .resolve_relative_from("res://s.tscn", "Wrap", "../Sibling")
            .unwrap();
        assert_eq!(r.native_type.as_deref(), Some("Timer"));
    }

    #[test]
    fn resolve_relative_parent_escape_above_root_is_none() {
        // `..` that escapes ABOVE the scene root must refuse (a path naming no node), not silently
        // resolve to the root (or an unrelated node). From `Wrap` (depth 1), `../../X` pops Wrap →
        // root, then pops the empty stack → escape → `None`. `../..` from `Wrap` likewise escapes.
        let mut idx = SceneIndex::new();
        idx.reindex(
            "res://s.tscn",
            "[gd_scene format=3]\n\
             [node name=\"Root\" type=\"Node\"]\n\
             [node name=\"Wrap\" type=\"Control\" parent=\".\"]\n\
             [node name=\"X\" type=\"Timer\" parent=\".\"]\n",
        );
        assert!(
            idx.resolve_relative_from("res://s.tscn", "Wrap", "../../X")
                .is_none(),
            "`../../X` from a depth-1 node escapes the scene root → None"
        );
        assert!(
            idx.resolve_relative_from("res://s.tscn", "Wrap", "../..")
                .is_none(),
            "`../..` from a depth-1 node escapes the scene root → None"
        );
        // Sanity: the in-bounds `../X` still resolves (the escape guard is not over-broad).
        let ok = idx
            .resolve_relative_from("res://s.tscn", "Wrap", "../X")
            .unwrap();
        assert_eq!(ok.native_type.as_deref(), Some("Timer"));
    }

    #[test]
    fn resolve_instanced_subscene_via_index_no_disk() {
        // The instanced sub-scene resolution walks the INDEX's parsed scenes — both scenes are in
        // the index, no text lookup closure. Parent's `Sub` is child.tscn instanced.
        let idx = build();
        let r = idx
            .resolve_relative_from("res://parent.tscn", "", "Sub")
            .unwrap();
        assert_eq!(r.native_type.as_deref(), Some("Panel"));
    }

    #[test]
    fn resolve_instanced_subscene_not_indexed_is_none() {
        // Parent instances child.tscn but child is NOT in the index → degrade to None (permissive),
        // never a bare/wrong type.
        let mut idx = SceneIndex::new();
        idx.reindex("res://parent.tscn", PARENT);
        assert!(idx
            .resolve_relative_from("res://parent.tscn", "", "Sub")
            .is_none());
    }

    #[test]
    fn resolve_cyclic_instance_terminates_none() {
        // a.tscn root instances b.tscn; b.tscn root instances a.tscn. Resolving the root via index
        // must terminate at the cycle (None), not hang.
        let mut idx = SceneIndex::new();
        idx.reindex(
            "res://a.tscn",
            "[gd_scene format=3]\n\
             [ext_resource type=\"PackedScene\" path=\"res://b.tscn\" id=\"1\"]\n\
             [node name=\"ARoot\" instance=ExtResource(\"1\")]\n",
        );
        idx.reindex(
            "res://b.tscn",
            "[gd_scene format=3]\n\
             [ext_resource type=\"PackedScene\" path=\"res://a.tscn\" id=\"1\"]\n\
             [node name=\"BRoot\" instance=ExtResource(\"1\")]\n",
        );
        // The root node of a.tscn IS the instanced node; `resolve_relative_from(.., "", "")` targets
        // the root (joined path "").
        assert!(idx.resolve_relative_from("res://a.tscn", "", "").is_none());
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
