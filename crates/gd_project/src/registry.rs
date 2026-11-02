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

use std::cmp::Ordering;

use camino::{Utf8Path, Utf8PathBuf};
use gd_syntax::ByteSpan;
use rustc_hash::FxHashMap;
use serde::{Deserialize, Serialize};

use crate::interface::{Extends, Interface};
use crate::scan_order::godot_scan_order;

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
    /// Godot's scan never reaches this file. See [`crate::interface::Interface::outside_godot_scan`].
    #[serde(default)]
    pub outside_godot_scan: bool,
}

impl ClassEntry {
    /// The registration of `iface`'s top-level `class_name`, declared by the file at `path`.
    #[must_use]
    pub fn declared_by(path: &Utf8Path, iface: &Interface) -> Self {
        let (line, name_span) = iface.class_name_loc.unwrap_or((1, ByteSpan::default()));
        ClassEntry {
            path: path.to_path_buf(),
            base: BaseRef::from_extends(&iface.extends),
            is_abstract: iface.is_abstract,
            line,
            name_span,
            outside_godot_scan: iface.outside_godot_scan,
        }
    }
}

/// `class_name` → [`ClassEntry`]. Only top-level `class_name` declarations are global (inner classes
/// are reachable only as `Outer.Inner`, never registered here), matching Godot.
///
/// Every file declaring a name is kept, not just one. The winner is the declarer Godot's editor
/// registers last ([`godot_scan_order`]), preferring any file Godot's scan reaches over one it
/// never sees, so it depends only on which files declare the name, never
/// on the order they were indexed or edited in. Removing the winner promotes the next declarer
/// instead of dropping the name: a scratch copy, or a new file passing through an existing name
/// while its `class_name` is typed, can never hide or take over the real class for good.
///
/// A `path → name` reverse map (`by_path`) keeps [`Self::remove_by_path`] cheap: a file declares at
/// most one global `class_name`, so reconciling a file's registration never scans the whole table.
///
/// Serialization stores every declarer; `by_path` is rebuilt on load.
#[derive(Clone, Debug, Default)]
pub struct ClassNameRegistry {
    /// Every declarer of a name, sorted by [`godot_scan_order`]. The last one wins. Never empty.
    by_name: FxHashMap<String, Vec<ClassEntry>>,
    by_path: FxHashMap<Utf8PathBuf, String>,
}

impl Serialize for ClassNameRegistry {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.by_name.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for ClassNameRegistry {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let stored = FxHashMap::<String, Vec<ClassEntry>>::deserialize(deserializer)?;
        // Re-insert rather than trust the stored order, so a cache can never carry a winner the
        // current ordering rule would not pick.
        let mut registry = ClassNameRegistry::new();
        for (name, declarers) in stored {
            for entry in declarers {
                registry.insert_entry(name.clone(), entry);
            }
        }
        Ok(registry)
    }
}

/// The order declarers of one name compete in; the greatest owns it. A file Godot's scan reaches
/// always beats one it doesn't, then Godot's own registration order decides.
fn ownership_order(a: &ClassEntry, b: &ClassEntry) -> Ordering {
    b.outside_godot_scan
        .cmp(&a.outside_godot_scan)
        .then_with(|| godot_scan_order(&a.path, &b.path))
}

impl ClassNameRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register `path`'s declaration of `name`, derived from the file's interface base. A name the
    /// file declared before is retired first. `name_loc` is the `class_name` identifier's (1-based
    /// line, byte span); `None` (defensive — a named class always has an identifier) anchors at
    /// line 1.
    ///
    /// Returns every name whose winning file changed, so the caller can re-link what refers to it.
    #[must_use]
    pub fn insert(
        &mut self,
        name: String,
        path: &Utf8Path,
        extends: &Extends,
        is_abstract: bool,
        name_loc: Option<(u32, ByteSpan)>,
    ) -> Vec<String> {
        let (line, name_span) = name_loc.unwrap_or((1, ByteSpan::default()));
        let entry = ClassEntry {
            path: path.to_path_buf(),
            base: BaseRef::from_extends(extends),
            is_abstract,
            line,
            name_span,
            outside_godot_scan: false,
        };
        self.insert_declaration(name, entry)
    }

    /// [`Self::insert`] for a fully built entry, keyed by `entry.path`.
    #[must_use]
    pub fn insert_declaration(&mut self, name: String, entry: ClassEntry) -> Vec<String> {
        let path = entry.path.clone();
        let path = path.as_path();
        let mut affected: Vec<String> = self.by_path.get(path).cloned().into_iter().collect();
        if !affected.contains(&name) {
            affected.push(name.clone());
        }
        let winners_before: Vec<Option<Utf8PathBuf>> =
            affected.iter().map(|n| self.winner_path(n)).collect();

        let _ = self.remove_by_path(path);
        self.insert_entry(name, entry);

        affected
            .into_iter()
            .zip(winners_before)
            .filter(|(n, before)| self.winner_path(n) != *before)
            .map(|(n, _)| n)
            .collect()
    }

    fn winner_path(&self, name: &str) -> Option<Utf8PathBuf> {
        self.get(name).map(|e| e.path.clone())
    }

    /// Add one declarer at its scan-order position.
    fn insert_entry(&mut self, name: String, entry: ClassEntry) {
        self.by_path.insert(entry.path.clone(), name.clone());
        let declarers = self.by_name.entry(name).or_default();
        let at = declarers.partition_point(|d| ownership_order(d, &entry) == Ordering::Less);
        declarers.insert(at, entry);
    }

    /// The file that owns `name`: of all its declarers, the one Godot registers last.
    pub fn get(&self, name: &str) -> Option<&ClassEntry> {
        self.by_name.get(name).and_then(|d| d.last())
    }

    pub fn contains(&self, name: &str) -> bool {
        self.by_name.contains_key(name)
    }

    /// Drop the `class_name` declared by `path` (a file lost its `class_name`, or was deleted).
    /// When `path` was the winner, the next declarer takes over.
    ///
    /// Returns the name if its winning file changed, so the caller can re-link its referencers.
    #[must_use]
    pub fn remove_by_path(&mut self, path: &Utf8Path) -> Vec<String> {
        let Some(name) = self.by_path.remove(path) else {
            return Vec::new();
        };
        let Some(declarers) = self.by_name.get_mut(&name) else {
            return Vec::new();
        };
        let Some(at) = declarers.iter().position(|d| d.path == path) else {
            return Vec::new();
        };
        let was_winner = at + 1 == declarers.len();
        declarers.remove(at);
        if declarers.is_empty() {
            self.by_name.remove(&name);
        }
        if was_winner {
            vec![name]
        } else {
            Vec::new()
        }
    }

    /// Every file declaring `name`, winner last.
    pub fn declarers(&self, name: &str) -> &[ClassEntry] {
        self.by_name.get(name).map_or(&[], Vec::as_slice)
    }

    /// Every `(name, declarer)` pair, losers included.
    pub fn all_declarations(&self) -> impl Iterator<Item = (&str, &ClassEntry)> {
        self.by_name
            .iter()
            .flat_map(|(name, declarers)| declarers.iter().map(move |d| (name.as_str(), d)))
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

    /// Iterate every `(name, winner)` pair. Used by M4's `implementation` handler (linear walk
    /// looking for subclasses of a given class) and `workspace/symbol` (class-prefix ranking).
    pub fn entries(&self) -> impl Iterator<Item = (&str, &ClassEntry)> {
        self.by_name
            .iter()
            .filter_map(|(name, declarers)| declarers.last().map(|winner| (name.as_str(), winner)))
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
        let _ = r.insert(
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
        let _ = r.insert("A".into(), &p("/a.gd"), &Extends::None, false, None);
        let _ = r.insert(
            "B".into(),
            &p("/b.gd"),
            &Extends::Path {
                path: "res://x.gd".into(),
                segments: Vec::new(),
            },
            false,
            None,
        );
        let _ = r.insert(
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
        let _ = r.insert(
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
        let _ = r.insert(
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
        let _ = r.insert(
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
        let _ = r.insert("Old".into(), &p("/a.gd"), &Extends::None, false, None);
        let _ = r.insert("New".into(), &p("/a.gd"), &Extends::None, false, None);
        assert!(r.get("Old").is_none());
        assert!(r.get("New").is_some());
        assert_eq!(r.len(), 1);
        assert_eq!(r.remove_by_path(&p("/a.gd")), vec!["New".to_string()]);
    }

    #[test]
    fn entries_returns_every_registered_name_with_entry() {
        let mut r = ClassNameRegistry::new();
        let _ = r.insert(
            "Hero".into(),
            &p("/a.gd"),
            &Extends::Names(vec!["Node2D".into()]),
            false,
            None,
        );
        let _ = r.insert("Enemy".into(), &p("/b.gd"), &Extends::None, true, None);
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

    fn declare(r: &mut ClassNameRegistry, name: &str, path: &str) -> Vec<String> {
        r.insert(name.into(), &p(path), &Extends::None, false, None)
    }

    #[test]
    fn duplicate_winner_follows_godot_scan_order_not_insert_order() {
        // Godot registers subdirectory files before a directory's own files, and the last one
        // registered wins, so the top-level file owns the name whichever was indexed first.
        for order in [
            ["/p/a/copy.gd", "/p/real.gd"],
            ["/p/real.gd", "/p/a/copy.gd"],
        ] {
            let mut r = ClassNameRegistry::new();
            for path in order {
                let _ = declare(&mut r, "Dup", path);
            }
            assert_eq!(r.get("Dup").unwrap().path, p("/p/real.gd"));
            assert_eq!(r.declarers("Dup").len(), 2);
            assert_eq!(r.len(), 1);
        }
    }

    #[test]
    fn removing_the_winner_promotes_the_next_declarer() {
        let mut r = ClassNameRegistry::new();
        let _ = declare(&mut r, "Dup", "/p/a/copy.gd");
        let _ = declare(&mut r, "Dup", "/p/real.gd");
        // Removing a loser changes no winner.
        assert!(r.remove_by_path(&p("/p/a/copy.gd")).is_empty());
        assert_eq!(r.get("Dup").unwrap().path, p("/p/real.gd"));

        let _ = declare(&mut r, "Dup", "/p/a/copy.gd");
        assert_eq!(r.remove_by_path(&p("/p/real.gd")), vec!["Dup".to_string()]);
        assert_eq!(r.get("Dup").unwrap().path, p("/p/a/copy.gd"));
        assert_eq!(
            r.remove_by_path(&p("/p/a/copy.gd")),
            vec!["Dup".to_string()]
        );
        assert!(r.is_empty());
    }

    #[test]
    fn typing_through_an_existing_name_leaves_its_owner_registered() {
        // A new file whose `class_name` is typed one keystroke at a time passes through an existing
        // name. The real owner must keep the name once the typing moves past it.
        let mut r = ClassNameRegistry::new();
        let _ = declare(&mut r, "CharacterEntity", "/p/src/character.gd");
        for typed in ["CharacterEnt", "CharacterEntity", "CharacterEntityA"] {
            let _ = declare(&mut r, typed, "/p/src/zz_new.gd");
        }
        assert_eq!(
            r.get("CharacterEntity").unwrap().path,
            p("/p/src/character.gd")
        );
        assert_eq!(
            r.get("CharacterEntityA").unwrap().path,
            p("/p/src/zz_new.gd")
        );
        assert!(r.get("CharacterEnt").is_none());
    }

    #[test]
    fn insert_reports_only_names_whose_winner_changed() {
        let mut r = ClassNameRegistry::new();
        assert_eq!(declare(&mut r, "A", "/p/a.gd"), vec!["A".to_string()]);
        // Re-declaring the same name from the same file changes no winner.
        assert!(declare(&mut r, "A", "/p/a.gd").is_empty());
        // A losing duplicate changes no winner.
        assert!(declare(&mut r, "A", "/p/sub/a.gd").is_empty());
        // Renaming the winner hands `A` to the other declarer and creates `B`.
        let mut changed = declare(&mut r, "B", "/p/a.gd");
        changed.sort();
        assert_eq!(changed, vec!["A".to_string(), "B".to_string()]);
        assert_eq!(r.get("A").unwrap().path, p("/p/sub/a.gd"));
    }

    #[test]
    fn serde_round_trip_keeps_every_declarer() {
        let mut r = ClassNameRegistry::new();
        let _ = declare(&mut r, "Dup", "/p/real.gd");
        let _ = declare(&mut r, "Dup", "/p/a/copy.gd");
        let json = serde_json::to_string(&r).unwrap();
        let mut back: ClassNameRegistry = serde_json::from_str(&json).unwrap();
        assert_eq!(back.get("Dup").unwrap().path, p("/p/real.gd"));
        assert_eq!(
            back.remove_by_path(&p("/p/real.gd")),
            vec!["Dup".to_string()]
        );
        assert_eq!(back.get("Dup").unwrap().path, p("/p/a/copy.gd"));
    }
}
