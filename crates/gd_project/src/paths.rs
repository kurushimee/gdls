//! `res://` ↔ filesystem mapping and the UID map.
//!
//! `gd_project` owns the project root, so it owns path resolution; `gd_server` bridges LSP URIs to
//! `res://`. Godot 4.4+ writes a `<resource>.uid` sidecar next to each resource containing its
//! `uid://…`; scanning those gives a `uid:// → res://` map so `uid://` autoloads and `main_scene`
//! can be resolved without parsing binary caches.

use camino::{Utf8Path, Utf8PathBuf};
use rustc_hash::FxHashMap;
use walkdir::WalkDir;

/// `"res://a/b.gd"` → `<root>/a/b.gd`.
///
/// Godot's `res://` is strictly project-rooted: the relative part is always a plain forward-slash
/// path with no traversal. We reject any `..`, absolute (`/x`), or drive-prefix (`C:`) component so
/// a crafted literal like `preload("res://../../etc/passwd")` can never join to an out-of-tree path
/// — which a consumer (`documentLink`/hover/`definition`) would otherwise surface as a `file://`
/// target outside the project root. Such literals are invalid in Godot anyway, so returning `None`
/// (unresolved) is faithful, not a regression.
pub fn res_to_path(root: &Utf8Path, res: &str) -> Option<Utf8PathBuf> {
    let rel = res.strip_prefix("res://")?;
    let all_normal = Utf8Path::new(rel).components().all(|c| {
        matches!(
            c,
            camino::Utf8Component::Normal(_) | camino::Utf8Component::CurDir
        )
    });
    if !all_normal {
        return None;
    }
    Some(root.join(rel))
}

/// `<root>/a/b.gd` → `"res://a/b.gd"` (forward slashes, as Godot writes them).
pub fn path_to_res(root: &Utf8Path, path: &Utf8Path) -> Option<String> {
    let rel = path.strip_prefix(root).ok()?;
    Some(format!("res://{}", rel.as_str().replace('\\', "/")))
}

/// Scan the project tree for `*.uid` sidecars and build a `uid:// → res://path` map. Skips the
/// import cache (`.godot/`). Unreadable files are silently skipped.
pub fn build_uid_map(root: &Utf8Path) -> FxHashMap<String, String> {
    let mut map = FxHashMap::default();
    for entry in WalkDir::new(root)
        .into_iter()
        .filter_entry(|e| e.file_name() != ".godot")
        .flatten()
    {
        let Some(path) = Utf8Path::from_path(entry.path()) else {
            continue;
        };
        if path.extension() != Some("uid") {
            continue;
        }
        let Ok(contents) = std::fs::read_to_string(path) else {
            continue;
        };
        let uid = contents.trim();
        if !uid.starts_with("uid://") {
            continue;
        }
        // `foo.gd.uid` describes the resource `foo.gd`.
        let Some(resource) = path.as_str().strip_suffix(".uid") else {
            continue;
        };
        if let Some(res) = path_to_res(root, Utf8Path::new(resource)) {
            map.insert(uid.to_owned(), res);
        }
    }
    map
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn res_path_round_trip() {
        let root = Utf8Path::new("/proj");
        let p = res_to_path(root, "res://src/a.gd").unwrap();
        assert_eq!(p, Utf8PathBuf::from("/proj/src/a.gd"));
        assert_eq!(path_to_res(root, &p).as_deref(), Some("res://src/a.gd"));
    }

    #[test]
    fn non_res_uri_is_none() {
        assert!(res_to_path(Utf8Path::new("/proj"), "user://x").is_none());
    }

    #[test]
    fn rejects_traversal_and_absolute_components() {
        let root = Utf8Path::new("/proj");
        // `..` traversal must not escape the project root.
        assert!(res_to_path(root, "res://../../etc/passwd").is_none());
        assert!(res_to_path(root, "res://a/../../b.gd").is_none());
        // A leading slash (absolute relative part) must not replace the root on join.
        assert!(res_to_path(root, "res:///etc/passwd").is_none());
        // `.` segments are harmless and stay rooted.
        assert_eq!(
            res_to_path(root, "res://./src/a.gd"),
            Some(Utf8PathBuf::from("/proj/src/a.gd"))
        );
    }
}
