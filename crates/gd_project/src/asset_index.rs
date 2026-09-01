//! The asset index: every project file that is NOT a `.gd` script and NOT a `.tscn` scene, keyed by
//! its normalized `res://` path. This is the third `res://` source the `load`/`preload` resource-path
//! completion unions, alongside the script [`Index`](crate::index::Index) (`.gd`) and the
//! [`SceneIndex`](crate::scene_index::SceneIndex) (`.tscn`).
//!
//! **Why a standalone structure parallel to `SceneIndex`.** Godot's editor lists EVERY project file
//! for a resource-path completion (`gdscript_editor.cpp` `_get_directory_contents`, called with no
//! type filter by both the `load` and `preload` paths), not just scripts and scenes. Scripts and
//! scenes are already indexed for type analysis; arbitrary assets (textures, audio, `.tres`, …) carry
//! no interface to parse — they are pure `res://` PATHS. So this index stores only paths: no parse, no
//! reverse maps, no cross-table invariants. It lives side by side with the other two indexes in the
//! workspace, mirroring `SceneIndex`'s "standalone, not folded into `Index`" decision for the same
//! reason (an asset is not a `FileId` and participates in no script-index machinery).
//!
//! **Source-of-truth + sorted cache** (mirrors [`SceneIndexCache`](crate::scene_index::SceneIndexCache)).
//! The serialized form is a sorted `Vec<String>` of the res paths; [`AssetIndex::from_cache`]
//! reconstructs the set. A warm-loaded asset index is identical to a cold-built one by construction.
//!
//! **Scope.** Read-only, completion-only. The diagnostic path never consumes this — an asset path is
//! not a type. It exists purely to broaden `load("res://…/<cursor>")` / `preload(...)` completion to
//! the same file set Godot's editor offers.

use camino::{Utf8Path, Utf8PathBuf};
use rustc_hash::FxHashSet;
use serde::{Deserialize, Serialize};
use walkdir::WalkDir;

use crate::scene;

/// All arbitrary-asset `res://` paths in a project (everything that is NOT a `.gd` script and NOT a
/// `.tscn` scene), keyed by normalized `res://` path. Paths only — no parse, no reverse maps.
#[derive(Clone, Debug, Default)]
pub struct AssetIndex {
    /// The set of normalized `res://…` paths of arbitrary project assets.
    assets: FxHashSet<String>,
}

impl AssetIndex {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Cold-build the asset index by scanning every file under `root` from disk that is NOT a `.gd`
    /// script (covered by [`Index`](crate::index::Index)) and NOT a `.tscn` scene (covered by
    /// [`SceneIndex`](crate::scene_index::SceneIndex)), sharing the script index's exclusion set
    /// ([`crate::exclude::is_excluded`] — `.godot/`, `.import/`, `.git/`, `target/`, `node_modules/`,
    /// `.gdls/`, editor temp suffixes) so all three indexes agree on what enters them. A file that
    /// can't be keyed to a `res://` path or isn't under `root` is skipped (degrade, never fail), and
    /// walk/UTF-8 errors are logged at `warn` — matching [`SceneIndex::build`](crate::scene_index::SceneIndex::build).
    /// Assets are keyed by their `res://` path.
    #[must_use]
    pub fn build(root: &Utf8Path) -> Self {
        let mut idx = AssetIndex::new();
        for entry_result in WalkDir::new(root).into_iter().filter_entry(|e| {
            Utf8Path::from_path(e.path()).is_none_or(|p| !crate::exclude::is_excluded(p, root))
        }) {
            let entry = match entry_result {
                Ok(e) => e,
                Err(e) => {
                    log::warn!("asset index: walk error: {e}");
                    continue;
                }
            };
            // Files only — directories are not assets (their res:// prefix is derived at completion
            // time from the files under them).
            if !entry.file_type().is_file() {
                continue;
            }
            let Some(p) = Utf8Path::from_path(entry.path()) else {
                log::warn!("asset index: skipping non-UTF-8 path under {root}");
                continue;
            };
            // Scripts (.gd) and scenes (.tscn) are indexed elsewhere; skip them here so the three
            // sources never double-count (the completion consumer unions all three).
            if p.extension() == Some("gd") || scene::is_scene_path(p) {
                continue;
            }
            let Some(res) = crate::paths::path_to_res(root, p) else {
                continue; // not under root (shouldn't happen post-walk) — skip rather than mis-key
            };
            idx.insert(res);
        }
        idx
    }

    /// Number of indexed assets.
    #[must_use]
    pub fn len(&self) -> usize {
        self.assets.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.assets.is_empty()
    }

    /// Record an asset at `res_path` (a `res://…` non-script/non-scene file). The key is normalized
    /// via [`scene::normalize_res`] so a `\`-spelled path and a `/`-spelled one collapse, matching the
    /// scene index's key discipline. An engine sidecar ([`is_engine_sidecar`]) is dropped here rather
    /// than at each call site, so the cold walk, the incremental file event, and a stale cache all
    /// agree on what an asset is.
    pub fn insert(&mut self, res_path: impl Into<String>) {
        let res = scene::normalize_res(&res_path.into());
        if is_engine_sidecar(&res) {
            return;
        }
        self.assets.insert(res);
    }

    /// Drop the asset at `res_path` (a deleted file).
    pub fn remove(&mut self, res_path: &str) {
        self.assets.remove(&scene::normalize_res(res_path));
    }

    /// Whether `res_path` is indexed as an asset. An engine sidecar is never indexed, so this is
    /// always `false` for one.
    #[must_use]
    pub fn contains(&self, res_path: &str) -> bool {
        self.assets.contains(&scene::normalize_res(res_path))
    }

    /// Iterate every asset `res://…` path currently held.
    #[must_use = "iterators are lazy and do nothing unless consumed"]
    pub fn iter(&self) -> impl Iterator<Item = &str> + '_ {
        self.assets.iter().map(String::as_str)
    }
}

// ---------------------------------------------------------------------------
// Cache integration — store the sorted path list; reconstruct the set on load.
// ---------------------------------------------------------------------------

/// A serializable snapshot of an [`AssetIndex`]. Stores the asset res paths as a sorted vec for
/// deterministic output, mirroring [`SceneIndexCache`](crate::scene_index::SceneIndexCache).
#[derive(Serialize, Deserialize, Default)]
pub struct AssetIndexCache {
    /// `res://…` (normalized) asset paths, as a sorted vec for deterministic output.
    assets: Vec<String>,
}

impl AssetIndex {
    /// Produce a serializable snapshot (the asset paths, sorted).
    #[must_use]
    pub fn to_cache(&self) -> AssetIndexCache {
        let mut assets: Vec<String> = self.assets.iter().cloned().collect();
        assets.sort();
        AssetIndexCache { assets }
    }

    /// Reconstruct an [`AssetIndex`] from a snapshot. Total — the snapshot is a flat path list with no
    /// cross-table invariants, so there is nothing to verify.
    #[must_use]
    pub fn from_cache(cache: AssetIndexCache) -> Self {
        let mut idx = AssetIndex::new();
        for res in cache.assets {
            idx.insert(res);
        }
        idx
    }
}

/// Whether `path` is one of the two bookkeeping sidecars the engine writes beside a real file:
/// `<file>.uid` (the resource UID) and `<file>.import` (the import settings). Neither is a loadable
/// resource, and Godot keeps both out of the file system it completes resource paths from — a file
/// enters `EditorFileSystemDirectory` only when its extension is a recognized resource extension
/// (`_process_file_system`, `editor/file_system/editor_file_system.cpp`), and the uid scan skips the
/// pair by name (`if (ext == "uid" || ext == "import") { continue; }`, same file). The comparison is
/// case-insensitive because the engine lowercases the extension before its own check.
///
/// Deliberately just these two. gdls has no importer registry to reproduce the full recognized-
/// extension set, and a project's own `.json` / `.txt` / `.csv` are legitimately preloadable, so a
/// wider whitelist would hide real files.
#[must_use]
pub fn is_engine_sidecar(path: &str) -> bool {
    Utf8Path::new(path)
        .extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("uid") || e.eq_ignore_ascii_case("import"))
}

/// Resolve a `res://` asset path to its absolute filesystem path under `root` (no existence check),
/// mirroring [`crate::paths::res_to_path`]. Kept beside the asset index for call-site clarity.
#[must_use]
pub fn res_to_path(root: &Utf8Path, res: &str) -> Option<Utf8PathBuf> {
    crate::paths::res_to_path(root, res)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn engine_sidecars_are_never_assets() {
        let mut idx = AssetIndex::new();
        // The two the engine writes beside a real file. Neither is loadable.
        idx.insert("res://src/player.gd.uid");
        idx.insert("res://art/icon.png.import");
        // Case-insensitive: the engine lowercases the extension before its own check.
        idx.insert("res://art/icon.png.IMPORT");
        idx.insert("res://src/player.gd.UID");
        assert!(idx.is_empty(), "a sidecar must not enter the asset index");
        assert!(!idx.contains("res://src/player.gd.uid"));

        // The real files beside them still do, including a bare name that merely CONTAINS the word.
        idx.insert("res://art/icon.png");
        idx.insert("res://data/uid");
        idx.insert("res://data/import.json");
        idx.insert("res://data/uid.txt");
        assert_eq!(idx.len(), 4);
    }

    /// A cache written before the sidecar filter existed still carries them; reconstructing must
    /// drop them rather than resurrect them, so an upgrade doesn't need a cache bump.
    #[test]
    fn from_cache_drops_sidecars_a_stale_snapshot_carries() {
        let idx = AssetIndex::from_cache(AssetIndexCache {
            assets: vec![
                "res://art/icon.png".to_owned(),
                "res://art/icon.png.import".to_owned(),
                "res://src/player.gd.uid".to_owned(),
            ],
        });
        assert_eq!(idx.len(), 1);
        assert!(idx.contains("res://art/icon.png"));
    }

    #[test]
    fn insert_remove_contains() {
        let mut idx = AssetIndex::new();
        assert!(idx.is_empty());
        idx.insert("res://art/icon.png");
        idx.insert("res://data/config.tres");
        assert_eq!(idx.len(), 2);
        assert!(idx.contains("res://art/icon.png"));
        idx.remove("res://art/icon.png");
        assert!(!idx.contains("res://art/icon.png"));
        assert_eq!(idx.len(), 1);
    }

    #[test]
    fn normalizes_backslash_keys() {
        let mut idx = AssetIndex::new();
        idx.insert(r"res://art\icon.png");
        // Stored as the `/`-spelled normalized form, so a `/`-spelled lookup hits.
        assert!(idx.contains("res://art/icon.png"));
    }

    #[test]
    fn cache_round_trip_is_sorted_and_total() {
        let mut idx = AssetIndex::new();
        idx.insert("res://z.tres");
        idx.insert("res://a.png");
        idx.insert("res://m/sound.ogg");
        let snapshot = idx.to_cache();
        // Deterministic sorted output.
        assert_eq!(
            snapshot.assets,
            vec![
                "res://a.png".to_string(),
                "res://m/sound.ogg".to_string(),
                "res://z.tres".to_string(),
            ]
        );
        let json = serde_json::to_string(&snapshot).unwrap();
        let restored_snapshot: AssetIndexCache = serde_json::from_str(&json).unwrap();
        let restored = AssetIndex::from_cache(restored_snapshot);
        assert_eq!(restored.len(), 3);
        assert!(restored.contains("res://a.png"));
        assert!(restored.contains("res://m/sound.ogg"));
        assert!(restored.contains("res://z.tres"));
    }

    #[test]
    fn build_excludes_scripts_scenes_and_engine_dirs_includes_assets() {
        let dir = tempfile::tempdir().unwrap();
        let root = Utf8Path::from_path(dir.path()).unwrap();
        let write = |rel: &str, contents: &str| {
            let p = root.join(rel);
            if let Some(parent) = p.parent() {
                std::fs::create_dir_all(parent).unwrap();
            }
            std::fs::write(p, contents).unwrap();
        };
        // Assets — must be indexed.
        write("art/icon.png", "PNG-PLACEHOLDER");
        write("data/config.tres", "[gd_resource]");
        write("LICENSE", "MIT"); // a no-extension file is still a project asset
                                 // Scripts + scenes — indexed elsewhere, must NOT appear here.
        write("src/hero.gd", "extends Node\n");
        write("scenes/main.tscn", "[gd_scene format=3]\n");
        // Engine dir — excluded.
        write(".godot/imported/icon.png-abc.ctex", "binary");

        let idx = AssetIndex::build(root);
        assert!(idx.contains("res://art/icon.png"), "png is an asset");
        assert!(idx.contains("res://data/config.tres"), "tres is an asset");
        assert!(
            idx.contains("res://LICENSE"),
            "no-extension file is an asset"
        );
        assert!(!idx.contains("res://src/hero.gd"), ".gd indexed elsewhere");
        assert!(
            !idx.contains("res://scenes/main.tscn"),
            ".tscn indexed elsewhere"
        );
        assert!(
            !idx.contains("res://.godot/imported/icon.png-abc.ctex"),
            ".godot/ is excluded"
        );
    }
}
