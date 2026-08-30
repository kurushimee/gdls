//! Persistent warm-start index cache for `gdls`.
//!
//! Writes the [`Index`] + a per-file stat table to `<project-root>/.gdls/index.<fmtver>.json`
//! (JSON-encoded, atomically via `tempfile` → same-fs rename so a concurrent reader never sees a
//! torn write). On next startup, [`load`] deserializes the cache, checks the [`CacheKey`] (binary
//! version, native-DB hash, project.godot fingerprint), runs [`Index::verify`] on the deserialized
//! index, and returns it if clean — avoiding a full cold walk of tens of thousands of `.gd` files.
//!
//! **Failure discipline:** every failure mode is a graceful cold fallback:
//! - File not found → `None` (first launch or concurrent save won — fine).
//! - Parse error → quarantine (rename-aside to `*.corrupt`) + `None`.
//! - Key mismatch → `None` (valid file, just stale; next `save` overwrites it).
//! - Structural [`Index::verify`] failure → quarantine + `None`.
//! - Write failure → `log::warn!` + return (costs one cold index on next launch).
//!
//! **Two processes on the same project:** `NamedTempFile::new_in` places the temp in the same
//! directory as the target, so `persist` (an atomic rename) is always same-filesystem. Two writers
//! racing use last-writer-wins: each renames its own complete temp over the target. A reader only
//! ever opens a complete old or complete new file.
//!
//! **`.gdls` exclusion:** `exclude.rs` already lists `.gdls` in `EXCLUDED_COMPONENTS`, so the cold
//! indexer and watcher never try to parse `index.<fmtver>.json` as a GDScript source.

use camino::{Utf8Path, Utf8PathBuf};
use serde::{Deserialize, Serialize};
use std::io::Write as _;

use crate::asset_index::{AssetIndex, AssetIndexCache};
use crate::index::{Index, IndexCache};
use crate::scene_index::{SceneIndex, SceneIndexCache};

// ---------------------------------------------------------------------------
// Public constants and structs (B4 reads these to construct the cache key).
// ---------------------------------------------------------------------------

/// Bump this whenever `CacheFile`'s layout or `IndexCache`'s format changes in a way that makes
/// old files unreadable. The cache filename embeds this version so old files are silently ignored,
/// not quarantined (a format bump is not corruption).
///
/// v2: `Interface` gained `unnamed_enum_values` and `EnumDecl.values` became
/// `Vec<EnumValueDecl { name, value }>` — a v1 cache would deserialize-fail or, worse,
/// misclassify every anonymous-enum const as a regular const, so v1 files are ignored.
///
/// v3: `Interface` gained `class_name_loc` and `ClassEntry` gained `line`/`name_span` (#33) — a
/// v2 cache would deserialize-fail (missing fields), so v2 files are ignored and rebuilt.
///
/// v4: `MemberDecl` gained `name_span` (#44/#46) — a v3 cache would deserialize-fail (missing
/// field), so v3 files are ignored and rebuilt. Deliberately NOT `#[serde(default)]`: a defaulted
/// zero-width span loaded from an old cache would silently degrade every name-token anchor.
///
/// v5 (M7 #62): `Interface`/`MemberDecl`/`EnumValueDecl` gained `doc` — a v4 cache would
/// otherwise warm-load doc-less interfaces and hover would silently show no prose until each
/// file's first edit. One cold re-index per project, self-healing.
///
/// v6 (M11 #76): `CacheFile` gained the `scenes` field (the persisted [`SceneIndex`] — `.tscn`
/// node/script/instance relations). A v5 cache would deserialize-fail (missing field), so v5 files
/// are ignored and rebuilt. Deliberately NOT `#[serde(default)]`: a defaulted empty scene index
/// loaded from an old cache would silently serve a project with no scene relations until each
/// `.tscn`'s first edit — the same "warm-load serves stale/empty derived state" hazard the v4 note
/// guards against. One cold re-index per project on upgrade, self-healing.
///
/// v7 (#173): `MemberFlags` gained `is_vararg` (whether a `func` declares a `...rest` parameter),
/// so cross-file/super-chain calls can suppress the too-many arity error on a vararg script
/// method. A v6 cache predates the field; rather than `#[serde(default)]`-load a `false` that would
/// resurface the very false positive this fixes (a vararg method warm-loaded as non-vararg), v6
/// files are ignored and rebuilt. One cold re-index per project on upgrade, self-healing.
///
/// v8 (#127): `CacheFile` gained the `assets` field (the persisted [`AssetIndex`] — the `res://`
/// paths of arbitrary project files, non-`.gd`/non-`.tscn`, that `load`/`preload` path completion
/// lists alongside scripts and scenes). A v7 cache predates the field. Deliberately NOT
/// `#[serde(default)]`: a defaulted empty asset index warm-loaded from an old cache would serve a
/// project with NO arbitrary-asset completions until each asset's first on-disk touch — the same
/// "warm-load serves stale/empty derived state" hazard the v6 note guards against. So v7 files are
/// ignored and rebuilt. One cold re-index per project on upgrade, self-healing.
/// v9 (#255): `Interface` gained `body_refs` — the identifiers a file references anywhere,
/// function bodies included, which `Index::recompute_edges` resolves through the `class_name`
/// registry into dependency edges. A v8 cache predates the field, so warm-loading it would rebuild
/// the dep graph WITHOUT the body-level edges and reproduce exactly the staleness this closes: an
/// open file whose only use of a class sits in a function body would never refresh when that class
/// changed. Not `#[serde(default)]` for that reason — v8 files are ignored and rebuilt. One cold
/// re-index per project on upgrade, self-healing.
/// v10 adds `dialect`: the resolved Godot dialect changes what the parser produces, so every
/// cached interface is dialect-specific. The `project_godot_fingerprint` catches a `config/features`
/// edit incidentally, but not an `initializationOptions.dialect` override, and the warning-code
/// renumbering that came with 4.7 support warrants the bump regardless. One cold re-index per
/// project on upgrade, self-healing.
/// v11 (#305): `EnumValueDecl` gained `line`/`name_span`, so `workspace/symbol` can report a named
/// enum's value at its own declaration instead of not at all. A v10 cache predates the fields and
/// would deserialize-fail; not `#[serde(default)]` for the same reason as v4's `name_span` — a
/// defaulted zero-width span warm-loaded from an old cache silently degrades every anchor it feeds.
/// One cold re-index per project on upgrade, self-healing.
/// v12 (#388): `Extends::Path` became a struct variant carrying the name segments after the path,
/// so `extends "res://x.gd".Inner` names the inner class rather than the file's head class. A v11
/// cache holds the old tuple shape and would deserialize-fail; not `#[serde(default)]`-able, and a
/// silently segment-less warm load is exactly the wrong answer this fixes — `class_parent` would
/// hand `rename` the head class and it would group the wrong overrides. v11 files are ignored and
/// rebuilt.
/// v13 (#406): `Interface` gained `parse_clean`, the bit that says the member list is the complete
/// set of declarations the source has. It gates the cross-file `Function "X()" not found in base`
/// claim, and a v12 cache holds no value for it — `#[serde(default)]` would warm-load `false` and
/// silently suppress that error for every file until the next cold index, which reads as "gdls
/// stopped reporting typos". v12 files are ignored and rebuilt.
/// v14 (#431): `initializer_type_expr` reads more shapes — a dotted `A.B.new()`, a cast, a `$`/`%`
/// node lookup — so a member that used to be recorded as untyped now carries a real type. The
/// shape did not change, so a v13 cache still deserializes; it just holds the older, poorer
/// answers, and a warm load would keep serving `Variant` for those members until the next cold
/// index. v13 files are ignored and rebuilt.
/// v15 (#431): `MemberDecl` gained `init`, the initializer's shape for a member the interface
/// could not type. `#[serde(default)]` would warm-load `None` for every member and the reader
/// would keep answering `Variant` for all of them until the next cold index — the same
/// serve-the-older-answer failure v14 fixed. v14 files are ignored and rebuilt.
/// v16 (#433): `preload_deps` and `InitShape::Preload` now record a path written relative to the
/// reading file, not just a `res://` one, so a const preloaded that way finally carries an edge
/// and a type. The shape did not change, so a v15 cache deserializes; it just holds a shorter
/// `preload_deps` and a `None` where a shape belongs, which reads as a missing dependency edge —
/// the consumer would go stale on an edit instead of re-analyzing. v15 files are ignored and
/// rebuilt.
pub const CACHE_FORMAT_VERSION: u32 = 16;

/// The cache file's basename within `<root>/.gdls/`. The `.json` extension is honest: the payload
/// is `serde_json`-encoded (see `save`/`load`), so a developer inspecting `.gdls/` or a backup tool
/// sees an inspectable JSON file, not an opaque blob. Single source of truth so `save` and `load`
/// can never disagree on the name (a divergence would be the silent always-cold failure mode).
#[must_use]
pub fn cache_file_name() -> String {
    format!("index.{CACHE_FORMAT_VERSION}.json")
}

/// All inputs that determine whether a cached index is still valid. A mismatch on any field means
/// the cached index may not reflect current project state; `load` returns `None` and lets the
/// caller cold-build.
///
/// `pub` because B4 constructs this from `workspace.rs` using the live NativeDb and the
/// project.godot fingerprint.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CacheKey {
    /// The cache file format version (embedded so old-format caches are detected even if
    /// `gdls_version` happened not to change across a format bump).
    pub cache_format_version: u32,
    /// The `gdls` binary version (`env!("CARGO_PKG_VERSION")`). Invalidates on toolchain upgrades.
    pub gdls_version: String,
    /// [`gd_types::NativeDb::content_hash`]. Invalidates when the native class database changes.
    pub native_db_content_hash: u64,
    /// Size + mtime of `project.godot`, mixed into a single `u64`. Invalidates when autoloads,
    /// warning config, or other project-level settings change (those live in `project.godot`).
    pub project_godot_fingerprint: u64,
    /// The resolved [`gd_syntax::Dialect`] as its `u8` discriminant. Interfaces are extracted from
    /// a parse tree, and the two dialects do not parse identically, so a cache written under one
    /// must never be served under the other. Kept separate from the `project.godot` fingerprint
    /// because the dialect can also come from an `initializationOptions` override that leaves the
    /// file untouched.
    pub dialect: u8,
}

/// Per-file stat snapshot stored alongside the index. B4 computes fresh stats on load and diffs
/// them against this table to find which files changed on disk since the cache was written —
/// re-parsing only those files instead of doing a full cold index.
///
/// `mtime_ns` is `i128` (signed nanoseconds since UNIX_EPOCH) so pre-1970 mtimes (possible on
/// network mounts or manually set) don't wrap. The `stat_from_metadata` helper converts
/// `SystemTime` to `i128`; B4 must use the same helper to stay consistent.
///
/// `pub` because B4's stat-diff loop constructs these from `std::fs::Metadata`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileStat {
    /// Absolute path (normalized via [`gd_project::normalize_path`]).
    pub path: Utf8PathBuf,
    /// On-disk file size in bytes.
    pub size: u64,
    /// Last-modified time, nanoseconds since UNIX_EPOCH (signed `i128`; pre-1970 → negative).
    pub mtime_ns: i128,
}

/// The return value of a successful [`load`]. The caller (B4) stat-diffs [`LoadedCache::files`]
/// against current disk state to find changed files, re-parses them into the [`LoadedCache::index`],
/// and then serves the warm-started index without a full cold walk.
pub struct LoadedCache {
    /// The deserialized and structurally verified index.
    pub index: Index,
    /// Per-file (path, size, mtime_ns) snapshot as of the time `save` was called.
    pub files: Vec<FileStat>,
    /// The deserialized scene index (`.tscn` relations). Rebuilt-on-load reverse maps; see
    /// [`SceneIndex::from_cache`]. Scene freshness on warm-start rides the same [`FileStat`]
    /// stat-diff as scripts — `.tscn` entries are included in [`Self::files`], so a scene edited
    /// while gdls was off is re-parsed by the caller's stat-diff pass (the scene index does not get
    /// its own validity key; the `CacheKey` only covers binary/native-DB/project.godot state).
    pub scenes: SceneIndex,
    /// The deserialized asset index (`res://` paths of arbitrary project files). Asset freshness on
    /// warm-start rides the same [`FileStat`] stat-diff as scripts and scenes — asset entries are
    /// included in [`Self::files`], so an asset added/removed while gdls was off is reconciled by the
    /// caller's stat-diff pass. Paths only (no parse); [`AssetIndex::from_cache`] is total.
    pub assets: AssetIndex,
}

// ---------------------------------------------------------------------------
// On-disk envelope (private — not exposed outside this module).
// ---------------------------------------------------------------------------

#[derive(Serialize, Deserialize)]
struct CacheFile {
    key: CacheKey,
    files: Vec<FileStat>,
    index: IndexCache,
    /// M11 #76: the persisted scene index (`.tscn` relations). See [`CACHE_FORMAT_VERSION`] v6.
    scenes: SceneIndexCache,
    /// #127: the persisted asset index (arbitrary `res://` asset paths). See
    /// [`CACHE_FORMAT_VERSION`] v8.
    assets: AssetIndexCache,
}

// ---------------------------------------------------------------------------
// Public helpers.
// ---------------------------------------------------------------------------

/// Convert `std::fs::Metadata` into a [`FileStat`]. Both `save` (which snapshots on-disk state)
/// and B4 (which re-stats to detect changes) must use this function so the `mtime_ns` encoding
/// is identical.
pub fn stat_from_metadata(path: Utf8PathBuf, meta: &std::fs::Metadata) -> FileStat {
    let mtime_ns = meta
        .modified()
        .ok()
        .and_then(|t| {
            t.duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos() as i128)
                .or_else(|e| {
                    // Pre-UNIX_EPOCH: negate the duration (negative nanos).
                    Ok::<i128, std::time::SystemTimeError>(-(e.duration().as_nanos() as i128))
                })
                .ok()
        })
        .unwrap_or(0);
    FileStat {
        path,
        size: meta.len(),
        mtime_ns,
    }
}

/// Compute a fingerprint for `<root>/project.godot`: a mix of its size and mtime_ns. This is
/// cheap (one `stat` syscall) and catches any write to the file (autoloads, warning config, etc.)
/// without reading its content on every startup.
///
/// Returns `0` if the file does not exist or cannot be stat'd. A project that has never had
/// `project.godot` (or always lacks it) will consistently produce `0`, which correctly matches
/// a cache saved under the same condition. A project that previously *had* the file and
/// subsequently lost it will produce `0 ≠ <old fingerprint>`, triggering a cold rebuild.
pub fn project_godot_fingerprint(root: &Utf8Path) -> u64 {
    let path = root.join("project.godot");
    match std::fs::metadata(path.as_std_path()) {
        Ok(meta) => {
            let stat = stat_from_metadata(path, &meta);
            // Mix size + mtime into a single u64 with a cheap multiply-xor hash.
            let a = stat.size;
            let b = stat.mtime_ns as u64; // truncate to 64 bits — lower bits carry the change
            let h = a.wrapping_mul(0x9e37_79b9_7f4a_7c15).wrapping_add(b);
            // Reserve 0 exclusively for the "absent/unstat-able" sentinel (Err branch). A real
            // file whose size+mtime mix lands on 0 would otherwise be indistinguishable from a
            // deleted project.godot and could match a stale cache; bump it to 1.
            if h == 0 {
                1
            } else {
                h
            }
        }
        Err(_) => 0,
    }
}

// ---------------------------------------------------------------------------
// save() — atomic write.
// ---------------------------------------------------------------------------

/// Serialize the index + stat table to `<root>/.gdls/index.<CACHE_FORMAT_VERSION>.json` atomically.
///
/// Serializes to a `NamedTempFile` in the **same directory** as the target (so `persist` is a
/// same-filesystem rename — never a cross-device copy). Two concurrent writers race to a
/// last-writer-wins outcome; a reader only ever sees a complete old or complete new file.
///
/// Write failures are logged at `warn` and return — "never crash" (CLAUDE.md). The worst case is
/// one cold index on the next launch.
pub fn save(
    root: &Utf8Path,
    index: &Index,
    scenes: &SceneIndex,
    assets: &AssetIndex,
    files: &[FileStat],
    key: CacheKey,
) {
    let dir = root.join(".gdls");
    if let Err(e) = std::fs::create_dir_all(dir.as_std_path()) {
        log::warn!("cache: mkdir .gdls failed: {e}");
        return;
    }
    let cache = CacheFile {
        key,
        files: files.to_vec(),
        index: index.to_cache(),
        scenes: scenes.to_cache(),
        assets: assets.to_cache(),
    };
    let bytes = match serde_json::to_vec(&cache) {
        Ok(b) => b,
        Err(e) => {
            log::warn!("cache: serialize failed: {e}");
            return;
        }
    };
    let target = dir.join(cache_file_name());
    // Write to a temp in the SAME dir so persist() is an atomic same-fs rename.
    let result = tempfile::NamedTempFile::new_in(dir.as_std_path()).and_then(|mut f| {
        f.write_all(&bytes)?;
        f.persist(target.as_std_path()).map_err(|e| e.error)
    });
    if let Err(e) = result {
        log::warn!(
            "cache: write/persist failed (skipping; costs one cold index on next launch): {e}"
        );
    }
}

// ---------------------------------------------------------------------------
// load() — validate → structural verify → stat-diff ready.
// ---------------------------------------------------------------------------

/// Load the cache if its key matches the current environment and the deserialized index passes
/// structural verification. Returns `None` on any failure (missing file, parse error, key
/// mismatch, verify violation) — the caller cold-indexes.
///
/// Quarantines (renames aside to `*.corrupt`) a file that fails JSON parsing or
/// [`Index::verify`] so it does not poison the next launch. A key-mismatch or missing file
/// is NOT quarantined — the file is valid but stale and will be overwritten by the next `save`.
#[must_use]
pub fn load(root: &Utf8Path, expected_key: &CacheKey) -> Option<LoadedCache> {
    let path = root.join(".gdls").join(cache_file_name());

    let bytes = std::fs::read(path.as_std_path()).ok()?;

    let cache: CacheFile = match serde_json::from_slice(&bytes) {
        Ok(c) => c,
        Err(e) => {
            log::warn!("cache: parse failed (cold-indexing): {e}");
            quarantine(&path);
            return None;
        }
    };

    if &cache.key != expected_key {
        log::info!("cache: key mismatch (cold-indexing)");
        return None;
    }

    let index = Index::from_cache(cache.index);

    // Structural self-check: paths/ids consistent, registry/depgraph reference only known FileIds.
    if let Err(violations) = index.verify() {
        log::warn!(
            "cache: index verify failed ({} violations), cold-indexing",
            violations.len()
        );
        quarantine(&path);
        return None;
    }

    // The scene index has no structural cross-table invariants to verify (it's a flat map + rebuilt
    // reverse maps); `from_cache` re-derives the reverse maps from the stored scenes, so a corrupt
    // scene entry can at worst be a partial-but-valid Scene (parse_scene is total). No quarantine
    // path needed beyond the whole-file JSON-parse guard above.
    let scenes = SceneIndex::from_cache(cache.scenes);

    // The asset index is a flat set of res:// paths with no cross-table invariants; `from_cache` is
    // total (it can't fail), so no quarantine path beyond the whole-file JSON-parse guard above.
    let assets = AssetIndex::from_cache(cache.assets);

    Some(LoadedCache {
        index,
        files: cache.files,
        scenes,
        assets,
    })
}

// ---------------------------------------------------------------------------
// Quarantine helper.
// ---------------------------------------------------------------------------

/// Rename `path` to `<path>.corrupt` so a garbled cache file is set aside rather than left in
/// place to poison every subsequent launch. Tolerates a failed rename (two concurrent processes
/// could both try to quarantine the same file; whichever loses is fine).
fn quarantine(path: &Utf8Path) {
    let corrupt = Utf8PathBuf::from(format!("{}.corrupt", path.as_str()));
    if let Err(e) = std::fs::rename(path.as_std_path(), corrupt.as_std_path()) {
        log::warn!("cache: quarantine rename failed (non-fatal): {e}");
    }
}
