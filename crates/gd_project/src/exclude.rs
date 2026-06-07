//! Shared path-exclusion predicate.
//!
//! One predicate, three callers: the cold index ([`crate::index::Index::build`]'s `gd_files`
//! walk), [`Workspace::reconcile`](../../gd_server/src/workspace.rs)'s disk walk, and the M4
//! filesystem watcher's post-receive filter. Keeping it here — in the lowest crate all three
//! share — is load-bearing: before this was shared, the cold index skipped only `.godot/` while
//! reconcile and the watcher skipped the full set, so startup over-included every `.gd` under
//! `target/`, `.git/`, `node_modules/`, etc. A vendored or build-copied script there could
//! register a `class_name` and shadow a real project class (last-writer-wins in the registry) —
//! the exact pollution the watcher's out-of-root guard and reconcile's exclusion were written to
//! prevent. All three now agree on what never enters the index.

use camino::Utf8Path;

/// Always-excluded path components per `docs/03 §6.1`. Plus defensive entries (`target/`,
/// `node_modules/`) for projects that share a dir with non-Godot tooling.
///
/// **`addons/` is deliberately NOT excluded.** Godot's addon mechanism puts user-installed
/// extensions there, including `.gd` scripts, `.gdextension` files, and `doc_classes/*.xml`
/// — all of which the index/watcher must surface so cross-file resolution stays correct. A stray
/// `.gd` under an addon will get indexed; this is intentional behavior. Operators who genuinely
/// don't want addon scripts in the index can omit them via project layout.
pub const EXCLUDED_COMPONENTS: &[&str] = &[
    ".godot",
    ".import",
    ".git",
    "target",
    "node_modules",
    ".gdls",
];

/// Suffixes matched on the *file name* part. Tmp/backup files from editors. (The cold index
/// already filters to the `.gd` extension, so these mostly matter to the watcher — but sharing
/// the full predicate keeps the three callers exactly in step.)
const EXCLUDED_SUFFIXES: &[&str] = &[".tmp", ".bak", ".swp", "~"];

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
    /// `strip_prefix` only fails when `path` isn't under the root; an excluded component in an
    /// *ancestor* of the root (a project at `~/dev/target/my-game/`) must never filter the whole
    /// tree, which is exactly what relativizing first prevents.
    #[must_use]
    pub fn relativize(self, path: &'a Utf8Path) -> &'a Utf8Path {
        path.strip_prefix(self.0).unwrap_or(path)
    }

    /// True if `path` is under (or matches) a path the index/watcher must never react to: an
    /// excluded *component* (`.godot/`, `target/`, …) between the root and the path, or an excluded
    /// filename suffix (`.tmp`, `~`, …). Matching is case-insensitive (macOS HFS+, Windows NTFS).
    #[must_use]
    pub fn is_excluded(self, path: &Utf8Path) -> bool {
        // Filename suffix check (works for both directories and files; a `.tmp/` directory is also
        // excluded). Lower-cased for case-insensitivity.
        if let Some(name) = path.file_name() {
            let lower = name.to_ascii_lowercase();
            if EXCLUDED_SUFFIXES
                .iter()
                .any(|suffix| lower.ends_with(suffix))
            {
                return true;
            }
        }
        self.relativize(path).components().any(|c| {
            let name = c.as_str().to_ascii_lowercase();
            EXCLUDED_COMPONENTS.iter().any(|&ex| ex == name)
        })
    }
}

/// True if `path` is under (or matches) a path the index/watcher must never react to.
///
/// For the watcher this is a *post-receive* filter (`notify` provides no per-path ignore API, so
/// the watcher post-filters every received batch). For the cold index and reconcile it gates
/// `WalkDir`'s `filter_entry` so excluded directories are never descended.
///
/// WP-RD14: a thin shim over [`ProjectRoot::is_excluded`] — the logic now lives on the newtype.
/// Kept as a free function because the hot walk-filter call sites read more clearly as
/// `!is_excluded(p, root)` than `!ProjectRoot::new(root).is_excluded(p)`.
pub fn is_excluded(path: &Utf8Path, project_root: &Utf8Path) -> bool {
    ProjectRoot::new(project_root).is_excluded(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use camino::Utf8PathBuf;

    fn p(s: &str) -> Utf8PathBuf {
        Utf8PathBuf::from(s)
    }

    #[test]
    fn is_excluded_catches_dot_dirs() {
        let root = p("/proj");
        assert!(is_excluded(&p("/proj/.godot/foo.bin"), &root));
        assert!(is_excluded(&p("/proj/.import/x.cache"), &root));
        assert!(is_excluded(&p("/proj/.git/HEAD"), &root));
        assert!(is_excluded(&p("/proj/target/debug/x"), &root));
        assert!(is_excluded(&p("/proj/node_modules/dep/x.gd"), &root));
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
        assert!(!is_excluded(&p("/proj/addons/cool/foo.gdextension"), &root));
        assert!(!is_excluded(&p("/proj/project.godot"), &root));
    }

    #[test]
    fn is_excluded_is_case_insensitive() {
        let root = p("/proj");
        assert!(is_excluded(&p("/proj/.GODOT/x"), &root));
        assert!(is_excluded(&p("/proj/src/X.TMP"), &root));
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
    fn is_excluded_ignores_excluded_components_in_parent_path() {
        // A project rooted under a dir literally named `target` must not have its whole tree
        // filtered: only components *between* the root and the path count.
        let root = p("/home/dev/target/my-game");
        assert!(!is_excluded(&root, &root));
        assert!(!is_excluded(
            &p("/home/dev/target/my-game/src/player.gd"),
            &root
        ));
        // But an excluded component *inside* the project still filters.
        assert!(is_excluded(
            &p("/home/dev/target/my-game/.godot/uid_cache.bin"),
            &root
        ));
        assert!(is_excluded(
            &p("/home/dev/target/my-game/target/build.gd"),
            &root
        ));
    }
}
