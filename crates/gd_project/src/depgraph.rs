//! The interface-level dependency graph and its invalidation primitive.
//!
//! Forward edge `A → B` means "A's *interface* references B's interface" (A `extends` B's
//! `class_name`/path, or a public member of A is typed by B's `class_name`). The reverse edges drive
//! invalidation: when B's interface changes, every transitive reverse-dependent must re-analyze
//! (`docs/03` §5). `preload`/`load` edges are *body-level* — they don't change a file's interface, so
//! they are deliberately **not** recorded here in M2.
//!
//! Files are referred to by an opaque [`FileId`] so the graph is a self-contained, deterministically
//! unit-testable structure with no path/string knowledge — the [`crate::index::Index`] owns the
//! `path ↔ FileId` mapping. M4's `notify` watcher is the eventual event source; M2 ships the graph and
//! the `on_file_changed` logic it will call.

use std::num::NonZeroU32;

use rustc_hash::{FxHashMap, FxHashSet};
use serde::{Deserialize, Serialize};

/// An opaque, `Copy` handle for a file within one [`crate::index::Index`].
///
/// **WP-RD2: backed by [`NonZeroU32`].** The interning arena ([`crate::index::Index::intern`])
/// assigns ids starting at 1, so `0` is unrepresentable — which retires the old `FileId(0)`
/// placeholder that orphan-file analysis used to invent (and which collided with whichever real
/// script the index interned first). A file outside the project is now carried as
/// `Option<FileId> = None` through [`gd_analyze::analyze`], not a colliding sentinel id.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct FileId(NonZeroU32);

impl FileId {
    /// Construct a `FileId` from a 1-based raw id. **Panics on `0`** — the index never mints `0`
    /// (interning is `paths.len() + 1`), so a `0` here is a construction bug, not an orphan (an
    /// orphan is `Option<FileId>::None`). Used by the index's interner and by tests.
    #[must_use]
    pub fn new(raw: u32) -> Self {
        FileId(NonZeroU32::new(raw).expect("invariant: FileId is 1-based and never zero"))
    }

    /// The raw 1-based id. The complement of [`Self::new`]; used for the `paths`-vec index
    /// (`get() - 1`) and the `<Script #N>` debug rendering.
    #[must_use]
    pub fn get(self) -> u32 {
        self.0.get()
    }

    /// Reserved self-identity for analysis of a file the index doesn't know (an `untitled:`
    /// buffer or a `.gd` outside the project, analyzed with `Option<FileId>::None`). Used **only**
    /// as the `ScriptRef.file` of such a file's own class types so the
    /// "no `class_node` leaves the result" rewrite in [`gd_analyze`] still has a concrete id;
    /// it is **never** recorded in a [`gd_analyze::Binding`] (orphan bindings record `None`), and
    /// since nothing in the project ever resolves *to* an orphan, this id collides with nothing
    /// meaningful. `u32::MAX` is chosen so it can never alias a real interned id short of a
    /// 4-billion-file project.
    pub const ORPHAN: FileId = FileId(NonZeroU32::MAX);
}

/// Forward + reverse interface-dependency edges.
///
/// Serialization stores only the `forward` map; `reverse` is rebuilt from it on deserialization
/// to avoid storing two copies that could drift.
#[derive(Clone, Debug, Default)]
pub struct DepGraph {
    /// `file → files it depends on`.
    forward: FxHashMap<FileId, FxHashSet<FileId>>,
    /// `file → files that depend on it` (the inverse of `forward`, maintained in lockstep).
    reverse: FxHashMap<FileId, FxHashSet<FileId>>,
}

impl Serialize for DepGraph {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        // Serialize as a map of FileId → Vec<FileId> (forward edges only).
        use serde::ser::SerializeMap;
        let mut map = serializer.serialize_map(Some(self.forward.len()))?;
        for (from, tos) in &self.forward {
            // Collect to a sorted Vec for deterministic output.
            let mut tos_vec: Vec<FileId> = tos.iter().copied().collect();
            tos_vec.sort_unstable();
            map.serialize_entry(from, &tos_vec)?;
        }
        map.end()
    }
}

impl<'de> Deserialize<'de> for DepGraph {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        // Deserialize as a map of FileId → Vec<FileId>, then rebuild reverse.
        let raw: Vec<(FileId, Vec<FileId>)> = {
            struct Visitor;
            impl<'de> serde::de::Visitor<'de> for Visitor {
                type Value = Vec<(FileId, Vec<FileId>)>;
                fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                    write!(f, "a map of FileId to Vec<FileId>")
                }
                fn visit_map<A: serde::de::MapAccess<'de>>(
                    self,
                    mut access: A,
                ) -> Result<Self::Value, A::Error> {
                    let mut out = Vec::new();
                    while let Some((k, v)) = access.next_entry::<FileId, Vec<FileId>>()? {
                        out.push((k, v));
                    }
                    Ok(out)
                }
            }
            deserializer.deserialize_map(Visitor)?
        };

        let mut graph = DepGraph::default();
        for (from, tos) in raw {
            let set: FxHashSet<FileId> = tos.into_iter().collect();
            for &to in &set {
                graph.reverse.entry(to).or_default().insert(from);
            }
            if !set.is_empty() {
                graph.forward.insert(from, set);
            }
        }
        Ok(graph)
    }
}

impl DepGraph {
    pub fn new() -> Self {
        Self::default()
    }

    /// Replace `file`'s outgoing edges with exactly `deps`. Self-edges are dropped (a file never
    /// depends on itself). Both directions stay consistent.
    pub fn set_deps(&mut self, file: FileId, deps: impl IntoIterator<Item = FileId>) {
        self.clear_forward(file);
        let mut set = FxHashSet::default();
        for dep in deps {
            if dep == file || !set.insert(dep) {
                continue;
            }
            self.reverse.entry(dep).or_default().insert(file);
        }
        if !set.is_empty() {
            self.forward.insert(file, set);
        }
    }

    /// Remove `file` entirely — both its outgoing edges and any edges pointing at it.
    pub fn remove(&mut self, file: FileId) {
        self.clear_forward(file);
        if let Some(dependents) = self.reverse.remove(&file) {
            for dep in dependents {
                if let Some(targets) = self.forward.get_mut(&dep) {
                    targets.remove(&file);
                }
            }
        }
    }

    /// Direct reverse-dependents of `file` (files that reference it).
    pub fn dependents(&self, file: FileId) -> impl Iterator<Item = FileId> + '_ {
        self.reverse.get(&file).into_iter().flatten().copied()
    }

    /// The transitive reverse-dependency closure of `file` — every file that must re-analyze when
    /// `file`'s interface changes. Excludes `file` itself. Cycle-safe (a `seen` set bounds the walk).
    pub fn reverse_closure(&self, file: FileId) -> FxHashSet<FileId> {
        let mut seen = FxHashSet::default();
        let mut stack = vec![file];
        while let Some(current) = stack.pop() {
            if let Some(dependents) = self.reverse.get(&current) {
                for &dep in dependents {
                    if seen.insert(dep) {
                        stack.push(dep);
                    }
                }
            }
        }
        seen.remove(&file);
        seen
    }

    /// Iterate all `(from, deps)` forward edges. Used by `Index::cache_equivalent` to compare
    /// two dep-graphs and by the custom `Serialize` impl.
    pub(crate) fn iter_forward(&self) -> impl Iterator<Item = (FileId, &FxHashSet<FileId>)> + '_ {
        self.forward.iter().map(|(&from, tos)| (from, tos))
    }

    /// The number of entries in the forward edge map. Used by `Index::cache_equivalent`.
    pub(crate) fn forward_len(&self) -> usize {
        self.forward.len()
    }

    /// Iterate the forward deps of a specific file, if any. Used by `Index::cache_equivalent`.
    pub(crate) fn forward_deps(&self, file: FileId) -> Option<&FxHashSet<FileId>> {
        self.forward.get(&file)
    }

    fn clear_forward(&mut self, file: FileId) {
        if let Some(targets) = self.forward.remove(&file) {
            for target in targets {
                if let Some(back) = self.reverse.get_mut(&target) {
                    back.remove(&file);
                }
            }
        }
    }

    /// M4 (WP-X3): verify forward/reverse are mutual inverses. Returns a list of
    /// [`crate::index::IndexInvariant::DepGraphAsymmetric`] entries for every breach; empty when
    /// the graph is consistent. Called from [`crate::index::Index::verify`].
    pub fn verify_symmetry(&self) -> Vec<crate::index::IndexInvariant> {
        let mut out = Vec::new();
        // Every forward edge (a → b) must have reverse[b].contains(a).
        for (&a, targets) in &self.forward {
            for &b in targets {
                let ok = self.reverse.get(&b).is_some_and(|set| set.contains(&a));
                if !ok {
                    out.push(crate::index::IndexInvariant::DepGraphAsymmetric {
                        forward: (a, b),
                        missing_reverse: true,
                    });
                }
            }
        }
        // Every reverse edge (b ← a) must have forward[a].contains(b).
        for (&b, sources) in &self.reverse {
            for &a in sources {
                let ok = self.forward.get(&a).is_some_and(|set| set.contains(&b));
                if !ok {
                    out.push(crate::index::IndexInvariant::DepGraphAsymmetric {
                        forward: (a, b),
                        missing_reverse: false,
                    });
                }
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // WP-RD2: `FileId` is `NonZeroU32`, so the opaque graph nodes are 1-based here (was 0-based).
    fn fid(n: u32) -> FileId {
        FileId::new(n)
    }

    fn ids<const N: usize>(set: FxHashSet<FileId>, expect: [u32; N]) {
        let mut got: Vec<u32> = set.iter().map(|f| f.get()).collect();
        got.sort_unstable();
        let mut want = expect.to_vec();
        want.sort_unstable();
        assert_eq!(got, want);
    }

    #[test]
    fn forward_and_reverse_stay_in_lockstep() {
        let mut g = DepGraph::new();
        // 1 → 2, 1 → 3  (1 depends on 2 and 3)
        g.set_deps(fid(1), [fid(2), fid(3)]);
        let deps: Vec<u32> = g.dependents(fid(2)).map(|f| f.get()).collect();
        assert_eq!(deps, vec![1]);
        assert_eq!(g.dependents(fid(3)).count(), 1);
    }

    #[test]
    fn reverse_closure_is_transitive() {
        let mut g = DepGraph::new();
        // chain: 1 → 2 → 3  (1 depends on 2, 2 depends on 3)
        g.set_deps(fid(1), [fid(2)]);
        g.set_deps(fid(2), [fid(3)]);
        // changing 3 must re-analyze both 2 and 1.
        ids(g.reverse_closure(fid(3)), [1, 2]);
        // changing 1 (a leaf dependent) re-analyzes nobody.
        assert!(g.reverse_closure(fid(1)).is_empty());
    }

    #[test]
    fn resetting_deps_drops_stale_reverse_edges() {
        let mut g = DepGraph::new();
        g.set_deps(fid(1), [fid(2)]);
        assert_eq!(g.dependents(fid(2)).count(), 1);
        g.set_deps(fid(1), []); // 1 no longer depends on 2
        assert_eq!(g.dependents(fid(2)).count(), 0);
    }

    #[test]
    fn remove_clears_both_directions() {
        let mut g = DepGraph::new();
        g.set_deps(fid(1), [fid(2)]);
        g.set_deps(fid(3), [fid(1)]);
        g.remove(fid(1));
        assert_eq!(g.dependents(fid(2)).count(), 0); // 1's forward edge gone
        assert_eq!(g.dependents(fid(1)).count(), 0); // edges into 1 gone
        assert!(g.reverse_closure(fid(2)).is_empty());
    }

    #[test]
    fn self_edges_are_ignored() {
        let mut g = DepGraph::new();
        g.set_deps(fid(1), [fid(1), fid(2)]);
        assert!(g.reverse_closure(fid(1)).is_empty());
        ids(g.reverse_closure(fid(2)), [1]);
    }

    #[test]
    fn cycles_terminate() {
        let mut g = DepGraph::new();
        g.set_deps(fid(1), [fid(2)]);
        g.set_deps(fid(2), [fid(1)]);
        ids(g.reverse_closure(fid(1)), [2]);
        ids(g.reverse_closure(fid(2)), [1]);
    }
}
