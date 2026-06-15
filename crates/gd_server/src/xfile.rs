//! `WorkspaceXFileQuery` — the [`CrossFileQuery`] impl that wraps [`SyntacticQuery`] and
//! surfaces the analyzer's recorded [`AnalysisResult::member_xrefs`] via its own cross-file
//! query.
//!
//! Activates the cross-file mutual-member cycle detection (`reducer.rs`'s Script-meta branch
//! marked `// WP-R2: cross-file mutual member cycle detection`) in the LSP path. Without this
//! wrapper, `Workspace::analyze` would use `SyntacticQuery` whose default
//! `member_initializer_xrefs` returns empty, leaving the cycle check inert under
//! `publishDiagnostics` even though the conformance harness still exercises it.
//!
//! Cache-key convention: the analysis cache is keyed by [`CanonicalKey`] — the percent-encoded
//! `file://` URI string. The xref query receives a `FileId`, so the wrapper maps `FileId → Index
//! path → CanonicalKey::for_path` and looks the entry up. Because `CanonicalKey` is the *only* key
//! type the cache accepts and `for_path` / `for_uri` route through the same percent-encoding
//! chokepoint, the writer (`Workspace::analyze`, keyed by the client's wire URI) and this reader
//! (keyed from the index path) agree by construction — no raw-vs-encoded drift. That drift used to
//! silently disable this cycle check for any project under `~/My Game/`; the old dual-probe
//! (try the canonical key, then a raw `path.as_str()` fallback) papered over it and is now gone.
//! Cache misses — and dirty (stale) cache *hits* — degrade silently to "no xrefs known". A
//! never-analyzed dependency yielding no xrefs is fine on its own; the consequence is that the
//! WP-R2 cross-file *cycle* diagnostic is therefore best-effort — deterministic only once the
//! dependency has been analyzed this session. Degrading is also the safe behavior for a dependency
//! whose interface changed but whose `AnalysisResult` hasn't been recomputed yet (`reindex` marks
//! it dirty without refreshing the analysis cache). The freshness gate in
//! [`WorkspaceXFileQuery::member_initializer_xrefs`] enforces the latter so a stale cross-file
//! cycle diagnostic can never be served (trading it for a possibly-missing one).

use camino::Utf8Path;
use gd_analyze::{
    AnalysisResult, CrossFileQuery, MemberXref, NodePathQuery, SceneNodeFacts, SyntacticQuery,
};
use gd_project::{FileId, Index, Interface, ResolvedRoot, SceneIndex};
use gd_types::NativeDb;
use lru::LruCache;
use rustc_hash::FxHashMap;

use crate::uri::CanonicalKey;
use crate::workspace::CacheEntry;

/// Wraps [`SyntacticQuery`] and overrides [`CrossFileQuery::member_initializer_xrefs`] and
/// [`CrossFileQuery::autoload_file`] with project-backed lookups. Every other method delegates
/// to the inner query.
pub struct WorkspaceXFileQuery<'a> {
    inner: SyntacticQuery<'a>,
    /// Held alongside `inner` (which also wraps it) so the WP-RD8 freshness gate in
    /// [`Self::member_initializer_xrefs`] can compare a cached entry's stamped epoch against the
    /// dependency's current [`Index::epoch_of`] before serving a cross-file result.
    index: &'a Index,
    /// M5 WP-H2: now an `LruCache` (was `HashMap` pre-WP-H2). The reader uses
    /// [`LruCache::peek`] (not `get`) below because a cross-file member-xrefs lookup is a
    /// recorded-fact read — the file whose xrefs we're consulting isn't the one the user is
    /// actively editing, so re-promoting its recency to MRU would distort the eviction order and
    /// shield seldom-touched files from the WP-H1 Soft-pressure shed.
    analysis_cache: &'a LruCache<CanonicalKey, CacheEntry<AnalysisResult>>,
    /// Autoload name → FileId map, built once per analysis context from the project's autoload
    /// table. Built per-call (a small `filter_map` over the autoload list) rather than cached on
    /// `Workspace` so there is no stale-cache class: the map is always consistent with the
    /// current index snapshot. Non-script autoloads and autoloads whose script has not been
    /// indexed yet are silently skipped — the resolver degrades gracefully to Variant for those.
    autoloads: FxHashMap<String, FileId>,
    /// The project's parsed scene index (M11 Phase 2), used by [`Self::scene_node_facts`] to resolve
    /// `$`/`%` accesses precisely. Borrowed from `Workspace.scenes`.
    scenes: &'a SceneIndex,
    /// The project root, normalized the same way the index normalizes its paths, so a `FileId`'s
    /// index path strips cleanly to its `res://` form (`path_to_res`) for the scene reverse-map
    /// lookup. Held by value (a cheap clone of `Workspace.project.root`) so the query owns a
    /// consistent root for the duration of one analysis.
    project_root: camino::Utf8PathBuf,
}

impl<'a> WorkspaceXFileQuery<'a> {
    pub(crate) fn new(
        index: &'a Index,
        native: &'a NativeDb,
        analysis_cache: &'a LruCache<CanonicalKey, CacheEntry<AnalysisResult>>,
        autoloads: FxHashMap<String, FileId>,
        scenes: &'a SceneIndex,
        project_root: &Utf8Path,
    ) -> Self {
        WorkspaceXFileQuery {
            inner: SyntacticQuery::new(index, native),
            index,
            analysis_cache,
            autoloads,
            scenes,
            project_root: gd_project::normalize_path(project_root),
        }
    }

    /// Resolve `query` against ONE scene `scene_res` that attaches `script_res`, returning the type
    /// fact of the access target. Returns `None` (→ permissive at the caller) on any uncertainty:
    ///
    /// * the script attaches at MULTIPLE nodes in this scene (relative resolution is ambiguous —
    ///   `$X` would resolve differently per attachment node);
    /// * the target node / unique name doesn't resolve (absent, unresolved instance, cycle).
    ///
    /// `ResolvedRoot` is mapped script-first: an attached script wins over a native `type=` (a node
    /// can carry both, and a navigation feature on `$Child.script_method()` should surface the
    /// SCRIPT's members, the more precise type).
    fn resolve_one_scene(
        &self,
        scene_res: &str,
        script_res: &str,
        query: &NodePathQuery,
    ) -> Option<SceneNodeFacts> {
        let scene = self.scenes.scene(scene_res)?;
        // The attachment node: the node whose `script=` is THIS script. A relative `$X` is resolved
        // against it. If the script is attached at more than one node in this scene, the relative
        // base is ambiguous → refuse. (A unique-name query is owner-scoped and attachment-
        // independent, but a multi-attach scene is still a degenerate shape we decline uniformly.)
        let mut attachment_path: Option<&str> = None;
        for node in &scene.nodes {
            if node.script.as_deref() == Some(script_res) {
                if attachment_path.is_some() {
                    return None; // multiple attachment nodes — ambiguous
                }
                attachment_path = Some(&node.path);
            }
        }
        let attachment_path = attachment_path?;

        let resolved: ResolvedRoot = match query {
            NodePathQuery::RelativePath(rel) => {
                self.scenes
                    .resolve_relative_from(scene_res, attachment_path, rel)?
            }
            NodePathQuery::UniqueName(name) => self.scenes.resolve_unique_in(scene_res, name)?,
        };
        self.resolved_root_to_facts(resolved)
    }

    /// Map a [`ResolvedRoot`] to a [`SceneNodeFacts`], SCRIPT-FIRST. An attached GDScript (resolvable
    /// to an indexed `FileId`) takes precedence over a native `type=`; a script that doesn't resolve
    /// to an indexed file falls back to the native type; a node with neither yields `None`
    /// (permissive — we know nothing precise to say).
    fn resolved_root_to_facts(&self, resolved: ResolvedRoot) -> Option<SceneNodeFacts> {
        if let Some(script_res) = &resolved.script {
            if let Some(fid) = self.inner.resolve_res_path(script_res) {
                return Some(SceneNodeFacts::Script(fid));
            }
            // The script attached to the node isn't an indexed `.gd` (e.g. a `.cs`/`.gdshader`, or a
            // path the index doesn't know): fall through to the native type rather than lying.
        }
        resolved.native_type.map(SceneNodeFacts::Native)
    }
}

impl CrossFileQuery for WorkspaceXFileQuery<'_> {
    fn global_class_file(&self, name: &str) -> Option<FileId> {
        self.inner.global_class_file(name)
    }

    fn interface(&self, file: FileId) -> Option<&Interface> {
        self.inner.interface(file)
    }

    fn resolve_res_path(&self, path: &str) -> Option<FileId> {
        self.inner.resolve_res_path(path)
    }

    fn resolve_path_from(&self, from: FileId, raw: &str) -> Option<FileId> {
        // MUST delegate: the trait default is raw-only, and relative `preload("sibling.gd")`
        // resolution lives in the SyntacticQuery override.
        self.inner.resolve_path_from(from, raw)
    }

    fn file_path(&self, file: FileId) -> Option<&str> {
        self.inner.file_path(file)
    }

    fn autoload_file(&self, name: &str) -> Option<FileId> {
        self.autoloads.get(name).copied()
    }

    /// Phase-3 navigation substrate (dormant; NOT consulted by the diagnostic path — `reduce_get_node`
    /// types `$`/`%` as bare `Node`, see [`gd_analyze::CrossFileQuery::scene_node_facts`]). Resolves a
    /// `$`/`%` access by `script_file` against the scene(s) it is attached to. CONSERVATIVE: returns
    /// `Some` only when EVERY attaching scene resolves the access to the identical fact; any miss /
    /// disagreement / unresolved instance yields `None` (a missed precise type is fine; a wrong one is
    /// not). The gd_project [`SceneIndex`] does the graph walk (instanced sub-scenes through its own
    /// parsed scenes); this method finds the attachment node, requires cross-scene agreement, and maps
    /// the resolved root to the analyzer's fact type.
    fn scene_node_facts(
        &self,
        script_file: FileId,
        query: &NodePathQuery,
    ) -> Option<SceneNodeFacts> {
        // FileId → the script's `res://` path (the scene reverse-map key). The index path is already
        // normalized; `path_to_res` string-strips the (normalized) project root.
        let script_path = self.file_path(script_file)?;
        let script_res = gd_project::path_to_res(&self.project_root, Utf8Path::new(script_path))?;

        // Every scene that attaches this script. Resolve in each and require unanimous agreement —
        // a script shared by two scenes that resolve `$X` to different types must stay permissive.
        let mut agreed: Option<SceneNodeFacts> = None;
        let mut any_scene = false;
        for scene_res in self.scenes.scenes_attaching_script(&script_res) {
            any_scene = true;
            let facts = self.resolve_one_scene(scene_res, &script_res, query)?;
            match &agreed {
                None => agreed = Some(facts),
                Some(prev) if *prev == facts => {}
                Some(_) => return None, // disagreement across scenes — permissive
            }
        }
        // No scene attaches this script → nothing to say (permissive). `agreed` is `Some` iff at
        // least one scene resolved AND all that did agreed.
        if !any_scene {
            return None;
        }
        agreed
    }

    fn member_initializer_xrefs(&self, file: FileId, member: &str) -> Vec<MemberXref> {
        // Map FileId → path through the `CrossFileQuery` surface (`file_path`) rather than reaching
        // into `inner.index`, so this wrapper depends only on the trait it implements — not on
        // `SyntacticQuery`'s private fields.
        let Some(path) = self.file_path(file) else {
            return Vec::new();
        };
        let Some(key) = CanonicalKey::for_path(camino::Utf8Path::new(path)) else {
            return Vec::new();
        };
        // WP-RD8 freshness gate (replaces the retired `Index::is_dirty_fid`): serve the cached
        // `member_xrefs` only when the cache entry's stamped epoch still equals the dependency's
        // current epoch. `reindex()` updates a dependency's interface (bumping its epoch through
        // `mark_dirty`) but deliberately does NOT refresh the analysis cache, so a re-indexed
        // dependency's cached entry carries a now-stale epoch — serving its `member_xrefs` would
        // evaluate the WP-R2 cross-file member-cycle diagnostic against the dependency's OLD
        // analysis (a silent "never lie" violation). Degrade to "no xrefs known" in that window:
        // the gate trades a possibly-stale hit for a possibly-MISSING diagnostic, which
        // auto-recovers once the dependency re-analyzes and re-stamps the entry with the current
        // epoch — but only for DepGraph-tracked edges. A cross-file cycle reached via a
        // `preload(...)` const initializer is NOT such an edge (a const has no type annotation, so
        // it's never a DepGraph dependency, and editing the dependency doesn't re-invalidate this
        // consumer), so for those the missing diagnostic does not auto-recover this session.
        //
        // `LruCache::peek` is the non-recency-touching accessor (matches `HashMap::get`'s shape):
        // using it instead of `get` keeps a cross-file member-xrefs read from shielding stale
        // entries from the WP-H1 Soft-pressure shed — `get` would promote the entry to MRU and the
        // LRU order would no longer reflect direct user activity.
        self.analysis_cache
            .peek(&key)
            .filter(|entry| entry.epoch == self.index.epoch_of(file))
            .and_then(|entry| entry.value.member_xrefs().get(member).cloned())
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use camino::Utf8Path;
    use gd_analyze::{AnalysisResult, MemberName, MemberXref};
    use gd_project::{FileId, Index};
    use gd_types::NativeDb;
    use lsp_types::Uri;
    use std::num::NonZeroUsize;
    use std::rc::Rc;

    /// LRU capacity for tests — generous enough that no test crosses it accidentally (tests below
    /// insert at most one or two entries; the production default is 512). NonZeroUsize::new only
    /// panics on zero, which 32 obviously isn't.
    const TEST_CACHE_CAPACITY: usize = 32;

    fn test_cache() -> LruCache<CanonicalKey, CacheEntry<AnalysisResult>> {
        LruCache::new(NonZeroUsize::new(TEST_CACHE_CAPACITY).unwrap())
    }

    fn empty_result_with_xrefs(xrefs: Vec<(&str, Vec<(FileId, &str)>)>) -> AnalysisResult {
        let map = xrefs
            .into_iter()
            .map(|(from, targets)| {
                let v: Vec<MemberXref> = targets
                    .into_iter()
                    .map(|(target_file, target_member)| MemberXref {
                        target_file,
                        target_member: MemberName::from(target_member),
                    })
                    .collect();
                (MemberName::from(from), v)
            })
            .collect();
        AnalysisResult::new_for_test(
            gd_analyze::TypeTable::new(0),
            gd_analyze::FoldTable::new(0),
            Vec::new(),
            map,
            Vec::new(),
        )
    }

    /// A cache slot from a result. The xref query ignores the content fingerprint (freshness is
    /// gated on the epoch comparison, not the hash), so a dummy hash is fine here. The epoch is
    /// stamped `0` to match a cold-indexed file's epoch (`set_interface` never bumps it); the
    /// dependency-dirty test below edits the file via `Index::txn` to advance its epoch past this
    /// stamp and prove the gate fires.
    fn entry(result: Rc<AnalysisResult>) -> CacheEntry<AnalysisResult> {
        CacheEntry {
            hash: 0,
            epoch: 0,
            value: result,
        }
    }

    /// The cache key the production writer (`Workspace::analyze`) stores under: the client's wire
    /// URI string. Built via [`CanonicalKey::for_uri`] from a parsed `Uri`.
    fn wire_key(uri: &str) -> CanonicalKey {
        CanonicalKey::for_uri(&uri.parse::<Uri>().unwrap())
    }

    #[test]
    fn returns_xrefs_when_cache_hit_by_uri() {
        // Build an index with one file (b.gd) and put a cached AnalysisResult under its URI key.
        let mut idx = Index::new(camino::Utf8PathBuf::from("/proj"));
        let b_iface = gd_project::extract_interface(
            &gd_syntax::parse("class_name B\nextends Node\nconst V = 1\n").tree,
        );
        let b_fid = idx.set_interface(Utf8Path::new("/proj/b.gd"), b_iface);
        let native = NativeDb::empty();

        // B's analyzer recorded that B's V references (a_fid, "X").
        let xrefs = vec![("V", vec![(FileId::new(7), "X")])];
        let cached = Rc::new(empty_result_with_xrefs(xrefs));
        let mut cache = test_cache();
        cache.put(wire_key("file:///proj/b.gd"), entry(Rc::clone(&cached)));

        let scenes = SceneIndex::default();
        let xfile = WorkspaceXFileQuery::new(
            &idx,
            &native,
            &cache,
            rustc_hash::FxHashMap::default(),
            &scenes,
            Utf8Path::new("/proj"),
        );
        let got = xfile.member_initializer_xrefs(b_fid, "V");
        assert_eq!(
            got,
            vec![MemberXref {
                target_file: FileId::new(7),
                target_member: "X".into()
            }]
        );

        // Unknown member: empty.
        assert!(xfile.member_initializer_xrefs(b_fid, "Other").is_empty());
    }

    #[test]
    fn returns_empty_when_cache_miss() {
        let mut idx = Index::new(camino::Utf8PathBuf::from("/proj"));
        let iface =
            gd_project::extract_interface(&gd_syntax::parse("class_name C\nextends Node\n").tree);
        let fid = idx.set_interface(Utf8Path::new("/proj/c.gd"), iface);
        let native = NativeDb::empty();
        let cache = test_cache();

        let scenes = SceneIndex::default();
        let xfile = WorkspaceXFileQuery::new(
            &idx,
            &native,
            &cache,
            rustc_hash::FxHashMap::default(),
            &scenes,
            Utf8Path::new("/proj"),
        );
        assert!(xfile.member_initializer_xrefs(fid, "anything").is_empty());
    }

    #[test]
    fn returns_empty_when_dependency_is_dirty() {
        // Freshness gate regression: a dirty dependency's cached
        // member_xrefs are stale — `reindex` updates the interface and marks dependents dirty but
        // does NOT refresh the analysis cache. The reader must degrade to "no xrefs known" rather
        // than serve the stale entry on a cache hit.
        let mut idx = Index::new(camino::Utf8PathBuf::from("/proj"));
        let b_iface = gd_project::extract_interface(
            &gd_syntax::parse("class_name B\nextends Node\nconst V = 1\n").tree,
        );
        let b_fid = idx.set_interface(Utf8Path::new("/proj/b.gd"), b_iface);
        idx.finish_cold_index();

        // Cache an xref under b's wire-URI key, exactly as `Workspace::analyze` would.
        let xrefs = vec![("V", vec![(FileId::new(7), "X")])];
        let cached = Rc::new(empty_result_with_xrefs(xrefs));
        let mut cache = test_cache();
        cache.put(wire_key("file:///proj/b.gd"), entry(Rc::clone(&cached)));

        // Edit b → `on_file_changed` always bumps b's cache epoch (WP-RD8 `mark_dirty`), simulating
        // a dependency reindex whose analysis cache was left unrefreshed: the cached entry above is
        // stamped epoch 0, but b's epoch is now 1.
        let b_iface2 = gd_project::extract_interface(
            &gd_syntax::parse("class_name B\nextends Node\nconst V = 2\n").tree,
        );
        // `on_file_changed` is `pub(crate)` in gd_project; outside the crate it is reached only
        // through the sealed `IndexMut` that `Index::txn` hands its closure.
        idx.txn(Utf8Path::new("/proj/b.gd"), |m| {
            m.on_file_changed(Utf8Path::new("/proj/b.gd"), b_iface2);
        });
        assert!(
            idx.epoch_of(b_fid) > 0,
            "precondition: b's cache epoch advanced past the cached entry's stamp after the edit"
        );

        let native = NativeDb::empty();
        let scenes = SceneIndex::default();
        let xfile = WorkspaceXFileQuery::new(
            &idx,
            &native,
            &cache,
            rustc_hash::FxHashMap::default(),
            &scenes,
            Utf8Path::new("/proj"),
        );
        assert!(
            xfile.member_initializer_xrefs(b_fid, "V").is_empty(),
            "a dirty (stale) dependency must degrade to no-xrefs, not serve the cached entry"
        );
    }

    #[test]
    fn returns_xrefs_when_project_path_contains_a_space() {
        // Regression: pre-CanonicalKey, the reader probed a raw `path.as_str()` candidate that
        // never percent-encoded the space, so the production cache (keyed by `uri.as_str()` from
        // the LSP wire URI — already `%20`-encoded) was missed for any project under a path with
        // a space, silently disabling the WP-R2 cross-file cycle check there. The newtype makes
        // the writer's `for_uri` key and the reader's `for_path` key equal by construction.
        let mut idx = Index::new(camino::Utf8PathBuf::from("/proj/My Game"));
        let iface = gd_project::extract_interface(
            &gd_syntax::parse("class_name B\nextends Node\nconst V = 1\n").tree,
        );
        let b_fid = idx.set_interface(Utf8Path::new("/proj/My Game/b.gd"), iface);
        let native = NativeDb::empty();

        // Cache the xrefs under the percent-encoded wire URI the production writer would use.
        let xrefs = vec![("V", vec![(FileId::new(7), "X")])];
        let cached = Rc::new(empty_result_with_xrefs(xrefs));
        let mut cache = test_cache();
        cache.put(
            wire_key("file:///proj/My%20Game/b.gd"),
            entry(Rc::clone(&cached)),
        );

        let scenes = SceneIndex::default();
        let xfile = WorkspaceXFileQuery::new(
            &idx,
            &native,
            &cache,
            rustc_hash::FxHashMap::default(),
            &scenes,
            Utf8Path::new("/proj"),
        );
        let got = xfile.member_initializer_xrefs(b_fid, "V");
        assert_eq!(
            got,
            vec![MemberXref {
                target_file: FileId::new(7),
                target_member: "X".into()
            }],
            "for_path must produce the percent-encoded URI so space-containing paths hit the cache"
        );
    }

    #[test]
    fn delegates_non_xref_methods_to_syntactic_query() {
        let mut idx = Index::new(camino::Utf8PathBuf::from("/proj"));
        let iface = gd_project::extract_interface(
            &gd_syntax::parse("class_name Hero\nextends Node\n").tree,
        );
        idx.set_interface(Utf8Path::new("/proj/hero.gd"), iface);
        idx.finish_cold_index();
        let native = NativeDb::empty();
        let cache = test_cache();

        let scenes = SceneIndex::default();
        let xfile = WorkspaceXFileQuery::new(
            &idx,
            &native,
            &cache,
            rustc_hash::FxHashMap::default(),
            &scenes,
            Utf8Path::new("/proj"),
        );
        assert!(xfile.global_class_file("Hero").is_some());
        assert!(xfile.global_class_file("Nonexistent").is_none());
    }

    /// WP-H2 reader-uses-peek invariant: a cross-file xref lookup is a recorded-fact read and
    /// must NOT reorder the LRU. If the reader used `get` instead of `peek`, an older entry
    /// would be promoted to MRU on every consult, shielding it from the WP-H1 Soft-pressure
    /// shed forever — silently inverting the eviction policy.
    #[test]
    fn xref_lookup_does_not_promote_lru_recency() {
        // Two entries; OLDEST is the one we look up. After the lookup, the OLDEST must still
        // be the OLDEST (a `pop_lru` would still return it, not the other).
        let mut idx = Index::new(camino::Utf8PathBuf::from("/proj"));
        let oldest_iface = gd_project::extract_interface(
            &gd_syntax::parse("class_name Older\nextends Node\nconst V = 1\n").tree,
        );
        let oldest_fid = idx.set_interface(Utf8Path::new("/proj/older.gd"), oldest_iface);
        let newer_iface = gd_project::extract_interface(
            &gd_syntax::parse("class_name Newer\nextends Node\n").tree,
        );
        idx.set_interface(Utf8Path::new("/proj/newer.gd"), newer_iface);
        idx.finish_cold_index();
        let native = NativeDb::empty();

        let xrefs = vec![("V", vec![(FileId::new(99), "T")])];
        let oldest_cached = Rc::new(empty_result_with_xrefs(xrefs));
        let newer_cached = Rc::new(empty_result_with_xrefs(vec![]));
        let mut cache = test_cache();
        // Insert oldest FIRST so it's at the LRU end.
        let oldest_key = wire_key("file:///proj/older.gd");
        let newer_key = wire_key("file:///proj/newer.gd");
        cache.put(oldest_key.clone(), entry(Rc::clone(&oldest_cached)));
        cache.put(newer_key.clone(), entry(Rc::clone(&newer_cached)));

        // Look up the oldest via the xfile reader → its xrefs come back, and recency must NOT
        // shift (otherwise the next pop_lru would target the wrong entry).
        let scenes = SceneIndex::default();
        let xfile = WorkspaceXFileQuery::new(
            &idx,
            &native,
            &cache,
            rustc_hash::FxHashMap::default(),
            &scenes,
            Utf8Path::new("/proj"),
        );
        let got = xfile.member_initializer_xrefs(oldest_fid, "V");
        assert_eq!(
            got,
            vec![MemberXref {
                target_file: FileId::new(99),
                target_member: "T".into()
            }]
        );

        // pop_lru must still drop the older entry — the reader's peek did not promote it.
        let (popped_key, _) = cache
            .pop_lru()
            .expect("two entries; pop_lru must return something");
        assert_eq!(
            popped_key, oldest_key,
            "xfile lookup used peek (not get), so the older entry stayed at the LRU end"
        );
    }
}
