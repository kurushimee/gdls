//! The `class_name` registry: the project-global map from a declared `class_name` to the file that
//! declares it and its (syntactic) base.
//!
//! Mirrors Godot's `ScriptServer` global-class table (`GlobalScriptClass { name, path, base,
//! is_abstract, is_tool }`). It is deliberately **DB-agnostic**: an `extends Foo` base is stored as a
//! bare [`BaseRef::Name`], *not* pre-classified as native-vs-script and *not* holding a
//! [`gd_types::Sym`]. Classifying a name (native class? another `class_name`? unknown?) needs the
//! native DB and the rest of the registry, which only the [`crate::index::Index`] has; doing it there
//! (Godot's separate `INHERITANCE_SOLVED` pass) keeps the registry free of stale cross-tier handles
//! after an M4 DB reload.

use camino::{Utf8Path, Utf8PathBuf};
use gd_syntax::ByteSpan;
use rustc_hash::FxHashMap;
use serde::{Deserialize, Serialize};

use crate::interface::Extends;

/// A class's immediate base, captured syntactically (resolved on demand by the [`crate::index::Index`]).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum BaseRef {
    /// No `extends` clause.
    None,
    /// `extends "<path>"` — the path literal as written (e.g. `res://b.gd`).
    Path(String),
    /// `extends Foo` / `extends A.B` — an identifier chain joined by `.`.
    Name(String),
}

impl BaseRef {
    fn from_extends(extends: &Extends) -> Self {
        match extends {
            Extends::None => BaseRef::None,
            Extends::Path { path, .. } => BaseRef::Path(path.clone()),
            Extends::Names(names) => BaseRef::Name(names.join(".")),
        }
    }
}

/// A registered global class: the file it lives in and its base.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClassEntry {
    /// Absolute path of the declaring `.gd` file.
    pub path: Utf8PathBuf,
    pub base: BaseRef,
    pub is_abstract: bool,
    /// 1-based source line of the `class_name` identifier — anchors `workspace/symbol` class
    /// results at the declaration instead of the file top (#33). `1` when the interface carried
    /// no location (defensive default; a registered class always has an identifier).
    pub line: u32,
    /// Byte span of the `class_name` identifier, for precise `definition` ranges without
    /// re-parsing the declaring file. Zero-width when unknown; consumers must bounds-check
    /// against the current file text (the indexed span can lag an unsaved edit).
    pub name_span: ByteSpan,
}

/// `class_name` → [`ClassEntry`]. Only top-level `class_name` declarations are global (inner classes
/// are reachable only as `Outer.Inner`, never registered here), matching Godot.
///
/// A `path → name` reverse map (`by_path`) makes [`Self::remove_by_path`] O(1): a file declares at
/// most one global `class_name`, so reconciling a file's registration on every (re)index never scans
/// the whole table — without it, the cold index is O(N²).
///
/// Serialization stores only `by_name` (the source of truth); `by_path` is rebuilt on load.
#[derive(Clone, Debug, Default)]
pub struct ClassNameRegistry {
    by_name: FxHashMap<String, ClassEntry>,
    by_path: FxHashMap<Utf8PathBuf, String>,
}

impl Serialize for ClassNameRegistry {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        // Serialize only the forward map (`by_name`). `by_path` is rebuilt on load.
        self.by_name.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for ClassNameRegistry {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let by_name = FxHashMap::<String, ClassEntry>::deserialize(deserializer)?;
        // Rebuild the reverse map from the forward data.
        let mut by_path = FxHashMap::default();
        for (name, entry) in &by_name {
            by_path.insert(entry.path.clone(), name.clone());
        }
        Ok(ClassNameRegistry { by_name, by_path })
    }
}

impl ClassNameRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register `name`'s declaration, derived from a file's interface base. If `path` previously
    /// declared a *different* name, that stale entry is dropped first. A duplicate `class_name` across
    /// two files is last-writer-wins (a project error the analyzer reports in M3; the registry does
    /// not arbitrate). `name_loc` is the `class_name` identifier's (1-based line, byte span) from
    /// the interface; `None` (defensive — a named class always has an identifier) anchors at line 1.
    pub fn insert(
        &mut self,
        name: String,
        path: &Utf8Path,
        extends: &Extends,
        is_abstract: bool,
        name_loc: Option<(u32, ByteSpan)>,
    ) {
        // If this path already declared a different name, retire it (guarding a name now owned by a
        // different path).
        if let Some(prev) = self.by_path.get(path).cloned() {
            if prev != name && self.by_name.get(&prev).is_some_and(|e| e.path == path) {
                self.by_name.remove(&prev);
            }
        }
        let (line, name_span) = name_loc.unwrap_or((1, ByteSpan::default()));
        let entry = ClassEntry {
            path: path.to_path_buf(),
            base: BaseRef::from_extends(extends),
            is_abstract,
            line,
            name_span,
        };
        self.by_path.insert(entry.path.clone(), name.clone());
        self.by_name.insert(name, entry);
    }

    pub fn get(&self, name: &str) -> Option<&ClassEntry> {
        self.by_name.get(name)
    }

    pub fn contains(&self, name: &str) -> bool {
        self.by_name.contains_key(name)
    }

    pub fn remove(&mut self, name: &str) -> Option<ClassEntry> {
        let entry = self.by_name.remove(name)?;
        // Keep `by_path` consistent only if it still points at this name.
        if self.by_path.get(&entry.path).is_some_and(|n| n == name) {
            self.by_path.remove(&entry.path);
        }
        Some(entry)
    }

    /// Drop the `class_name` declared by `path` (a file lost its `class_name`, or was deleted), in
    /// O(1). Returns the removed name(s) so the caller can re-link anything that referenced them.
    pub fn remove_by_path(&mut self, path: &Utf8Path) -> Vec<String> {
        let Some(name) = self.by_path.remove(path) else {
            return Vec::new();
        };
        // Only retire the `by_name` entry if it still belongs to this path (a duplicate name may have
        // been re-pointed at another file).
        if self.by_name.get(&name).is_some_and(|e| e.path == path) {
            self.by_name.remove(&name);
        }
        vec![name]
    }

    pub fn len(&self) -> usize {
        self.by_name.len()
    }

    pub fn is_empty(&self) -> bool {
        self.by_name.is_empty()
    }

    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.by_name.keys().map(String::as_str)
    }

    /// Iterate every `(name, &ClassEntry)` pair. Used by M4's `implementation` handler (linear walk
    /// looking for subclasses of a given class) and `workspace/symbol` (class-prefix ranking).
    pub fn entries(&self) -> impl Iterator<Item = (&str, &ClassEntry)> {
        self.by_name
            .iter()
            .map(|(name, entry)| (name.as_str(), entry))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> Utf8PathBuf {
        Utf8PathBuf::from(s)
    }

    #[test]
    fn insert_and_get() {
        let mut r = ClassNameRegistry::new();
        r.insert(
            "Hero".into(),
            &p("/proj/hero.gd"),
            &Extends::Names(vec!["Node2D".into()]),
            false,
            None,
        );
        let e = r.get("Hero").unwrap();
        assert_eq!(e.path, p("/proj/hero.gd"));
        assert_eq!(e.base, BaseRef::Name("Node2D".into()));
        assert!(!e.is_abstract);
        assert!(r.get("Missing").is_none());
    }

    #[test]
    fn base_ref_from_each_extends_shape() {
        let mut r = ClassNameRegistry::new();
        r.insert("A".into(), &p("/a.gd"), &Extends::None, false, None);
        r.insert(
            "B".into(),
            &p("/b.gd"),
            &Extends::Path {
                path: "res://x.gd".into(),
                segments: Vec::new(),
            },
            false,
            None,
        );
        r.insert(
            "C".into(),
            &p("/c.gd"),
            &Extends::Names(vec!["Outer".into(), "Inner".into()]),
            true,
            None,
        );
        assert_eq!(r.get("A").unwrap().base, BaseRef::None);
        assert_eq!(r.get("B").unwrap().base, BaseRef::Path("res://x.gd".into()));
        assert_eq!(
            r.get("C").unwrap().base,
            BaseRef::Name("Outer.Inner".into())
        );
        assert!(r.get("C").unwrap().is_abstract);
    }

    #[test]
    fn insert_stores_name_loc_with_defensive_default() {
        let mut r = ClassNameRegistry::new();
        r.insert(
            "Hero".into(),
            &p("/proj/hero.gd"),
            &Extends::None,
            false,
            Some((3, ByteSpan::new(27, 31))),
        );
        let e = r.get("Hero").unwrap();
        assert_eq!(e.line, 3);
        assert_eq!(e.name_span, ByteSpan::new(27, 31));
        // No location recorded → the defensive line-1 anchor, zero-width span.
        r.insert(
            "Bare".into(),
            &p("/proj/bare.gd"),
            &Extends::None,
            false,
            None,
        );
        let e = r.get("Bare").unwrap();
        assert_eq!(e.line, 1);
        assert!(e.name_span.is_empty());
    }

    #[test]
    fn remove_by_path_returns_names() {
        let mut r = ClassNameRegistry::new();
        r.insert(
            "Hero".into(),
            &p("/proj/hero.gd"),
            &Extends::None,
            false,
            None,
        );
        let removed = r.remove_by_path(&p("/proj/hero.gd"));
        assert_eq!(removed, vec!["Hero".to_string()]);
        assert!(r.is_empty());
    }

    #[test]
    fn reinsert_same_path_replaces_prior_name() {
        // A file renaming its class_name (insert without a preceding remove) must not leave the old
        // name registered.
        let mut r = ClassNameRegistry::new();
        r.insert("Old".into(), &p("/a.gd"), &Extends::None, false, None);
        r.insert("New".into(), &p("/a.gd"), &Extends::None, false, None);
        assert!(r.get("Old").is_none());
        assert!(r.get("New").is_some());
        assert_eq!(r.len(), 1);
        assert_eq!(r.remove_by_path(&p("/a.gd")), vec!["New".to_string()]);
    }

    #[test]
    fn entries_returns_every_registered_name_with_entry() {
        let mut r = ClassNameRegistry::new();
        r.insert(
            "Hero".into(),
            &p("/a.gd"),
            &Extends::Names(vec!["Node2D".into()]),
            false,
            None,
        );
        r.insert("Enemy".into(), &p("/b.gd"), &Extends::None, true, None);
        let mut entries: Vec<(&str, &Utf8PathBuf, bool)> = r
            .entries()
            .map(|(n, e)| (n, &e.path, e.is_abstract))
            .collect();
        entries.sort_by_key(|(n, _, _)| *n);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].0, "Enemy");
        assert!(entries[0].2); // is_abstract
        assert_eq!(entries[1].0, "Hero");
        assert!(!entries[1].2);
    }

    #[test]
    fn duplicate_name_across_paths_is_last_writer_wins() {
        // Two files declaring the same class_name: the last wins; removing the *loser*'s path leaves
        // the winner registered.
        let mut r = ClassNameRegistry::new();
        r.insert("Dup".into(), &p("/first.gd"), &Extends::None, false, None);
        r.insert("Dup".into(), &p("/second.gd"), &Extends::None, false, None);
        assert_eq!(r.get("Dup").unwrap().path, p("/second.gd"));
        // Removing the first file reports the name (so referencers relink) but keeps the winner.
        assert_eq!(r.remove_by_path(&p("/first.gd")), vec!["Dup".to_string()]);
        assert_eq!(r.get("Dup").unwrap().path, p("/second.gd"));
        // Removing the winner clears it.
        assert_eq!(r.remove_by_path(&p("/second.gd")), vec!["Dup".to_string()]);
        assert!(r.is_empty());
    }
}
