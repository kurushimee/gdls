//! The project index: the façade that ties the per-file [`Interface`] tables, the [`class_name`
//! registry](ClassNameRegistry), and the [`DepGraph`] together, and resolves cross-file *names*
//! against them plus the native DB.
//!
//! This is M2's headline deliverable. Resolution stops at **names** (Godot's `INHERITANCE_SOLVED`
//! pass): `extends MyBase` in one file links to the `class_name MyBase` declared in another, whose own
//! `extends` chains to a native-DB class. No *types* are checked — that is M3 reading these same
//! tables.
//!
//! Invalidation (WP-E): [`Index::on_file_changed`] / [`Index::on_file_removed`] are the
//! incremental-update entry points the M4 `notify` watcher drives (via `Workspace::reindex` /
//! `Workspace::remove`, funnelled through [`Index::txn`]); M2 unit-tests them directly.
//! Interface-vs-body granularity comes from [`Interface::signature_hash`] — a body-only edit
//! invalidates only the edited file; an interface change invalidates its transitive
//! reverse-dependents. The republish path drains them via [`Index::take_dirty`]; cache *validity*
//! is keyed on the per-file [`Index::epoch_of`] (WP-RD8), not the dirty set.

use camino::{Utf8Path, Utf8PathBuf};
use gd_syntax::ParseTree;
use gd_types::NativeDb;
use rustc_hash::{FxHashMap, FxHashSet};
use serde::{Deserialize, Serialize};
use walkdir::WalkDir;

use crate::depgraph::{DepGraph, FileId};
use crate::interface::{self, Extends, Interface, TypeExpr};
use crate::registry::ClassNameRegistry;

/// What a name resolves to. `Unknown` is a first-class answer (degrade to dynamic, never a false
/// "unknown class" error — `docs/00`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Resolution {
    /// A project script class (`class_name`) declared in this file.
    Script(FileId),
    /// A native class (engine, or a GDExtension class merged from doc XML) of this name.
    Native,
    /// Not a known class anywhere in the project or the native DB.
    Unknown,
}

/// The assembled project index.
pub struct Index {
    /// `res://` root, for resolving `extends "res://…"` path literals.
    root: Utf8PathBuf,
    /// `FileId` → absolute path (the id is the index into this vec).
    paths: Vec<Utf8PathBuf>,
    /// Absolute path → `FileId`.
    ids: FxHashMap<Utf8PathBuf, FileId>,
    /// Per-file shallow interface (the eager table; the only thing kept for closed files).
    interfaces: FxHashMap<FileId, Interface>,
    registry: ClassNameRegistry,
    deps: DepGraph,
    /// Names each file's interface references — lets a `class_name` add/rename/remove re-link exactly
    /// the files that name it, even ones with no resolved edge yet.
    file_refs: FxHashMap<FileId, FxHashSet<String>>,
    /// The inverse of `file_refs`: name → files referencing it.
    name_referencers: FxHashMap<String, FxHashSet<FileId>>,
    /// Each file's `extends "res://…"` target, normalized to an absolute path — the path-keyed
    /// analogue of `file_refs`. A path-`extends` references no `class_name`, so the `name_*` tables
    /// can't re-link it; this lets a file *appearing at* that path re-link the consumers waiting on
    /// it, even ones with no resolved edge yet. At most one per file (a class extends a single base).
    file_path_ref: FxHashMap<FileId, Utf8PathBuf>,
    /// The inverse of `file_path_ref`: normalized target path → files that `extends "res://…"` it.
    /// Keys may name a path with no live file (a *waiting* referencer — the whole point).
    path_referencers: FxHashMap<Utf8PathBuf, FxHashSet<FileId>>,
    /// Files whose full analysis is stale and must be recomputed on next demand. Drained by
    /// the LSP republish path (`Index::take_dirty`) after a watcher-driven reindex — its sole
    /// remaining job is *republish targeting* (which open buffers to refresh). Cache *validity*
    /// is no longer keyed off this set; see [`Self::epochs`].
    dirty: FxHashSet<FileId>,
    /// WP-RD8: a monotonic per-file cache epoch. Bumped in lockstep with every `dirty` insertion
    /// (the [`Self::mark_dirty`] chokepoint), so "this file's cached analysis is stale" is encoded
    /// as "its epoch advanced past the epoch its cache entry was stamped with". `Workspace`'s
    /// analysis cache stamps each entry with the file's epoch at analysis time and serves a hit
    /// only when the stamp still matches — a self-validating composite key that needs no
    /// dirty-bit propagation, no `clear_dirty_one`, and no `take_dirty`/`dirty_paths` ordering
    /// constraint (the coherence-bug class). A never-touched file has epoch 0
    /// ([`Self::epoch_of`]).
    epochs: FxHashMap<FileId, u64>,
}

impl Index {
    pub fn new(root: Utf8PathBuf) -> Self {
        Index {
            root,
            paths: Vec::new(),
            ids: FxHashMap::default(),
            interfaces: FxHashMap::default(),
            registry: ClassNameRegistry::new(),
            deps: DepGraph::new(),
            file_refs: FxHashMap::default(),
            name_referencers: FxHashMap::default(),
            file_path_ref: FxHashMap::default(),
            path_referencers: FxHashMap::default(),
            dirty: FxHashSet::default(),
            epochs: FxHashMap::default(),
        }
    }

    // --- Building -------------------------------------------------------------------------------

    /// Record (or replace) a file's interface and reconcile the `class_name` registry. Used both for
    /// the cold index and as the core of [`Self::on_file_changed`]. Does **not** (re)compute edges —
    /// the cold index defers that to [`Self::finish_cold_index`] so forward references resolve.
    pub fn set_interface(&mut self, path: &Utf8Path, iface: Interface) -> FileId {
        let key = normalize(path);
        let fid = self.intern(&key);
        // Drop any class_name this file used to declare, then register its current one.
        self.registry.remove_by_path(&key);
        if let Some(name) = &iface.class_name {
            self.registry.insert(
                name.clone(),
                &key,
                &iface.extends,
                iface.is_abstract,
                iface.class_name_loc,
            );
        }
        self.interfaces.insert(fid, iface);
        fid
    }

    /// Extract a parsed tree's interface and record it. Convenience over [`Self::set_interface`].
    pub fn set_interface_from_tree(&mut self, path: &Utf8Path, tree: &ParseTree) -> FileId {
        self.set_interface(path, interface::extract(tree))
    }

    /// Compute every file's dependency edges after a batch of [`Self::set_interface`] calls. The cold
    /// index calls this once, when the registry is complete, so forward references resolve.
    pub fn finish_cold_index(&mut self) {
        let fids: Vec<FileId> = self.interfaces.keys().copied().collect();
        for fid in fids {
            self.recompute_edges(fid);
        }
    }

    /// Build a fresh index by cold-scanning every `.gd` under `root` from disk, skipping the shared
    /// exclusion set ([`crate::exclude::is_excluded`], applied via [`gd_files`] — `.godot/`,
    /// `.import/`, `.git/`, `target/`, `node_modules/`, editor temp suffixes) so the cold index,
    /// `Workspace::reconcile`, and the watcher agree on what enters the index. At startup no editor
    /// buffers are open yet, so disk is authoritative; a file that can't be read is skipped
    /// (degrade, never fail). This is the server's startup step.
    ///
    /// Walk errors (permission denied, vanished mid-walk, symlink loops, non-UTF-8 paths) are
    /// counted and surfaced via `log::warn!` rather than silently dropped — `Workspace::reconcile`'s
    /// post-startup pass uses the same accounting, and a permission glitch at cold-index time on
    /// a single subdir would otherwise quietly halve the project. Persistent breakage shows up on
    /// the operator's stderr at default log level.
    pub fn build(root: &Utf8Path) -> Self {
        let mut idx = Index::new(root.to_path_buf());
        let scan = gd_files(root);
        if scan.walk_errors > 0 || scan.skipped_non_utf8 > 0 {
            log::warn!(
                "cold_index_walk had errors: walk_errors={} skipped_non_utf8={} \
                 (cross-file resolution may be missing entries until next reconcile)",
                scan.walk_errors,
                scan.skipped_non_utf8,
            );
        }
        let mut skipped_unreadable = 0usize;
        for path in scan.files {
            match std::fs::read_to_string(&path) {
                Ok(text) => {
                    idx.set_interface_from_tree(&path, &gd_syntax::parse(&text).tree);
                }
                Err(e) => {
                    skipped_unreadable += 1;
                    // Warn — matches the watcher path's level for the same situation
                    // (`server.rs::apply_reaction` uses warn for cannot-read-source).
                    log::warn!("cold index: skipping unreadable {path}: {e}");
                }
            }
        }
        if skipped_unreadable > 0 {
            log::warn!(
                "cold_index_unreadable count={skipped_unreadable} \
                 (these scripts are absent from the registry until next reconcile)"
            );
        }
        idx.finish_cold_index();
        // Verify the cold-index post-state UNCONDITIONALLY — not behind `debug_assert!`, whose
        // expression is compiled out in release. A release build that cold-scans into a corrupt
        // index would otherwise silently serve bad cross-file resolution for the whole session.
        // Debug panics with the violation list (a bug in `set_interface_from_tree` /
        // `finish_cold_index`); release logs + quarantines the named files the same way
        // `Index::txn` does for runtime mutations, then keeps serving (never crash).
        if let Err(violations) = idx.verify() {
            if cfg!(debug_assertions) {
                panic!(
                    "cold_index post-state violated Index invariants: {violations:?}\n\
                     A violation here is a bug in `set_interface_from_tree` / `finish_cold_index` \
                     that Index::txn would catch on a runtime mutation but bypasses on \
                     cold-build."
                );
            }
            log::error!("cold_index_invariant_violated invariants={violations:?}; quarantining");
            quarantine_violations(&mut idx, "cold_index", &violations, None);
        }
        idx
    }

    // --- Incremental update ---------------------------------------------------------------------

    /// Re-index one changed file — the incremental-update hook the M4 watcher drives (through
    /// [`Index::txn`], via `Workspace::reindex`). Re-extracts the interface, updates the
    /// registry and edges immediately, and invalidates exactly what must re-analyze:
    /// always the file itself; its reverse-dependents iff its *interface* changed; and any file naming
    /// a `class_name` this edit added/renamed/removed.
    ///
    /// `pub(crate)`: outside `gd_project` this is reachable only via the [`IndexMut`] that
    /// [`Index::txn`] hands its closure, so a runtime mutation can never skip the
    /// post-state [`Index::verify`].
    pub(crate) fn on_file_changed(&mut self, path: &Utf8Path, iface: Interface) {
        let key = normalize(path); // normalize once, like every other entry point
        let fid = self.intern(&key);
        let old = self.interfaces.get(&fid);
        let old_hash = old.map(|i| i.signature_hash());
        let old_class_name = old.and_then(|i| i.class_name.clone());
        let was_live = old.is_some(); // did this path already hold a live interface?
        let new_hash = iface.signature_hash();
        let new_class_name = iface.class_name.clone();

        self.set_interface(&key, iface); // (1) re-extract; registry updated now
        self.recompute_edges(fid);
        self.mark_dirty(fid); // (2) the file itself always re-analyzes (dirty + epoch bump)

        if old_hash != Some(new_hash) {
            // (3) interface changed ⇒ transitive reverse-dependents re-analyze
            self.invalidate_all(self.deps.reverse_closure(fid));
        } // body-only ⇒ nothing else (docs/03 §5)

        if old_class_name != new_class_name {
            // (4) a global name appeared/disappeared/renamed ⇒ re-link & invalidate its referencers,
            //     including files that had no edge to it yet.
            for name in old_class_name.into_iter().chain(new_class_name) {
                self.relink_referencers(&name);
            }
        }

        if !was_live {
            // (5) the file just APPEARED at `key` ⇒ re-link every file that `extends "res://…"`
            //     pointing here. Their path edge could not resolve until now, and (3)'s reverse
            //     closure can't reach them: with no resolvable target there was no edge to traverse.
            //     This is the path-literal analogue of (4); `name_referencers` can't cover it
            //     because a path-`extends` names nothing.
            self.relink_path_referencers(&key);
        }
    }

    /// Drop a deleted file from the index and invalidate everything that depended on it or named a
    /// `class_name` it declared.
    ///
    /// `pub(crate)` for the same reason as [`Self::on_file_changed`]: external mutation funnels
    /// through [`IndexMut`] / [`Index::txn`].
    pub(crate) fn on_file_removed(&mut self, path: &Utf8Path) {
        let key = normalize(path);
        // Drop the `class_name` registry entry by PATH *unconditionally*, before the interned-FileId
        // check below. A path can hold a registry entry without a live `ids`/`interfaces` slot: the
        // `DanglingClassName` quarantine path (`quarantine_violations`) calls `on_file_removed` with
        // exactly such a path. Gating the registry prune behind the `ids.get(...)` early-return left
        // that entry in place, so quarantine could never remediate a `DanglingClassName` violation —
        // it re-fired on every post-quarantine `verify`. `remove_by_path` is a no-op when the file
        // declared no `class_name`.
        let removed_names = self.registry.remove_by_path(&key);
        if let Some(fid) = self.ids.get(&key).copied() {
            // Capture dependents before tearing edges down.
            self.invalidate_all(self.deps.reverse_closure(fid));
            self.interfaces.remove(&fid);
            self.deps.remove(fid);
            self.set_name_refs(fid, FxHashSet::default());
            self.set_path_ref(fid, None); // drop this file's own path-extends bookkeeping
            self.dirty.remove(&fid);
        }
        for name in removed_names {
            self.relink_referencers(&name);
        }
        // No path-referencer relink here (cf. `on_file_changed` branch 5): a removed file's
        // path-extends consumers already held a live edge to it, so `reverse_closure` above
        // dirtied them and `deps.remove` dropped the edge; and a `res://` path resolves to exactly
        // one file, so — unlike a `class_name` collision — there is no alternate target to re-point
        // at. They stay in `path_referencers`, re-linked if the path is recreated.
    }

    // --- Resolution (WP-D queries) --------------------------------------------------------------

    /// Resolve a bare class name: a project `class_name` (→ its file), else a native class, else
    /// unknown. A project class shadows a native of the same name (Godot forbids the collision; if it
    /// occurs we trust the user's code rather than report a phantom error).
    pub fn resolve_name(&self, name: &str, native: &NativeDb) -> Resolution {
        if let Some(entry) = self.registry.get(name) {
            if let Some(&fid) = self.ids.get(&entry.path) {
                return Resolution::Script(fid);
            }
        }
        if native.class_named(name).is_some() {
            Resolution::Native
        } else {
            Resolution::Unknown
        }
    }

    /// Resolve a file's immediate base (its `extends` target).
    pub fn resolve_base(&self, fid: FileId, native: &NativeDb) -> Resolution {
        let Some(iface) = self.interfaces.get(&fid) else {
            return Resolution::Unknown;
        };
        match &iface.extends {
            Extends::None => Resolution::Unknown,
            Extends::Path(p) => self
                .resolve_path(p)
                .map_or(Resolution::Unknown, Resolution::Script),
            Extends::Names(names) => match names.first() {
                Some(name) => self.resolve_name(name, native),
                None => Resolution::Unknown,
            },
        }
    }

    /// Resolve an `extends "res://path.gd"` literal to an indexed file. M2's [`Self::resolve_base`]
    /// only exposed this folded into a [`Resolution`]; M3's analyzer needs the bare `FileId` so it can
    /// build the base's `DataType` itself.
    pub fn resolve_res_path(&self, res: &str) -> Option<FileId> {
        self.resolve_path(res)
    }

    /// `res://…` → its absolute path under the project root — a pure path-join with **no existence
    /// check** (mirrors [`ProjectModel::res_to_path`](crate::ProjectModel::res_to_path)). Unlike
    /// [`Self::resolve_res_path`], this does not require the target to be an indexed `.gd` file, so
    /// callers can resolve references to non-GDScript resources (`.tscn`/`.tres`/assets — the index
    /// holds only `.gd`; see `gd_files`). The caller is responsible for confirming the path exists.
    pub fn res_to_path(&self, res: &str) -> Option<Utf8PathBuf> {
        crate::paths::res_to_path(&self.root, res)
    }

    // --- Read accessors -------------------------------------------------------------------------

    pub fn file_id(&self, path: &Utf8Path) -> Option<FileId> {
        if let Some(&fid) = self.ids.get(&normalize(path)) {
            return Some(fid);
        }
        // WP-RD9 slow path: the cheap string normalization missed. The path may reach the file
        // through an NTFS junction / 8.3 short name / symlink, or carry different component case
        // than the interned (disk-walked) key. Resolve it to its real on-disk path and retry.
        // `dunce::canonicalize` is junction- and UNC-aware and never emits a `\\?\` verbatim prefix
        // that `normalize` would split on; it requires the path to exist, so a not-yet-created /
        // in-memory path just stays a miss. Bounded to the miss path so the common hit remains a
        // pure hashmap lookup with no syscall (the nav handlers do many `file_id` lookups).
        let canonical = dunce::canonicalize(path.as_std_path())
            .ok()
            .and_then(|pb| Utf8PathBuf::from_path_buf(pb).ok())?;
        self.ids.get(&normalize(&canonical)).copied()
    }

    pub fn path(&self, fid: FileId) -> Option<&Utf8Path> {
        // WP-RD2: `FileId` is 1-based (`NonZeroU32`); `paths` is 0-indexed, so subtract one.
        self.paths
            .get(fid.get() as usize - 1)
            .map(Utf8PathBuf::as_path)
    }

    pub fn interface(&self, fid: FileId) -> Option<&Interface> {
        self.interfaces.get(&fid)
    }

    pub fn interface_of(&self, path: &Utf8Path) -> Option<&Interface> {
        self.file_id(path).and_then(|f| self.interface(f))
    }

    pub fn registry(&self) -> &ClassNameRegistry {
        &self.registry
    }

    pub fn file_count(&self) -> usize {
        self.interfaces.len()
    }

    /// Files whose interface references `name`. Used by `textDocument/references` and
    /// `callHierarchy/incomingCalls` to narrow the candidate set before the per-file binding
    /// scan. Empty iterator when no file references the name.
    ///
    /// Returns the *interface-pass* reference set (every name a file's shallow extract
    /// mentioned). Analyzer-resolved references are a strict subset; the caller filters via
    /// `AnalysisResult.bindings` to drop interface-named-but-resolved-to-something-else hits.
    #[must_use = "iterators are lazy and do nothing unless consumed"]
    pub fn name_referencers<'a>(&'a self, name: &str) -> impl Iterator<Item = FileId> + 'a {
        self.name_referencers
            .get(name)
            .into_iter()
            .flat_map(|set| set.iter().copied())
    }

    /// Iterate every `(FileId, &Interface)` pair currently held. Used by `implementation` and
    /// `workspace/symbol` to enumerate the project's class graph without re-reading disk.
    #[must_use = "iterators are lazy and do nothing unless consumed"]
    pub fn iter_interfaces(&self) -> impl Iterator<Item = (FileId, &Interface)> + '_ {
        self.interfaces.iter().map(|(fid, iface)| (*fid, iface))
    }

    // --- Invalidation state -------------------------------------------------------------------

    /// WP-RD8: the file's monotonic cache epoch — `0` for a file no mutation has ever touched.
    /// `Workspace`'s analysis cache stamps each entry with this value at analysis time and serves
    /// a hit only while it still matches, so a dependency change (which bumps the consumer's epoch
    /// via [`Self::mark_dirty`] on the reverse-dependency closure) self-invalidates the consumer's
    /// cache with no dirty-bit override. The cross-file xref reader
    /// (`gd_server::xfile::WorkspaceXFileQuery`) reads it by `FileId` to gate serving a stale
    /// dependency's cached `member_xrefs` (the entry's stamped epoch must equal the dependency's
    /// current epoch).
    pub fn epoch_of(&self, fid: FileId) -> u64 {
        self.epochs.get(&fid).copied().unwrap_or(0)
    }

    pub fn dirty_count(&self) -> usize {
        self.dirty.len()
    }

    /// Drain and return the set of files awaiting re-analysis. WP-RD8: the LSP republish path
    /// drains here (it no longer needs the non-draining variant the retired `clear_dirty_one`
    /// dance required) — cache validity is keyed on [`Self::epoch_of`], so a closed dependent
    /// dropped from this set on drain still re-analyzes when opened (its cache entry's stamped
    /// epoch no longer matches).
    pub fn take_dirty(&mut self) -> Vec<Utf8PathBuf> {
        let fids: Vec<FileId> = self.dirty.drain().collect();
        fids.into_iter()
            .filter_map(|f| self.path(f).map(Utf8Path::to_path_buf))
            .collect()
    }

    pub fn clear_dirty(&mut self) {
        self.dirty.clear();
    }

    /// Mark `fid` as needing re-analysis: add it to the republish dirty set AND bump its cache
    /// epoch (WP-RD8). The two are the same event — "this file's cached analysis is now stale" —
    /// so they move in lockstep through this one chokepoint. Every dirtying site
    /// (`on_file_changed`, the reverse-dependency closure, the name/path relink passes) routes
    /// here, which is what keeps the epoch a faithful "must re-analyze" signal for the cache.
    fn mark_dirty(&mut self, fid: FileId) {
        self.dirty.insert(fid);
        *self.epochs.entry(fid).or_insert(0) += 1;
    }

    // --- Invariant verification ---------------------------------------------------------------

    /// Check every cross-table invariant the [`Index`] is supposed to uphold. Empty `Ok(())`
    /// when consistent; `Err(violations)` lists every breach found in a single pass (no
    /// short-circuit) so the caller can quarantine in one go. Cheap enough for after-every-
    /// mutation use ([`Index::txn`]); not free, so production callers should funnel through
    /// [`Index::txn`] which only spends the cost on actually-mutating paths.
    ///
    /// Invariants (load-bearing for nav handlers + cross-file resolution correctness):
    ///   1. Every `FileId` in `interfaces` has a path slot in `paths` AND `ids` round-trips.
    ///      One-directional by design: a `paths`/`ids` entry with NO `interfaces` entry is *not* a
    ///      violation — `paths` is the append-only `FileId`→path arena (a removed file keeps its
    ///      slot so live `FileId`s never shift), so only files with a live interface are checked.
    ///   2. Every `class_name` in `registry` resolves to a live `FileId`.
    ///   3. `DepGraph.forward` and `DepGraph.reverse` are mutual inverses.
    ///   4. Every `FileId` in `name_referencers` values is in `interfaces.keys()`.
    ///   5. `file_refs` and `name_referencers` are mutual inverses on their domains.
    ///   6. Every `FileId` in `path_referencers` values is in `interfaces.keys()` (the key path
    ///      itself may be a not-yet-created target — a waiting referencer).
    ///   7. `file_path_ref` and `path_referencers` are mutual inverses on their domains.
    pub fn verify(&self) -> Result<(), Vec<IndexInvariant>> {
        let mut violations = Vec::new();

        // Invariant 1: every interfaces key has a path and the reverse map agrees.
        // WP-RD2: `FileId` is 1-based; `paths` is 0-indexed.
        for &fid in self.interfaces.keys() {
            match self.paths.get(fid.get() as usize - 1) {
                Some(p) => {
                    if self.ids.get(p) != Some(&fid) {
                        violations.push(IndexInvariant::IdsPathsMismatch { fid });
                    }
                }
                None => violations.push(IndexInvariant::OrphanedFileId { fid }),
            }
        }

        // Invariant 2: every registered class_name's path is a known FileId.
        for (name, entry) in self.registry.entries() {
            if self.file_id(&entry.path).is_none() {
                violations.push(IndexInvariant::DanglingClassName {
                    name: name.to_string(),
                    path: entry.path.clone(),
                });
            }
        }

        // Invariant 3: depgraph forward/reverse are mutual inverses.
        violations.extend(self.deps.verify_symmetry());

        // Invariant 4: name_referencers values ⊆ interfaces.keys().
        for (name, referencers) in &self.name_referencers {
            for &fid in referencers {
                if !self.interfaces.contains_key(&fid) {
                    violations.push(IndexInvariant::NameRefererNotIndexed {
                        name: name.clone(),
                        fid,
                    });
                }
            }
        }

        // Invariant 5: file_refs ↔ name_referencers are mutual inverses on their domains.
        for (&fid, names) in &self.file_refs {
            for name in names {
                let in_inverse = self
                    .name_referencers
                    .get(name)
                    .is_some_and(|set| set.contains(&fid));
                if !in_inverse {
                    violations.push(IndexInvariant::FileRefsInverseMissing {
                        fid,
                        name: name.clone(),
                    });
                }
            }
        }
        for (name, set) in &self.name_referencers {
            for &fid in set {
                let in_inverse = self
                    .file_refs
                    .get(&fid)
                    .is_some_and(|names| names.contains(name));
                if !in_inverse {
                    violations.push(IndexInvariant::NameRefsInverseMissing {
                        fid,
                        name: name.clone(),
                    });
                }
            }
        }

        // Invariant 6: path_referencers values ⊆ interfaces.keys(). (Keys — target paths — may be
        // uncreated waiting targets; only the referencing FileIds must be live.)
        for (path, referencers) in &self.path_referencers {
            for &fid in referencers {
                if !self.interfaces.contains_key(&fid) {
                    violations.push(IndexInvariant::PathRefererNotIndexed {
                        path: path.clone(),
                        fid,
                    });
                }
            }
        }

        // Invariant 7: file_path_ref ↔ path_referencers are mutual inverses on their domains.
        for (&fid, path) in &self.file_path_ref {
            let in_inverse = self
                .path_referencers
                .get(path)
                .is_some_and(|set| set.contains(&fid));
            if !in_inverse {
                violations.push(IndexInvariant::FilePathRefInverseMissing {
                    fid,
                    path: path.clone(),
                });
            }
        }
        for (path, set) in &self.path_referencers {
            for &fid in set {
                let in_inverse = self.file_path_ref.get(&fid).is_some_and(|p| p == path);
                if !in_inverse {
                    violations.push(IndexInvariant::PathRefsInverseMissing {
                        fid,
                        path: path.clone(),
                    });
                }
            }
        }

        if violations.is_empty() {
            Ok(())
        } else {
            Err(violations)
        }
    }

    // --- Internals ------------------------------------------------------------------------------

    fn intern(&mut self, path: &Utf8Path) -> FileId {
        let key = normalize(path);
        if let Some(&id) = self.ids.get(&key) {
            return id;
        }
        // WP-RD2: ids are 1-based (`NonZeroU32`), so the new id is `len + 1`; `path()` /
        // `verify()` subtract one to index back into the 0-based `paths` vec.
        let id = FileId::new(self.paths.len() as u32 + 1);
        self.paths.push(key.clone());
        self.ids.insert(key, id);
        id
    }

    /// `res://…` → an already-indexed *and live* `FileId`, if that file is known.
    fn resolve_path(&self, res: &str) -> Option<FileId> {
        let abs = crate::paths::res_to_path(&self.root, res)?;
        let fid = self.ids.get(&normalize(&abs)).copied()?;
        // Liveness gate: `ids`/`paths` is the append-only "ever-seen" arena — a
        // removed file keeps its slot so live `FileId`s never shift (see `verify` invariant 1) —
        // but `interfaces` is the "currently live" set. Without this check, an `extends
        // "res://deleted.gd"` after the target was removed resolved to a `FileId` whose
        // `interface()` is `None`, handing the analyzer a dead id (a silent "never lie" violation).
        // The name-based path (`resolve_name`) is already live-gated via the pruned registry; this
        // brings the path-based path (`resolve_res_path` / `resolve_base`'s `Extends::Path` arm) to
        // parity.
        self.interfaces.contains_key(&fid).then_some(fid)
    }

    /// Recompute one file's forward edges from its interface against the current registry, and refresh
    /// its name-reference bookkeeping.
    fn recompute_edges(&mut self, fid: FileId) {
        let (names, path_extends, preloads) = match self.interfaces.get(&fid) {
            Some(iface) => (
                referenced_names(iface),
                path_extends_of(iface),
                iface.preload_deps.clone(),
            ),
            None => (FxHashSet::default(), None, Vec::new()),
        };

        let mut deps = FxHashSet::default();
        for name in &names {
            if let Some(entry) = self.registry.get(name) {
                if let Some(&target) = self.ids.get(&entry.path) {
                    deps.insert(target);
                }
            }
        }
        if let Some(target) = path_extends.as_deref().and_then(|p| self.resolve_path(p)) {
            deps.insert(target);
        }
        // WP-RD12: `preload("res://…")` / `load("res://…")` targets become dep edges so editing a
        // preloaded script re-invalidates this consumer — the preload-const cross-file
        // member-cycle case the M2 design deliberately left out (`depgraph.rs` module doc). The
        // existing reverse-closure invalidation flow carries the rest.
        for res in &preloads {
            if let Some(target) = self.resolve_path(res) {
                deps.insert(target);
            }
        }

        self.deps.set_deps(fid, deps);
        self.set_name_refs(fid, names);
        // Record the path-`extends` target *regardless of liveness* (unlike the live-gated edge
        // above) so a file later created at that path can re-link this consumer — see
        // `relink_path_referencers` / `on_file_changed` branch 5. `res_to_path` + `normalize`
        // produces the same key `resolve_path` and `intern` use, so the created file's
        // `on_file_changed` lands in this `path_referencers` bucket.
        let path_target = path_extends
            .as_deref()
            .and_then(|res| crate::paths::res_to_path(&self.root, res))
            .map(|abs| normalize(&abs));
        self.set_path_ref(fid, path_target);
    }

    /// Replace `fid`'s recorded name references, keeping the `name → referencers` inverse consistent.
    fn set_name_refs(&mut self, fid: FileId, names: FxHashSet<String>) {
        if let Some(old) = self.file_refs.remove(&fid) {
            for name in old {
                if let Some(set) = self.name_referencers.get_mut(&name) {
                    set.remove(&fid);
                }
            }
        }
        for name in &names {
            self.name_referencers
                .entry(name.clone())
                .or_default()
                .insert(fid);
        }
        if !names.is_empty() {
            self.file_refs.insert(fid, names);
        }
    }

    /// Replace `fid`'s recorded path-`extends` target, keeping the `target path → referencers`
    /// inverse consistent. `None` clears it (the file no longer path-extends, or was removed).
    /// Mirror of [`Self::set_name_refs`] for the single-valued path edge; prunes empty inverse
    /// sets so removed targets don't accumulate.
    fn set_path_ref(&mut self, fid: FileId, target: Option<Utf8PathBuf>) {
        if let Some(old) = self.file_path_ref.remove(&fid) {
            if let Some(set) = self.path_referencers.get_mut(&old) {
                set.remove(&fid);
                if set.is_empty() {
                    self.path_referencers.remove(&old);
                }
            }
        }
        if let Some(target) = target {
            self.path_referencers
                .entry(target.clone())
                .or_default()
                .insert(fid);
            self.file_path_ref.insert(fid, target);
        }
    }

    /// Re-resolve and invalidate every file that path-extends `target` (a file just appeared at, or
    /// vanished from, that path). The path analogue of [`Self::relink_referencers`].
    fn relink_path_referencers(&mut self, target: &Utf8Path) {
        let referencers: Vec<FileId> = self
            .path_referencers
            .get(target)
            .into_iter()
            .flatten()
            .copied()
            .collect();
        for fid in referencers {
            self.recompute_edges(fid);
            self.mark_dirty(fid);
        }
    }

    /// Re-resolve and invalidate every file that references `name` (its resolution just changed).
    fn relink_referencers(&mut self, name: &str) {
        let referencers: Vec<FileId> = self
            .name_referencers
            .get(name)
            .into_iter()
            .flatten()
            .copied()
            .collect();
        for fid in referencers {
            self.recompute_edges(fid);
            self.mark_dirty(fid);
        }
    }

    fn invalidate_all(&mut self, fids: FxHashSet<FileId>) {
        // WP-RD8: route through `mark_dirty` so each invalidated dependent both joins the
        // republish set AND bumps its cache epoch (the reverse-dependency closure is exactly where
        // a consumer's cache must self-invalidate after a dependency's interface changed).
        for fid in fids {
            self.mark_dirty(fid);
        }
    }
}

/// All bare type/`extends` names an interface references (the heads that can resolve to a project
/// file), recursively through inner classes and container type args. Path-based `extends` is handled
/// as a separate edge, not a name.
fn referenced_names(iface: &Interface) -> FxHashSet<String> {
    let mut names = FxHashSet::default();
    collect_names(iface, &mut names);
    names
}

fn collect_names(iface: &Interface, out: &mut FxHashSet<String>) {
    if let Extends::Names(chain) = &iface.extends {
        if let Some(head) = chain.first() {
            out.insert(head.clone());
        }
    }
    for member in &iface.members {
        collect_type_names(&member.ty, out);
        for param in &member.params {
            collect_type_names(param, out);
        }
    }
    for inner in &iface.inner {
        collect_names(inner, out);
    }
}

fn collect_type_names(ty: &TypeExpr, out: &mut FxHashSet<String>) {
    if let TypeExpr::Named { path, args } = ty {
        if let Some(head) = path.first() {
            out.insert(head.clone());
        }
        for arg in args {
            collect_type_names(arg, out);
        }
    }
}

fn path_extends_of(iface: &Interface) -> Option<String> {
    match &iface.extends {
        Extends::Path(p) => Some(p.clone()),
        _ => None,
    }
}

/// Canonicalize a path so a Windows backslash path (from a disk walk) and a forward-slash path
/// (decoded from a `file://` URI) hash to the same `FileId`: convert `\` → `/` **and** upper-case a
/// leading Windows drive letter. All `Index` map keys pass through here. No-op on POSIX paths
/// (which start with `/`, not `<letter>:`). Re-exported at the crate root as [`crate::normalize_path`]
/// so `gd_server`'s path-keyed code (watcher open-buffer set, republish, reconcile, `path_is_within`)
/// shares this one definition instead of open-coding `replace('\\', "/")`.
///
/// The drive-case fold closes a silent Windows correctness bug: a client lower-cases
/// the drive in its `file://` URIs (`c:`), while the disk walk inherits the project-root casing
/// (`C:` in practice). Routed through `replace('\\', "/")` alone, the two diverged and every
/// open-buffer-vs-index path comparison (`apply_reaction`'s open-buffer guard, `reconcile`'s removal
/// guard, `republish_dirty_open_buffers`) silently missed — an external edit could clobber an
/// unsaved buffer, and a dependency change failed to republish an open buffer's diagnostics. We fold
/// to UPPER (not lower) to match the existing index/test path form. The URI-keyed analysis cache is
/// brought to parity separately in `gd_server::uri::CanonicalKey`.
pub fn normalize(path: &Utf8Path) -> Utf8PathBuf {
    Utf8PathBuf::from(fold_windows_drive(path.as_str().replace('\\', "/")))
}

/// Upper-case a leading `<letter>:` drive so drive case never splits an `Index` key. Allocates only
/// when it actually folds (a lower-case drive); already-upper and POSIX paths return the input
/// string unchanged.
fn fold_windows_drive(s: String) -> String {
    let b = s.as_bytes();
    if b.len() >= 2 && b[0].is_ascii_lowercase() && b[1] == b':' {
        let mut out = String::with_capacity(s.len());
        out.push((b[0] as char).to_ascii_uppercase());
        out.push_str(&s[1..]);
        out
    } else {
        s
    }
}

// ---------------------------------------------------------------------------
// Index cache integration — B2: to_cache / from_cache / cache_equivalent.
// ---------------------------------------------------------------------------

/// A serializable snapshot of an [`Index`].
///
/// This is the "plain-data view" of the index that gets written to the warm-start cache (B3).
/// Runtime-only fields (`dirty`, `epochs`) are excluded — a freshly-loaded index has an empty
/// dirty set and epoch 0 for every file, which is correct for a warm-start.
///
/// Inverse maps (`ids`, `name_referencers`, `path_referencers`) are rebuilt from their sources
/// of truth on [`Index::from_cache`] to avoid storing two copies that could drift.
#[derive(Serialize, Deserialize)]
pub struct IndexCache {
    /// `res://` root of the project.
    root: Utf8PathBuf,
    /// FileId arena — `paths[i]` is the path for `FileId::new(i as u32 + 1)`.
    /// **Stored in insertion order** so FileId stability is preserved after round-trip.
    paths: Vec<Utf8PathBuf>,
    /// Per-file shallow interfaces (keyed by FileId).
    interfaces: Vec<(FileId, Interface)>,
    /// The `class_name` registry (forward data only; reverse rebuilt on load).
    registry: ClassNameRegistry,
    /// The dependency graph (forward edges only; reverse rebuilt on load).
    deps: DepGraph,
    /// `file → set of class names it references` — the forward half of the name-reference index.
    /// `name_referencers` (the inverse) is rebuilt from this on load.
    file_refs: Vec<(FileId, Vec<String>)>,
    /// `file → path it `extends "res://…"`to` — the forward half of the path-reference index.
    /// `path_referencers` (the inverse) is rebuilt from this on load.
    file_path_ref: Vec<(FileId, Utf8PathBuf)>,
}

impl Index {
    /// Produce a serializable snapshot of this index. Runtime-only state (`dirty`, `epochs`) is
    /// excluded; inverse maps are omitted (they are rebuilt from sources of truth on
    /// [`Self::from_cache`]).
    pub fn to_cache(&self) -> IndexCache {
        IndexCache {
            root: self.root.clone(),
            paths: self.paths.clone(),
            interfaces: self
                .interfaces
                .iter()
                .map(|(&fid, iface)| (fid, iface.clone()))
                .collect(),
            registry: self.registry.clone(),
            deps: self.deps.clone(),
            file_refs: self
                .file_refs
                .iter()
                .map(|(&fid, names)| {
                    let mut names_vec: Vec<String> = names.iter().cloned().collect();
                    names_vec.sort_unstable();
                    (fid, names_vec)
                })
                .collect(),
            file_path_ref: self
                .file_path_ref
                .iter()
                .map(|(&fid, path)| (fid, path.clone()))
                .collect(),
        }
    }

    /// Reconstruct an [`Index`] from a serialized snapshot. Inverse maps (`ids`,
    /// `name_referencers`, `path_referencers`) are rebuilt from the stored forward data.
    /// Runtime-only fields (`dirty`, `epochs`) start empty/zeroed — correct for a freshly-loaded
    /// warm-start index.
    pub fn from_cache(cache: IndexCache) -> Self {
        // Rebuild the `ids` reverse map from `paths` in stored order so FileId stability holds.
        let mut ids = FxHashMap::default();
        for (i, path) in cache.paths.iter().enumerate() {
            let fid = FileId::new(i as u32 + 1);
            ids.insert(path.clone(), fid);
        }

        // Rebuild `interfaces` map from the stored vec.
        let interfaces: FxHashMap<FileId, Interface> = cache.interfaces.into_iter().collect();

        // Rebuild `file_refs` map and the `name_referencers` inverse from the stored forward data.
        let mut file_refs: FxHashMap<FileId, FxHashSet<String>> = FxHashMap::default();
        let mut name_referencers: FxHashMap<String, FxHashSet<FileId>> = FxHashMap::default();
        for (fid, names) in cache.file_refs {
            let set: FxHashSet<String> = names.into_iter().collect();
            for name in &set {
                name_referencers
                    .entry(name.clone())
                    .or_default()
                    .insert(fid);
            }
            if !set.is_empty() {
                file_refs.insert(fid, set);
            }
        }

        // Rebuild `file_path_ref` map and the `path_referencers` inverse.
        let mut file_path_ref: FxHashMap<FileId, Utf8PathBuf> = FxHashMap::default();
        let mut path_referencers: FxHashMap<Utf8PathBuf, FxHashSet<FileId>> = FxHashMap::default();
        for (fid, path) in cache.file_path_ref {
            path_referencers
                .entry(path.clone())
                .or_default()
                .insert(fid);
            file_path_ref.insert(fid, path);
        }

        Index {
            root: cache.root,
            paths: cache.paths,
            ids,
            interfaces,
            registry: cache.registry,
            deps: cache.deps,
            file_refs,
            name_referencers,
            file_path_ref,
            path_referencers,
            // Runtime-only fields start empty/zeroed — correct for a fresh warm-start.
            dirty: FxHashSet::default(),
            epochs: FxHashMap::default(),
        }
    }

    /// Assert that a warm-started (loaded) index is structurally equivalent to a cold-built one
    /// for the same project state. Compares the sources of truth (interfaces, registry forward
    /// data, depgraph forward edges, path arena) — inverse maps are not compared as they are
    /// always derived from these.
    ///
    /// Returns `true` when `self` and `other` carry the same project state. Used by the B3 load
    /// path to validate a cache hit before trusting it, and by the round-trip spike test.
    ///
    /// **Distinct from [`Self::verify`]**, which checks internal consistency of a single index
    /// (cross-table invariants). This method checks structural equality *between* two indexes.
    pub fn cache_equivalent(&self, other: &Index) -> bool {
        // Roots must match.
        if self.root != other.root {
            return false;
        }
        // Path arenas must be identical (same order, same paths — FileId stability depends on it).
        if self.paths != other.paths {
            return false;
        }
        // Interface maps must match.
        if self.interfaces.len() != other.interfaces.len() {
            return false;
        }
        for (fid, iface) in &self.interfaces {
            if other.interfaces.get(fid) != Some(iface) {
                return false;
            }
        }
        // Registry forward data must match. Use the public `entries()` API.
        if self.registry.len() != other.registry.len() {
            return false;
        }
        for (name, entry) in self.registry.entries() {
            if other.registry.get(name) != Some(entry) {
                return false;
            }
        }
        // DepGraph forward edges must match. Use the crate-internal `iter_forward` API.
        if self.deps.forward_len() != other.deps.forward_len() {
            return false;
        }
        for (fid, targets) in self.deps.iter_forward() {
            let Some(other_targets) = other.deps.forward_deps(fid) else {
                return false;
            };
            if targets != other_targets {
                return false;
            }
        }
        // File-refs forward data must match.
        if self.file_refs.len() != other.file_refs.len() {
            return false;
        }
        for (fid, names) in &self.file_refs {
            if other.file_refs.get(fid) != Some(names) {
                return false;
            }
        }
        // File-path-ref forward data must match.
        if self.file_path_ref.len() != other.file_path_ref.len() {
            return false;
        }
        for (fid, path) in &self.file_path_ref {
            if other.file_path_ref.get(fid) != Some(path) {
                return false;
            }
        }
        true
    }
}

// ---------------------------------------------------------------------------
// Index::txn mutation chokepoint + invariant enum.
// ---------------------------------------------------------------------------

/// A single cross-table inconsistency found by [`Index::verify`]. Each variant names what is
/// wrong and (when applicable) the FileId / class_name / path that triggered it, so the
/// quarantine path can act on a specific file.
#[derive(Debug, Clone)]
pub enum IndexInvariant {
    /// A `FileId` is keyed in `interfaces` but missing from `paths` (its slot in the FileId-indexed
    /// vec doesn't exist).
    OrphanedFileId { fid: FileId },
    /// `paths[fid.0]` exists but `ids[path]` ≠ `fid` (the path↔id reverse map fell out of sync).
    IdsPathsMismatch { fid: FileId },
    /// A `class_name` in the registry points at a path that the index doesn't know about.
    DanglingClassName { name: String, path: Utf8PathBuf },
    /// `DepGraph.forward[a]` contains `b` but `DepGraph.reverse[b]` does not contain `a`, or
    /// vice versa.
    DepGraphAsymmetric {
        forward: (FileId, FileId),
        missing_reverse: bool,
    },
    /// `name_referencers[name]` contains a `FileId` not present in `interfaces`.
    NameRefererNotIndexed { name: String, fid: FileId },
    /// `file_refs[fid]` contains a name whose `name_referencers` set does NOT contain `fid`.
    FileRefsInverseMissing { fid: FileId, name: String },
    /// `name_referencers[name]` contains a `FileId` whose `file_refs[fid]` does NOT contain `name`.
    NameRefsInverseMissing { fid: FileId, name: String },
    /// `path_referencers[path]` contains a `FileId` not present in `interfaces` (a stale referencer).
    /// The *key* path may legitimately have no file — that's a waiting referencer — but every
    /// referencing `FileId` must be live.
    PathRefererNotIndexed { path: Utf8PathBuf, fid: FileId },
    /// `file_path_ref[fid] = path` but `path_referencers[path]` does NOT contain `fid`.
    FilePathRefInverseMissing { fid: FileId, path: Utf8PathBuf },
    /// `path_referencers[path]` contains `fid` whose `file_path_ref[fid]` ≠ `path`.
    PathRefsInverseMissing { fid: FileId, path: Utf8PathBuf },
}

/// The sole mutation surface for the *runtime incremental* mutators outside `gd_project`.
///
/// [`Index::on_file_changed`] / [`Index::on_file_removed`] are `pub(crate)`; the only way for an
/// external crate (the LSP server) to drive a runtime mutation is to obtain an `IndexMut`, and the
/// only source of one is [`Index::txn`], which runs [`Index::verify`] when the closure
/// returns. That makes "mutated the index at runtime without verifying" unrepresentable across the
/// crate boundary — enforced by visibility + the borrow checker, not by convention. Taking
/// `&mut self` in [`Index::txn`] (rather than a free function with a `&mut Index` argument) means
/// the verify-wrapped closure provably runs against a live `Index`.
///
/// The cold-build primitives [`Index::set_interface`] / [`Index::set_interface_from_tree`] /
/// [`Index::finish_cold_index`] stay public for fixture construction and [`Index::build`], which
/// runs the same unconditional verify + quarantine on that path; they are *not* exposed here because
/// the incremental path is the one that must never skip verify in a long-running session.
pub struct IndexMut<'a> {
    inner: &'a mut Index,
}

impl IndexMut<'_> {
    /// Re-index one changed file. Forwards to [`Index::on_file_changed`] — see it for the
    /// invalidation contract.
    pub fn on_file_changed(&mut self, path: &Utf8Path, iface: Interface) {
        self.inner.on_file_changed(path, iface);
    }

    /// Drop a deleted file from the index. Forwards to [`Index::on_file_removed`].
    pub fn on_file_removed(&mut self, path: &Utf8Path) {
        self.inner.on_file_removed(path);
    }

    /// WP-RD10 release-test seam: inject a guaranteed [`Index::verify`] violation **without
    /// panicking**, so the release unit tests can drive [`Index::txn`]'s post-verify + quarantine
    /// recovery path directly. It inserts a `DanglingClassName` (a `class_name` registered for a
    /// path the index never interned), which `verify` flags and `txn` then quarantines, then
    /// re-verifies clean.
    ///
    /// This path is **release-only and not fuzzable**. `txn` answers any post-state violation with
    /// its debug-only `panic!` ("a mutator desynced the index"), so the quarantine branch runs only
    /// when debug-assertions are OFF — i.e. `cargo test -p gd_project --release`, where the sibling
    /// test [`Index::txn`]'s caller skips itself under `cfg!(debug_assertions)`. cargo-fuzz keeps
    /// debug-assertions ON, so an injected violation there would hit the debug-`panic!` and
    /// libfuzzer's hook would abort before quarantine ran — which is why the index-invariant fuzz
    /// target drives only the change/remove mutators and leaves recovery to these release tests.
    ///
    /// Gated on `cfg(test)`, so it never exists in a production `gdls` binary (nor in the fuzz
    /// build, which no longer references it).
    #[cfg(test)]
    pub fn inject_verify_violation(&mut self) {
        self.inner.registry.insert(
            "FuzzGhost".to_string(),
            &normalize(Utf8Path::new("/fuzz/ghost_never_interned.gd")),
            &Extends::None,
            false,
            None,
        );
    }
}

impl Index {
    /// Run `mutation` against this index through the sealed [`IndexMut`] guard, then verify the
    /// post-state. The mutation chokepoint: taking `&mut self` means the verify-wrapped closure
    /// cannot be invoked except against a live `Index`, a stronger guarantee than a free function
    /// behind a `pub(crate)` fence.
    ///
    /// On a verify failure, debug builds panic naming every file referenced by the violation
    /// set; release builds log `index_invariant_violated`, drop each named file from the
    /// index, then re-verify. Persistent violations after quarantine escalate to
    /// `log::error!` and are left in place — the LSP keeps serving with degraded
    /// cross-file resolution rather than crashing.
    ///
    /// Behavior on a panicking `mutation`: the panic is caught, the index is logged as
    /// possibly half-mutated, and `verify` runs anyway so a corrupt post-state is
    /// quarantined rather than silently ridden. The `panic = "unwind"` workspace setting
    /// then keeps the LSP session alive.
    ///
    /// **`AssertUnwindSafe` is best-effort, not provably-sound.** A panicking `mutation` can
    /// leave the `Index` half-mutated (e.g. a slot inserted into `interfaces` before a
    /// downstream panic skipped its `file_refs` insertion). The post-verify catches the
    /// resulting cross-table inconsistency and quarantines the named files; the
    /// quarantine pass itself reads/writes through `on_file_removed`, which mutates the
    /// same tables the panicking mutation half-touched. We accept this as a "least-bad"
    /// recovery — quarantining a known-broken file beats killing the LSP session — but
    /// the operation is not "no logic depends on coherent pre-panic state": the
    /// quarantine logic does. The `index_invariant_persists_after_quarantine` log line
    /// below is the operator-visible signal when partial-panic recovery itself fails,
    /// and the `index_mutation_panic_after_partial_mutation_keeps_index_verifiable` test pins
    /// the invariant that release-mode `txn` returns with a verifiable index.
    pub fn txn<F>(&mut self, file: &Utf8Path, mutation: F)
    where
        F: FnOnce(&mut IndexMut<'_>) + std::panic::UnwindSafe,
    {
        // Catch the panic so a stray unwrap inside `mutation` doesn't leave the index in a
        // half-mutated state with no verify pass. `AssertUnwindSafe` on the &mut Index is
        // *best-effort*, not provably-sound: the failure path's `on_file_removed` calls do
        // depend on coherent pre-panic state. See the doc comment on `txn` above for the
        // recovery-vs-correctness trade-off.
        //
        // `mutation` runs through a sealed [`IndexMut`] — the only thing exposing the `pub(crate)`
        // incremental mutators outside this crate — so it cannot reach a mutator that skips this
        // verify. The guard is scoped in its own block so its `&mut` reborrow of `self` ends
        // before the post-state verify re-borrows `self`.
        let result = {
            let mut guard = IndexMut { inner: &mut *self };
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| mutation(&mut guard)))
        };
        let mutation_panicked = result.is_err();
        if mutation_panicked {
            log::error!(
                "index_mutation_panicked file={file}; running verify to quarantine partial state"
            );
        }

        let violations = match self.verify() {
            Ok(()) => return,
            Err(v) => v,
        };

        if cfg!(debug_assertions) && !mutation_panicked {
            panic!(
                "index_invariant_violated file={file} invariants={violations:?}\n\
                 This is a bug — verify() caught an inconsistency that on_file_* mutators \
                 should never produce. File a report with the violation list above."
            );
        }

        log::error!("index_invariant_violated file={file} invariants={violations:?}; quarantining");
        quarantine_violations(self, &format!("file={file}"), &violations, Some(file));
    }
}

/// Quarantine every file named by `violations`: drop each from the index (via
/// [`Index::on_file_removed`]), then re-verify. Shared by [`Index::txn`] (runtime
/// mutations) and [`Index::build`] (cold scan), so a release build that produces a corrupt index
/// from *either* path drops the broken files and keeps serving rather than silently resolving
/// against bad state.
///
/// Quarantines every file the violation set names — not just the mutation target: a bug in
/// `on_file_changed(A)` whose violation names B must remove B, not A. `fallback` is quarantined
/// only when the violation set names no file of its own (`apply` passes its mutation target;
/// cold build has none). A residual violation after quarantine is escalated to `log::error!` but
/// never unwinds — the LSP session must stay alive.
fn quarantine_violations(
    index: &mut Index,
    context: &str,
    violations: &[IndexInvariant],
    fallback: Option<&Utf8Path>,
) {
    let mut quarantine: Vec<Utf8PathBuf> = Vec::new();
    let mut seen: FxHashSet<Utf8PathBuf> = FxHashSet::default();
    for v in violations {
        for path in violation_paths(v, index) {
            if seen.insert(path.clone()) {
                quarantine.push(path);
            }
        }
    }
    if quarantine.is_empty() {
        if let Some(f) = fallback {
            let file_buf = normalize(f);
            if seen.insert(file_buf.clone()) {
                quarantine.push(file_buf);
            }
        }
    }

    for path in &quarantine {
        index.on_file_removed(path);
    }

    // Re-verify after quarantining. A residual violation indicates a deeper bug (the quarantined
    // mutator is itself emitting bad state); surface it loudly at error level — the most we can do
    // without killing the session.
    if let Err(residual) = index.verify() {
        log::error!(
            "index_invariant_persists_after_quarantine {context} \
             quarantined={quarantine:?} residual={residual:?}; \
             cross-file resolution may be degraded until the next cold reindex"
        );
    }
}

/// Path(s) named by a single [`IndexInvariant`]. The quarantine path walks every variant's
/// embedded identity (FileId → path via the index's reverse map, or a directly carried path)
/// so the cleanup removes the actually-broken file, not just whichever mutation tripped the
/// breach.
fn violation_paths(v: &IndexInvariant, index: &Index) -> Vec<Utf8PathBuf> {
    let from_fid = |fid: FileId| index.path(fid).map(Utf8Path::to_path_buf);
    match v {
        IndexInvariant::OrphanedFileId { fid } | IndexInvariant::IdsPathsMismatch { fid } => {
            from_fid(*fid).into_iter().collect()
        }
        IndexInvariant::DanglingClassName { path, .. } => vec![path.clone()],
        IndexInvariant::DepGraphAsymmetric {
            forward: (a, b), ..
        } => {
            let mut out = Vec::new();
            if let Some(p) = from_fid(*a) {
                out.push(p);
            }
            if let Some(p) = from_fid(*b) {
                out.push(p);
            }
            out
        }
        IndexInvariant::NameRefererNotIndexed { fid, .. }
        | IndexInvariant::FileRefsInverseMissing { fid, .. }
        | IndexInvariant::NameRefsInverseMissing { fid, .. }
        | IndexInvariant::PathRefererNotIndexed { fid, .. }
        | IndexInvariant::FilePathRefInverseMissing { fid, .. }
        | IndexInvariant::PathRefsInverseMissing { fid, .. } => {
            from_fid(*fid).into_iter().collect()
        }
    }
}

/// Outcome of a cold-index disk walk: the `.gd` files found plus per-error accounting.
/// Mirrors the bookkeeping `Workspace::reconcile` does — a permission glitch on a single
/// subdirectory is a `walk_errors` increment + a `log::warn!`, not a silent drop.
struct GdFilesScan {
    files: Vec<Utf8PathBuf>,
    walk_errors: usize,
    skipped_non_utf8: usize,
}

/// Every `.gd` file under `root`, skipping the shared exclusion set ([`crate::exclude::is_excluded`]
/// — `.godot/`, `.import/`, `.git/`, `target/`, `node_modules/`, editor temp suffixes), so the cold
/// index, `Workspace::reconcile`, and the watcher agree on what enters the index. Before this was
/// shared, cold index skipped only `.godot/` and over-included `.gd` under `target/` etc., letting a
/// vendored/build-copy script shadow a real project class. Walk errors (permission, vanished
/// mid-walk, symlink loops) and non-UTF-8 paths are counted + logged at `warn` so a startup glitch
/// on a single subdir doesn't silently halve the project — the prior `.flatten()` swallowed every
/// `Err` and re-introduced the exact bug `reconcile` was rewritten to avoid.
fn gd_files(root: &Utf8Path) -> GdFilesScan {
    let mut files = Vec::new();
    let mut walk_errors = 0usize;
    let mut skipped_non_utf8 = 0usize;
    for entry_result in WalkDir::new(root).into_iter().filter_entry(|e| {
        // Skip excluded directories *before* descending. Non-UTF-8 entries can't be classified
        // here; keep them so the loop below counts them as `skipped_non_utf8` rather than
        // silently dropping them at the filter.
        camino::Utf8Path::from_path(e.path()).is_none_or(|p| !crate::exclude::is_excluded(p, root))
    }) {
        let entry = match entry_result {
            Ok(e) => e,
            Err(e) => {
                walk_errors += 1;
                let path_display = e
                    .path()
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|| "<unknown>".to_string());
                log::warn!("cold index: walk error at {path_display}: {e}");
                continue;
            }
        };
        let Some(p) = Utf8Path::from_path(entry.path()) else {
            skipped_non_utf8 += 1;
            log::warn!(
                "cold index: skipping non-UTF-8 path under {root}; this file will not be \
                 considered for cross-file resolution until next reconcile"
            );
            continue;
        };
        if p.extension() == Some("gd") {
            files.push(p.to_path_buf());
        }
    }
    GdFilesScan {
        files,
        walk_errors,
        skipped_non_utf8,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A tiny native DB: `Object ← Node ← CanvasItem ← Node2D`.
    fn native_db() -> NativeDb {
        let json = r#"{
            "header": {"version_major": 4, "version_minor": 6, "version_patch": 3},
            "classes": [
                {"name": "Object"},
                {"name": "Node", "inherits": "Object"},
                {"name": "CanvasItem", "inherits": "Node"},
                {"name": "Node2D", "inherits": "CanvasItem"}
            ]
        }"#;
        NativeDb::from_json(json).expect("valid mini dump")
    }

    fn root() -> Utf8PathBuf {
        Utf8PathBuf::from("/proj")
    }

    // Forward-slash absolute path, matching the `Index`'s normalized keys (see `normalize`).
    fn abs(rel: &str) -> Utf8PathBuf {
        Utf8PathBuf::from(format!("/proj/{rel}"))
    }

    /// Build a cold index from `(res-relative-path, source)` pairs.
    fn cold_index(files: &[(&str, &str)]) -> Index {
        let mut idx = Index::new(root());
        for (rel, src) in files {
            let tree = gd_syntax::parse(src).tree;
            idx.set_interface_from_tree(&abs(rel), &tree);
        }
        idx.finish_cold_index();
        idx
    }

    /// WP-RD8: a file's cache epoch by relative path, for invalidation assertions. The epoch is
    /// bumped wherever the dirty set is populated (the `mark_dirty` chokepoint), so "X was
    /// invalidated by this mutation" reads as "X's epoch advanced" and "X was NOT invalidated"
    /// reads as "X's epoch is unchanged". A never-touched / unindexed file reads 0.
    fn epoch(idx: &Index, rel: &str) -> u64 {
        idx.file_id(&abs(rel)).map_or(0, |f| idx.epoch_of(f))
    }

    #[test]
    fn cross_file_name_resolves_through_registry() {
        // The M2 exit criterion, concretely: A `extends MyBase` → B's `class_name MyBase`, whose own
        // `extends Node2D` chains into the native DB.
        let db = native_db();
        let idx = cold_index(&[
            ("a.gd", "extends MyBase\nfunc go():\n\tpass\n"),
            ("b.gd", "class_name MyBase\nextends Node2D\n"),
        ]);

        // A's base resolves to B (a project script class).
        let a = idx.file_id(&abs("a.gd")).unwrap();
        let Resolution::Script(b) = idx.resolve_base(a, &db) else {
            panic!("A's base should resolve to a project script");
        };
        assert_eq!(idx.path(b).unwrap(), abs("b.gd"));

        // B's own base resolves into the native DB, which knows Node2D ⊂ Object.
        assert_eq!(idx.resolve_base(b, &db), Resolution::Native);
        assert!(db.is_subclass_of_named("Node2D", "Object"));
    }

    #[test]
    fn extends_res_path_resolves_to_file() {
        let db = native_db();
        let idx = cold_index(&[
            ("a.gd", "extends \"res://b.gd\"\n"),
            ("b.gd", "extends Node\n"),
        ]);
        let a = idx.file_id(&abs("a.gd")).unwrap();
        let Resolution::Script(b) = idx.resolve_base(a, &db) else {
            panic!("path-based extends should resolve to the target file");
        };
        assert_eq!(idx.path(b).unwrap(), abs("b.gd"));
    }

    #[test]
    fn unknown_name_degrades_not_errors() {
        let db = native_db();
        let idx = cold_index(&[("a.gd", "extends Nonexistent\n")]);
        let a = idx.file_id(&abs("a.gd")).unwrap();
        assert_eq!(idx.resolve_base(a, &db), Resolution::Unknown);
    }

    #[test]
    fn native_name_resolves_when_not_a_script_class() {
        let db = native_db();
        let idx = cold_index(&[("a.gd", "extends Node\n")]);
        assert_eq!(idx.resolve_name("Node", &db), Resolution::Native);
        assert_eq!(idx.resolve_name("CanvasItem", &db), Resolution::Native);
        assert_eq!(idx.resolve_name("Ghost", &db), Resolution::Unknown);
    }

    #[test]
    fn body_only_edit_invalidates_only_the_edited_file() {
        let mut idx = cold_index(&[
            ("a.gd", "extends MyBase\n"),
            (
                "b.gd",
                "class_name MyBase\nextends Node\nfunc f():\n\tpass\n",
            ),
        ]);
        idx.clear_dirty();
        let (e_a, e_b) = (epoch(&idx, "a.gd"), epoch(&idx, "b.gd"));

        // Edit only B's function *body* — its signature is unchanged.
        let new_b = interface::extract(
            &gd_syntax::parse("class_name MyBase\nextends Node\nfunc f():\n\tprint(\"hi\")\n").tree,
        );
        idx.on_file_changed(&abs("b.gd"), new_b);

        assert!(epoch(&idx, "b.gd") > e_b, "the edited file re-analyzes");
        assert_eq!(
            epoch(&idx, "a.gd"),
            e_a,
            "a body-only edit must not invalidate dependents"
        );
    }

    #[test]
    fn class_name_change_invalidates_dependents() {
        let mut idx = cold_index(&[
            ("a.gd", "extends MyBase\n"),
            ("b.gd", "class_name MyBase\nextends Node\n"),
        ]);
        idx.clear_dirty();
        let (e_a, e_b) = (epoch(&idx, "a.gd"), epoch(&idx, "b.gd"));

        // Rename B's class_name MyBase → MyOther: A's `extends MyBase` no longer resolves to B.
        let new_b =
            interface::extract(&gd_syntax::parse("class_name MyOther\nextends Node\n").tree);
        idx.on_file_changed(&abs("b.gd"), new_b);

        assert!(epoch(&idx, "b.gd") > e_b);
        assert!(
            epoch(&idx, "a.gd") > e_a,
            "the file that extends the renamed class must re-analyze"
        );
    }

    #[test]
    fn newly_added_class_name_links_waiting_referencer() {
        let db = native_db();
        // A references MyBase before any file declares it.
        let mut idx = cold_index(&[("a.gd", "extends MyBase\n"), ("b.gd", "extends Node\n")]);
        idx.clear_dirty();
        let a = idx.file_id(&abs("a.gd")).unwrap();
        let a_epoch0 = epoch(&idx, "a.gd");
        assert_eq!(idx.resolve_base(a, &db), Resolution::Unknown);

        // B now declares class_name MyBase.
        let new_b = interface::extract(&gd_syntax::parse("class_name MyBase\nextends Node\n").tree);
        idx.on_file_changed(&abs("b.gd"), new_b);

        // A now resolves to B, and was invalidated to re-analyze.
        let Resolution::Script(b) = idx.resolve_base(a, &db) else {
            panic!("A should now resolve to B");
        };
        assert_eq!(idx.path(b).unwrap(), abs("b.gd"));
        assert!(epoch(&idx, "a.gd") > a_epoch0);
    }

    #[test]
    fn newly_added_file_links_waiting_path_extends_referencer() {
        let db = native_db();
        // A path-extends `res://b.gd` before any file exists at that path. The name machinery
        // cannot cover this: a path-`extends` references no `class_name`, so `name_referencers`
        // never lists A. This is the `extends "res://…"` analogue of
        // `newly_added_class_name_links_waiting_referencer`.
        let mut idx = cold_index(&[("a.gd", "extends \"res://b.gd\"\n")]);
        idx.clear_dirty();
        let a = idx.file_id(&abs("a.gd")).unwrap();
        let a_epoch0 = epoch(&idx, "a.gd");
        assert_eq!(idx.resolve_base(a, &db), Resolution::Unknown);

        // b.gd is created — deliberately WITHOUT a class_name (the gap name-relinking misses).
        let new_b = interface::extract(&gd_syntax::parse("extends Node\n").tree);
        idx.on_file_changed(&abs("b.gd"), new_b);

        // A's base now resolves to b.gd by path (resolution was always live), AND A was
        // invalidated to re-analyze — the load-bearing half: the watcher republish path drains
        // `take_dirty` (and A's cache epoch advanced), so without this an open `a.gd` would show
        // phantom "unknown base" diagnostics for the rest of the session.
        let Resolution::Script(b) = idx.resolve_base(a, &db) else {
            panic!("A should now resolve to the newly-created b.gd by path");
        };
        assert_eq!(idx.path(b).unwrap(), abs("b.gd"));
        assert!(
            epoch(&idx, "a.gd") > a_epoch0,
            "a file path-extending the newly-created target must re-analyze"
        );
    }

    #[test]
    fn path_extends_edge_links_after_target_appears_so_later_edits_invalidate() {
        // After the target appears and the waiting referencer is re-linked, the forward edge must
        // exist so a *subsequent* interface edit to the target invalidates the referencer too —
        // proving the re-link created the dep edge, not just a one-shot dirty.
        let mut idx = cold_index(&[("a.gd", "extends \"res://b.gd\"\n")]);
        let b0 = interface::extract(&gd_syntax::parse("extends Node\n").tree);
        idx.on_file_changed(&abs("b.gd"), b0); // b.gd appears, A re-linked.
        idx.clear_dirty();
        let a_epoch0 = epoch(&idx, "a.gd");

        // Edit b.gd's interface (add a public function ⇒ signature_hash changes).
        let b1 = interface::extract(&gd_syntax::parse("extends Node\nfunc g():\n\tpass\n").tree);
        idx.on_file_changed(&abs("b.gd"), b1);

        assert!(
            epoch(&idx, "a.gd") > a_epoch0,
            "the path-extends edge created on add must invalidate A on a later target edit"
        );
        assert!(
            idx.verify().is_ok(),
            "path-ref bookkeeping keeps the index cross-table consistent"
        );
    }

    #[test]
    fn removing_a_path_extends_target_invalidates_its_referencer() {
        // Test-gap closure: the path-extends ADD and MODIFY directions are pinned
        // (`newly_added_file_links_waiting_path_extends_referencer`,
        // `path_extends_edge_links_after_target_appears_so_later_edits_invalidate`), but DELETE was
        // not. Unlike ADD, removal does NOT relink `path_referencers` (see the closing comment in
        // `on_file_removed`): a removed path-extends target held a live edge, so its referencer is
        // re-queued via the reverse-dependency closure, and a `res://` path resolves to exactly one
        // file (no alternate target to re-point at). Pin that dirty propagation so a later
        // "simplification" of the removal path can't silently strand an open `extends "res://…"`
        // buffer on phantom "unknown base" diagnostics for the rest of the session.
        let db = native_db();
        let mut idx = cold_index(&[
            ("a.gd", "extends \"res://b.gd\"\n"),
            ("b.gd", "extends Node\n"), // path target, deliberately NO class_name
        ]);
        let a = idx.file_id(&abs("a.gd")).unwrap();
        assert!(
            matches!(idx.resolve_base(a, &db), Resolution::Script(_)),
            "precondition: A resolves to the live b.gd by path"
        );
        idx.clear_dirty();
        let a_epoch0 = epoch(&idx, "a.gd");

        idx.on_file_removed(&abs("b.gd"));

        assert!(
            epoch(&idx, "a.gd") > a_epoch0,
            "deleting a path-extends target must re-queue its referencer (via the reverse-dependency \
             closure) so the open buffer republishes — the delete-direction mirror of the add fix"
        );
        assert_eq!(
            idx.resolve_base(a, &db),
            Resolution::Unknown,
            "A's path-based base degrades to Unknown once the target is gone, never a dead Script id"
        );
        assert!(idx.verify().is_ok(), "removal leaves a verifiable index");
    }

    #[test]
    fn name_referencers_returns_files_naming_a_class() {
        // a.gd and c.gd both extend MyBase; b.gd does not.
        let idx = cold_index(&[
            ("a.gd", "extends MyBase\n"),
            ("b.gd", "extends Node\n"),
            ("c.gd", "extends MyBase\n"),
            ("d.gd", "class_name MyBase\nextends Node\n"),
        ]);
        let mut refs: Vec<Utf8PathBuf> = idx
            .name_referencers("MyBase")
            .map(|fid| idx.path(fid).unwrap().to_path_buf())
            .collect();
        refs.sort();
        assert_eq!(refs, vec![abs("a.gd"), abs("c.gd")]);

        // A name nobody references returns an empty iterator.
        assert_eq!(idx.name_referencers("Nonexistent").count(), 0);
    }

    #[test]
    fn verify_holds_after_cold_scan() {
        let idx = cold_index(&[
            ("a.gd", "extends MyBase\n"),
            ("b.gd", "class_name MyBase\nextends Node\n"),
            ("c.gd", "extends MyBase\n"),
        ]);
        assert!(
            idx.verify().is_ok(),
            "cold-scan must satisfy every invariant"
        );
    }

    #[test]
    fn verify_holds_after_on_file_changed() {
        let mut idx = cold_index(&[
            ("a.gd", "extends MyBase\n"),
            ("b.gd", "class_name MyBase\nextends Node\n"),
        ]);
        let new_b =
            interface::extract(&gd_syntax::parse("class_name MyOther\nextends Node\n").tree);
        idx.on_file_changed(&abs("b.gd"), new_b);
        assert!(idx.verify().is_ok(), "verify must pass after a rename");
    }

    #[test]
    fn verify_holds_after_on_file_removed() {
        let mut idx = cold_index(&[
            ("a.gd", "extends MyBase\n"),
            ("b.gd", "class_name MyBase\nextends Node\n"),
        ]);
        idx.on_file_removed(&abs("b.gd"));
        assert!(idx.verify().is_ok(), "verify must pass after a remove");
    }

    #[test]
    fn resolve_res_path_returns_none_after_remove() {
        // `on_file_removed` keeps the path→FileId reverse map entry (the
        // append-only arena gives stable FileIds), but `resolve_path` must NOT hand back a FileId
        // whose interface was pruned — that dead id's `interface()` is `None`, so resolving
        // `extends "res://b.gd"` to it is a silent "never lie" violation.
        let db = native_db();
        let mut idx = cold_index(&[
            ("a.gd", "extends \"res://b.gd\"\n"),
            ("b.gd", "class_name MyBase\nextends Node\n"),
        ]);
        assert!(
            idx.resolve_res_path("res://b.gd").is_some(),
            "b resolves while it is live"
        );

        idx.on_file_removed(&abs("b.gd"));

        assert!(
            idx.resolve_res_path("res://b.gd").is_none(),
            "a removed file's res:// path must not resolve to a dead FileId"
        );
        let a = idx.file_id(&abs("a.gd")).expect("a is still live");
        assert_eq!(
            idx.resolve_base(a, &db),
            Resolution::Unknown,
            "the dependent's path-based base must degrade to Unknown, not a dead Script id"
        );
        assert!(idx.verify().is_ok(), "removal leaves a verifiable index");
    }

    #[test]
    fn on_file_removed_clears_dangling_registry_entry_even_when_not_interned() {
        // The `DanglingClassName` quarantine path
        // calls `on_file_removed` with a registry path that was never interned. Before the fix, the
        // `ids.get(...)` early-return skipped the registry prune, so the violation re-fired on every
        // post-quarantine `verify` and quarantine could never converge. Construct that exact state
        // (white-box) and assert removal clears it.
        let mut idx = Index::new(Utf8PathBuf::from("/proj"));
        let ghost = normalize(&abs("ghost.gd"));
        idx.registry
            .insert("Ghost".to_string(), &ghost, &Extends::None, false, None);

        match idx.verify() {
            Err(v) if v.len() == 1 && matches!(v[0], IndexInvariant::DanglingClassName { .. }) => {}
            other => panic!("expected exactly one DanglingClassName violation, got {other:?}"),
        }

        idx.on_file_removed(&abs("ghost.gd"));
        assert!(
            idx.verify().is_ok(),
            "on_file_removed must prune the dangling registry entry so quarantine converges"
        );
    }

    #[test]
    fn normalize_folds_windows_drive_to_upper() {
        // A client lower-cases the drive (`c:`), the disk walk keeps the
        // root's case (`C:`); `normalize` folds to UPPER so both hash to one key and every
        // open-buffer-vs-index path comparison agrees.
        assert_eq!(
            normalize(Utf8Path::new("c:/proj/a.gd")).as_str(),
            "C:/proj/a.gd"
        );
        assert_eq!(
            normalize(Utf8Path::new("c:\\proj\\a.gd")).as_str(),
            "C:/proj/a.gd"
        );
        assert_eq!(
            normalize(Utf8Path::new("C:/proj/a.gd")).as_str(),
            "C:/proj/a.gd"
        );
        // POSIX paths (no `<letter>:` prefix) are untouched.
        assert_eq!(
            normalize(Utf8Path::new("/proj/a.gd")).as_str(),
            "/proj/a.gd"
        );
    }

    #[test]
    fn index_mutation_apply_runs_mutation_and_verify_ok() {
        let mut idx = cold_index(&[("a.gd", "extends Node\n")]);
        let new_iface = interface::extract(&gd_syntax::parse("class_name A\nextends Node\n").tree);
        // No panic in debug because the mutation keeps invariants intact.
        idx.txn(&abs("a.gd"), |i| i.on_file_changed(&abs("a.gd"), new_iface));
        assert!(idx.verify().is_ok());
        assert!(idx.registry().get("A").is_some());
    }

    #[test]
    fn index_mutation_release_handles_mutation_panic() {
        // Skip in debug builds — debug-only verify-violation panics interact with
        // catch_unwind in ways the test doesn't need to assert. Release behavior is the
        // recoverable session-survival path we care about.
        if cfg!(debug_assertions) {
            return;
        }
        let mut idx = cold_index(&[("a.gd", "extends Node\n")]);
        // A mutation that panics partway: verify still runs (catch_unwind), the index
        // stays consistent because no mutator was called, and the session keeps going.
        idx.txn(&abs("a.gd"), |_i| {
            panic!("simulated panic inside mutation closure");
        });
        assert!(
            idx.verify().is_ok(),
            "post-panic verify should pass when nothing was actually mutated"
        );
    }

    #[test]
    fn index_mutation_panic_after_partial_mutation_keeps_index_verifiable() {
        // The pre-fix test only exercised the empty-mutation panic case. The
        // `AssertUnwindSafe` contract claim that "no logic depends on coherent pre-panic
        // state" is wrong in general — the quarantine path's `on_file_removed` does. This
        // test exercises the harder case: a mutation that performs ONE legal on_file_*
        // call (taking the index from one consistent state to another) and THEN panics.
        // The recovery should still leave the index in a verifiable state — possibly with
        // some files quarantined — and the LSP session must stay alive.
        if cfg!(debug_assertions) {
            return;
        }
        let mut idx = cold_index(&[
            ("a.gd", "class_name A\nextends Node\n"),
            ("b.gd", "class_name B\nextends A\n"),
        ]);

        let new_a_iface =
            interface::extract(&gd_syntax::parse("class_name A2\nextends Node\n").tree);
        idx.txn(&abs("a.gd"), |i| {
            // First: a real consistent mutation that succeeds. After this call the index is
            // still verifiable on its own.
            i.on_file_changed(&abs("a.gd"), new_a_iface);
            // Then: a panic mid-closure, simulating a buggy reducer that aborts after some
            // table writes already landed. The catch_unwind boundary captures this.
            panic!("simulated mid-closure panic after a successful on_file_changed");
        });

        // The session must still be alive (we returned from `apply` instead of unwinding
        // past it) and the post-state must be verifiable. The class_name rename may have
        // partially landed; that's acceptable because the quarantine path runs after.
        assert!(
            idx.verify().is_ok(),
            "post-partial-panic verify must pass; otherwise the LSP session is serving from \
             a corrupt index"
        );
    }

    #[test]
    fn index_mutation_quarantines_the_offending_file_after_a_violating_panic() {
        // The two apply-panic tests above leave a SELF-CONSISTENT post-state, so the quarantine
        // *remediation* — verify-fails ⇒ drop the offending file ⇒ re-verify clean — is asserted
        // nowhere. Here the closure panics AFTER a partial write that
        // leaves a real `DanglingClassName` (a class_name registered for a path never interned —
        // e.g. a reducer that wrote the registry then aborted before interning the file). Because
        // the closure panicked, `apply` skips its debug-only invariant panic
        // (`cfg!(debug_assertions) && !mutation_panicked` is false) and runs the quarantine path in
        // BOTH debug and release — so this exercises remediation under the normal `cargo test`
        // profile, which `index_mutation_release_handles_mutation_panic` cannot (it early-returns
        // in debug).
        let mut idx = cold_index(&[("a.gd", "extends Node\n")]);

        idx.txn(&abs("a.gd"), |i| {
            // `i` is the sealed IndexMut; reach the raw registry table through its (in-crate)
            // private `inner` field to force a DanglingClassName the safe mutators never would.
            i.inner.registry.insert(
                "Ghost".to_string(),
                &normalize(&abs("ghost.gd")),
                &Extends::None,
                false,
                None,
            );
            panic!("simulated mid-mutation panic after a partial registry write");
        });

        // apply must have QUARANTINED ghost.gd (dropped the dangling entry) and re-verified clean —
        // not merely returned because the post-state happened to be consistent.
        assert!(
            idx.verify().is_ok(),
            "quarantine must remediate the forced violation, leaving a verifiable index"
        );
        assert!(
            idx.registry().get("Ghost").is_none(),
            "the offending dangling class_name must be dropped by the quarantine pass"
        );
        assert!(
            idx.file_id(&abs("a.gd")).is_some(),
            "quarantine must drop only the offending file, not pre-existing good ones"
        );
    }

    #[test]
    fn txn_quarantines_injected_verify_violation_without_panic() {
        // WP-RD10: the NON-panicking failure trigger. `inject_verify_violation` registers a dangling
        // `class_name` (a `DanglingClassName`) but does NOT panic, so `txn` runs verify → detects it
        // → quarantines the offending entry → re-verifies clean. Because the closure didn't panic,
        // `txn`'s debug-only invariant panic is bypassed only by the quarantine path here too: a
        // `DanglingClassName` is a real violation, and in debug `txn` would panic on it UNLESS the
        // mutation panicked. So the remediation path only runs in RELEASE (debug `txn` panics on a
        // non-panic violation by design) — which is also exactly why the index-invariant fuzz target
        // (cargo-fuzz keeps debug-assertions ON) cannot exercise recovery and leaves it to this
        // release test. Mirror `index_mutation_release_handles_mutation_panic`'s guard.
        if cfg!(debug_assertions) {
            return;
        }
        let mut idx = cold_index(&[("a.gd", "extends Node\n")]);
        idx.txn(&abs("a.gd"), |i| i.inject_verify_violation());
        assert!(
            idx.verify().is_ok(),
            "txn must quarantine the injected violation and leave a verifiable index (no panic path)"
        );
        assert!(
            idx.registry().get("FuzzGhost").is_none(),
            "the injected dangling class_name must have been quarantined"
        );
        assert!(
            idx.file_id(&abs("a.gd")).is_some(),
            "quarantine must drop only the offending entry, not the pre-existing good file"
        );
    }

    #[test]
    fn violation_paths_extracts_fid_from_every_variant() {
        // Unit-test the helper that drives quarantine target selection: every variant
        // names a file the cleanup pass can act on (modulo when the fid is itself
        // orphaned, where `path()` returns None).
        let idx = cold_index(&[("a.gd", "class_name A\nextends Node\n")]);
        let a_fid = idx.file_id(&abs("a.gd")).unwrap();

        let cases = [
            IndexInvariant::OrphanedFileId { fid: a_fid },
            IndexInvariant::IdsPathsMismatch { fid: a_fid },
            IndexInvariant::DanglingClassName {
                name: "X".into(),
                path: abs("phantom.gd"),
            },
            IndexInvariant::DepGraphAsymmetric {
                forward: (a_fid, a_fid),
                missing_reverse: true,
            },
            IndexInvariant::NameRefererNotIndexed {
                name: "X".into(),
                fid: a_fid,
            },
            IndexInvariant::FileRefsInverseMissing {
                fid: a_fid,
                name: "X".into(),
            },
            IndexInvariant::NameRefsInverseMissing {
                fid: a_fid,
                name: "X".into(),
            },
        ];
        for v in &cases {
            let paths = super::violation_paths(v, &idx);
            assert!(
                !paths.is_empty(),
                "violation_paths returned empty for variant {v:?}"
            );
        }
    }

    #[test]
    fn iter_interfaces_yields_every_indexed_file() {
        let idx = cold_index(&[
            ("a.gd", "class_name A\nextends Node\n"),
            ("b.gd", "class_name B\nextends Node\n"),
        ]);
        let names: Vec<String> = idx
            .iter_interfaces()
            .filter_map(|(_, iface)| iface.class_name.clone())
            .collect();
        let mut sorted = names;
        sorted.sort();
        assert_eq!(sorted, vec!["A".to_string(), "B".to_string()]);
    }

    #[test]
    fn member_type_creates_dependency_edge() {
        // A public member typed by another class_name is an interface-level dependency.
        let mut idx = cold_index(&[
            ("a.gd", "extends Node\nvar target: Enemy\n"),
            ("enemy.gd", "class_name Enemy\nextends Node\n"),
        ]);
        idx.clear_dirty();
        let a_epoch0 = epoch(&idx, "a.gd");

        // Changing Enemy's interface invalidates A (which exposes an `Enemy`-typed member).
        let new_enemy =
            interface::extract(&gd_syntax::parse("class_name Enemy\nextends Node2D\n").tree);
        idx.on_file_changed(&abs("enemy.gd"), new_enemy);
        assert!(epoch(&idx, "a.gd") > a_epoch0);
    }

    #[test]
    fn preload_const_target_creates_invalidation_edge() {
        // WP-RD12: `const B = preload("res://b.gd")` makes A depend on B's interface, so editing
        // B's interface invalidates A — the cross-file member-cycle case M2 deliberately left out
        // (a const has no type annotation, so it was never a name/path-extends edge).
        let mut idx = cold_index(&[
            (
                "a.gd",
                "extends Node\nconst B = preload(\"res://b.gd\")\nvar v = B.Y\n",
            ),
            ("b.gd", "class_name B\nextends Node\nconst Y = 1\n"),
        ]);
        idx.clear_dirty();
        let a_epoch0 = epoch(&idx, "a.gd");

        // Change B's interface (rename the const Y → Z).
        let new_b =
            interface::extract(&gd_syntax::parse("class_name B\nextends Node\nconst Z = 2\n").tree);
        idx.on_file_changed(&abs("b.gd"), new_b);

        assert!(
            epoch(&idx, "a.gd") > a_epoch0,
            "editing a preloaded script's interface must invalidate the file that preloads it \
             (the WP-RD12 preload-const DepGraph edge)"
        );
    }

    #[test]
    fn removing_a_file_invalidates_dependents() {
        let mut idx = cold_index(&[
            ("a.gd", "extends MyBase\n"),
            ("b.gd", "class_name MyBase\nextends Node\n"),
        ]);
        idx.clear_dirty();
        let a_epoch0 = epoch(&idx, "a.gd");
        idx.on_file_removed(&abs("b.gd"));

        assert!(idx.interface_of(&abs("b.gd")).is_none());
        assert!(idx.registry().get("MyBase").is_none());
        assert!(epoch(&idx, "a.gd") > a_epoch0);
    }

    /// Spike: serde round-trip on a populated Index.
    ///
    /// Validates:
    /// - Every serialized field type can actually derive Serialize+Deserialize.
    /// - FileId stability: same path → same FileId after round-trip.
    /// - Structural equivalence: warm == cold for same on-disk state.
    #[test]
    fn index_serde_roundtrip() {
        // Build a small Index with several interned files, an interface with members/inner/enums,
        // a registered class_name, and a depgraph edge — exercises every serialized field shape.
        let idx = cold_index(&[
            (
                "hero.gd",
                "class_name Hero\nextends Node2D\n\
                 @export var speed: float = 10.0\n\
                 @onready var label: Label\n\
                 func move(dir: Vector2, speed: float) -> void:\n\tpass\n\
                 signal hit(amount: int)\n\
                 enum State { IDLE, RUN, JUMP }\n",
            ),
            (
                "enemy.gd",
                "class_name Enemy\nextends Hero\n\
                 var hp: int\n\
                 class Inner extends Resource:\n\
                     var x: Array[int]\n",
            ),
            (
                "scene.gd",
                "extends Node\nconst E = preload(\"res://enemy.gd\")\n",
            ),
            // A path-based extends: exercises file_path_ref / path_referencers round-trip.
            // Without this fixture those maps are empty and cache_equivalent compares
            // empty-vs-empty vacuously, hiding any bug in `from_cache`'s inverse rebuild.
            ("waiter.gd", "extends \"res://hero.gd\"\n"),
        ]);

        // Capture reference FileIds before serialization.
        let hero_id_before = idx.file_id(&abs("hero.gd")).expect("hero.gd interned");
        let enemy_id_before = idx.file_id(&abs("enemy.gd")).expect("enemy.gd interned");
        let scene_id_before = idx.file_id(&abs("scene.gd")).expect("scene.gd interned");
        let waiter_id_before = idx.file_id(&abs("waiter.gd")).expect("waiter.gd interned");

        // Serialize the cache view.
        let cache = idx.to_cache();
        let bytes = serde_json::to_vec(&cache).expect("Index cache must serialize to JSON");

        // Deserialize back.
        let cache2: IndexCache = serde_json::from_slice(&bytes).expect("JSON must deserialize");
        let restored = Index::from_cache(cache2);

        // FileId stability: same paths → same FileIds after round-trip.
        assert_eq!(
            restored.file_id(&abs("hero.gd")),
            Some(hero_id_before),
            "hero.gd FileId must be stable across round-trip"
        );
        assert_eq!(
            restored.file_id(&abs("enemy.gd")),
            Some(enemy_id_before),
            "enemy.gd FileId must be stable across round-trip"
        );
        assert_eq!(
            restored.file_id(&abs("scene.gd")),
            Some(scene_id_before),
            "scene.gd FileId must be stable across round-trip"
        );
        assert_eq!(
            restored.file_id(&abs("waiter.gd")),
            Some(waiter_id_before),
            "waiter.gd FileId must be stable across round-trip"
        );

        // The restored Index must be structurally equivalent to the original.
        assert!(
            idx.cache_equivalent(&restored),
            "restored Index must be structurally equivalent to the original"
        );

        // verify() should still pass on the restored index.
        assert!(
            restored.verify().is_ok(),
            "restored Index must satisfy all invariants"
        );

        // Runtime-only fields are zeroed on load (dirty set is empty, epochs all 0).
        assert_eq!(
            restored.dirty_count(),
            0,
            "freshly-loaded index must have an empty dirty set"
        );

        // Independent oracle: run real query operations on the restored index, exercising
        // the rebuilt inverse maps (ids, name_referencers, path_referencers) rather than
        // just the structural check.

        // resolve_base on enemy.gd (extends Hero) must yield the hero FileId.
        let db = native_db();
        assert_eq!(
            restored.resolve_base(enemy_id_before, &db),
            Resolution::Script(hero_id_before),
            "enemy.gd must resolve its base (Hero) to hero.gd's FileId after round-trip"
        );

        // name_referencers("Hero") on the restored index must yield the same FileIds.
        let mut orig_refs: Vec<FileId> = idx.name_referencers("Hero").collect();
        let mut restored_refs: Vec<FileId> = restored.name_referencers("Hero").collect();
        orig_refs.sort();
        restored_refs.sort();
        assert_eq!(
            orig_refs, restored_refs,
            "name_referencers must match across round-trip"
        );

        // path_referencers oracle: waiter.gd uses `extends "res://hero.gd"` (a path-based
        // extends), so the restored index's path_referencers inverse map must link hero.gd's
        // absolute path back to waiter's FileId. This directly tests that `from_cache` correctly
        // rebuilds `path_referencers` from the serialized `file_path_ref` forward data — a bug
        // there would silently break cross-file re-linking after a warm start, but `cache_equivalent`
        // alone wouldn't catch it when the maps are empty (no fixture uses path extends). Direct field
        // access is valid here: this test lives in the same module as Index.
        let path_refs_for_hero = restored.path_referencers.get(&abs("hero.gd")).expect(
            "path_referencers must have an entry for hero.gd after round-trip \
                     (waiter.gd path-extends it)",
        );
        assert!(
            path_refs_for_hero.contains(&waiter_id_before),
            "path_referencers[hero.gd] must contain waiter.gd's FileId after round-trip; \
             got: {path_refs_for_hero:?}"
        );

        // Class registry: restored index knows Hero is in hero.gd.
        let hero_entry = restored
            .registry()
            .get("Hero")
            .expect("Hero registered after round-trip");
        assert_eq!(hero_entry.path, abs("hero.gd"));
    }
}
