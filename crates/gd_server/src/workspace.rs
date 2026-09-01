//! [`Workspace`] — the per-session project environment the server queries: the native-class DB, the
//! parsed `project.godot` model, the eager-interface index, and a small parse cache shared between
//! `documentSymbol` and `publishDiagnostics` (closing the M1 double-parse).
//!
//! All construction degrades rather than fails (`docs/00`: never crash): a missing/garbage
//! `extension_api.json` yields an empty native DB (types go dynamic) plus one log notice, and an
//! unreadable `.gd` is skipped during indexing.

use std::rc::Rc;

use camino::{Utf8Path, Utf8PathBuf};
use gd_analyze::{code_from_name, AnalysisResult, StrictProfile, StrictSettings, WarnPolicy};
use gd_project::cache::{self, FileStat};
use gd_project::{
    resolve_dialect, AssetIndex, DialectOrigin, Index, LoadOutcome, ProjectModel, SceneIndex,
};
use gd_syntax::{Dialect, ParseResult, ParseTree};
use gd_types::{DocXmlError, NativeDb};
use lru::LruCache;
use rustc_hash::{FxHashMap, FxHashSet};
use walkdir::WalkDir;

use crate::uri::CanonicalKey;
use crate::xfile::{AutoloadEnv, WorkspaceXFileQuery};
use gd_project::is_excluded;

use crate::config::{InitializationOptions, StrictConfig, StrictProfile as ServerStrictProfile};

/// One parse/analysis cache slot. Validity is **content-addressed**: a hit requires `hash` to
/// equal the [`fingerprint`] of the text the caller is asking about — not the LSP `version`
/// counter the cache used to key on.
///
/// Why content, not version: the closed-file nav path (`references` / `callHierarchy/*` reading a
/// cross-file candidate) has no editor version, so it read the file from disk and passed
/// `version = 0`. A version-only check then returned a *stale* parse forever after an on-disk
/// edit — the next disk read passed `0` again and hit the old entry, so navigation pointed at
/// dead byte spans until the file was opened in an editor. Keying validity on a content
/// fingerprint makes a hit correct regardless of how the text arrived: open buffer, disk read, or
/// a change the watcher never observed (remote FS, wake-from-suspend — the drift `diagnose
/// --reconcile` exists for). Because every nav handler re-reads the candidate from disk before
/// asking the cache, the fingerprint always reflects current content, so it is also what makes
/// eviction unnecessary on the (hot) open-buffer reindex path.
pub(crate) struct CacheEntry<T> {
    pub(crate) hash: u64,
    /// WP-RD8: the [`gd_project::Index::epoch_of`] of this entry's file at analysis time — the
    /// dependency-aware half of the composite cache key. A hit requires both `hash` (own content)
    /// AND this `epoch` to still match the file's current epoch, so a dependency interface change
    /// (which bumped the file's epoch through the reverse-dependency closure) self-invalidates the
    /// entry with no dirty-bit override. The `parse_cache` is content-only (a parse depends on
    /// nothing but its own bytes), so its entries carry `epoch: 0` and never consult this field.
    pub(crate) epoch: u64,
    pub(crate) value: Rc<T>,
}

/// Normalized owned path in the index's key form (`gd_project::normalize_path`).
fn normalize_path_buf(path: &Utf8Path) -> Utf8PathBuf {
    gd_project::normalize_path(path)
}

/// Content fingerprint for cache validation: a fast non-cryptographic hash of the full text.
/// FxHasher is a multiply-xor hash with weaker-than-ideal avalanche/distribution (no collision-
/// resistance guarantee), so the true collision rate is worse than an idealized 1/2⁶⁴ — but it is
/// still orders of magnitude below the false-hit rate of the previous version-only check, which was
/// wrong for *every* closed-file disk read. (If a fingerprint collision ever proves to matter,
/// swap in a stronger 64-bit hash here — the call sites only depend on equality, not the algorithm.)
pub(crate) fn fingerprint(text: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = rustc_hash::FxHasher::default();
    text.hash(&mut hasher);
    hasher.finish()
}

/// Everything the server knows about the open project.
pub struct Workspace {
    /// Native classes (engine, from JSON; GDExtension classes merged from doc XML). Empty when
    /// no dump is configured/loadable — callers treat unknown natives as dynamic.
    pub native: NativeDb,
    /// `project.godot`: root, autoloads, warning config, enumerated GDExtensions, UID map.
    pub project: ProjectModel,
    /// The Godot feature release this project's scripts are read as, resolved once from
    /// `initializationOptions.dialect` or `project.godot`'s `application/config/features`
    /// ([`gd_project::resolve_dialect`]). Every parse and analyze in the session uses it, and it
    /// is part of the warm-start cache key. A `project.godot` edit that changes it forces a full
    /// reload — see [`Self::reload_project_and_native`].
    pub dialect: Dialect,
    /// How [`Self::dialect`] was arrived at, so the server can tell the user when gdls guessed or
    /// clamped rather than read a declared version.
    pub dialect_origin: DialectOrigin,
    /// Set when a native dump was rejected for naming a different Godot release than the project
    /// declared (#329) — the message to show the user, already phrased for the wire. `None` in
    /// the ordinary case where the dump and the project agree.
    pub native_release_notice: Option<String>,
    /// `class_name` registry + per-file interface tables + dependency graph.
    pub index: Index,
    /// Effective per-warning level, resolved from `project.godot` + the client's strict profile +
    /// fine-grained overrides. The same policy is reused for every per-file analyze run in the
    /// session; rebuilt only on `project.godot` change via [`Self::reload_project_and_native`].
    pub policy: WarnPolicy,
    /// `CanonicalKey → parse`. One parse per distinct content; reused across handlers. Content-
    /// addressed (see [`CacheEntry`]), so an edit re-parses exactly once and a stale closed-file
    /// entry is never served. M5 WP-H2: bounded LRU so a long-running session that opens
    /// transient files for nav can't grow the cache without limit. Capacity comes from
    /// `initializationOptions.memory.cacheCapacity` (default
    /// [`crate::config::MemoryConfig::DEFAULT_CACHE_CAPACITY`]). Insert triggers eviction of the
    /// least-recently-used entry once the cap is reached; the WP-H1 Soft-pressure ladder calls
    /// [`Self::evict_half`] to bulk-drop the oldest half before that point if peak RSS climbs
    /// past the soft cap.
    pub(crate) parse_cache: LruCache<CanonicalKey, CacheEntry<ParseResult>>,
    /// `CanonicalKey → analyze`, same content-addressed validity as `parse_cache`. Additionally
    /// cleared wholesale on a `project.godot` / native-DB reload, since those entries were computed
    /// against a policy / native lattice that just changed (a content fingerprint can't see that).
    /// M5 WP-H2: bounded LRU (see [`Self::parse_cache`]).
    pub(crate) analysis_cache: LruCache<CanonicalKey, CacheEntry<AnalysisResult>>,
    /// M5 WP-O3 session-wide fixpoint cap, mirrored from `initializationOptions.analyzer.iterLimit`.
    /// Default `None` ⇒ the analyzer falls back to [`gd_analyze::DEFAULT_ITER_LIMIT`]. Per-call
    /// [`Self::analyze_with_options`] overrides win when present; this field is just the
    /// session default the bare [`Self::analyze`] caller (which has no per-request context)
    /// picks up.
    analyzer_iter_limit: Option<u32>,
    /// M7 (#57) session-wide checkpoint sleep, mirrored from
    /// `initializationOptions.analyzer.checkpointDelayUs` — the test/diagnostic governor that
    /// makes every analyze pass deterministically slow. Same per-call override semantics as
    /// [`Self::analyzer_iter_limit`]. `None` in production.
    analyzer_checkpoint_delay: Option<std::time::Duration>,
    /// M7 (#61): bumped whenever the analysis cache is cleared wholesale (`project.godot` /
    /// native-DB reload, and runtime config changes once #59 lands) — invalidations the
    /// content-hash + epoch composite key cannot see, because neither the file's bytes nor its
    /// dependency epochs changed. The third component of the pull-diagnostics `resultId`, so a
    /// pulled report is never wrongly answered `unchanged` across such a reload.
    analysis_generation: u64,
    /// M7 (#60): content fingerprint of the last DISK-sourced reindex per canonical path —
    /// the dedupe gate for double delivery of the same change by the native watcher AND a
    /// client's `didChangeWatchedFiles`. `Index::on_file_changed` unconditionally bumps the
    /// file's epoch (forcing re-analysis), so an identical-content reindex is NOT a free no-op;
    /// this gate is what makes the belt-and-suspenders delivery free. Content-addressed (not a
    /// time window), so an A→B→A edit sequence applies all three. Bounded LRU; entries drop on
    /// [`Self::remove`].
    last_applied_disk: LruCache<Utf8PathBuf, u64>,
    /// Per-file stat snapshot used for warm-start cache saves and stat-based reconcile. Keyed by
    /// normalized path (`gd_project::normalize_path`). Populated during warm-load (stat-diff walk)
    /// and after a cold `Index::build` (stat sweep of all interned files). Updated by
    /// [`Self::reconcile`] as files are added/modified/removed so the table stays current
    /// throughout the session. Consumed by [`Self::save_cache`] → `gd_project::cache::save`.
    pub(crate) stat_table: FxHashMap<Utf8PathBuf, FileStat>,
    /// M11 (#76): the project's `.tscn` scene index — node/script/instance relations, keyed by
    /// `res://` path. Built parallel to [`Self::index`] (scenes aren't `FileId`s; see
    /// [`gd_project::SceneIndex`]). Persisted/restored via the same warm-start cache. The diagnostic
    /// path does NOT read it — a valid `$`/`%` types as bare `NATIVE Node` (faithful to Godot),
    /// scene-independent; this field is the substrate the precise NAVIGATION surfaces read (hover /
    /// definition / typeDefinition / completion, through [`Self::scene_node_facts`]).
    pub(crate) scenes: SceneIndex,
    /// #127: the project's arbitrary-asset index — the `res://` paths of every project file that is
    /// NOT a `.gd` script (covered by [`Self::index`]) and NOT a `.tscn` scene (covered by
    /// [`Self::scenes`]): textures, audio, `.tres`, fonts, … Built parallel to those two and
    /// persisted/restored via the same warm-start cache. Consumed only by `load`/`preload` path
    /// completion, which lists scripts ∪ scenes ∪ assets to match Godot's `_get_directory_contents`
    /// (every file, no type filter). Paths only — never parsed; freshness rides the shared stat
    /// table like scenes.
    pub(crate) assets: AssetIndex,
}

impl Workspace {
    /// Build the workspace for a project rooted at `root`: load the native DB, parse `project.godot`,
    /// then either warm-start from the index cache (stat-diff only changed files) or cold-index
    /// every `.gd`. Runs after the `initialize` response is sent, so a large scan never stalls the
    /// handshake.
    ///
    /// Never spawns Godot — since v1.0.2 (issue #25) NO resolution path does. The auto-dump runs
    /// on a background thread (`api_dump::spawn_background_dump`, started by `serve_inner` after
    /// this load) and is adopted mid-session via [`Self::reload_native`], so direct callers
    /// (`gdls diagnose`, every test) and the session startup are equally process-free.
    pub fn load(root: &Utf8Path, options: &InitializationOptions) -> Self {
        Self::load_with_progress(root, options, &mut crate::progress::NoopSink)
    }

    /// [`Self::load`] reporting per-file progress into `sink` (M7 #58) — the cold-index parse
    /// walk reports with a known total; the warm-start stat-diff walk reports indeterminately
    /// (WalkDir streams, no total up front). Direct callers with nothing to show
    /// ([`Self::load`]: tests, `gdls diagnose`) pass the no-op sink.
    pub(crate) fn load_with_progress(
        root: &Utf8Path,
        options: &InitializationOptions,
        sink: &mut dyn crate::progress::ProgressSink,
    ) -> Self {
        // M5 WP-O1: cold_index span. Captures the full bootstrap — project model + native DB +
        // eager interface index + warn policy — so a hierarchical-profiler dump nests anything
        // that crosses the threshold under it. Fields are recorded with `Empty` and filled in
        // before the span closes, so the on-close event carries the final elapsed + file_count.
        let _start = std::time::Instant::now();
        let _span = tracing::info_span!(
            "cold_index",
            root = %root,
            elapsed_us = tracing::field::Empty,
            file_count = tracing::field::Empty,
        );
        let _enter = _span.enter();
        let (project, project_outcome) = ProjectModel::load_checked(root);
        let (dialect, dialect_origin) = resolve_dialect(
            options.dialect(),
            project.declared_engine_version,
            project_outcome == LoadOutcome::Loaded,
        );
        log::info!(
            "dialect: reading scripts as Godot {dialect} (origin: {dialect_origin:?}, \
             project.godot declared: {:?})",
            project.declared_engine_version,
        );
        let (native, native_release_notice) =
            load_native(options, &project, root, dialect, dialect_origin);

        // Build the cache key for warm-start attempt.
        let key = build_cache_key(&native, root, dialect);

        // Attempt warm-start: load the persisted cache and stat-diff it against disk.
        // On any failure (missing file, key mismatch, verify failure) fall through to cold build.
        let (mut index, mut scenes, assets, stat_table) = match cache::load(root, &key) {
            Some(loaded) => {
                log::info!(
                    "cache: warm-start candidate found; stat-diffing {} cached files",
                    loaded.files.len()
                );
                warm_index_from_cache(loaded, root, dialect, sink)
            }
            None => {
                // Cold build — then sweep all interned files to populate the stat table. The scene
                // index is cold-built in parallel (its own `.tscn` walk, shared exclusion set), and
                // the asset index in another (every other project file, same exclusion set).
                let idx = Index::build_with_progress(root, dialect, &mut |done, total| {
                    sink.progress(done, Some(total), "parsing scripts");
                });
                let scene_idx = SceneIndex::build(root, project.uids.clone());
                let asset_idx = AssetIndex::build(root);
                let mut stats = build_stat_table_from_index(&idx);
                add_scene_stats(&mut stats, &scene_idx, root);
                add_asset_stats(&mut stats, &asset_idx, root);
                (idx, scene_idx, asset_idx, stats)
            }
        };

        // #447: the index resolves `uid://` in a `preload` or a path-`extends` through this map,
        // so it has to be in place before the first analysis reads a dependency edge. The scan
        // itself already ran as part of `ProjectModel::load_checked`.
        set_index_uid_map(&mut index, root, project.uids.clone());

        // #484: the scene index resolves a `path`-less `[ext_resource]` through the same map. A
        // COLD build already had it; a WARM one loaded scenes canonicalized against the previous
        // session's map, and a sidecar changed while gdls was off leaves the `.tscn` untouched, so
        // no stat diff catches it. Re-read the scenes that name a uid with no path — by the
        // issue's own framing that set is near-empty in any editor-written project.
        scenes.set_uid_map(project.uids.clone());
        for res in scenes.uid_referencing_scenes() {
            let Some(path) = gd_project::res_to_path(root, &res) else {
                continue;
            };
            match std::fs::read_to_string(&path) {
                Ok(text) => scenes.reindex(&res, &text),
                // Unreadable — keep the prior scene, matching every other reindex path.
                Err(e) => log::warn!("scene index: uid re-resolve skipped unreadable {path}: {e}"),
            }
        }

        let policy = WarnPolicy::build(
            &project.warnings,
            &strict_settings(&options.strict),
            dialect,
        );
        let file_count = index.file_count();
        log::info!(
            "indexed {} script(s); {} class_name(s); {} native class(es)",
            file_count,
            index.registry().len(),
            native.class_count(),
        );
        _span.record("elapsed_us", _start.elapsed().as_micros() as u64);
        _span.record("file_count", file_count as u64);
        // `cache_capacity()` returns a `NonZeroUsize` (clamping a client `0`/absent up to the
        // default), so the "never zero" invariant `lru::LruCache::new` requires is enforced by the
        // type — no fallible unwrap at this call site.
        let cap = options.memory.cache_capacity();
        Workspace {
            native,
            project,
            dialect,
            dialect_origin,
            native_release_notice,
            index,
            policy,
            parse_cache: LruCache::new(cap),
            analysis_cache: LruCache::new(cap),
            analyzer_iter_limit: options.analyzer.iter_limit,
            analyzer_checkpoint_delay: options
                .analyzer
                .checkpoint_delay_us
                .map(std::time::Duration::from_micros),
            analysis_generation: 0,
            last_applied_disk: LruCache::new(
                std::num::NonZeroUsize::new(4096)
                    .expect("invariant: the dedupe cache capacity is a nonzero constant"),
            ),
            stat_table,
            scenes,
            assets,
        }
    }

    /// M7 (#60): `true` when a disk-sourced reindex of `path` with exactly this content was
    /// already applied — the duplicate-delivery gate (see [`Self::last_applied_disk`]).
    pub(crate) fn disk_apply_is_duplicate(&mut self, path: &Utf8Path, text: &str) -> bool {
        let hash = fingerprint(text);
        self.last_applied_disk
            .get(&normalize_path_buf(path))
            .is_some_and(|h| *h == hash)
    }

    /// M7 (#60): record a disk-sourced reindex so the other delivery channel can dedupe it.
    pub(crate) fn record_disk_apply(&mut self, path: &Utf8Path, text: &str) {
        self.last_applied_disk
            .put(normalize_path_buf(path), fingerprint(text));
    }

    /// M7 (#61): the wholesale-invalidation counter component of the pull-diagnostics
    /// `resultId` — see the field doc.
    pub fn analysis_generation(&self) -> u64 {
        self.analysis_generation
    }

    /// M7 (#59): apply a runtime strict-config change — rebuild the warning policy against the
    /// unchanged `project.godot` warning config, and invalidate every cached analysis (its
    /// diagnostics were filtered through the old policy; content hash + epoch can't see that,
    /// hence the generation bump). The caller republishes open buffers.
    pub fn apply_strict(&mut self, strict: &StrictConfig) {
        self.policy = WarnPolicy::build(
            &self.project.warnings,
            &strict_settings(strict),
            self.dialect,
        );
        self.analysis_cache.clear();
        self.analysis_generation += 1;
    }

    /// M7 (#59): apply runtime analyzer knobs (iteration cap, checkpoint delay). Cached results
    /// computed under the old knobs stay valid in content terms, but an operator lowering the
    /// cap to force-trip the governor expects fresh runs — clear + bump like [`Self::apply_strict`].
    pub fn set_analyzer_config(&mut self, analyzer: &crate::config::AnalyzerConfig) {
        self.analyzer_iter_limit = analyzer.iter_limit;
        self.analyzer_checkpoint_delay = analyzer
            .checkpoint_delay_us
            .map(std::time::Duration::from_micros);
        self.analysis_cache.clear();
        self.analysis_generation += 1;
    }

    /// M7 (#59): resize both LRU caches to a runtime `memory.cacheCapacity`. `lru::resize`
    /// evicts oldest entries when shrinking; no invalidation semantics change.
    pub fn set_cache_capacity(&mut self, cap: std::num::NonZeroUsize) {
        self.parse_cache.resize(cap);
        self.analysis_cache.resize(cap);
    }

    /// Atomically persist the index + stat table to the `.gdls` cache directory.
    /// Fire-and-forget: failures are logged at `warn` and never propagated (never crash).
    /// Call AFTER build + reconcile have settled — not mid-reconcile.
    ///
    /// The persisted stat table contains only files that are NOT currently open in an editor
    /// buffer. Files with open buffers may have a buffer-only interface in the index (never
    /// written to disk), so persisting their disk-stat would cause warm-load to see
    /// stored==disk-stat and skip re-parsing — serving the old on-disk interface as if it were
    /// the buffer's unsaved content ("never lie" violation). By excluding them from the persisted
    /// stat table, warm-load treats them as "unknown stat" and re-parses from disk, correctly
    /// recovering the on-disk state. Use [`Self::save_cache`] when no buffers are open (startup,
    /// diagnose CLI); use [`Self::save_cache_excluding_open`] at shutdown or any point where
    /// unsaved editor buffers may exist.
    pub fn save_cache(&self) {
        self.save_cache_excluding_open(&FxHashSet::default());
    }

    /// Like [`Self::save_cache`] but excludes `open_paths` from the persisted stat table, so
    /// warm-load re-parses those files from disk rather than trusting a buffer-only interface.
    /// See [`Self::save_cache`] for the rationale.
    pub fn save_cache_excluding_open(&self, open_paths: &FxHashSet<Utf8PathBuf>) {
        let root = &self.project.root;
        let key = build_cache_key(&self.native, root, self.dialect);
        // Exclude files currently open in an editor buffer: their stat_table entry still reflects
        // the pre-edit disk state (stat_table is only updated by disk-sourced reindexes), so if
        // we persisted that entry, warm-load would see stored==current disk stat (unchanged since
        // the buffer was opened) and skip re-parsing — serving the stale interface. Omitting the
        // entry forces warm-load to re-parse the file from disk and get the correct interface.
        let files: Vec<FileStat> = self
            .stat_table
            .iter()
            .filter(|(path, _)| !open_paths.contains(*path))
            .map(|(_, stat)| stat.clone())
            .collect();
        cache::save(root, &self.index, &self.scenes, &self.assets, &files, key);
    }

    /// Parse `text`, reusing the cached result when the content fingerprint is unchanged. Both
    /// `documentSymbol` and `publishDiagnostics` go through here, so an edit parses exactly once;
    /// and a closed-file nav candidate read fresh from disk re-parses iff its bytes changed (see
    /// [`CacheEntry`] for why this is content-addressed rather than `(uri, version)`-keyed).
    pub fn parse(&mut self, key: &CanonicalKey, text: &str) -> Rc<ParseResult> {
        let hash = fingerprint(text);
        if let Some(entry) = self.parse_cache.get(key) {
            if entry.hash == hash {
                return Rc::clone(&entry.value);
            }
        }
        let parsed = Rc::new(parse_in_dialect(text, self.dialect));
        // `LruCache::put` overwrites any existing entry under `key` and, when at capacity,
        // evicts the least-recently-used entry. The evicted slot is returned; we drop it
        // immediately — the only state it carried was the `Rc<ParseResult>`, which the dropped
        // `Rc` releases when no other handler is mid-use.
        self.parse_cache.put(
            key.clone(),
            CacheEntry {
                hash,
                // Parse validity is content-only (WP-RD8): a parse depends on nothing but its own
                // bytes, so the epoch field is unused on this cache and stamped 0.
                epoch: 0,
                value: Rc::clone(&parsed),
            },
        );
        parsed
    }

    /// Analyze `tree`, reusing the cached result when the content fingerprint of `text` is
    /// unchanged (and the cache hasn't been cleared by a policy/native reload). The cross-file
    /// query is M2's [`SyntacticQuery`] — `extends`/`class_name`/`res://` lookups via the eager
    /// interface index, no re-parse — and the warning policy is the workspace-level one. `path` is
    /// the on-disk path (an open `.gd` will have been interned by [`Self::reindex`] before this
    /// runs, so its [`gd_project::FileId`] is stable); when unknown (e.g. an `untitled:` buffer or
    /// a `.gd` outside the project), the analyzer is run with `file = None` so per-file type
    /// analysis still produces well-typed results.
    ///
    /// **WP-RD2 (FileId(0) placeholder retired).** The former design fell back to a `FileId(0)`
    /// placeholder for orphan files; those bindings then recorded `target_file = Some(FileId(0))`,
    /// colliding with whichever real script the index interned first and mis-attributing the
    /// orphan's references. `FileId` is now `NonZeroU32` and the orphan case threads
    /// `Option<FileId>::None` through [`gd_analyze::analyze`], so the reducer records `None`
    /// ("don't know") for an orphan's bindings — cross-script nav for such a buffer correctly
    /// returns empty instead of pointing at the wrong target.
    pub fn analyze(
        &mut self,
        key: &CanonicalKey,
        path: &Utf8Path,
        tree: &ParseTree,
        text: &str,
    ) -> Rc<AnalysisResult> {
        self.analyze_with_options(key, path, tree, text, gd_analyze::AnalyzeOptions::default())
    }

    /// The warm-start cache key this workspace would write.
    ///
    /// Exposed so a test can prove that a change which must invalidate the cache — the dialect
    /// above all, since the two dialects do not parse identically — actually reaches the key.
    #[must_use]
    pub fn cache_key(&self) -> cache::CacheKey {
        build_cache_key(&self.native, &self.project.root, self.dialect)
    }

    /// Parse `text` under this project's dialect, without touching the parse cache.
    ///
    /// The one-shot counterpart to [`Self::parse`], for the handlers that need a tree for some
    /// *other* file while an analysis borrow is live. Going through here rather than
    /// `gd_syntax::parse` is what keeps a nav or completion parse from reading the project under
    /// [`Dialect::DEFAULT`] when it is pinned to something else.
    #[must_use]
    pub fn parse_source(&self, text: &str) -> ParseResult {
        parse_in_dialect(text, self.dialect)
    }

    /// Return a valid cached analysis for `text` without running the analyzer. Used by the Hard
    /// memory-pressure diagnostic path: cached diagnostics may still serve, but a cache miss must
    /// not allocate a fresh full-analysis working set while the server is shedding.
    pub fn cached_analysis(
        &mut self,
        key: &CanonicalKey,
        path: &Utf8Path,
        text: &str,
    ) -> Option<Rc<AnalysisResult>> {
        let hash = fingerprint(text);
        let file = self.index.file_id(path);
        let current_epoch = file.map_or(0, |f| self.index.epoch_of(f));
        self.analysis_cache
            .get(key)
            .filter(|entry| entry.hash == hash && entry.epoch == current_epoch)
            .map(|entry| Rc::clone(&entry.value))
    }

    /// `analyze` with per-call knobs — M5 WP-O3 (fixpoint governor cap) and WP-O4 (cancellation
    /// token). The token, when present, is checked every 256 nodes inside the analyzer's hot
    /// reducer / resolver loops; on cancel the analyzer bails with a synthetic
    /// `analyzer: request cancelled` diagnostic and returns the partial result. Production LSP
    /// callers — the request handlers — go through here with a freshly-registered token; the
    /// notification-driven `publish_diagnostics` path goes through bare [`Self::analyze`] (no
    /// per-request id to cancel against). Span / cache / dirty-bit semantics are identical to
    /// the wrapper.
    pub fn analyze_with_options<'a>(
        &mut self,
        key: &CanonicalKey,
        path: &Utf8Path,
        tree: &'a ParseTree,
        text: &str,
        mut options: gd_analyze::AnalyzeOptions<'a>,
    ) -> Rc<AnalysisResult> {
        // Default iter_limit to the operator-configurable session-wide cap when the caller
        // hasn't picked an explicit one. The analyzer crate's `DEFAULT_ITER_LIMIT` is the
        // ultimate fallback when neither this nor `analyzer.iterLimit` is set.
        if options.iter_limit.is_none() {
            options.iter_limit = self.analyzer_iter_limit;
        }
        // Same session-default rule for the M7 (#57) checkpoint-sleep governor.
        if options.checkpoint_delay.is_none() {
            options.checkpoint_delay = self.analyzer_checkpoint_delay;
        }
        // The dialect is a property of the project, not of the request, so it is stamped here
        // rather than defaulted: the workspace resolved it once and no caller has a legitimate
        // reason to analyze one file under a different Godot version than the index was built
        // with. Overriding unconditionally means a new call site cannot forget it.
        options.dialect = self.dialect;
        let hash = fingerprint(text);
        // M5 WP-O1: analyze span. The plan's draft field-set is `file, version, ... elapsed_us,
        // diagnostics_count`; the cache is content-addressed (no LSP version threads through
        // here), so the field analogous to "version" is the content fingerprint that actually
        // drives cache hits — record that as `text_hash`. The `cache_hit` boolean (recorded
        // before close) lets a hierarchical-profiler dump distinguish a real reduction-path
        // analyze from a cheap Rc-clone hit.
        let _start = std::time::Instant::now();
        let _span = tracing::info_span!(
            "analyze",
            file = %path,
            text_hash = hash,
            cache_hit = tracing::field::Empty,
            elapsed_us = tracing::field::Empty,
            diagnostics_count = tracing::field::Empty,
        );
        let _enter = _span.enter();
        // WP-RD2: an `untitled:` buffer or a `.gd` outside the project isn't interned, so
        // `file_id` is `None`. Thread that `Option` straight through to `analyze` — the reducer
        // records `None`-attributed bindings for it (no colliding `FileId(0)`), so cross-script
        // nav correctly answers "don't know" for the orphan.
        let file = self.index.file_id(path);
        // WP-RD8: the composite cache key is `(own content hash, dependency-aware epoch)`. A
        // dependency's *interface* change bumps THIS file's epoch (`Index::on_file_changed` →
        // reverse-dependency closure → `mark_dirty`), so a cached entry stamped with the old epoch
        // no longer matches and self-invalidates — even though this file's own bytes (and so its
        // content `hash`) are unchanged. That retires the M4 dirty-bit override + clear-after
        // dance (no `is_dirty`, no `clear_dirty_one`, no ordering constraint). (`parse` stays
        // content-only — a dependency's interface change never alters *this* file's bytes, only
        // its analysis — so a future "consistency" refactor must NOT add the epoch to `parse`.)
        let current_epoch = file.map_or(0, |f| self.index.epoch_of(f));
        let cached = self
            .analysis_cache
            .get(key)
            .filter(|entry| entry.hash == hash && entry.epoch == current_epoch)
            .map(|entry| Rc::clone(&entry.value));
        let cached_used = cached.is_some();
        // #210: set when a bailed re-analyze recovered the cached COMPLETE entry for identical bytes
        // (a same-`hash`, stale-`epoch` entry). That entry is intentionally NOT re-stamped to the
        // current epoch (the bail must self-heal on the next call), so the WP-RD8 epoch-exact
        // postcondition below does not apply to it.
        let mut served_bail_recovery = false;
        let result = match cached {
            Some(hit) => hit,
            None => {
                // Godot threads `parser->script_path` into the head class's `fqcn` for
                // `<file.gd>.<EnumName>` rendering (`gdscript_analyzer.cpp`; working-tree line
                // numbers drift, so cite the symbol). We pass the file basename (e.g. `foo.gd`)
                // rather than the absolute Windows path so the `Display for DataType` `get_file()`
                // mirror (which strips at the last `/`) produces the same string Godot emits.
                let script_path = path.file_name().unwrap_or_default();
                let result = {
                    // The borrow of `&self.analysis_cache` ends with this block, before the
                    // `analysis_cache.insert(...)` below. WorkspaceXFileQuery overrides
                    // `member_initializer_xrefs` and `autoload_file` against the cache/project;
                    // every other CrossFileQuery method delegates to SyntacticQuery.
                    //
                    // Build the autoload typing maps per-call from the project's autoload list +
                    // scene index (M11 Phase 4). Cost is negligible against a full analyze, and
                    // building it per-call avoids any stale-map risk (e.g. an autoload script/scene
                    // indexed after project load). `autoload_typing` mirrors Godot's arm: a
                    // script-backed autoload (direct `.gd`, `uid://`→`.gd`, or scene→root-`.gd`)
                    // populates the FileId map; a scriptless scene populates the native-`Node` floor;
                    // everything else / unindexed degrades to Variant.
                    let autoloads = self.build_autoload_maps(&self.project, &self.scenes);
                    let xfile = WorkspaceXFileQuery::new(
                        &self.index,
                        &self.native,
                        &self.analysis_cache,
                        autoloads,
                        &self.scenes,
                        &self.project.root,
                    );
                    Rc::new(gd_analyze::analyze_with_options(
                        tree,
                        file,
                        script_path,
                        &self.native,
                        &xfile,
                        &self.policy,
                        options,
                    ))
                };
                // Like `parse_cache.put` above: overwrites under the key, evicts the LRU entry
                // when at capacity. The entry is stamped with `current_epoch` so a later
                // dependency change invalidates it.
                //
                // WP-O3/O4: a *bailed* result (fixpoint governor cap hit or request cancelled) has
                // partial side tables, so caching it would silently re-serve a truncated analysis to
                // the next hover/definition/references request on this unchanged content ("never
                // lie"). Skip the cache so the next call re-attempts a full analysis — the governor
                // self-heals once the file is re-analyzed (or a cancel doesn't recur).
                if result.bailed {
                    tracing::warn!(
                        name = "analyze_bailed_uncached",
                        file = %path,
                        "analysis bailed (fixpoint governor / cancellation); not caching the partial result"
                    );
                    // #210: never serve the partial when a COMPLETE result for IDENTICAL bytes is
                    // already cached. The epoch-exact lookup above missed (a dependency interface
                    // change bumped this file's epoch in the gap), forcing this re-analyze — which
                    // then bailed. The cache only ever stores complete results (a bail is never
                    // cached, this very branch), so any same-`hash` entry is complete. Identical
                    // bytes ⇒ identical tree ⇒ exact same-file byte-derived resolution
                    // (`smallest_typed_containing` etc.); only a cross-file dependency interface
                    // could be stale — strictly better than the partial's missing types / a null
                    // lie. A bounded, documented relaxation of the WP-RD8 epoch-exact key, and
                    // self-healing: the bail is still not cached, so the next call re-attempts.
                    if let Some(complete) = self
                        .analysis_cache
                        .get(key)
                        .filter(|e| e.hash == hash)
                        .map(|e| Rc::clone(&e.value))
                    {
                        tracing::warn!(
                            name = "analyze_bailed_served_cached_complete",
                            file = %path,
                            "serving the cached COMPLETE analysis for identical content (epoch relaxed) instead of the bailed partial"
                        );
                        served_bail_recovery = true;
                        complete
                    } else {
                        result
                    }
                } else {
                    self.analysis_cache.put(
                        key.clone(),
                        CacheEntry {
                            hash,
                            epoch: current_epoch,
                            value: Rc::clone(&result),
                        },
                    );
                    result
                }
            }
        };
        // WP-RD8 postcondition (the self-validating-key analog of the retired `!is_dirty`
        // invariant): the cache entry now serving `path` is stamped with the current epoch, so the
        // next caller hits it iff no dependency has since changed. A future change that let a
        // stale-epoch entry reach a caller fails loudly here rather than silently shipping a wrong
        // cross-file diagnostic (never lie).
        debug_assert!(
            result.bailed
                || served_bail_recovery
                || self
                    .analysis_cache
                    .peek(key)
                    .is_some_and(|e| e.epoch == current_epoch),
            "invariant: a completed analyze() must leave {path}'s cached entry stamped with the \
             current epoch (a bailed result is intentionally not cached; a #210 bail-recovery serves \
             a deliberately stale-epoch complete entry without re-stamping it)"
        );
        _span.record("cache_hit", cached_used);
        _span.record("elapsed_us", _start.elapsed().as_micros() as u64);
        _span.record("diagnostics_count", result.diagnostics.len() as u64);
        result
    }

    /// Build the [`AutoloadEnv`] the [`WorkspaceXFileQuery`] consumes (M11 Phase 4), mirroring Godot's
    /// autoload arm (`gdscript_analyzer.cpp:4570-4609`) via [`ProjectModel::autoload_typing`]:
    ///
    /// * `script` (name → [`FileId`](gd_project::FileId)): autoloads with a backing GDScript — a
    ///   direct `.gd`, a `uid://`→`.gd`, OR a scene whose resolved root attaches an indexed `.gd`.
    ///   Drives `autoload_file` → precise Script-instance typing (the #19 path).
    /// * `native` (name → `"Node"`): SCENE autoloads with no backing script. Drives
    ///   `autoload_native_type` → the bare-`Node` floor.
    /// * `names` (every configured autoload, resolved or not): drives `is_autoload`, suppressing the
    ///   "Identifier not declared" fallthrough for an unresolvable autoload (no false positive).
    ///
    /// Built per-call (cheap against a full analyze) rather than cached, so the maps are always
    /// consistent with the current index/scene snapshot — no stale-map class (e.g. an autoload's
    /// script/scene indexed after project load). A scene→root-`.gd` whose script isn't indexed *yet*,
    /// or a `uid://` that doesn't dereference, is silently skipped from `script`/`native` and degrades
    /// to the prior generic typing. `&ProjectModel`/`&SceneIndex` are passed explicitly (not via
    /// `&self`) so a caller already borrowing `&self.scenes` immutably can reuse that borrow.
    fn build_autoload_maps(&self, project: &ProjectModel, scenes: &SceneIndex) -> AutoloadEnv {
        let mut env = AutoloadEnv::default();
        for a in &project.autoloads {
            // Godot gates the autoload typing arm on `is_singleton` (gdscript_analyzer.cpp:4572): a
            // non-`*` autoload is registered but NOT a global singleton, so a bare reference is
            // "Identifier not declared". Skip it entirely — it must not seed `names` (which would
            // wrongly suppress that diagnostic via `is_autoload`) nor `script`/`native` (typing).
            if !a.is_singleton {
                continue;
            }
            // EVERY singleton autoload name, resolved or not (the `is_autoload` membership set).
            env.names.insert(a.name.clone());
            match project.autoload_typing(&a.name, scenes) {
                Some(gd_project::AutoloadTyping::Script(path)) => {
                    // Skip (degrade) if the backing script isn't indexed yet — no entry in `script`.
                    if let Some(fid) = self.index.resolve_res_path(&path) {
                        env.script.insert(a.name.clone(), fid);
                    }
                }
                Some(gd_project::AutoloadTyping::NativeNode) => {
                    env.native.insert(a.name.clone(), "Node".to_owned());
                }
                None => {}
            }
        }
        env
    }

    /// Analyze a tree/text **without reading or writing either cache** — the M10 (#75) codeAction
    /// mutation gate's probe. The mutating warning quickfixes apply their candidate edit to an
    /// in-memory copy of the buffer and re-analyze it to confirm the edit introduces no new ERROR
    /// (broken code) before OFFERING the fix; that probe must NOT pollute the real file's cached
    /// analysis (which concurrent readers serve) nor read a stale cache entry.
    ///
    /// `path` is the REAL on-disk path (so `file_id` resolves and the cross-file / native-class /
    /// autoload environment is identical to a normal analyze — node-ness, `extends`, autoload typing
    /// all resolve correctly), but the result is returned by value and never cached. Same xfile-query
    /// construction as the cache-miss arm of [`Self::analyze_with_options`]; the only difference is the
    /// missing cache get/put. Cheap to call once per candidate fix (a mutating-fix lightbulb is an
    /// acceptable place to pay an analyze).
    pub fn analyze_ephemeral(&self, path: &Utf8Path, tree: &ParseTree) -> AnalysisResult {
        let file = self.index.file_id(path);
        let script_path = path.file_name().unwrap_or_default();
        let autoloads = self.build_autoload_maps(&self.project, &self.scenes);
        let xfile = WorkspaceXFileQuery::new(
            &self.index,
            &self.native,
            &self.analysis_cache,
            autoloads,
            &self.scenes,
            &self.project.root,
        );
        gd_analyze::analyze_with_options(
            tree,
            file,
            script_path,
            &self.native,
            &xfile,
            &self.policy,
            gd_analyze::AnalyzeOptions {
                dialect: self.dialect,
                ..Default::default()
            },
        )
    }

    /// M11 follow-up (#125) — the scene-precise fact for a `$`/`%`/`get_node("…")` access made by
    /// the script at `path`, for NAVIGATION only (hover / definition / typeDefinition; see
    /// [`crate::scene_nav`]).
    ///
    /// This reads the same seam the analyzer deliberately leaves dormant: `reduce_get_node` types a
    /// valid `$`/`%` as bare `NATIVE Node` (faithful to Godot), because a scene-precise type in the
    /// DIAGNOSTIC path would reject the sibling downcasts Godot tolerates (`docs/02` §11). The
    /// precise fact is safe on the read-only surfaces, which run no compatibility check.
    ///
    /// CONSERVATIVE: `None` unless every scene attaching this script resolves the access to the same
    /// target (the [`WorkspaceXFileQuery::scene_node_facts`] contract).
    #[must_use]
    pub fn scene_node_facts(
        &self,
        path: &Utf8Path,
        query: &gd_analyze::NodePathQuery,
    ) -> Option<gd_analyze::SceneNodeFacts> {
        use gd_analyze::CrossFileQuery as _;
        let file = self.index.file_id(path)?;
        let autoloads = self.build_autoload_maps(&self.project, &self.scenes);
        let xfile = WorkspaceXFileQuery::new(
            &self.index,
            &self.native,
            &self.analysis_cache,
            autoloads,
            &self.scenes,
            &self.project.root,
        );
        xfile.scene_node_facts(file, query)
    }

    /// Drop a URI's cached parse and analysis (on `didClose`).
    pub fn forget(&mut self, key: &CanonicalKey) {
        // `LruCache::pop` is the spelling for "remove by key, return the removed value"; it
        // matches `HashMap::remove`'s semantics. We ignore the return — both caches just drop
        // the contained `Rc`s.
        self.parse_cache.pop(key);
        self.analysis_cache.pop(key);
    }

    /// M5 WP-H1 Soft-pressure action: drop the LRU-oldest half of both caches in one pass.
    /// Returns the total number of entries evicted across both caches so the server can record
    /// it as a structured-trace field on the `memory_soft_cap_evicted` event.
    ///
    /// "Half" is `len / 2`, taken before any eviction, so the evicted count is independent of
    /// the post-eviction state and `evict_half()` on an empty cache is a no-op. Choosing
    /// `len / 2` over a fixed fraction keeps the policy proportional to whatever has accumulated.
    /// Note the shed fires once per *transition into* Soft, not once per held tick: the ticker is
    /// transition-gated (`react_to_memory_pressure` early-returns when the level is unchanged), so
    /// a session that simply sits at Soft does not re-shed every tick — between transitions the
    /// LRU's own eviction-on-insert (at the configured cache capacity, default 512) bounds further
    /// growth.
    ///
    /// `pop_lru` is `lru`'s direct primitive for "remove the least-recently-used entry". Iterating
    /// via `iter()` would visit MRU-first (the documented order) and would require a side buffer
    /// of keys to pop, since `iter()` holds a `&` borrow. The repeated `pop_lru()` is therefore
    /// also the cheapest spelling.
    pub fn evict_half(&mut self) -> usize {
        let parse_drop = self.parse_cache.len() / 2;
        let analysis_drop = self.analysis_cache.len() / 2;
        for _ in 0..parse_drop {
            self.parse_cache.pop_lru();
        }
        for _ in 0..analysis_drop {
            self.analysis_cache.pop_lru();
        }
        parse_drop + analysis_drop
    }

    /// Observability hook for the WP-H1 ticker + the `memory_pressure` integration tests:
    /// `(parse_len, analysis_len)`. Read-only — the consumer cannot mutate the caches through
    /// this surface, so it's safe to expose at `pub` (the underlying fields stay `pub(crate)`).
    pub fn cache_lens(&self) -> (usize, usize) {
        (self.parse_cache.len(), self.analysis_cache.len())
    }

    /// Test hook: directly insert a synthetic entry into the parse cache. Used by the
    /// `memory_pressure` integration test to stuff the cache without driving real parses. The
    /// `#[cfg(any(test, debug_assertions))]` gate keeps this out of release builds (it is still
    /// compiled into a debug `gdls` binary, but never into a release artifact).
    #[cfg(any(test, debug_assertions))]
    pub fn debug_insert_parse_entry(&mut self, key: CanonicalKey, hash: u64, value: ParseResult) {
        self.parse_cache.put(
            key,
            CacheEntry {
                hash,
                epoch: 0,
                value: Rc::new(value),
            },
        );
    }

    /// See [`Self::debug_insert_parse_entry`] — sibling for the analysis cache.
    #[cfg(any(test, debug_assertions))]
    pub fn debug_insert_analysis_entry(
        &mut self,
        key: CanonicalKey,
        hash: u64,
        value: AnalysisResult,
    ) {
        self.analysis_cache.put(
            key,
            CacheEntry {
                hash,
                epoch: 0,
                value: Rc::new(value),
            },
        );
    }

    /// Re-index a file from a fresh parse tree (an open buffer's current contents, or disk).
    /// Funnels through [`Index::txn`](gd_project::Index::txn) so every mutation is verified post-apply.
    ///
    /// Deliberately does **not** evict the parse/analysis cache. Two reasons: (1) on the hot
    /// open-buffer edit path this is called between the `parse` that populated the cache and the
    /// `publishDiagnostics` that consumes it — evicting here would reintroduce the M1 double-parse;
    /// (2) it isn't needed for correctness, because cache validity is content-addressed
    /// ([`CacheEntry`]) and every reader passes current text, so a changed file misses the cache on
    /// its own. Eviction is reserved for [`Self::remove`], which is off the hot path.
    ///
    /// **Does NOT update `stat_table`** — call [`Self::update_stat_from_disk`] after this when the
    /// source is a disk read (watcher, `didClose` reindex). On the buffer path the disk stat is
    /// unchanged (the buffer text hasn't been written to disk), so `stat_table` must NOT be updated,
    /// and the caller must ensure the file's stat_table entry is excluded from [`Self::save_cache`]
    /// via `save_cache_excluding_open` so warm-load re-parses from disk rather than serving the
    /// never-persisted buffer interface as disk truth.
    pub fn reindex(&mut self, path: &Utf8Path, tree: &ParseTree) {
        let iface = gd_project::extract_interface(tree);
        self.index.txn(path, |idx| {
            idx.on_file_changed(path, iface);
        });
    }

    /// Refresh `stat_table` for `path` from its current on-disk metadata. Call this after a
    /// disk-sourced [`Self::reindex`] (watcher, `reindex_from_disk` on close). Keeps the stat
    /// snapshot current so the next warm-load sees the updated stat and skips re-parsing the file
    /// if it hasn't changed again. A stat failure is logged and the entry is removed so the next
    /// reconcile retries defensively.
    ///
    /// **Must NOT be called after a buffer-only [`Self::reindex`]**: the buffer text hasn't hit
    /// disk, so the disk stat is stale relative to the buffer interface now live in the index.
    /// Calling this on a buffer reindex would mark the file "up to date" with disk when it isn't,
    /// causing warm-load to serve the old disk interface — the "never lie" violation. The buffer
    /// path uses [`Self::save_cache_excluding_open`] instead to ensure those files are re-parsed
    /// fresh on the next launch.
    pub fn update_stat_from_disk(&mut self, path: &Utf8Path) {
        match std::fs::metadata(path.as_std_path()) {
            Ok(meta) => {
                let stat = cache::stat_from_metadata(path.to_path_buf(), &meta);
                self.stat_table.insert(path.to_path_buf(), stat);
            }
            Err(e) => {
                log::debug!(
                    "update_stat_from_disk: could not stat {path}, removing from table: {e}"
                );
                self.stat_table.remove(path);
            }
        }
    }

    /// Drop a deleted file from the index. Wrapped in [`Index::txn`](gd_project::Index::txn) for invariant verification.
    /// Also evicts any cached parse/analysis for the path: a deleted file can never be re-read to
    /// produce fresh text, so its content-fingerprint check can't fire — without eviction the stale
    /// entry would linger for the session. `remove` is only called off the hot edit path (watcher
    /// delete, `didClose` of a vanished file), so eviction here can't trigger a double-parse.
    pub fn remove(&mut self, path: &Utf8Path) {
        // M7 (#60): a deleted file's dedupe record must not suppress a future re-create with
        // identical content.
        self.last_applied_disk.pop(&normalize_path_buf(path));
        self.index.txn(path, |idx| {
            idx.on_file_removed(path);
        });
        if let Some(key) = CanonicalKey::for_path(path) {
            self.forget(&key);
        }
        // Drop the stat entry: the file is gone, so the next warm-load must not see a stale entry
        // that would skip re-parsing a file that has since been recreated with the same name.
        self.stat_table.remove(path);
    }

    /// M11 (#76): the project's `.tscn` scene index (read-only). Phase 2 scene typing and any
    /// scene-aware LSP query reads through here; Phase 1 exposes it so tests can assert the index
    /// stays live across watcher events.
    #[must_use]
    pub fn scenes(&self) -> &SceneIndex {
        &self.scenes
    }

    /// #127: the project's arbitrary-asset index (read-only). `load`/`preload` path completion reads
    /// through here to list non-script/non-scene project files (textures, audio, `.tres`, …)
    /// alongside scripts and scenes. Exposed so tests can assert the index stays live across watcher
    /// events.
    #[must_use]
    pub fn assets(&self) -> &AssetIndex {
        &self.assets
    }

    /// M11 (#76): re-index a `.tscn` scene from disk into the [`SceneIndex`] (watcher-driven). The
    /// scene is keyed by its `res://` path; reading/parsing failures are logged and skipped (never
    /// crash). The stat table is refreshed so the next warm-load can skip an unchanged scene. Phase
    /// 1: this keeps the scene index live but does NOT re-diagnose attached scripts (no analyzer
    /// consumption yet).
    pub fn reindex_scene(&mut self, path: &Utf8Path) {
        let Some(res) = self.project.path_to_res(path) else {
            log::debug!("reindex_scene: {path} is not under the project root; skipping");
            return;
        };
        match std::fs::read_to_string(path.as_std_path()) {
            Ok(text) => {
                self.scenes.reindex(&res, &text);
                self.update_stat_from_disk(path);
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                // Vanished between event and read (transient delete / cross-mount rename source).
                self.remove_scene(path);
            }
            Err(e) => log::warn!("reindex_scene: cannot read {path}: {e}; keeping prior scene"),
        }
    }

    /// M11 (#76): drop a deleted `.tscn` from the [`SceneIndex`] and its stat entry.
    pub fn remove_scene(&mut self, path: &Utf8Path) {
        if let Some(res) = self.project.path_to_res(path) {
            self.scenes.remove(&res);
        }
        self.stat_table.remove(path);
    }

    /// #127: record an arbitrary asset (a non-script/non-scene project file) in the [`AssetIndex`]
    /// from a watcher Created/Modified event. An asset has no content to parse — only its `res://`
    /// path is indexed — so this is bound-to-root, computes the path, inserts it, and refreshes the
    /// stat entry so the next warm-load can skip the unchanged file. A path outside the project root
    /// is skipped (never crash). Does NOT re-diagnose anything (an asset path can't change a script's
    /// diagnostics).
    pub fn reindex_asset(&mut self, path: &Utf8Path) {
        let Some(res) = self.project.path_to_res(path) else {
            log::debug!("reindex_asset: {path} is not under the project root; skipping");
            return;
        };
        self.assets.insert(res);
        self.update_stat_from_disk(path);
    }

    /// #447: a `<resource>.uid` sidecar was created or edited — re-read it and re-point the
    /// resource's uid in both the project model and the index, so `preload("uid://…")` follows the
    /// change without waiting for a restart. An unreadable or non-`uid://` body drops the mapping
    /// rather than keeping a stale one.
    pub fn sync_uid_sidecar(&mut self, sidecar: &Utf8Path) {
        let Some(resource) = sidecar.as_str().strip_suffix(".uid") else {
            return;
        };
        let uid = std::fs::read_to_string(sidecar)
            .ok()
            .map(|text| text.trim().to_owned())
            .filter(|uid| uid.starts_with("uid://"));
        self.apply_uid_sidecar(Utf8Path::new(resource), uid.as_deref());
    }

    /// #447: a `.uid` sidecar was deleted — its resource has no uid any more.
    pub fn drop_uid_sidecar(&mut self, sidecar: &Utf8Path) {
        if let Some(resource) = sidecar.as_str().strip_suffix(".uid") {
            self.apply_uid_sidecar(Utf8Path::new(resource), None);
        }
    }

    /// The shared half of the two calls above: translate the resource to `res://` and push the
    /// mapping into the project model and the index together, so the two never disagree.
    fn apply_uid_sidecar(&mut self, resource: &Utf8Path, uid: Option<&str>) {
        let Some(res) = self.project.path_to_res(resource) else {
            log::debug!("uid sidecar for {resource} is not under the project root; skipping");
            return;
        };
        // The uid this resource used to answer to. On a re-point, the scenes that named the OLD
        // uid must re-resolve too — they now point at nothing, and leaving them on the stale target
        // would be the one outcome worse than degrading.
        let previous_uid: Option<String> = self
            .project
            .uids
            .iter()
            .find(|(_, target)| *target == &res)
            .map(|(u, _)| u.clone());
        self.project.uids.retain(|_, target| target != &res);
        if let Some(uid) = uid {
            self.project.uids.insert(uid.to_owned(), res.clone());
        }
        let owned = res.clone();
        let uid_owned = uid.map(str::to_owned);
        {
            let uid_owned = uid_owned.clone();
            self.index.txn(resource, move |idx| {
                idx.sync_uid_for_resource(&owned, uid_owned.as_deref());
            });
        }

        // #484: the scene index resolves a `path`-less `[ext_resource]` through the same map, so it
        // has to move in lockstep — the two maps disagreeing is how a scene ends up pointing at a
        // file the project no longer declares.
        self.refresh_uid_scenes(previous_uid.as_deref(), uid_owned.as_deref());
    }

    /// Re-read every scene that names either uid with no `path`, after the uid map has changed.
    /// Covers all four sidecar events: a uid that appeared resolves those scenes for the first
    /// time, one that was re-pointed re-resolves both sides, and one that was deleted degrades its
    /// scenes back to "no script" rather than leaving them on a stale target. #484.
    fn refresh_uid_scenes(&mut self, old_uid: Option<&str>, new_uid: Option<&str>) {
        self.scenes.set_uid_map(self.project.uids.clone());
        let mut targets: Vec<String> = Vec::new();
        for uid in [old_uid, new_uid].into_iter().flatten() {
            targets.extend(self.scenes.scenes_referencing_uid(uid).map(str::to_owned));
        }
        targets.sort();
        targets.dedup();
        let root = self.project.root.clone();
        for res in targets {
            let Some(path) = gd_project::res_to_path(&root, &res) else {
                continue;
            };
            match std::fs::read_to_string(&path) {
                Ok(text) => self.scenes.reindex(&res, &text),
                Err(e) => log::warn!("scene index: uid re-resolve skipped unreadable {path}: {e}"),
            }
        }
    }

    /// #127: drop a deleted asset from the [`AssetIndex`] and its stat entry (watcher Deleted).
    pub fn remove_asset(&mut self, path: &Utf8Path) {
        if let Some(res) = self.project.path_to_res(path) {
            self.assets.remove(&res);
        }
        self.stat_table.remove(path);
    }

    /// Re-read `project.godot` from disk (file changed via the M4 watcher), rebuild the policy, and
    /// re-load the native DB (the gdextensions list lives in `ProjectModel` and the doc-XML merge
    /// step reads it). Cheaper than a full re-`load`: keeps the index and parse cache.
    ///
    /// Returns `true` when the reload changed the resolved dialect, meaning the caller must do a
    /// full workspace reload rather than trusting anything the index or caches already hold.
    pub fn reload_project_and_native(&mut self, options: &InitializationOptions) -> bool {
        let root = self.project.root.clone();
        let (project, outcome) = ProjectModel::load_checked(&root);
        // WP-RD13: a *present-but-unreadable* project.godot (locked mid-save, permission denied) OR
        // a *corrupt-but-parseable* one (garbled content the tolerant parser accepts as a
        // near-default "clean" parse) must NOT clobber the last good configuration on a transient
        // glitch. Preserve the WHOLE prior state — project model, native DB, AND warning policy —
        // rather than just the policy (the pre-RD13 behaviour rebuilt `self.project` + `self.native`
        // from empty defaults even when it kept the policy, briefly wiping autoloads / gdextensions
        // / UID map and republishing every buffer against an empty model). The watcher fires again
        // once the file is readable/valid. A genuinely absent file is *not* a failure — it rebuilds
        // normally (the legitimate standalone-`.gd` case).
        if outcome.should_preserve_prior() {
            log::warn!(
                "project.godot reload {outcome:?}; keeping the previous project model, native DB, \
                 and warning policy rather than resetting to defaults"
            );
            return false;
        }
        let (dialect, dialect_origin) = resolve_dialect(
            options.dialect(),
            project.declared_engine_version,
            outcome == LoadOutcome::Loaded,
        );
        // A `config/features` edit that moves the project to another Godot version invalidates
        // every parse tree in the session, interfaces in the index included — the two dialects do
        // not parse identically. Report it and let the caller rebuild from scratch rather than
        // patching caches that were derived under the old rules.
        if dialect != self.dialect {
            log::info!(
                "dialect changed {} -> {dialect} on project.godot reload; the workspace must be \
                 rebuilt",
                self.dialect,
            );
            self.dialect = dialect;
            self.dialect_origin = dialect_origin;
            self.project = project;
            return true;
        }
        self.dialect_origin = dialect_origin;
        self.project = project;
        // A `project.godot` reload re-scans the `.uid` sidecars, so hand the fresh map over; every
        // file whose uid target moved is re-resolved and marked dirty by the swap.
        let uids = self.project.uids.clone();
        set_index_uid_map(&mut self.index, &root, uids);
        // #484: the scene index reads the same map, and the whole map may have moved here — so
        // re-resolve every scene that names a uid, not just the two a single sidecar event touches.
        self.scenes.set_uid_map(self.project.uids.clone());
        for res in self.scenes.uid_referencing_scenes() {
            let Some(path) = gd_project::res_to_path(&root, &res) else {
                continue;
            };
            match std::fs::read_to_string(&path) {
                Ok(text) => self.scenes.reindex(&res, &text),
                Err(e) => log::warn!("scene index: uid re-resolve skipped unreadable {path}: {e}"),
            }
        }
        // Mid-session reloads never spawn Godot (no resolution path does since v1.0.2); a
        // `.gdextension` change marks the auto-dump meta stale and the next startup's background
        // dump refreshes it.
        let (native, notice) = load_native(
            options,
            &self.project,
            &root,
            self.dialect,
            self.dialect_origin,
        );
        self.native_release_notice = notice;
        // No content-hash dedupe here: a doc-XML merge changes the DB without changing the dump
        // text the hash covers, and this path IS the doc-XML/gdextension reaction.
        self.adopt_native(native, false);
        self.policy = WarnPolicy::build(
            &self.project.warnings,
            &strict_settings(&options.strict),
            self.dialect,
        );
        self.analysis_cache.clear();
        self.analysis_generation += 1;
        false
    }

    /// Re-load only the native DB (extension_api.json + every installed gdextension's doc XML).
    /// Returns whether the live DB actually changed — callers republish + re-save the warm cache
    /// only on `true`. Never spawns Godot — see [`Self::reload_project_and_native`].
    pub fn reload_native(&mut self, options: &InitializationOptions) -> bool {
        let root = self.project.root.clone();
        let (native, notice) = load_native(
            options,
            &self.project,
            &root,
            self.dialect,
            self.dialect_origin,
        );
        self.native_release_notice = notice;
        self.adopt_native(native, true)
    }

    /// Install a freshly-resolved native DB, with two session-stability rules (issue #25):
    ///
    /// 1. **Never downgrade.** A mid-session resolution that comes back strictly worse — empty
    ///    where the live DB is populated, or non-`Exact` where the live DB is `Exact` — is a
    ///    transient artifact (a torn read of a mid-write dump, a momentarily-missing file), not
    ///    a real change. Keep the live DB; a genuine source change re-fires the watcher once the
    ///    file is whole, and a restart re-resolves from disk anyway.
    /// 2. **Dedupe by content** (only when `dedupe_by_hash` — the extension_api-triggered path,
    ///    where the doc-XML merge inputs are unchanged): the post-adoption watcher echo of the
    ///    dump file re-resolves to byte-identical content; skip the cache flush + republish.
    ///
    /// Returns whether `self.native` changed (callers gate republish on it). Clears the
    /// analysis cache on adoption so types pick up the new native lattice.
    fn adopt_native(&mut self, new: NativeDb, dedupe_by_hash: bool) -> bool {
        use gd_types::ApiProvenance;
        let downgrade = (new.is_empty() && !self.native.is_empty())
            || (self.native.provenance() == ApiProvenance::Exact
                && new.provenance() != ApiProvenance::Exact);
        if downgrade {
            log::warn!(
                "native API: reload resolved a strictly worse source ({} classes, {:?} \
                 provenance); keeping the live DB ({} classes, {:?})",
                new.class_count(),
                new.provenance(),
                self.native.class_count(),
                self.native.provenance(),
            );
            return false;
        }
        if dedupe_by_hash
            && new.content_hash() == self.native.content_hash()
            && new.provenance() == self.native.provenance()
        {
            log::debug!("native API: reload resolved identical content; nothing to do");
            return false;
        }
        self.native = new;
        self.analysis_cache.clear();
        self.analysis_generation += 1;
        true
    }

    /// Walk the project root for every `.gd` and reconcile the live index against the disk: add
    /// missing files, modify interface-changed files, drop deleted files. Idempotent — safe to
    /// re-run (the M4 watcher fires it again on a `need_rescan`/overflow flag). Logged as
    /// `cold_index_reconciled{added, modified, removed, walked, walk_errors, skipped_unreadable,
    /// skipped_non_utf8}` via `log::info!` (M5 swaps to `tracing` spans; the marker line stays).
    ///
    /// `open_paths` is the set of files the editor currently has open (normalized like the index
    /// keys). The open buffer is the source of truth over disk (docs/01, `vfs.rs`), so a file in
    /// this set is skipped by **both** the disk-reindex pass (its in-index interface, set from the
    /// buffer by `reindex_open_buffer`, is authoritative) and the removal pass (a transiently
    /// deleted-on-disk open file — `git stash`, atomic save — must not be dropped). At startup and
    /// in `gdls diagnose` the set is empty.
    ///
    /// Doesn't drop the parse or analysis caches: any open URI's cached analysis is still
    /// authoritative for that buffer, and a watcher-driven reindex flowing through `Index::on_file_changed`
    /// already marks dependents dirty in `Index.dirty`. Callers republish via `Index::take_dirty()`
    /// after `reconcile()` to refresh diagnostics for any open URI whose interface dependents shifted.
    ///
    /// Safety against transient FS errors: `WalkDir` errors (permission denied, vanished
    /// mid-walk, symlink loops, non-UTF-8 paths) are counted and logged but never abort the
    /// walk. When any such error occurs, the "removed = anything in the index but not in the
    /// walk" pass is **skipped** — a permission glitch on `.godot/` must not wipe every
    /// indexed file from the index. Operators see the skip as a `walk_errors` count in the
    /// summary line so they can investigate.
    pub fn reconcile(&mut self, open_paths: &FxHashSet<Utf8PathBuf>) -> ReconciliationReport {
        self.reconcile_with(ReconcileMode::FullStat, open_paths)
    }

    /// [`Self::reconcile`] with an explicit [`ReconcileMode`]. `FullStat` is the historical
    /// behavior (stat-diff every walked file); `DiscoverOnly` is the startup backstop when a
    /// live watcher is armed BEFORE the workspace loads: the load's own stat pass already
    /// validated every known file and any modification since lands as a queued watcher event,
    /// so the only job left is discovering files ADDED or REMOVED outside both — enumeration
    /// plus stat/parse for unknown paths only. On NTFS at 2.3k files that turns the 7–9 s
    /// re-stat walk into a sub-second directory enumeration (issue #14).
    pub fn reconcile_with(
        &mut self,
        mode: ReconcileMode,
        open_paths: &FxHashSet<Utf8PathBuf>,
    ) -> ReconciliationReport {
        self.reconcile_with_progress(mode, open_paths, &mut crate::progress::NoopSink)
    }

    /// [`Self::reconcile_with`] reporting per-file walk progress into `sink` (M7 #58) —
    /// indeterminate (the walk streams; no total up front), throttled by the reporter.
    pub(crate) fn reconcile_with_progress(
        &mut self,
        mode: ReconcileMode,
        open_paths: &FxHashSet<Utf8PathBuf>,
        sink: &mut dyn crate::progress::ProgressSink,
    ) -> ReconciliationReport {
        // M5 WP-O1: reconcile span. Both the cold-start post-load reconcile and the watcher's
        // `need_rescan` overflow path flow through here. The 6 counters in the on-close fields
        // mirror the `cold_index_reconciled` marker line below — the marker line stays for
        // log-grep compatibility, the recorded fields give structured-trace consumers something
        // to facet on without parsing the marker string.
        let _start = std::time::Instant::now();
        let _span = tracing::info_span!(
            "reconcile",
            open_paths_len = open_paths.len() as u64,
            added = tracing::field::Empty,
            modified = tracing::field::Empty,
            removed = tracing::field::Empty,
            walked = tracing::field::Empty,
            walk_errors = tracing::field::Empty,
            elapsed_us = tracing::field::Empty,
        );
        let _enter = _span.enter();
        let root = self.project.root.clone();
        let mut walked = 0usize;
        let mut added = 0usize;
        let mut modified = 0usize;
        let mut walk_errors = 0usize;
        let mut skipped_unreadable = 0usize;
        let mut skipped_non_utf8 = 0usize;
        let mut walked_paths: FxHashSet<Utf8PathBuf> = FxHashSet::default();
        // M11 (#76): scenes get the same stat-diff treatment as scripts on this path. Without it,
        // the watcher-overflow (`need_rescan`) and disabled-watcher liveness-tick recovery paths —
        // both routed through this reconcile — would recover drifted `.gd` but leave `.tscn` stale
        // (the scene index has no other backstop on those paths; cold/warm/per-event all handle it).
        let mut walked_scene_paths: FxHashSet<Utf8PathBuf> = FxHashSet::default();
        // #127: assets reconciled on this path too (watcher-overflow / disabled-watcher recovery),
        // so a drifted asset add/delete is caught alongside scripts and scenes. Separate set since
        // asset keys aren't `FileId`s (like `walked_scene_paths`).
        let mut walked_asset_paths: FxHashSet<Utf8PathBuf> = FxHashSet::default();

        let walker = WalkDir::new(root.as_std_path())
            .into_iter()
            .filter_entry(|e| {
                // Skip excluded directories *before* descending — saves walking .godot/, etc.
                if let Some(p) = camino::Utf8Path::from_path(e.path()) {
                    !is_excluded(p, &root)
                } else {
                    true
                }
            });

        // Inspect each WalkDir result explicitly — `.filter_map(Result::ok)` would silently
        // drop permission-denied / I/O / symlink-loop errors, and the downstream "removed"
        // pass would then delete every unreachable file from the index. Track errors so we
        // can suppress that pass when the walk wasn't authoritative.
        for entry_result in walker {
            let entry = match entry_result {
                Ok(e) => e,
                Err(e) => {
                    let path_display = e
                        .path()
                        .map(|p| p.display().to_string())
                        .unwrap_or_else(|| "<unknown>".to_string());
                    log::warn!("reconcile: walk error at {path_display}: {e}");
                    walk_errors += 1;
                    continue;
                }
            };
            if !entry.file_type().is_file() {
                continue;
            }
            let Some(p) = camino::Utf8Path::from_path(entry.path()) else {
                // Non-UTF-8 path — surface a count so files don't silently vanish from the
                // "removed" computation simply because the walk couldn't name them.
                skipped_non_utf8 += 1;
                log::warn!(
                    "reconcile: skipping non-UTF-8 path under {root}; this file will not be \
                     considered for the removed-files pass"
                );
                continue;
            };
            // M11 (#76): a `.tscn` is reconciled into the scene index (never the script interner).
            // Same stat-diff shape as scripts: re-parse only when (size, mtime_ns) changed vs the
            // stat table. The stat table is shared (scenes were added to it at cold/warm build).
            if gd_project::is_scene_path(p) {
                let path = gd_project::normalize_path(p);
                walked += 1;
                walked_scene_paths.insert(path.clone());
                sink.progress(walked, None, "reconciling scenes");
                let new_stat = entry
                    .metadata()
                    .ok()
                    .map(|m| cache::stat_from_metadata(path.clone(), &m));
                let stat_changed = match (self.stat_table.get(&path), &new_stat) {
                    (None, _) => true,
                    (Some(_), None) => true,
                    (Some(old), Some(new)) => old.size != new.size || old.mtime_ns != new.mtime_ns,
                };
                if stat_changed {
                    // `reindex_scene` reads the file, updates the scene index, and refreshes the
                    // stat entry; on a vanished file it routes to `remove_scene` (never crash).
                    self.reindex_scene(&path);
                }
                continue;
            }
            // #127: a non-`.gd`/non-`.tscn` file is an arbitrary asset — reconcile its res:// path
            // into the asset index (path only, no parse), same stat-diff shape. Done before the
            // `.gd` gate so only scripts fall through to the interner below.
            if p.extension() != Some("gd") {
                let path = gd_project::normalize_path(p);
                walked += 1;
                walked_asset_paths.insert(path.clone());
                sink.progress(walked, None, "reconciling assets");
                let new_stat = entry
                    .metadata()
                    .ok()
                    .map(|m| cache::stat_from_metadata(path.clone(), &m));
                let stat_changed = match (self.stat_table.get(&path), &new_stat) {
                    (None, _) => true,
                    (Some(_), None) => true,
                    (Some(old), Some(new)) => old.size != new.size || old.mtime_ns != new.mtime_ns,
                };
                if stat_changed {
                    // `reindex_asset` inserts the res path and refreshes the stat entry.
                    self.reindex_asset(&path);
                }
                continue;
            }
            // Normalize the way Index keys do (the shared `gd_project::normalize_path`) so the
            // `walked_paths.contains(...)` check at the end against `Index::path(fid)` succeeds.
            let path = gd_project::normalize_path(p);
            walked += 1;
            walked_paths.insert(path.clone());
            sink.progress(walked, None, "reconciling scripts");

            // Open buffer wins over disk (docs/01, `vfs.rs`): skip the disk-driven reindex for a
            // file the editor has open. It stays in `walked_paths` (above) so the removal pass
            // won't drop it either, and its authoritative interface is already live in the index
            // from `reindex_open_buffer`. Skipping before the read also avoids the wasted parse.
            if open_paths.contains(&path) {
                continue;
            }

            // DiscoverOnly: a path already in the stat table was validated by the load's own
            // stat pass moments ago, and any modification since is a queued watcher event —
            // skip the per-file stat entirely. Unknown paths (added while the server was off)
            // fall through to the full stat + parse below.
            if mode == ReconcileMode::DiscoverOnly && self.stat_table.contains_key(&path) {
                continue;
            }

            // Stat-based change detection: compare (size, mtime_ns) against the stored table.
            // If stat fails (vanished mid-walk, permission), fall back to re-parsing so we don't
            // silently drop an added file or leave a stale interface. The stat comes from the
            // walk entry — free on Windows (populated by directory enumeration, issue #14), the
            // same single stat on unix.
            let new_stat = entry
                .metadata()
                .ok()
                .map(|m| cache::stat_from_metadata(path.clone(), &m));

            let in_table = self.stat_table.get(&path);
            let stat_changed = match (in_table, &new_stat) {
                (None, _) => true,       // added — not in table
                (Some(_), None) => true, // stat failed — re-parse defensively
                (Some(old), Some(new)) => old.size != new.size || old.mtime_ns != new.mtime_ns,
            };

            if !stat_changed {
                // Stat unchanged — trust the cached interface, skip re-parse.
                continue;
            }

            // Stat changed (or new file): re-read and re-parse.
            let text = match std::fs::read_to_string(&path) {
                Ok(t) => t,
                Err(e) => {
                    log::warn!("reconcile: skipping unreadable {path}: {e}");
                    skipped_unreadable += 1;
                    continue;
                }
            };
            let tree = parse_in_dialect(&text, self.dialect).tree;
            let new_iface = gd_project::extract_interface(&tree);
            let is_added = self.index.interface_of(&path).is_none();
            self.index.txn(&path, |idx| {
                idx.on_file_changed(&path, new_iface);
            });
            // Update stat table with the fresh stat (or remove entry if stat failed so next
            // reconcile retries).
            if let Some(s) = new_stat {
                self.stat_table.insert(path.clone(), s);
            } else {
                self.stat_table.remove(&path);
            }
            if is_added {
                added += 1;
            } else {
                modified += 1;
            }
        }

        // Removed = anything in the index whose path didn't show up in the walk.
        // SAFETY: when the walk had errors (permission, I/O, non-UTF-8), `walked_paths` is
        // incomplete by definition — files exist on disk that we just couldn't enumerate.
        // Skip the removal pass entirely in that case rather than phantom-deleting live
        // files. Operators see the skip in the summary line and can investigate.
        let removed = if walk_errors > 0 || skipped_non_utf8 > 0 {
            log::warn!(
                "reconcile: skipping removal pass (walk_errors={walk_errors}, \
                 skipped_non_utf8={skipped_non_utf8}); the walk was not authoritative"
            );
            0
        } else {
            let removed_paths: Vec<Utf8PathBuf> = self
                .index
                .iter_interfaces()
                .filter_map(|(fid, _)| self.index.path(fid).map(camino::Utf8Path::to_path_buf))
                // Never drop an open buffer: a file the editor has open whose on-disk copy vanished
                // (git stash, atomic save mid-walk) is still authoritative from its buffer.
                .filter(|p| !walked_paths.contains(p) && !open_paths.contains(p))
                .collect();
            let n = removed_paths.len();
            for path in &removed_paths {
                self.index.txn(path, |idx| idx.on_file_removed(path));
                self.stat_table.remove(path);
            }
            // M11 (#76): parallel scene removal pass — a `.tscn` in the index but absent from the
            // walk was deleted while the watcher was off/overflowed. Scene keys are res:// paths;
            // map each back to its absolute path to test against the walked-scene set. Guarded by
            // the same authoritative-walk check as scripts (this whole branch).
            let removed_scenes: Vec<String> = self
                .scenes
                .iter()
                .map(|(res, _)| res.to_owned())
                .filter(|res| {
                    gd_project::res_to_path(&root, res)
                        .map(|abs| gd_project::normalize_path(&abs))
                        .is_none_or(|abs| {
                            !walked_scene_paths.contains(&abs) && !open_paths.contains(&abs)
                        })
                })
                .collect();
            for res in &removed_scenes {
                self.scenes.remove(res);
                if let Some(abs) = gd_project::res_to_path(&root, res) {
                    self.stat_table.remove(&gd_project::normalize_path(&abs));
                }
            }
            // #127: parallel asset removal pass — an asset in the index but absent from the walk
            // was deleted while the watcher was off/overflowed. Same res→abs mapping + authoritative
            // -walk guard (this whole branch). Open buffers don't apply (assets aren't editor docs).
            let removed_assets: Vec<String> = self
                .assets
                .iter()
                .map(str::to_owned)
                .filter(|res| {
                    gd_project::res_to_path(&root, res)
                        .map(|abs| gd_project::normalize_path(&abs))
                        .is_none_or(|abs| !walked_asset_paths.contains(&abs))
                })
                .collect();
            for res in &removed_assets {
                self.assets.remove(res);
                if let Some(abs) = gd_project::res_to_path(&root, res) {
                    self.stat_table.remove(&gd_project::normalize_path(&abs));
                }
            }
            n
        };

        // M5 WP-O1 — preserved verbatim marker (operators & log-greppers depend on this exact
        // label; the trailing `mode=` field is additive). Migrated from `log::info!` to
        // `tracing::info!` so the event is attached to the surrounding `reconcile` span instead
        // of arriving at root scope.
        let mode_label = match mode {
            ReconcileMode::FullStat => "full",
            ReconcileMode::DiscoverOnly => "discover",
        };
        tracing::info!(
            "cold_index_reconciled added={added} modified={modified} removed={removed} \
             walked={walked} walk_errors={walk_errors} skipped_unreadable={skipped_unreadable} \
             skipped_non_utf8={skipped_non_utf8} mode={mode_label}"
        );
        _span.record("added", added as u64);
        _span.record("modified", modified as u64);
        _span.record("removed", removed as u64);
        _span.record("walked", walked as u64);
        _span.record("walk_errors", walk_errors as u64);
        _span.record("elapsed_us", _start.elapsed().as_micros() as u64);

        ReconciliationReport {
            added,
            modified,
            removed,
            walked,
            walk_errors,
            skipped_unreadable,
            skipped_non_utf8,
        }
    }
}

/// How [`Workspace::reconcile_with`] treats files already known to the stat table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReconcileMode {
    /// Stat-diff every walked file (the historical behavior). The watcher `need_rescan` overflow
    /// path, the watcher-disabled fallback tick, and `gdls diagnose --reconcile` need this —
    /// they run when freshness has genuinely degraded.
    FullStat,
    /// Enumeration-only for known paths: stat + parse only files absent from the stat table
    /// (added), plus the standard removal pass. Sound only when a live watcher was armed before
    /// the workspace loaded (modifications in the gap are queued events).
    DiscoverOnly,
}

/// Outcome of a [`Workspace::reconcile`] pass. Used by `gdls diagnose --reconcile` (WP-T3) and by
/// watcher event handling on the `need_rescan` overflow flag (WP-W3).
#[derive(Debug, Clone, Copy, Default)]
pub struct ReconciliationReport {
    pub added: usize,
    pub modified: usize,
    pub removed: usize,
    pub walked: usize,
    /// WalkDir per-entry errors (permission denied, vanished mid-walk, symlink loops, etc.).
    /// When nonzero, [`ReconciliationReport::removed`] is forced to 0 — see [`Workspace::reconcile`].
    pub walk_errors: usize,
    /// Files the walk reached but couldn't `read_to_string` (locked, perms, non-UTF-8 content).
    /// Their interfaces stay last-known in the index.
    pub skipped_unreadable: usize,
    /// Filesystem entries with non-UTF-8 paths. Disables the removal pass for the same reason
    /// `walk_errors` does — the walk wasn't authoritative.
    pub skipped_non_utf8: usize,
}

impl ReconciliationReport {
    /// Whether the walk hit any error that should be surfaced to operators (driving the
    /// nonzero-exit path of `gdls diagnose --reconcile`).
    pub fn had_errors(&self) -> bool {
        self.walk_errors > 0 || self.skipped_unreadable > 0 || self.skipped_non_utf8 > 0
    }
}

// ---------------------------------------------------------------------------
// Cache helpers (warm-start support).
// ---------------------------------------------------------------------------

/// Build the cache key from the live native DB and project root. Used by both `load` (warm-start
/// attempt) and `save_cache` (persist after build/reconcile) so the key construction is
/// identical — a divergent key is the silent always-cold failure mode.
/// Parse under an explicit dialect. The single funnel for every production parse in `gd_server`,
/// so a new call site cannot silently fall back to [`Dialect::DEFAULT`] and index a project under
/// the wrong Godot version.
pub(crate) fn parse_in_dialect(text: &str, dialect: Dialect) -> ParseResult {
    gd_syntax::parse_with_options(
        text,
        &gd_syntax::ParseOptions {
            dialect,
            ..Default::default()
        },
    )
}

fn build_cache_key(native: &NativeDb, root: &Utf8Path, dialect: Dialect) -> cache::CacheKey {
    cache::CacheKey {
        cache_format_version: cache::CACHE_FORMAT_VERSION,
        gdls_version: env!("CARGO_PKG_VERSION").to_string(),
        native_db_content_hash: native.content_hash(),
        project_godot_fingerprint: cache::project_godot_fingerprint(root),
        dialect: dialect as u8,
    }
}

/// Walk the project root, stat-diff each `.gd` against the cached table, re-parse only changed
/// files, and return the updated index + new stat table. This is the warm-start path — it avoids
/// re-reading every file, only touching files whose `(size, mtime_ns)` changed.
///
/// Preserves `FileId` stability by reusing the deserialized index's path arena; new files append.
/// Walk errors are logged but do not abort — the post-load reconcile is a backstop.
///
/// That backstop means a warm startup stats every `.gd` twice — this walk plus `reconcile`'s — a
/// deliberate cost, not an oversight. Reconcile stays unconditional so cold and warm startups
/// converge on the same authoritative settle pass: it has final authority on removals, produces
/// the dirty set and the `post_cold_reconcile` marker line, and settles the state the cache save
/// then persists. The warm-start gate (>5×; 14.7× measured on the 3 000-file synthetic project)
/// holds with the double walk in place, so folding the two passes (teaching reconcile to trust
/// this walk's fresh stat table) is deferred to the first project that actually flags startup
/// stat cost, per the plan's "lands OR documented bench witness" rule.
fn warm_index_from_cache(
    loaded: gd_project::cache::LoadedCache,
    root: &Utf8Path,
    dialect: Dialect,
    sink: &mut dyn crate::progress::ProgressSink,
) -> (
    Index,
    SceneIndex,
    AssetIndex,
    FxHashMap<Utf8PathBuf, FileStat>,
) {
    let gd_project::cache::LoadedCache {
        mut index,
        files,
        mut scenes,
        mut assets,
    } = loaded;

    // Build a lookup table from the cached file stats.
    let mut stat_table: FxHashMap<Utf8PathBuf, FileStat> =
        files.into_iter().map(|s| (s.path.clone(), s)).collect();

    // Walk current disk state — same walker reconcile uses.
    let walker = WalkDir::new(root.as_std_path())
        .into_iter()
        .filter_entry(|e| {
            if let Some(p) = camino::Utf8Path::from_path(e.path()) {
                !is_excluded(p, root)
            } else {
                true
            }
        });

    let mut walk_errors = 0usize;
    let mut skipped_non_utf8 = 0usize;
    let mut walked_paths: FxHashSet<Utf8PathBuf> = FxHashSet::default();
    // #127: assets get the same stat-diff + removal treatment as scenes (they share the stat table),
    // so an asset added/deleted while gdls was off is reconciled on warm-start. Tracked separately
    // from `walked_paths` (which the script removal pass keys on) since asset keys aren't `FileId`s.
    let mut walked_asset_paths: FxHashSet<Utf8PathBuf> = FxHashSet::default();
    let mut reparsed = 0usize;
    let mut added = 0usize;
    // Stat-changed files that could not be re-read. They sit in `walked_paths` but were neither
    // reparsed nor added, so they must be excluded from the "unchanged" count below (otherwise a
    // locked/permission-denied file is mis-reported as unchanged).
    let mut skipped_unreadable = 0usize;

    for entry_result in walker {
        let entry = match entry_result {
            Ok(e) => e,
            Err(e) => {
                let path_display = e
                    .path()
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|| "<unknown>".to_string());
                log::warn!("warm_index: walk error at {path_display}: {e}");
                walk_errors += 1;
                continue;
            }
        };
        if !entry.file_type().is_file() {
            continue;
        }
        let Some(p) = camino::Utf8Path::from_path(entry.path()) else {
            // Non-UTF-8 path — count it so the removal pass below stays off (same authority
            // rule as reconcile): a walk that couldn't name a file isn't authoritative.
            skipped_non_utf8 += 1;
            log::warn!("warm_index: skipping non-UTF-8 path under {root}");
            continue;
        };
        let is_gd = p.extension() == Some("gd");
        let is_scene = gd_project::is_scene_path(p);
        if !is_gd && !is_scene {
            // #127: every other (non-excluded) file is an arbitrary asset — index its res:// path
            // only (no parse). Stat-diff against the shared table so an asset replaced on disk while
            // gdls was off refreshes its stat entry; insert is idempotent (the path is the key).
            let path = gd_project::normalize_path(p);
            walked_asset_paths.insert(path.clone());
            let new_stat = entry
                .metadata()
                .ok()
                .map(|m| cache::stat_from_metadata(path.clone(), &m));
            let stat_changed = match (stat_table.get(&path), &new_stat) {
                (None, _) => true,
                (Some(_), None) => true,
                (Some(old), Some(new)) => old.size != new.size || old.mtime_ns != new.mtime_ns,
            };
            if stat_changed {
                if let Some(res) = gd_project::path_to_res(root, &path) {
                    assets.insert(res);
                }
                if let Some(s) = new_stat {
                    stat_table.insert(path.clone(), s);
                } else {
                    stat_table.remove(&path);
                }
            }
            continue;
        }
        let path = gd_project::normalize_path(p);
        walked_paths.insert(path.clone());
        // Indeterminate progress (WalkDir streams; no total up front) — the reporter throttles.
        sink.progress(walked_paths.len(), None, "checking cached files");

        // Stat from the walk entry, not a fresh `fs::metadata`: on Windows the DirEntry's
        // metadata is populated from the directory enumeration itself (zero extra syscalls —
        // issue #14's per-file CreateFile cost), and on unix it's the same one stat.
        let new_stat = entry
            .metadata()
            .ok()
            .map(|m| cache::stat_from_metadata(path.clone(), &m));

        let stat_changed = match (stat_table.get(&path), &new_stat) {
            (None, _) => true,       // added — not in cache
            (Some(_), None) => true, // stat failed — re-parse defensively
            (Some(old), Some(new)) => old.size != new.size || old.mtime_ns != new.mtime_ns,
        };

        if stat_changed {
            // Re-read and re-parse this file.
            match std::fs::read_to_string(&path) {
                Ok(text) => {
                    if is_scene {
                        // `.tscn` → the scene index ONLY (never the script interner). Key by the
                        // file's res:// path so it matches the cold-built scene index's keys.
                        if let Some(res) = gd_project::path_to_res(root, &path) {
                            scenes.reindex(&res, &text);
                        }
                        reparsed += 1;
                    } else {
                        // `.gd` → the script index, exactly as before. `is_added` is checked before
                        // the txn (which would otherwise have already interned the interface).
                        let is_added = index.interface_of(&path).is_none();
                        let tree = parse_in_dialect(&text, dialect).tree;
                        let iface = gd_project::extract_interface(&tree);
                        index.txn(&path, |idx| {
                            idx.on_file_changed(&path, iface);
                        });
                        if is_added {
                            added += 1;
                        } else {
                            reparsed += 1;
                        }
                    }
                    if let Some(s) = new_stat {
                        stat_table.insert(path.clone(), s);
                    } else {
                        stat_table.remove(&path);
                    }
                }
                Err(e) => {
                    skipped_unreadable += 1;
                    log::warn!("warm_index: skipping unreadable {path}: {e}");
                }
            }
        } else {
            // Stat unchanged — the cached stat is still current, so keep it as-is.
            // No re-parse needed.
        }
    }

    // Drop files that were in the cache but are no longer on disk (only when the walk was
    // authoritative — same guard as reconcile: any unnamed/errored entry means the walked set
    // is incomplete, so a missing-from-walk file might still exist).
    if walk_errors == 0 && skipped_non_utf8 == 0 {
        let removed_paths: Vec<Utf8PathBuf> = index
            .iter_interfaces()
            .filter_map(|(fid, _)| index.path(fid).map(camino::Utf8Path::to_path_buf))
            .filter(|p| !walked_paths.contains(p))
            .collect();
        for path in &removed_paths {
            index.txn(path, |idx| idx.on_file_removed(path));
            stat_table.remove(path);
        }
        // Parallel removal pass for scenes: a `.tscn` cached but no longer on disk is dropped from
        // the scene index. Scene keys are res:// paths, so map each back to its absolute path to
        // test against the walked set.
        let removed_scenes: Vec<String> = scenes
            .iter()
            .map(|(res, _)| res.to_owned())
            .filter(|res| {
                gd_project::res_to_path(root, res)
                    .map(|abs| gd_project::normalize_path(&abs))
                    .is_none_or(|abs| !walked_paths.contains(&abs))
            })
            .collect();
        for res in &removed_scenes {
            scenes.remove(res);
            // Prune the dead stat entry too (mirrors the reconcile removal pass), so a scene
            // deleted while offline doesn't leave an immortal FileStat that re-persists forever.
            if let Some(abs) = gd_project::res_to_path(root, res) {
                stat_table.remove(&gd_project::normalize_path(&abs));
            }
        }
        // #127: parallel removal pass for assets — an asset cached but no longer on disk is dropped
        // from the asset index (and its stat entry pruned), same authoritative-walk guard.
        let removed_assets: Vec<String> = assets
            .iter()
            .map(str::to_owned)
            .filter(|res| {
                gd_project::res_to_path(root, res)
                    .map(|abs| gd_project::normalize_path(&abs))
                    .is_none_or(|abs| !walked_asset_paths.contains(&abs))
            })
            .collect();
        for res in &removed_assets {
            assets.remove(res);
            if let Some(abs) = gd_project::res_to_path(root, res) {
                stat_table.remove(&gd_project::normalize_path(&abs));
            }
        }
        log::info!(
            "warm_index: stat-diff complete: {} unchanged, {} reparsed, {} added, {} removed, \
             {} skipped (unreadable)",
            walked_paths
                .len()
                .saturating_sub(reparsed + added + skipped_unreadable),
            reparsed,
            added,
            removed_paths.len(),
            skipped_unreadable,
        );
    } else {
        log::info!(
            "warm_index: stat-diff complete (walk not authoritative — walk_errors={walk_errors}, \
             skipped_non_utf8={skipped_non_utf8} — skipping removal pass): {} reparsed, {} added, \
             {} skipped (unreadable)",
            reparsed,
            added,
            skipped_unreadable,
        );
    }

    (index, scenes, assets, stat_table)
}

/// Build a stat table by iterating all interned files in the index after a cold build.
/// Used so the cold path and the warm path both produce a populated stat table — the cold build
/// doesn't stat files during `Index::build`, so we sweep them here.
/// Install a fresh `uid:// → res://` map on the index, through the verifying [`Index::txn`] seam.
///
/// `txn` wants the file a mutation is about; a whole-project map swap is about `project.godot`,
/// which is where the scan is triggered from and which the index never interns, so a quarantine
/// pass on it is a no-op.
fn set_index_uid_map(index: &mut Index, root: &Utf8Path, uids: FxHashMap<String, String>) {
    let anchor = root.join("project.godot");
    index.txn(&anchor, move |idx| idx.set_uid_map(uids));
}

fn build_stat_table_from_index(index: &Index) -> FxHashMap<Utf8PathBuf, FileStat> {
    let mut table = FxHashMap::default();
    for (fid, _) in index.iter_interfaces() {
        let Some(path) = index.path(fid) else {
            continue;
        };
        match std::fs::metadata(path.as_std_path()) {
            Ok(meta) => {
                let stat = cache::stat_from_metadata(path.to_path_buf(), &meta);
                table.insert(path.to_path_buf(), stat);
            }
            Err(e) => {
                log::warn!("cold_index: could not stat {path} for cache table: {e}");
            }
        }
    }
    table
}

/// Add a [`FileStat`] for every indexed scene to `table`, so the warm-start stat-diff re-parses a
/// `.tscn` edited while gdls was off (the scene index has no `CacheKey` component of its own —
/// freshness rides this stat table, exactly like scripts). Scenes are keyed by res:// path; map
/// each back to its absolute path to stat it. A scene whose file vanished between the index build
/// and this sweep is skipped.
fn add_scene_stats(
    table: &mut FxHashMap<Utf8PathBuf, FileStat>,
    scenes: &SceneIndex,
    root: &Utf8Path,
) {
    for (res, _) in scenes.iter() {
        let Some(abs) = gd_project::res_to_path(root, res) else {
            continue;
        };
        let key = gd_project::normalize_path(&abs);
        match std::fs::metadata(abs.as_std_path()) {
            Ok(meta) => {
                table.insert(key.clone(), cache::stat_from_metadata(key, &meta));
            }
            Err(e) => log::warn!("cold_index: could not stat scene {abs} for cache table: {e}"),
        }
    }
}

/// Add a [`FileStat`] for every indexed asset to `table`, so the warm-start stat-diff reconciles an
/// asset added/removed while gdls was off (the asset index has no `CacheKey` component of its own —
/// freshness rides this stat table, exactly like scripts and scenes). Assets are keyed by res://
/// path; map each back to its absolute path to stat it. An asset whose file vanished between the
/// index build and this sweep is skipped.
fn add_asset_stats(
    table: &mut FxHashMap<Utf8PathBuf, FileStat>,
    assets: &AssetIndex,
    root: &Utf8Path,
) {
    for res in assets.iter() {
        let Some(abs) = gd_project::res_to_path(root, res) else {
            continue;
        };
        let key = gd_project::normalize_path(&abs);
        match std::fs::metadata(abs.as_std_path()) {
            Ok(meta) => {
                table.insert(key.clone(), cache::stat_from_metadata(key, &meta));
            }
            Err(e) => log::warn!("cold_index: could not stat asset {abs} for cache table: {e}"),
        }
    }
}

/// Project the server's `initializationOptions.strict` (its own enum, kept off the analyzer crate)
/// onto the analyzer's [`StrictSettings`]. The two profiles enumerate the same variants 1:1 by
/// design (see `gd_analyze::warn_policy`'s module doc).
///
/// Side effect: warn-logs any name in the three override lists that doesn't resolve to a known
/// `WarningCode`. This lenient "skip the unknown name and keep going" behavior is **gdls-specific,
/// not Godot parity**: in Godot,
/// `gdscript_warning.cpp`'s `get_code_from_name` is consulted only by the `@warning_ignore` /
/// `@warning_ignore_region` annotation handlers (`gdscript_parser.cpp`), which
/// `push_error("Invalid warning name")` on a miss — they do **not** silently continue. gdls's
/// project-settings warning-level path never routes arbitrary names through that Godot code at all,
/// so there is no Godot behavior to mirror here; we deliberately choose leniency for *config* (an
/// unknown name in `initializationOptions` is a config typo, not GDScript source) and surface the
/// unknown-name list to stderr at startup so the typo stays debuggable.
fn strict_settings(strict: &StrictConfig) -> StrictSettings {
    warn_on_unknown_codes(&strict.enable_warnings, "enableWarnings");
    warn_on_unknown_codes(&strict.disable_warnings, "disableWarnings");
    warn_on_unknown_codes(&strict.error_warnings, "errorWarnings");
    StrictSettings {
        profile: match strict.profile {
            ServerStrictProfile::Godot => StrictProfile::Godot,
            ServerStrictProfile::Strict => StrictProfile::Strict,
            ServerStrictProfile::Off => StrictProfile::Off,
        },
        enable_warnings: strict.enable_warnings.clone(),
        disable_warnings: strict.disable_warnings.clone(),
        error_warnings: strict.error_warnings.clone(),
    }
}

fn warn_on_unknown_codes(names: &[String], context: &str) {
    for name in names {
        if code_from_name(&name.to_ascii_uppercase()).is_none() {
            log::warn!(
                "unknown warning code in initializationOptions.strict.{context}: {name:?} (no such WarningCode; ignored per gdls config leniency)"
            );
        }
    }
}

/// Load the native DB from `extensionApiPath` (degrading to empty on absence/error), then merge each
/// installed GDExtension's `doc_classes` XML — those classes are absent from the stock dump.
fn load_native(
    options: &InitializationOptions,
    project: &ProjectModel,
    root: &Utf8Path,
    dialect: Dialect,
    dialect_origin: DialectOrigin,
) -> (NativeDb, Option<String>) {
    let mut db = match options.extension_api_path.as_deref() {
        Some(path) => match NativeDb::load(path) {
            Ok(db) => {
                log::info!(
                    "loaded native API: {} classes from {path}",
                    db.class_count()
                );
                db
            }
            Err(e) => {
                // A pinned-but-unreadable path still beats nothing: fall back to the embedded
                // stock surface (Generic provenance) before degrading to the empty DB.
                match options
                    .embedded_api_fallback
                    .then(|| crate::api_dump::embedded_stock_db(dialect))
                    .flatten()
                {
                    Some(db) => {
                        log::warn!(
                            "extensionApiPath unreadable ({e}); using the embedded stock surface \
                             — fix the path for an exact dump"
                        );
                        db
                    }
                    None => {
                        log::warn!("native API unavailable ({e}); native types degrade to dynamic");
                        NativeDb::empty()
                    }
                }
            }
        },
        // No explicit path: the managed resolution — fresh .gdls dump → stale dump →
        // project-root file → embedded stock → empty (crate::api_dump has the full ladder +
        // logs). Never spawns; the auto-dump is `spawn_background_dump`'s job.
        None => crate::api_dump::resolve_native_db(options, project, root, dialect),
    };

    // #329: every source above arrives stamped `Exact` without anyone comparing the dump's own
    // header against the release the project declared. A dump from an OLDER release cannot carry
    // that claim, so this is where a stale one is replaced or demoted.
    let release_notice = reconcile_release(&mut db, options, dialect, dialect_origin);

    let mut merged = 0usize;
    for ext in &project.gdextensions {
        let before = merged;
        for xml in ext.doc_xml_files() {
            match gd_types::doc_xml::parse_file(xml.as_str()) {
                Ok(class) => {
                    if db.merge_doc_class(class) {
                        merged += 1;
                    }
                }
                // Most `.xml` under an addon aren't class docs — expected, stay quiet.
                Err(DocXmlError::NotAClass(_) | DocXmlError::MissingName) => {}
                // A malformed `<class>` doc or an unreadable file IS actionable: that class
                // silently goes dynamic. Warn so the operator sees the breadcrumb at default
                // log level — `debug` would hide it where it matters.
                Err(e) => log::warn!("skipping GDExtension doc XML {xml}: {e}"),
            }
        }
        // `[icons]` named classes but no usable doc XML was found ⇒ those types degrade to
        // dynamic — but only when the dump doesn't already carry them (a dump taken with the
        // extension loaded has the classes; shipping no doc XML is then normal, not a
        // degradation). `extension_class_notice` decides; capturing `class_hints` is the
        // whole reason this check exists (`gdextension.rs`).
        if let Some(notice) = extension_class_notice(ext, &db, merged - before) {
            log::warn!("GDExtension {}: {notice}", ext.config);
        }
    }
    if merged > 0 {
        log::info!("merged {merged} GDExtension class(es) from doc XML");
    }
    (db, release_notice)
}

/// The degradation notice for one GDExtension whose classes this session cannot see.
///
/// `[icons]` hints exist, this pass merged no doc XML for the extension, and the dump doesn't
/// carry the classes either — every hinted type degrades to dynamic. The causes all look
/// identical from here: a stock surface, a never-imported project (no
/// `.godot/extension_list.cfg`, the api_dump caveat), or a dump whose extension load failed
/// (one extension's DLL failure silently unregisters the rest). Name the remediation, at `warn`:
/// this is the difference between a typed extension API and `Variant`.
///
/// The hinted classes an extension ships stand or fall together in every one of those causes, so
/// one class present in the dump is taken as the extension being visible and nothing is logged —
/// an extension captured into the dump simply ships no doc XML, which is normal. The missing ones
/// are named so a partially-visible extension is still legible in the log rather than being
/// rounded off to silence.
fn extension_class_notice(
    ext: &gd_project::gdextension::GdExtension,
    db: &NativeDb,
    merged_this_ext: usize,
) -> Option<String> {
    if merged_this_ext > 0 || ext.class_hints.is_empty() {
        return None;
    }
    let missing: Vec<&str> = ext
        .class_hints
        .iter()
        .filter(|h| db.class_named(h).is_none())
        .map(String::as_str)
        .collect();
    if missing.len() < ext.class_hints.len() {
        return None;
    }
    Some(format!(
        "{} declared class(es) are absent from both doc XML and the native dump ({}) — those \
         types degrade to dynamic; a dump taken with the extension loaded fixes this (open the \
         project in the Godot editor once — this generates .godot/extension_list.cfg — then \
         restart gdls so it re-dumps)",
        missing.len(),
        missing.join(", "),
    ))
}

/// Act on a native surface whose release is not the one the project is read as (#329).
///
/// Every source but the embedded fallback can disagree with the dialect: a pinned
/// `extensionApiPath` left over from an upgrade, a cached auto-dump from the binary that used to
/// be on `PATH`, a checked-in `extension_api.json`. All of them arrive stamped
/// [`ApiProvenance::Exact`], which is the claim that this dump IS the engine surface — and that
/// claim is what unlocks every negative gdls emits (`Identifier "X" not declared`,
/// `Cannot find member`, `Function "f()" not found in base`).
///
/// A dump from an **older** release cannot carry that claim. Each API the newer release added is
/// simply missing from it, and the miss reads as a user error: Pixelorama shipped a checked-in
/// 4.6.3 dump under `config/features=("4.7")`, and gdls fabricated four errors out of 4.7-only
/// APIs. Since v3.0.0 there is a better answer sitting right there — the embedded stock surface
/// is the complete official API *for the project's own declared release* — so that is what
/// replaces it. The GDExtension `doc_classes` merge runs afterwards either way, so extension
/// classes are not lost with the dump. Where the embedded asset is unavailable
/// (`embeddedApiFallback` off, or a corrupt asset), the stale dump is kept but demoted to
/// `Generic`, which at least stops it from disproving names it never knew.
///
/// A **newer** dump is left alone: it is a superset, so absence from it still proves absence from
/// the older engine and every negative stays sound. Only the positives can over-offer, and
/// provenance does not gate those.
fn reconcile_release(
    db: &mut NativeDb,
    options: &InitializationOptions,
    dialect: Dialect,
    dialect_origin: DialectOrigin,
) -> Option<String> {
    if !dialect_origin.is_evidenced() {
        // Nothing pinned the release: `project.godot` declared none, or there is no project file
        // at all, and gdls fell back to `Dialect::NEWEST`. The dump's own header is then better
        // evidence of the engine than that default, so it wins — replacing a real 4.6 dump on the
        // strength of a guess would be the more damaging mistake.
        return None;
    }
    let notice = version_mismatch_notice(db, dialect)?;
    log::warn!("native API: {notice}");

    let (major, minor) = dialect.version();
    let (dump_major, dump_minor) = {
        let h = db.header();
        (h.version_major, h.version_minor)
    };
    if (dump_major, dump_minor) > (major, minor) {
        // Superset: absence from it still proves absence from the older engine, so every negative
        // stays sound and the log line above is the whole response.
        return None;
    }
    let dump_release = format!("{dump_major}.{dump_minor}");

    match options
        .embedded_api_fallback
        .then(|| crate::api_dump::embedded_stock_db(dialect))
        .flatten()
    {
        Some(stock) => {
            log::warn!(
                "native API: replacing it with the embedded stock Godot {dialect} surface, which \
                 is complete for that release; GDExtension classes still merge from their doc XML"
            );
            *db = stock;
            Some(format!(
                "gdls ignored the Godot {dump_release} extension_api.json it found: this project \
                 declares Godot {dialect}, and every API added since {dump_release} is missing \
                 from that dump, so code that is fine would be reported as errors. The built-in \
                 stock {dialect} surface is being used instead, which means classes from your own \
                 Godot build are not available. Re-dump with a {dialect} binary (set \
                 `godotBinaryPath`, or the GDLS_GODOT environment variable), or fix \
                 `application/config/features` in project.godot."
            ))
        }
        None => {
            log::warn!(
                "native API: no embedded stock Godot {dialect} surface to replace it with — \
                 keeping the older dump, but gdls will not report unknown identifiers or members \
                 against it"
            );
            db.set_provenance(gd_types::ApiProvenance::Generic);
            Some(format!(
                "the extension_api.json gdls found is for Godot {dump_release} but this project \
                 declares Godot {dialect}. gdls will not report unknown identifiers or members \
                 against it, since every API added since {dump_release} is missing from it. \
                 Re-dump with a {dialect} binary (set `godotBinaryPath`, or the GDLS_GODOT \
                 environment variable), or fix `application/config/features` in project.godot."
            ))
        }
    }
}

/// Say so when the native surface gdls ended up with is not the release the project is read as.
///
/// Signatures, enum values, and the class list all shift between feature releases, so a mismatch
/// shows up as diagnostics the user cannot explain from their own code.
///
/// `None` when they agree, and when the dump carries no version at all — an empty DB or a
/// hand-written test fixture has nothing to disagree with. Real Godot dumps always carry a
/// header; treating a headerless one as a mismatch would silence the very negatives the
/// hand-written fixtures exist to pin.
fn version_mismatch_notice(db: &NativeDb, dialect: Dialect) -> Option<String> {
    let header = db.header();
    let (major, minor) = dialect.version();
    if header.version_major == 0 || (header.version_major, header.version_minor) == (major, minor) {
        return None;
    }
    Some(format!(
        "the loaded dump is Godot {}.{} but scripts are read as Godot {dialect} — engine \
         signatures, enum values, and the class list differ between releases, so some diagnostics \
         will not match what Godot reports. Point extensionApiPath or godotBinaryPath at a \
         {dialect} build, or set the dialect explicitly.",
        header.version_major, header.version_minor,
    ))
}

#[cfg(test)]
mod extension_notice_tests {
    use super::*;

    fn ext(hints: &[&str]) -> gd_project::gdextension::GdExtension {
        gd_project::gdextension::GdExtension {
            config: Utf8PathBuf::from("res://addons/x/x.gdextension"),
            addon_dir: Utf8PathBuf::from("res://addons/x"),
            class_hints: hints.iter().map(|s| s.to_string()).collect(),
        }
    }

    fn db_with(class: &str) -> NativeDb {
        let json = format!(
            r#"{{"header":{{"version_major":4,"version_minor":6,"version_patch":0}},
                "classes":[{{"name":"{class}"}}],"builtin_classes":[],"global_enums":[],
                "global_constants":[],"utility_functions":[],"singletons":[]}}"#
        );
        NativeDb::from_json(&json).expect("fixture dump must ingest")
    }

    #[test]
    fn hints_absent_everywhere_warn_with_the_remediation() {
        let notice = extension_class_notice(&ext(&["BTPlayer", "BTTask"]), &db_with("Node"), 0)
            .expect("hints the session cannot see must be called out");
        assert!(notice.contains("degrade to dynamic"), "{notice}");
        assert!(notice.contains("extension_list.cfg"), "{notice}");
        assert!(notice.contains("restart gdls"), "{notice}");
        // The missing classes are named, so a log reader can tell which types went dynamic.
        assert!(notice.contains("BTPlayer, BTTask"), "{notice}");
    }

    #[test]
    fn hints_already_in_the_dump_mean_no_degradation() {
        // A dump captured with the extension loaded carries the class; shipping no doc XML
        // is then normal, and a "types degrade to dynamic" line would be a lie.
        assert!(extension_class_notice(&ext(&["BTPlayer"]), &db_with("BTPlayer"), 0).is_none());
    }

    #[test]
    fn merged_doc_xml_or_no_hints_stay_silent() {
        assert!(extension_class_notice(&ext(&["BTPlayer"]), &db_with("Node"), 1).is_none());
        assert!(extension_class_notice(&ext(&[]), &db_with("Node"), 0).is_none());
    }
}

#[cfg(test)]
mod version_mismatch_tests {
    use super::*;

    fn db_at(major: u32, minor: u32) -> NativeDb {
        let json = format!(
            r#"{{"header":{{"version_major":{major},"version_minor":{minor},"version_patch":0}},
                "classes":[],"builtin_classes":[],"global_enums":[],"global_constants":[],
                "utility_functions":[],"singletons":[]}}"#
        );
        NativeDb::from_json(&json).expect("fixture dump must ingest")
    }

    #[test]
    fn a_matching_dump_says_nothing() {
        assert!(version_mismatch_notice(&db_at(4, 7), Dialect::Godot4_7).is_none());
        assert!(version_mismatch_notice(&db_at(4, 6), Dialect::Godot4_6).is_none());
    }

    #[test]
    fn a_dump_from_another_release_names_both_versions() {
        let notice = version_mismatch_notice(&db_at(4, 6), Dialect::Godot4_7)
            .expect("a 4.6 dump under a 4.7 project must be called out");
        assert!(notice.contains("Godot 4.6"), "{notice}");
        assert!(notice.contains("Godot 4.7"), "{notice}");
    }

    /// An empty DB (no native source at all) has no version to disagree with, and the "native API
    /// unavailable" path already said so — a second, wronger line would just be noise.
    #[test]
    fn a_versionless_dump_says_nothing() {
        assert!(version_mismatch_notice(&NativeDb::empty(), Dialect::Godot4_7).is_none());
    }

    // --- #329: acting on the mismatch, not just logging it ---

    fn opts(embedded_fallback: bool) -> InitializationOptions {
        InitializationOptions::parse(Some(&serde_json::json!({
            "autoDumpExtensionApi": false,
            "embeddedApiFallback": embedded_fallback,
        })))
    }

    /// The reproduction from the issue: a 4.6 dump under a project that declares 4.7. The dump is
    /// dropped for the embedded stock 4.7 surface, which is complete for that release, and the
    /// user is told what happened and how to fix it.
    #[test]
    fn an_older_dump_is_replaced_by_the_declared_releases_stock_surface() {
        let mut db = db_at(4, 6);
        let notice = reconcile_release(
            &mut db,
            &opts(true),
            Dialect::Godot4_7,
            DialectOrigin::Declared,
        )
        .expect("the user must hear about a rejected dump");

        assert_eq!(
            (db.header().version_major, db.header().version_minor),
            (4, 7),
            "the surface in use must be the declared release's"
        );
        assert!(
            db.class_count() > 100,
            "the embedded stock surface should be a real class list, got {}",
            db.class_count()
        );
        assert!(notice.contains("4.6") && notice.contains("4.7"), "{notice}");
        assert!(
            notice.contains("godotBinaryPath") && notice.contains("config/features"),
            "the notice must name both fixes: {notice}"
        );
    }

    /// With no stock surface to swap in, the dump is kept — it is still the only positives gdls
    /// has — but demoted, so it can no longer disprove a name it never knew.
    #[test]
    fn an_older_dump_without_a_fallback_is_kept_but_demoted() {
        let mut db = db_at(4, 6);
        let notice = reconcile_release(
            &mut db,
            &opts(false),
            Dialect::Godot4_7,
            DialectOrigin::Declared,
        )
        .expect("a demoted surface is still worth telling the user about");

        assert_eq!(
            (db.header().version_major, db.header().version_minor),
            (4, 6),
            "the dump itself must be kept"
        );
        assert_eq!(db.provenance(), gd_types::ApiProvenance::Generic);
        assert!(notice.contains("4.6") && notice.contains("4.7"), "{notice}");
    }

    /// A NEWER dump is a superset: absence from it still proves absence from the older engine, so
    /// the negatives stay sound and nothing is replaced, demoted, or shown to the user.
    #[test]
    fn a_newer_dump_is_left_alone() {
        let mut db = db_at(4, 7);
        let notice = reconcile_release(
            &mut db,
            &opts(true),
            Dialect::Godot4_6,
            DialectOrigin::Declared,
        );

        assert!(notice.is_none(), "{notice:?}");
        assert_eq!(
            (db.header().version_major, db.header().version_minor),
            (4, 7)
        );
        assert_eq!(db.provenance(), gd_types::ApiProvenance::Exact);
    }

    /// When nothing declared a release, `Dialect::NEWEST` is a guess, and the dump's own header is
    /// the better witness — so the guess must not be allowed to throw the dump away.
    #[test]
    fn an_unevidenced_dialect_never_overrides_the_dump() {
        for origin in [DialectOrigin::DefaultedNewest, DialectOrigin::NoProject] {
            let mut db = db_at(4, 6);
            let notice = reconcile_release(&mut db, &opts(true), Dialect::Godot4_7, origin);
            assert!(notice.is_none(), "at {origin:?}: {notice:?}");
            assert_eq!(
                (db.header().version_major, db.header().version_minor),
                (4, 6),
                "at {origin:?}"
            );
            assert_eq!(
                db.provenance(),
                gd_types::ApiProvenance::Exact,
                "at {origin:?}"
            );
        }
    }

    /// A matching dump is untouched on every evidenced origin — the ordinary case must cost
    /// nothing.
    #[test]
    fn a_matching_dump_is_untouched() {
        for origin in [
            DialectOrigin::Override,
            DialectOrigin::Declared,
            DialectOrigin::ClampedNewer,
            DialectOrigin::ClampedOlder,
        ] {
            let mut db = db_at(4, 7);
            let notice = reconcile_release(&mut db, &opts(true), Dialect::Godot4_7, origin);
            assert!(notice.is_none(), "at {origin:?}: {notice:?}");
            assert_eq!(
                db.provenance(),
                gd_types::ApiProvenance::Exact,
                "at {origin:?}"
            );
        }
    }

    /// A hand-written fixture with no header has nothing to disagree with — treating it as a
    /// mismatch would silence the very negatives those fixtures exist to pin.
    #[test]
    fn a_versionless_dump_is_never_reconciled() {
        let mut db = NativeDb::empty();
        assert!(reconcile_release(
            &mut db,
            &opts(true),
            Dialect::Godot4_7,
            DialectOrigin::Declared
        )
        .is_none());
    }
}

#[cfg(test)]
mod bail_recovery_tests {
    //! #210: a bailed (fixpoint-governor / cancellation) re-analyze must NOT serve its partial when
    //! a COMPLETE result for the identical bytes is already cached — it serves the complete one
    //! (hash-match, epoch relaxed), never lying with truncated side tables.
    use super::*;

    #[test]
    fn bailed_reanalyze_serves_cached_complete_for_identical_bytes() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("utf-8 temp dir");
        std::fs::write(dir.path().join("project.godot"), "config_version=5\n")
            .expect("write project.godot");
        let src = "extends Node\nfunc go() -> void:\n\tvar a := 1\n\tvar b := a + 1\n\tprint(b)\n";
        let gd_path = root.join("main.gd");
        std::fs::write(gd_path.as_std_path(), src).expect("write main.gd");

        let options = InitializationOptions::parse(Some(&serde_json::json!({
            "projectRoot": root.as_str(),
            "autoDumpExtensionApi": false,
        })));
        let mut ws = Workspace::load(&root, &options);
        let key = CanonicalKey::for_path(&gd_path).expect("canonical key for main.gd");

        let tree = ws.parse(&key, src).tree.clone();

        // 1. A complete analyze caches the full result at the file's current epoch.
        let complete = ws.analyze(&key, &gd_path, &tree, src);
        assert!(
            !complete.bailed,
            "the seeding analyze must complete, not bail"
        );

        // 2. Bump this file's epoch — the dependency-interface-change analog the bug needs: a
        //    watcher epoch bump in the gap before a request makes the request's epoch-exact lookup
        //    miss. `reindex` re-applies the same interface, which unconditionally bumps the epoch.
        ws.reindex(&gd_path, &tree);

        // 3. Re-analyze IDENTICAL bytes with a tiny iter_limit so the fixpoint governor bails on the
        //    first checkpoint. The epoch-exact cache lookup misses (epoch bumped), forcing the
        //    re-analyze; it bails; #210 then recovers the cached complete entry for the same hash.
        let opts = gd_analyze::AnalyzeOptions {
            iter_limit: Some(1),
            ..Default::default()
        };
        let served = ws.analyze_with_options(&key, &gd_path, &tree, src, opts);

        assert!(
            !served.bailed,
            "a bailed re-analyze must serve the cached COMPLETE result for identical bytes (#210), \
             not the partial"
        );
        assert!(
            Rc::ptr_eq(&served, &complete),
            "the served result must be the SAME complete Rc that the seeding analyze cached"
        );
    }
}
