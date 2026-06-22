//! Tests for `gd_project::cache` — round-trip, failure modes, and multi-writer race.
//!
//! These tests exercise the full `cache::save` → `cache::load` seam against real disk. They use
//! `tempfile::tempdir()` so every invocation is isolated. The concurrency test guards the
//! "two gdls processes never corrupt the cache" success criterion.

use camino::{Utf8Path, Utf8PathBuf};
use gd_project::cache::{self, CacheKey, FileStat, CACHE_FORMAT_VERSION};
use gd_project::Index;
use std::fs;
use std::sync::{Arc, Barrier};
use std::thread;
use tempfile::tempdir;

// ---------------------------------------------------------------------------
// Helper: build a minimal project in a tempdir and return the root.
// ---------------------------------------------------------------------------

fn make_project(dir: &tempfile::TempDir) -> Utf8PathBuf {
    let root = Utf8PathBuf::from(dir.path().to_str().expect("UTF-8 temp path"));
    // Write a minimal project.godot so the fingerprint code has something to stat.
    fs::write(root.join("project.godot"), "config_version=5\n").expect("write project.godot");
    // A couple of real .gd files for the cold-indexer to find.
    fs::write(root.join("a.gd"), "extends Node\n").expect("write a.gd");
    fs::write(root.join("b.gd"), "class_name MyBase\nextends Node\n").expect("write b.gd");
    // A .tscn attaching a.gd, so the SceneIndex has a real relation to round-trip through the cache.
    fs::write(
        root.join("a.tscn"),
        "[gd_scene format=3]\n\n[ext_resource type=\"Script\" path=\"res://a.gd\" id=\"1\"]\n\n\
         [node name=\"Root\" type=\"Node\"]\nscript = ExtResource(\"1\")\n",
    )
    .expect("write a.tscn");
    root
}

/// Compute a project.godot fingerprint (size + mtime_ns as a combined hash) identical to what
/// `cache::project_godot_fingerprint` would produce. Exposed through the function itself.
fn project_godot_fingerprint(root: &Utf8Path) -> u64 {
    cache::project_godot_fingerprint(root)
}

/// Build a `CacheKey` for the given root using the current binary version + an empty NativeDb.
///
/// Tests use `NativeDb::empty()` (hash=0) because there's no built-in DB in the test binary.
/// The key's semantic correctness (hash matches the DB actually used) is what matters; here the
/// DB used to build the index is also empty, so the hash is consistently 0.
fn make_key(root: &Utf8Path) -> CacheKey {
    use gd_types::NativeDb;
    let db = NativeDb::empty();
    CacheKey {
        cache_format_version: CACHE_FORMAT_VERSION,
        gdls_version: env!("CARGO_PKG_VERSION").to_string(),
        native_db_content_hash: db.content_hash(),
        project_godot_fingerprint: project_godot_fingerprint(root),
    }
}

/// Build a `FileStat` list from the `.gd` files in `root` (no .gdls dir).
fn stat_gd_files(root: &Utf8Path) -> Vec<FileStat> {
    let mut stats = Vec::new();
    for entry in walkdir::WalkDir::new(root.as_std_path())
        .max_depth(2)
        .into_iter()
        .flatten()
    {
        let p = entry.path();
        if p.extension().and_then(|e| e.to_str()) == Some("gd") {
            if let (Some(utf8), Ok(meta)) = (Utf8Path::from_path(p), fs::metadata(p)) {
                stats.push(cache::stat_from_metadata(utf8.to_path_buf(), &meta));
            }
        }
    }
    stats
}

// ---------------------------------------------------------------------------
// Test 1: round-trip — warm index is cache_equivalent to cold build.
// ---------------------------------------------------------------------------

#[test]
fn round_trip_warm_equivalent_to_cold() {
    let tmp = tempdir().expect("tempdir");
    let root = make_project(&tmp);

    // Build a cold index and snapshot it.
    let cold = Index::build(&root);
    let scenes = gd_project::SceneIndex::build(&root);
    let assets = gd_project::AssetIndex::build(&root);
    let cold_file_count = cold.file_count();
    assert!(
        cold_file_count >= 2,
        "cold index should have found a.gd + b.gd"
    );

    let key = make_key(&root);
    let files = stat_gd_files(&root);
    cache::save(&root, &cold, &scenes, &assets, &files, key.clone());

    // The .gdls dir should exist, the cache file should be there.
    let cache_path = root.join(".gdls").join(cache::cache_file_name());
    assert!(cache_path.exists(), "cache file written");

    // Load it back.
    let loaded = cache::load(&root, &key).expect("load must succeed");

    // Warm must be structurally equivalent to cold.
    assert!(
        loaded.index.cache_equivalent(&cold),
        "warm-started index must equal the cold-built index"
    );

    // Scene index round-trips through the cache: a.tscn and its script reverse-map edge survive.
    assert_eq!(scenes.len(), 1, "cold scene index should have found a.tscn");
    assert_eq!(
        loaded.scenes.len(),
        scenes.len(),
        "warm scene index must equal the cold-built scene index"
    );
    assert!(
        loaded.scenes.scene("res://a.tscn").is_some(),
        "warm scene index must contain a.tscn"
    );
    assert!(
        loaded
            .scenes
            .scenes_attaching_script("res://a.gd")
            .any(|res| res == "res://a.tscn"),
        "warm scene index must rebuild the script→scene reverse-map edge"
    );

    // Bonus: rebuild cold again, confirm file_count unchanged (the cache file didn't re-enter the index).
    let cold2 = Index::build(&root);
    assert_eq!(
        cold2.file_count(),
        cold_file_count,
        "re-building the index must not pick up the .gdls cache file"
    );
}

// ---------------------------------------------------------------------------
// Test 2: corruption → cold fallback + quarantine (no panic).
// ---------------------------------------------------------------------------

#[test]
fn corrupt_file_yields_none_and_quarantines() {
    let tmp = tempdir().expect("tempdir");
    let root = make_project(&tmp);

    let key = make_key(&root);
    let cold = Index::build(&root);
    let scenes = gd_project::SceneIndex::build(&root);
    let assets = gd_project::AssetIndex::build(&root);
    let files = stat_gd_files(&root);
    cache::save(&root, &cold, &scenes, &assets, &files, key.clone());

    let cache_path = root.join(".gdls").join(cache::cache_file_name());
    assert!(cache_path.exists());

    // Garble the file (write garbage bytes).
    fs::write(&cache_path, b"\x00\x01\x02not json at all").expect("write garble");

    // load() must return None without panicking.
    let result = cache::load(&root, &key);
    assert!(
        result.is_none(),
        "corrupt file must yield None (cold fallback)"
    );

    // The file should be quarantined (renamed aside) — not left in place.
    assert!(
        !cache_path.exists(),
        "corrupt cache file must not remain after quarantine"
    );
    let corrupt_path = root
        .join(".gdls")
        .join(format!("{}.corrupt", cache::cache_file_name()));
    assert!(
        corrupt_path.exists(),
        "corrupt file must be quarantined alongside the original path"
    );
}

// ---------------------------------------------------------------------------
// Test 3: key mismatch → None (no quarantine, no panic).
// ---------------------------------------------------------------------------

#[test]
fn key_mismatch_yields_none_without_quarantine() {
    let tmp = tempdir().expect("tempdir");
    let root = make_project(&tmp);

    let save_key = make_key(&root);
    let cold = Index::build(&root);
    let scenes = gd_project::SceneIndex::build(&root);
    let assets = gd_project::AssetIndex::build(&root);
    let files = stat_gd_files(&root);
    cache::save(&root, &cold, &scenes, &assets, &files, save_key);

    // Load with a different gdls_version → key mismatch.
    let mismatched_key = CacheKey {
        cache_format_version: CACHE_FORMAT_VERSION,
        gdls_version: "0.0.0-wrong".to_string(),
        native_db_content_hash: {
            use gd_types::NativeDb;
            NativeDb::empty().content_hash()
        },
        project_godot_fingerprint: project_godot_fingerprint(&root),
    };

    let result = cache::load(&root, &mismatched_key);
    assert!(result.is_none(), "key mismatch must yield None");

    // The cache file must still be present (not quarantined — it's valid, just stale).
    let cache_path = root.join(".gdls").join(cache::cache_file_name());
    assert!(
        cache_path.exists(),
        "stale-but-valid file must not be quarantined on key mismatch"
    );
    let corrupt_path = root
        .join(".gdls")
        .join(format!("{}.corrupt", cache::cache_file_name()));
    assert!(!corrupt_path.exists(), "no quarantine for a key mismatch");
}

// ---------------------------------------------------------------------------
// Test 4: FileId stability — a path interned at a given FileId before save
//         resolves to the same FileId after load.
// ---------------------------------------------------------------------------

#[test]
fn file_id_stable_across_round_trip() {
    let tmp = tempdir().expect("tempdir");
    let root = make_project(&tmp);

    let cold = Index::build(&root);
    let scenes = gd_project::SceneIndex::build(&root);
    let assets = gd_project::AssetIndex::build(&root);
    let a_path = root.join("a.gd");
    let b_path = root.join("b.gd");

    let fid_a_before = cold.file_id(&a_path).expect("a.gd in cold index");
    let fid_b_before = cold.file_id(&b_path).expect("b.gd in cold index");

    let key = make_key(&root);
    let files = stat_gd_files(&root);
    cache::save(&root, &cold, &scenes, &assets, &files, key.clone());

    let loaded = cache::load(&root, &key).expect("load");

    let fid_a_after = loaded.index.file_id(&a_path).expect("a.gd in warm index");
    let fid_b_after = loaded.index.file_id(&b_path).expect("b.gd in warm index");

    assert_eq!(
        fid_a_before, fid_a_after,
        "a.gd must have the same FileId after round-trip"
    );
    assert_eq!(
        fid_b_before, fid_b_after,
        "b.gd must have the same FileId after round-trip"
    );
}

// ---------------------------------------------------------------------------
// Test 5: stat-delta — the loaded stat table matches on-disk state; after
//         changing a file's size its FileStat differs from the fresh stat.
// ---------------------------------------------------------------------------

#[test]
fn stat_delta_detects_size_change() {
    let tmp = tempdir().expect("tempdir");
    let root = make_project(&tmp);

    let cold = Index::build(&root);
    let scenes = gd_project::SceneIndex::build(&root);
    let assets = gd_project::AssetIndex::build(&root);
    let key = make_key(&root);

    // Snapshot stats before save.
    let files_before = stat_gd_files(&root);
    cache::save(&root, &cold, &scenes, &assets, &files_before, key.clone());

    let loaded = cache::load(&root, &key).expect("load");

    // The loaded stat table must match the snapshotted stats (size + mtime_ns).
    for stat_before in &files_before {
        let found = loaded
            .files
            .iter()
            .find(|s| s.path == stat_before.path)
            .expect("every saved path must appear in loaded stat table");
        assert_eq!(found.size, stat_before.size, "size must round-trip");
        assert_eq!(
            found.mtime_ns, stat_before.mtime_ns,
            "mtime_ns must round-trip"
        );
    }

    // Now mutate a.gd's SIZE (deterministic, unlike mtime which may have coarse resolution).
    let a_path = root.join("a.gd");
    fs::write(&a_path, "extends Node\n# extra content to change size\n").expect("write a.gd");
    let new_meta = fs::metadata(a_path.as_std_path()).expect("stat a.gd");
    let new_stat = cache::stat_from_metadata(a_path.clone(), &new_meta);

    // Find the saved stat for a.gd and assert it differs (size changed).
    let saved_stat = loaded
        .files
        .iter()
        .find(|s| s.path == a_path)
        .expect("a.gd in loaded stats");
    assert_ne!(
        saved_stat.size, new_stat.size,
        "mutating a.gd's content must change its size, flagging it for re-parse"
    );

    // b.gd was NOT changed — its saved stat must match current disk stat exactly.
    let b_path = root.join("b.gd");
    let b_meta = fs::metadata(b_path.as_std_path()).expect("stat b.gd");
    let b_current_stat = cache::stat_from_metadata(b_path.clone(), &b_meta);
    let b_saved_stat = loaded
        .files
        .iter()
        .find(|s| s.path == b_path)
        .expect("b.gd in loaded stats");
    assert_eq!(
        b_saved_stat.size, b_current_stat.size,
        "b.gd was not mutated — its size must be unchanged (only a.gd flagged for re-parse)"
    );
    assert_eq!(
        b_saved_stat.mtime_ns, b_current_stat.mtime_ns,
        "b.gd was not mutated — its mtime_ns must be unchanged"
    );
}

// ---------------------------------------------------------------------------
// Test 6: concurrent-writers race — two threads save() in a loop while a
//         third thread load()s; every load must yield None or a complete
//         valid cache. After join: zero quarantine files, final load succeeds.
// ---------------------------------------------------------------------------

#[test]
fn concurrent_writers_never_corrupt_the_cache() {
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    let tmp = tempdir().expect("tempdir");
    let root = make_project(&tmp);
    let root = Arc::new(root);

    // Pre-build a cold index and key to reuse across threads.
    let cold = Index::build(&root);
    let scenes = Arc::new(gd_project::SceneIndex::build(&root));
    let assets = Arc::new(gd_project::AssetIndex::build(&root));
    let key = Arc::new(make_key(&root));
    let files = Arc::new(stat_gd_files(&root));

    // Give the writers a head start — save once before the reader starts.
    cache::save(&root, &cold, &scenes, &assets, &files, (*key).clone());

    let barrier = Arc::new(Barrier::new(3));
    let stop = Arc::new(AtomicBool::new(false));
    // Count how many loads returned Some (so we can assert non-vacuous).
    let some_count = Arc::new(AtomicUsize::new(0));
    // Count how many loads returned a parse error / quarantine signal.
    let corrupt_count = Arc::new(AtomicUsize::new(0));

    // Writer thread factory.
    let make_writer = |root: Arc<Utf8PathBuf>,
                       cold: Arc<Index>,
                       scenes: Arc<gd_project::SceneIndex>,
                       assets: Arc<gd_project::AssetIndex>,
                       files: Arc<Vec<FileStat>>,
                       key: Arc<CacheKey>,
                       barrier: Arc<Barrier>,
                       stop: Arc<AtomicBool>| {
        thread::spawn(move || {
            barrier.wait();
            let mut iters = 0usize;
            while !stop.load(Ordering::Relaxed) || iters < 50 {
                cache::save(&root, &cold, &scenes, &assets, &files, (*key).clone());
                iters += 1;
                if iters >= 200 {
                    break;
                }
            }
        })
    };

    let cold = Arc::new(cold);

    let w1 = make_writer(
        root.clone(),
        cold.clone(),
        scenes.clone(),
        assets.clone(),
        files.clone(),
        key.clone(),
        barrier.clone(),
        stop.clone(),
    );
    let w2 = make_writer(
        root.clone(),
        cold.clone(),
        scenes.clone(),
        assets.clone(),
        files.clone(),
        key.clone(),
        barrier.clone(),
        stop.clone(),
    );

    // Reader thread.
    let root_r = root.clone();
    let key_r = key.clone();
    let barrier_r = barrier.clone();
    let stop_r = stop.clone();
    let some_count_r = some_count.clone();
    let corrupt_count_r = corrupt_count.clone();

    let reader = thread::spawn(move || {
        barrier_r.wait();
        let mut iters = 0usize;
        while !stop_r.load(Ordering::Relaxed) || iters < 100 {
            match cache::load(&root_r, &key_r) {
                Some(_) => {
                    some_count_r.fetch_add(1, Ordering::Relaxed);
                }
                None => {
                    // None is allowed (writer between writes, or key mismatch race).
                    // But if a .corrupt file appeared, that's a torn-read indicator.
                    let corrupt_path = root_r
                        .join(".gdls")
                        .join(format!("{}.corrupt", cache::cache_file_name()));
                    if corrupt_path.exists() {
                        corrupt_count_r.fetch_add(1, Ordering::Relaxed);
                    }
                }
            }
            iters += 1;
            if iters >= 300 {
                break;
            }
        }
        stop_r.store(true, Ordering::Relaxed);
    });

    reader.join().expect("reader thread");
    stop.store(true, Ordering::Relaxed);
    w1.join().expect("writer 1");
    w2.join().expect("writer 2");

    // Assert non-vacuous: the reader saw at least one valid load.
    assert!(
        some_count.load(Ordering::Relaxed) > 0,
        "reader must have observed at least one valid cache load (test is vacuous otherwise)"
    );

    // Assert no torn reads: zero quarantine files.
    assert_eq!(
        corrupt_count.load(Ordering::Relaxed),
        0,
        "no torn reads — atomic save must never let a reader see a partial write"
    );
    let corrupt_path = root
        .join(".gdls")
        .join(format!("{}.corrupt", cache::cache_file_name()));
    assert!(
        !corrupt_path.exists(),
        "no .corrupt quarantine file after concurrent writes (atomic rename guards against torn reads)"
    );

    // Assert final file is clean.
    let final_loaded = cache::load(&root, &key).expect("final load must succeed");
    assert!(
        final_loaded.index.verify().is_ok(),
        "finally loaded index must pass structural verify()"
    );
}
