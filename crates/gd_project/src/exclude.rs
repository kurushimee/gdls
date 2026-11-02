//! What never enters the index: exactly what Godot's editor filesystem scan never reaches.
//!
//! One set of rules, three callers: the cold index ([`crate::index::Index::build`]'s `gd_files`
//! walk), [`Workspace::reconcile`](../../gd_server/src/workspace.rs)'s disk walk, and the
//! filesystem watcher's post-receive filter. A script Godot never scans can't own a `class_name`
//! in Godot, so indexing one lets a scratch copy take over or hide a real project class.
//!
//! Godot's rules (`EditorFileSystem`, editor_file_system.cpp:1174-1185 and
//! `_should_skip_directory` at :3487-3506; the same loop again at :1475):
//! - a directory whose name starts with `.` (which covers `.godot/`, `.git/`, `.import/` and
//!   gdls's own `.gdls/` cache);
//! - a hidden entry: on Unix a name starting with `.`, on Windows the hidden file attribute;
//! - a directory holding a `.gdignore` file;
//! - a directory holding its own `project.godot` (a nested project).
//!
//! Godot skips nothing else: `addons/`, `target/` and `node_modules/` are all scanned. The editor
//! temp suffixes below never name a `.gd`, `.tscn` or resource Godot loads, so dropping them only
//! keeps watcher noise out.
//!
//! The name rules need no filesystem access ([`is_excluded`]); the directory-content and
//! attribute rules do ([`is_skipped_dir`], [`ScanFilter`], [`keep_walk_entry`]).

use camino::{Utf8Component, Utf8Path, Utf8PathBuf};
use rustc_hash::FxHashMap;

/// Suffixes matched on the *file name* part. Tmp/backup files from editors. (The cold index
/// already filters to the `.gd` extension, so these mostly matter to the watcher — but sharing
/// the full predicate keeps the three callers exactly in step.)
const EXCLUDED_SUFFIXES: &[&str] = &[".tmp", ".bak", ".swp", "~"];

/// A directory holding this file is skipped with everything under it.
pub const GDIGNORE: &str = ".gdignore";

/// A directory holding this file (other than the project root) is another project, skipped.
pub const PROJECT_GODOT: &str = "project.godot";

/// The Windows `FILE_ATTRIBUTE_HIDDEN` bit.
#[cfg(windows)]
const FILE_ATTRIBUTE_HIDDEN: u32 = 0x2;

/// WP-RD14: a project root, as the home of the "is this path under me, minus excluded
/// components?" contract. Replaces the implicit `(path, root)`-pair passed loosely to the former
/// free `is_excluded`: the relativization rule (strip the root prefix, fall back to the raw path
/// when the path is outside) now lives in one place — [`Self::relativize`] — that both
/// [`Self::is_excluded`] and any future root-relative query share.
///
/// Borrowed (`&Utf8Path`) rather than owning a `Utf8PathBuf` so the hot `WalkDir::filter_entry`
/// closures (one call per directory entry over a 10k-file tree) construct it for free, and so it
/// can wrap `ProjectModel.root` (a `Utf8PathBuf` used pervasively as a plain path) without forcing
/// that field's type to change.
#[derive(Clone, Copy, Debug)]
pub struct ProjectRoot<'a>(&'a Utf8Path);

impl<'a> ProjectRoot<'a> {
    #[must_use]
    pub fn new(root: &'a Utf8Path) -> Self {
        ProjectRoot(root)
    }

    /// The root path itself.
    #[must_use]
    pub fn as_path(self) -> &'a Utf8Path {
        self.0
    }

    /// `path` made relative to this root — or the raw `path` when it lies outside the root (so a
    /// caller's component scan still fires defensively for an unexpected out-of-root input).
    /// `strip_prefix` only fails when `path` isn't under the root; a dot-named *ancestor* of the
    /// root (a project at `~/.local/my-game/`) must never filter the whole tree, which is exactly
    /// what relativizing first prevents.
    #[must_use]
    pub fn relativize(self, path: &'a Utf8Path) -> &'a Utf8Path {
        path.strip_prefix(self.0).unwrap_or(path)
    }

    /// True if Godot's scan skips `path` by name alone: a dot-named directory between the root
    /// and the path, a hidden (dot-named) file on Unix, or an editor temp suffix. Needs no
    /// filesystem access; [`ScanFilter::is_ignored`] adds the rules that do.
    #[must_use]
    pub fn is_excluded(self, path: &Utf8Path) -> bool {
        if let Some(name) = path.file_name() {
            let lower = name.to_ascii_lowercase();
            if EXCLUDED_SUFFIXES
                .iter()
                .any(|suffix| lower.ends_with(suffix))
            {
                return true;
            }
        }
        let components: Vec<&str> = self
            .relativize(path)
            .components()
            .filter_map(|c| match c {
                Utf8Component::Normal(name) => Some(name),
                _ => None,
            })
            .collect();
        let Some((last, dirs)) = components.split_last() else {
            return false;
        };
        dirs.iter().any(|c| is_dot_name(c)) || (cfg!(not(windows)) && is_dot_name(last))
    }
}

/// A name Godot treats as hidden or special: it starts with `.` (and isn't `.` or `..`).
fn is_dot_name(name: &str) -> bool {
    name.starts_with('.') && name != "." && name != ".."
}

/// True if Godot's scan skips `path` by name alone. See [`ProjectRoot::is_excluded`].
///
/// WP-RD14: a thin shim over [`ProjectRoot::is_excluded`]. Kept as a free function because the
/// call sites read more clearly as `!is_excluded(p, root)`.
pub fn is_excluded(path: &Utf8Path, project_root: &Utf8Path) -> bool {
    ProjectRoot::new(project_root).is_excluded(path)
}

/// True if Godot's scan never descends into `dir`: it is dot-named, hidden, holds a `.gdignore`,
/// or holds its own `project.godot`. The project root itself is never skipped.
#[must_use]
pub fn is_skipped_dir(dir: &Utf8Path, project_root: &Utf8Path) -> bool {
    if dir == project_root {
        return false;
    }
    dir.file_name().is_some_and(is_dot_name)
        || dir.join(GDIGNORE).exists()
        || dir.join(PROJECT_GODOT).exists()
        || std::fs::symlink_metadata(dir).is_ok_and(|m| has_hidden_attribute(&m))
}

/// The `WalkDir::filter_entry` predicate for every project walk: keep `entry` unless Godot's scan
/// skips it. Non-UTF-8 entries are kept, so callers can count them rather than lose them here.
#[must_use]
pub fn keep_walk_entry(entry: &walkdir::DirEntry, project_root: &Utf8Path) -> bool {
    let Some(path) = Utf8Path::from_path(entry.path()) else {
        return true;
    };
    if path == project_root {
        return true;
    }
    if is_excluded(path, project_root) {
        return false;
    }
    if entry.file_type().is_dir() {
        return !is_skipped_dir(path, project_root);
    }
    // On Windows the walk's metadata comes from the directory listing, so this costs no stat.
    !entry.metadata().is_ok_and(|m| has_hidden_attribute(&m))
}

/// Whether `path` is a file whose appearance or removal moves a scan boundary: a `.gdignore`, or a
/// `project.godot` below the root. The index must then be re-walked, since a whole subtree joins
/// or leaves the project.
#[must_use]
pub fn is_scan_boundary_marker(path: &Utf8Path, project_root: &Utf8Path) -> bool {
    match path.file_name() {
        Some(GDIGNORE) => path.parent() != Some(project_root),
        Some(PROJECT_GODOT) => path.parent() != Some(project_root),
        _ => false,
    }
}

#[cfg(windows)]
fn has_hidden_attribute(metadata: &std::fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    metadata.file_attributes() & FILE_ATTRIBUTE_HIDDEN != 0
}

#[cfg(not(windows))]
fn has_hidden_attribute(_metadata: &std::fs::Metadata) -> bool {
    false // Unix hides by name, which `is_excluded` already covers.
}

/// Answers "would Godot's scan reach this path?" for one batch of watcher events, remembering
/// each directory's verdict so a burst of thousands of paths stats each directory once.
#[derive(Debug)]
pub struct ScanFilter<'a> {
    root: &'a Utf8Path,
    skipped_dirs: FxHashMap<Utf8PathBuf, bool>,
}

impl<'a> ScanFilter<'a> {
    #[must_use]
    pub fn new(project_root: &'a Utf8Path) -> Self {
        ScanFilter {
            root: project_root,
            skipped_dirs: FxHashMap::default(),
        }
    }

    pub fn root(&self) -> &'a Utf8Path {
        self.root
    }

    /// True if Godot's scan never reaches `path`: excluded by name, hidden, or under a directory
    /// Godot skips. A path outside the root is judged by name alone.
    pub fn is_ignored(&mut self, path: &Utf8Path) -> bool {
        if is_excluded(path, self.root) {
            return true;
        }
        if std::fs::symlink_metadata(path).is_ok_and(|m| has_hidden_attribute(&m)) {
            return true;
        }
        let mut dirs: Vec<&Utf8Path> = path
            .ancestors()
            .skip(1)
            .take_while(|dir| *dir != self.root && dir.starts_with(self.root))
            .collect();
        dirs.reverse(); // outermost first: one skipped ancestor settles it
        dirs.into_iter().any(|dir| self.dir_is_skipped(dir))
    }

    fn dir_is_skipped(&mut self, dir: &Utf8Path) -> bool {
        if let Some(&skipped) = self.skipped_dirs.get(dir) {
            return skipped;
        }
        let skipped = is_skipped_dir(dir, self.root);
        self.skipped_dirs.insert(dir.to_path_buf(), skipped);
        skipped
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> Utf8PathBuf {
        Utf8PathBuf::from(s)
    }

    #[test]
    fn is_excluded_catches_dot_dirs() {
        let root = p("/proj");
        assert!(is_excluded(&p("/proj/.godot/foo.bin"), &root));
        assert!(is_excluded(&p("/proj/.import/x.cache"), &root));
        assert!(is_excluded(&p("/proj/.git/HEAD"), &root));
        assert!(is_excluded(&p("/proj/.backup/old/player.gd"), &root));
        // The warm-start cache must never re-enter the index or the watcher.
        assert!(is_excluded(&p("/proj/.gdls/index.1.json"), &root));
        assert!(is_excluded(&p("/proj/.gdls/stray.gd"), &root));
    }

    #[test]
    fn godot_scans_build_and_tooling_dirs() {
        // Godot skips no directory by name except dot-named ones.
        let root = p("/proj");
        assert!(!is_excluded(&p("/proj/target/debug/x.gd"), &root));
        assert!(!is_excluded(&p("/proj/node_modules/dep/x.gd"), &root));
        assert!(!is_excluded(&p("/proj/addons/cool/foo.gdextension"), &root));
    }

    #[cfg(not(windows))]
    #[test]
    fn a_dot_file_is_hidden_on_unix() {
        assert!(is_excluded(&p("/proj/src/.hidden.gd"), &p("/proj")));
    }

    #[test]
    fn is_excluded_catches_tmp_suffixes() {
        let root = p("/proj");
        assert!(is_excluded(&p("/proj/src/foo.gd.tmp"), &root));
        assert!(is_excluded(&p("/proj/src/foo.bak"), &root));
        assert!(is_excluded(&p("/proj/src/.foo.swp"), &root));
        assert!(is_excluded(&p("/proj/src/foo~"), &root));
    }

    #[test]
    fn is_excluded_passes_normal_gd() {
        let root = p("/proj");
        assert!(!is_excluded(&p("/proj/src/foo.gd"), &root));
        assert!(!is_excluded(&p("/proj/project.godot"), &root));
        assert!(!is_excluded(&root, &root));
    }

    #[test]
    fn is_excluded_is_case_insensitive_for_suffixes() {
        assert!(is_excluded(&p("/proj/src/X.TMP"), &p("/proj")));
    }

    #[test]
    fn project_root_relativize_strips_prefix_and_falls_back() {
        let root = ProjectRoot::new(Utf8Path::new("/proj"));
        // Under the root → stripped.
        assert_eq!(
            root.relativize(Utf8Path::new("/proj/src/foo.gd")),
            Utf8Path::new("src/foo.gd")
        );
        // Outside the root → returned verbatim (so a defensive component scan still fires).
        assert_eq!(
            root.relativize(Utf8Path::new("/elsewhere/x.gd")),
            Utf8Path::new("/elsewhere/x.gd")
        );
        // The root itself → empty relative path.
        assert_eq!(root.relativize(Utf8Path::new("/proj")), Utf8Path::new(""));
    }

    #[test]
    fn a_dot_dir_above_the_root_filters_nothing() {
        // A project under `~/.local/` must not have its whole tree filtered: only components
        // *between* the root and the path count.
        let root = p("/home/dev/.local/my-game");
        assert!(!is_excluded(&root, &root));
        assert!(!is_excluded(
            &p("/home/dev/.local/my-game/src/player.gd"),
            &root
        ));
        assert!(is_excluded(
            &p("/home/dev/.local/my-game/.godot/uid_cache.bin"),
            &root
        ));
    }

    /// A scratch project tree on disk.
    struct Tree(tempfile::TempDir);

    impl Tree {
        fn new() -> Self {
            Tree(tempfile::tempdir().unwrap())
        }

        fn root(&self) -> Utf8PathBuf {
            Utf8PathBuf::from_path_buf(self.0.path().to_path_buf()).unwrap()
        }

        fn write(&self, rel: &str) -> Utf8PathBuf {
            let path = self.root().join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, "").unwrap();
            path
        }
    }

    #[test]
    fn a_gdignore_dir_is_skipped_with_everything_under_it() {
        let t = Tree::new();
        let root = t.root();
        t.write("project.godot");
        t.write("outputs/.gdignore");
        let copy = t.write("outputs/old/character.gd");
        let real = t.write("src/character.gd");

        assert!(is_skipped_dir(&root.join("outputs"), &root));
        assert!(!is_skipped_dir(&root.join("src"), &root));
        let mut filter = ScanFilter::new(&root);
        assert!(filter.is_ignored(&copy));
        assert!(!filter.is_ignored(&real));
    }

    #[test]
    fn a_nested_project_is_skipped_but_the_root_is_not() {
        let t = Tree::new();
        let root = t.root();
        t.write("project.godot");
        t.write("demo/project.godot");
        let nested = t.write("demo/player.gd");

        assert!(!is_skipped_dir(&root, &root));
        assert!(is_skipped_dir(&root.join("demo"), &root));
        assert!(ScanFilter::new(&root).is_ignored(&nested));
    }

    #[test]
    fn the_walk_never_enters_a_skipped_dir() {
        let t = Tree::new();
        let root = t.root();
        t.write("project.godot");
        t.write("outputs/.gdignore");
        t.write("outputs/copy.gd");
        t.write("demo/project.godot");
        t.write("demo/x.gd");
        t.write(".backup/y.gd");
        t.write("target/z.gd");
        t.write("src/real.gd");

        let mut found: Vec<String> = walkdir::WalkDir::new(&root)
            .into_iter()
            .filter_entry(|e| keep_walk_entry(e, &root))
            .filter_map(Result::ok)
            .filter(|e| e.path().extension().is_some_and(|x| x == "gd"))
            .map(|e| {
                Utf8Path::from_path(e.path())
                    .unwrap()
                    .strip_prefix(&root)
                    .unwrap()
                    .as_str()
                    .replace('\\', "/")
            })
            .collect();
        found.sort();
        assert_eq!(found, vec!["src/real.gd", "target/z.gd"]);
    }

    #[test]
    fn scan_boundary_markers() {
        let root = p("/proj");
        assert!(is_scan_boundary_marker(
            &p("/proj/outputs/.gdignore"),
            &root
        ));
        assert!(is_scan_boundary_marker(
            &p("/proj/demo/project.godot"),
            &root
        ));
        assert!(!is_scan_boundary_marker(&p("/proj/project.godot"), &root));
        assert!(!is_scan_boundary_marker(&p("/proj/src/a.gd"), &root));
    }
}
