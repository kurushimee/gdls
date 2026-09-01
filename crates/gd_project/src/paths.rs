//! `res://` ↔ filesystem mapping and the UID map.
//!
//! `gd_project` owns the project root, so it owns path resolution; `gd_server` bridges LSP URIs to
//! `res://`. Godot 4.4+ gives every resource a uid and writes it in one of three places — a
//! `<resource>.uid` sidecar, the `[remap]` block of a `<resource>.import`, or the header line of a
//! text `.tres`/`.tscn`. Scanning all three gives a `uid:// → res://` map, so a `uid://` autoload,
//! `main_scene`, or `preload` resolves without parsing the editor's binary caches.

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

/// Where a resource's uid was written. Godot spreads it across three file kinds, and a project
/// carries all three at once: a script has a `.uid` sidecar, an imported texture has its uid in the
/// `[remap]` block of its `.import`, and a text resource or scene carries it in its own header
/// line. Ranked so a resource that somehow declares two keeps a deterministic answer — the
/// `.import` is what the editor's own scan trusts for a file it imports, the header next, the bare
/// sidecar last.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum UidSource {
    Sidecar,
    Header,
    Import,
}

/// Godot's sentinel for "this file has no uid" (`core/io/resource_uid.cpp`). It must never enter
/// the map, or every file that carries it would resolve to whichever one was walked last.
const INVALID_UID: &str = "uid://<invalid>";

/// The uid a single file declares, and the resource it declares it FOR. `None` for a file kind
/// that declares none, an unreadable file, or a body that is not a `uid://…`.
pub fn uid_declaration(path: &Utf8Path) -> Option<(Utf8PathBuf, String, UidSource)> {
    let (resource, uid, source) = match path.extension() {
        // `foo.gd.uid` describes the resource `foo.gd`, and its whole body is the uid.
        Some("uid") => {
            let resource = Utf8PathBuf::from(path.as_str().strip_suffix(".uid")?);
            let uid = std::fs::read_to_string(path).ok()?.trim().to_owned();
            (resource, uid, UidSource::Sidecar)
        }
        // `icon.png.import` describes `icon.png`, and carries `uid="uid://…"` under `[remap]`.
        Some("import") => {
            let resource = Utf8PathBuf::from(path.as_str().strip_suffix(".import")?);
            let text = std::fs::read_to_string(path).ok()?;
            (resource, quoted_uid(&text)?, UidSource::Import)
        }
        // A text resource or scene declares its own uid on its first line, `[gd_resource … uid="…"]`.
        Some("tres") | Some("tscn") => {
            let first = first_line(path)?;
            (path.to_owned(), quoted_uid(&first)?, UidSource::Header)
        }
        _ => return None,
    };
    (uid.starts_with("uid://") && uid != INVALID_UID).then_some((resource, uid, source))
}

/// The first line of a file, without reading the rest of it. A scene is the largest text file in a
/// Godot project and only its header line ever carries a uid.
fn first_line(path: &Utf8Path) -> Option<String> {
    use std::io::BufRead;
    let file = std::fs::File::open(path).ok()?;
    let mut line = String::new();
    std::io::BufReader::new(file).read_line(&mut line).ok()?;
    Some(line)
}

/// `uid="uid://abc"` anywhere in `text` → `uid://abc`. Written as a scan for the quoted assignment
/// rather than a line parse so it reads the same out of a `.import` block and a header line.
fn quoted_uid(text: &str) -> Option<String> {
    let at = text.find("uid=\"")?;
    let rest = &text[at + 5..];
    let end = rest.find('"')?;
    Some(rest[..end].to_owned())
}

/// Scan the project tree and build a `uid:// → res://path` map. Skips the import cache (`.godot/`),
/// whose copies carry the same uids as the originals. Unreadable files are silently skipped.
///
/// Two resources claiming ONE uid — what copying a file without re-importing it leaves behind —
/// drops the uid entirely. Answering with either one would be a coin flip, and an unresolved
/// `preload` degrades to the same `Variant` it had before the map existed.
pub fn build_uid_map(root: &Utf8Path) -> FxHashMap<String, String> {
    // res:// → (source rank, uid), so a resource declaring its uid twice settles deterministically.
    let mut by_resource: FxHashMap<String, (UidSource, String)> = FxHashMap::default();
    for entry in WalkDir::new(root)
        .into_iter()
        .filter_entry(|e| e.file_name() != ".godot")
        .flatten()
    {
        let Some(path) = Utf8Path::from_path(entry.path()) else {
            continue;
        };
        let Some((resource, uid, source)) = uid_declaration(path) else {
            continue;
        };
        let Some(res) = path_to_res(root, &resource) else {
            continue;
        };
        match by_resource.get(&res) {
            Some((seen, _)) if *seen >= source => {}
            _ => {
                by_resource.insert(res, (source, uid));
            }
        }
    }
    let mut map: FxHashMap<String, String> = FxHashMap::default();
    let mut contested: Vec<String> = Vec::new();
    for (res, (_, uid)) in by_resource {
        if let Some(other) = map.insert(uid.clone(), res) {
            log::debug!(
                "uid {uid} is claimed by more than one resource (including {other}); dropping it"
            );
            contested.push(uid);
        }
    }
    for uid in contested {
        map.remove(&uid);
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

    /// The three places Godot writes a uid, harvested from one walk.
    #[test]
    fn the_uid_map_reads_all_three_sources() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = Utf8Path::from_path(dir.path()).expect("utf-8 tempdir");
        std::fs::write(root.join("main.gd"), "extends Node\n").unwrap();
        std::fs::write(root.join("main.gd.uid"), "uid://script1\n").unwrap();
        std::fs::write(root.join("icon.png"), "").unwrap();
        std::fs::write(
            root.join("icon.png.import"),
            "[remap]\n\nimporter=\"texture\"\ntype=\"CompressedTexture2D\"\nuid=\"uid://tex1\"\n",
        )
        .unwrap();
        std::fs::write(
            root.join("theme.tres"),
            "[gd_resource type=\"Theme\" format=3 uid=\"uid://theme1\"]\n\n[resource]\n",
        )
        .unwrap();
        std::fs::write(
            root.join("world.tscn"),
            "[gd_scene format=3 uid=\"uid://scene1\"]\n\n[node name=\"Root\" type=\"Node\"]\n",
        )
        .unwrap();

        let map = build_uid_map(root);
        assert_eq!(
            map.get("uid://script1").map(String::as_str),
            Some("res://main.gd")
        );
        assert_eq!(
            map.get("uid://tex1").map(String::as_str),
            Some("res://icon.png")
        );
        assert_eq!(
            map.get("uid://theme1").map(String::as_str),
            Some("res://theme.tres")
        );
        assert_eq!(
            map.get("uid://scene1").map(String::as_str),
            Some("res://world.tscn")
        );
    }

    /// Godot's own "no uid" sentinel sits in freshly created files. Letting it in would point every
    /// one of them at whichever file the walk reached last.
    #[test]
    fn the_invalid_sentinel_never_enters_the_map() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = Utf8Path::from_path(dir.path()).expect("utf-8 tempdir");
        std::fs::write(root.join("a.gd.uid"), "uid://<invalid>\n").unwrap();
        std::fs::write(
            root.join("b.tres"),
            "[gd_resource type=\"Resource\" format=3]\n",
        )
        .unwrap();
        std::fs::write(root.join("c.png.import"), "[remap]\n\ntype=\"Image\"\n").unwrap();
        assert!(build_uid_map(root).is_empty());
    }

    /// A file copied without re-importing carries its source's uid. Answering with either resource
    /// would be a coin flip, so the uid resolves to nothing at all.
    #[test]
    fn a_uid_two_resources_claim_resolves_to_neither() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = Utf8Path::from_path(dir.path()).expect("utf-8 tempdir");
        std::fs::write(root.join("a.gd.uid"), "uid://same\n").unwrap();
        std::fs::write(root.join("b.gd.uid"), "uid://same\n").unwrap();
        std::fs::write(root.join("c.gd.uid"), "uid://other\n").unwrap();
        let map = build_uid_map(root);
        assert!(!map.contains_key("uid://same"));
        assert_eq!(
            map.get("uid://other").map(String::as_str),
            Some("res://c.gd")
        );
    }

    /// One resource declaring its uid twice settles on the higher-ranked source rather than on
    /// whichever the walk happened to reach last.
    #[test]
    fn two_sources_for_one_resource_settle_by_rank() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = Utf8Path::from_path(dir.path()).expect("utf-8 tempdir");
        std::fs::write(root.join("icon.png"), "").unwrap();
        std::fs::write(root.join("icon.png.uid"), "uid://stale\n").unwrap();
        std::fs::write(
            root.join("icon.png.import"),
            "[remap]\n\nuid=\"uid://fresh\"\n",
        )
        .unwrap();
        let map = build_uid_map(root);
        assert_eq!(
            map.get("uid://fresh").map(String::as_str),
            Some("res://icon.png")
        );
        assert!(!map.contains_key("uid://stale"));
    }

    /// The import cache holds copies carrying the originals' uids; walking it would make every
    /// entry contested and collapse the whole map.
    #[test]
    fn the_import_cache_is_not_walked() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = Utf8Path::from_path(dir.path()).expect("utf-8 tempdir");
        std::fs::create_dir_all(root.join(".godot/imported")).unwrap();
        std::fs::write(root.join("a.gd.uid"), "uid://only\n").unwrap();
        std::fs::write(root.join(".godot/imported/a.gd.uid"), "uid://only\n").unwrap();
        let map = build_uid_map(root);
        assert_eq!(
            map.get("uid://only").map(String::as_str),
            Some("res://a.gd")
        );
    }
}
