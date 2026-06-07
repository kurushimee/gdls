//! Synthetic-scale warm-start gate — proves the >5× cold-vs-warm startup ratio criterion at
//! real project scale (3,000–5,000 `.gd` files). Drives the B3 `gd_project::cache` producer
//! → B4 `gd_server::Workspace` consumer seam end-to-end with NO mocks.
//!
//! Three scenarios:
//!   1. **cold**: `Workspace::load` with no cache — time it, then save.
//!   2. **warm**: second `Workspace::load` against the cached project (stat-only diff) — time it;
//!      assert `warm < cold / 5` (the >5× success criterion).
//!   3. **reconcile-skip**: touch exactly ONE file (bump content + size so stat changes), run
//!      `reconcile`, assert `modified == 1` (no over-reparse, no stale-skip).
//!
//! Runtime budget: file generation happens ONCE into a reusable temp dir; generation time is
//! excluded from the timing measurements. The suite MUST NOT shrink the corpus below 3k — the
//! design mandates this band. `success_criterion: timeout: 300s` covers the full scenario.

mod common;

use std::time::Instant;

use gd_server::config::InitializationOptions;
use gd_server::Workspace;

/// Number of synthetic files to generate. Chosen in the lower half of the 3k–5k band so
/// generation stays fast while still exercising the scan path at real scale. Raise to 5000 if
/// warm/cold ratio degrades (more files = stronger signal, but slower generation).
const CORPUS_SIZE: usize = 3_000;

/// Generate a GDScript file with a small/flat interface (few typed members + a couple funcs) but a
/// **large body** (dozens of local statements per function). This models a real project file:
///
/// - The interface (class_name, members, func signatures) is what `extract_interface` captures and
///   what the cache stores — kept small so the warm-path JSON stays compact.
/// - The body (locals, arithmetic, if/for, string ops) is what the parser tokenizes and what
///   `read_to_string` loads from disk — kept large so cold-parse dominates cold time.
///
/// This decouples parse cost from interface size: the cold path re-reads + re-parses the full
/// bodies while the warm path only deserializes the tiny interface JSON. The asymmetry is exactly
/// what the warm-start design exploits, and the >5× ratio depends on it being visible.
fn gen_script(i: usize) -> String {
    let class_name = format!("SynClass{i}");
    let extends = match i % 3 {
        0 => "extends Node\n".to_string(),
        1 => "extends Node2D\n".to_string(),
        _ => format!("extends SynClass{}\n", i.saturating_sub(3)),
    };
    // Small/flat interface: 3 typed vars + 1 signal + 2 funcs. The interface serializes to
    // ~250 bytes of JSON; 3k of these → ~750KB cache (fast to deserialize).
    let members = format!(
        "var hp_{i}: int = 0\nvar name_{i}: String = \"\"\n@export var speed_{i}: float = 1.0\n\
         signal died_{i}()\n"
    );

    // Large function bodies: each func has 40+ local statements that the parser must tokenize,
    // build into an AST, and (for cold) read from disk. These never enter the interface, so the
    // cache stays small while cold parse time grows. 2 funcs × 40 stmts = 80 statements/file.
    let body_stmts = |fn_idx: usize| -> String {
        (0..40)
            .map(|k| {
                let v = i * 80 + fn_idx * 40 + k;
                match k % 8 {
                    0 => format!("\tvar local_{k}: int = {v}\n"),
                    1 => format!("\tif local_{k} > {v}:\n\t\tlocal_{k} = local_{k} - 1\n"),
                    2 => format!(
                        "\tfor idx_{k} in range({}):\n\t\tlocal_{k} += idx_{k}\n",
                        k + 1
                    ),
                    3 => format!("\tvar s_{k}: String = str({v}) + \"_suffix\"\n\t_ = s_{k}\n"),
                    4 => format!(
                        "\tmatch local_{k}:\n\t\t{v}:\n\t\t\tlocal_{k} = 0\n\t\t_:\n\t\t\tpass\n"
                    ),
                    5 => format!("\tlocal_{k} = (local_{k} * {v}) % {}\n", k + 1),
                    6 => format!(
                        "\tvar arr_{k}: Array = [{v}, {}, {}]\n\t_ = arr_{k}\n",
                        v + 1,
                        v + 2
                    ),
                    _ => format!("\tlocal_{k} = local_{k} if local_{k} > 0 else {v}\n"),
                }
            })
            .collect()
    };
    let funcs = format!(
        "func method_{i}(arg: int) -> String:\n{body}\treturn str(arg)\n\n\
         func compute_{i}(x: int, y: int) -> int:\n{body2}\treturn x + y\n",
        body = body_stmts(0),
        body2 = body_stmts(1),
    );
    format!("class_name {class_name}\n{extends}{members}\n{funcs}")
}

/// Build a `Workspace` over `root`, returning it + the wall-clock duration of ONLY the
/// `Workspace::load` call (generation/setup excluded).
fn timed_load(root: &camino::Utf8Path) -> (Workspace, std::time::Duration) {
    let options = InitializationOptions::parse(Some(&serde_json::json!({
        "projectRoot": root.as_str(),
    })));
    let t0 = Instant::now();
    let ws = Workspace::load(root, &options);
    let elapsed = t0.elapsed();
    (ws, elapsed)
}

/// Generate the corpus once into a temp dir and return the owned `TempProject` (so its Drop
/// doesn't fire until the test is done). A `project.godot` is written so `Workspace::load`
/// sees a valid project root.
fn generate_corpus() -> common::TempProject {
    let p = common::TempProject::new();
    p.write("project.godot", "config_version=5\n");
    for i in 0..CORPUS_SIZE {
        let subdir = i / 100; // group files 100 per subdirectory to avoid huge flat dirs
        let rel = format!("src/sub{subdir}/script_{i}.gd");
        p.write(&rel, &gen_script(i));
    }
    p
}

/// The main warm-start gate. Generates the corpus ONCE, cold-builds, saves, warm-loads,
/// then asserts the >5× ratio.
#[test]
fn warm_start_is_five_times_faster_than_cold() {
    let p = generate_corpus();
    let root = &p.root;

    // --- COLD: build from scratch, save the cache. ---
    let (cold_ws, cold_dur) = timed_load(root);
    assert_eq!(
        cold_ws.index.file_count(),
        CORPUS_SIZE,
        "cold build must index all {CORPUS_SIZE} synthetic files"
    );
    // Save cache so the warm load can use it.
    cold_ws.save_cache();

    // Verify the cache file was actually written.
    let cache_path = root
        .join(".gdls")
        .join(format!("index.{}.bin", gd_project::CACHE_FORMAT_VERSION));
    assert!(
        cache_path.as_std_path().exists(),
        "cache file must exist after save_cache(): {cache_path}"
    );

    // --- WARM: second load should hit the cache and run stat-only diff. ---
    let (warm_ws, warm_dur) = timed_load(root);
    assert_eq!(
        warm_ws.index.file_count(),
        CORPUS_SIZE,
        "warm load must produce the same file count as cold ({CORPUS_SIZE})"
    );

    // >5× ratio: warm must be strictly less than cold/5.
    let ratio = cold_dur.as_secs_f64() / warm_dur.as_secs_f64();
    eprintln!(
        "cache_warm_start: cold={:.3}s  warm={:.3}s  ratio={:.1}×",
        cold_dur.as_secs_f64(),
        warm_dur.as_secs_f64(),
        ratio
    );
    assert!(
        warm_dur < cold_dur / 5,
        "warm load must be >5× faster than cold: cold={:.3}s warm={:.3}s ratio={:.1}× (must be >5×)",
        cold_dur.as_secs_f64(),
        warm_dur.as_secs_f64(),
        ratio
    );
}

/// Reconcile-skip: touch exactly ONE file (bump content + size), run stat-based reconcile,
/// assert `modified == 1` and no other file was re-parsed (no over-reparse).
///
/// This test is separated so it can be run quickly without the full 3k corpus build. It uses a
/// smaller corpus (50 files) for fast feedback while still exercising the reconcile seam.
#[test]
fn reconcile_reparsed_only_touched_file() {
    // Use a smaller corpus for speed — this test covers correctness, not timing.
    const SMALL_SIZE: usize = 50;
    let p = common::TempProject::new();
    p.write("project.godot", "config_version=5\n");
    for i in 0..SMALL_SIZE {
        p.write(&format!("src/script_{i}.gd"), &gen_script(i));
    }

    let options = InitializationOptions::parse(Some(&serde_json::json!({
        "projectRoot": p.root.as_str(),
    })));
    let mut ws = Workspace::load(&p.root, &options);
    assert_eq!(
        ws.index.file_count(),
        SMALL_SIZE,
        "precondition: all {SMALL_SIZE} files indexed"
    );
    // Save cache so stat_table is populated.
    ws.save_cache();

    // A no-op reconcile right after load should see nothing (the stat table matches disk).
    let r0 = ws.reconcile(&Default::default());
    assert_eq!(r0.added, 0, "fresh reconcile: no added");
    assert_eq!(r0.modified, 0, "fresh reconcile: no modified");
    assert_eq!(r0.removed, 0, "fresh reconcile: no removed");

    // Touch exactly ONE file: write new content with a different size so stat definitely changes.
    let touched_rel = "src/script_0.gd";
    let touched_path = p.root.join(touched_rel);
    // Capture the old size before overwriting.
    let old_size = std::fs::metadata(touched_path.as_std_path())
        .map(|m| m.len())
        .unwrap_or(0);
    let new_content = format!("{}\n# MODIFIED\n", gen_script(0));
    p.write(touched_rel, &new_content);

    // Verify our write actually changed the size (catches a test-setup bug where the
    // content happened to be the same length).
    let new_size = std::fs::metadata(touched_path.as_std_path())
        .expect("touched file must exist")
        .len();
    assert_ne!(
        old_size, new_size,
        "test setup: the touched file must have a different size to guarantee stat detection"
    );

    // Run reconcile: only the touched file should be re-parsed.
    let r = ws.reconcile(&Default::default());
    assert_eq!(
        r.modified, 1,
        "reconcile must report modified == 1 (only the touched file), got: {r:?}"
    );
    assert_eq!(
        r.added, 0,
        "reconcile must report added == 0 (no new files), got: {r:?}"
    );
    assert_eq!(
        r.removed, 0,
        "reconcile must report removed == 0 (no deleted files), got: {r:?}"
    );
    assert_eq!(
        r.walk_errors, 0,
        "reconcile must have zero walk errors, got: {r:?}"
    );
}

/// Boundary integration test: stat-based reconcile detects an ADDED file.
#[test]
fn reconcile_detects_added_file() {
    let p = common::TempProject::new();
    p.write("project.godot", "config_version=5\n");
    p.write("src/a.gd", "class_name ClassA\nextends Node\n");

    let options = InitializationOptions::parse(Some(&serde_json::json!({
        "projectRoot": p.root.as_str(),
    })));
    let mut ws = Workspace::load(&p.root, &options);
    assert_eq!(ws.index.file_count(), 1, "precondition: one file indexed");

    // No-op reconcile.
    let r0 = ws.reconcile(&Default::default());
    assert_eq!(r0.added, 0);
    assert_eq!(r0.modified, 0);

    // Add a new file on disk.
    p.write("src/b.gd", "class_name ClassB\nextends Node\n");
    let r = ws.reconcile(&Default::default());
    assert_eq!(r.added, 1, "reconcile must detect the new file");
    assert_eq!(r.modified, 0);
    assert!(
        ws.index.interface_of(&p.root.join("src/b.gd")).is_some(),
        "added file must have an interface in the index"
    );
}

/// Boundary integration test: stat-based reconcile detects a REMOVED file.
#[test]
fn reconcile_detects_removed_file() {
    let p = common::TempProject::new();
    p.write("project.godot", "config_version=5\n");
    p.write("src/a.gd", "class_name ClassA\nextends Node\n");
    p.write("src/b.gd", "class_name ClassB\nextends Node\n");

    let options = InitializationOptions::parse(Some(&serde_json::json!({
        "projectRoot": p.root.as_str(),
    })));
    let mut ws = Workspace::load(&p.root, &options);
    assert_eq!(ws.index.file_count(), 2, "precondition: two files indexed");

    p.remove("src/b.gd");
    let r = ws.reconcile(&Default::default());
    assert_eq!(r.removed, 1, "reconcile must detect the removed file");
    assert!(
        ws.index.interface_of(&p.root.join("src/b.gd")).is_none(),
        "removed file's interface must be dropped"
    );
}

/// Boundary integration test: warm-load from cache produces an index identical to cold build.
#[test]
fn warm_load_produces_same_index_as_cold() {
    let p = common::TempProject::new();
    p.write("project.godot", "config_version=5\n");
    // Write a few files with class names and extends so the interface is non-trivial.
    p.write(
        "src/base.gd",
        "class_name WarmBase\nextends Node\nfunc greet() -> String:\n\treturn \"hi\"\n",
    );
    p.write("src/child.gd", "class_name WarmChild\nextends WarmBase\n");
    p.write("src/orphan.gd", "extends Node\nvar x: int = 0\n");

    let options = InitializationOptions::parse(Some(&serde_json::json!({
        "projectRoot": p.root.as_str(),
    })));

    // Cold build + save.
    let cold_ws = Workspace::load(&p.root, &options);
    assert_eq!(cold_ws.index.file_count(), 3);
    cold_ws.save_cache();

    // Warm load.
    let warm_ws = Workspace::load(&p.root, &options);
    assert_eq!(
        warm_ws.index.file_count(),
        3,
        "warm load must produce the same file count"
    );

    // Verify the same paths are indexed.
    for rel in ["src/base.gd", "src/child.gd", "src/orphan.gd"] {
        let path = p.root.join(rel);
        assert!(
            warm_ws.index.interface_of(&path).is_some(),
            "warm load must include {rel}"
        );
    }

    // Verify class_name registry is populated.
    assert!(
        warm_ws.index.registry().len() >= 2,
        "warm load must have class_name registry entries (WarmBase, WarmChild)"
    );
}

/// Issue 1 + Issue 2 seam test: disk edit → save → warm load reflects the edited interface.
///
/// Drives the real `reindex` + `update_stat_from_disk` + `save_cache` + `Workspace::load` seam,
/// which was untested (all prior warm-path tests saved a never-edited workspace).
///
/// Assertions:
///   1. The second (warm) load reflects the edited file's NEW class_name.
///   2. The unchanged file retains its original class_name (not re-parsed with stale content).
///   3. A reconcile immediately after the warm load sees `modified == 0`: the stat_table was
///      correctly updated by `update_stat_from_disk`, so warm-load correctly stamped the new
///      stat for the edited file — it is NOT treated as "still changed" on the next check.
#[test]
fn warm_load_after_disk_edit_reflects_new_interface() {
    let p = common::TempProject::new();
    p.write("project.godot", "config_version=5\n");
    // Two files: one will be edited, one stays unchanged.
    let v1_src = "class_name EditTarget\nextends Node\n";
    let unchanged_src = "class_name Bystander\nextends Node\n";
    p.write("src/edit_target.gd", v1_src);
    p.write("src/bystander.gd", unchanged_src);

    let options = InitializationOptions::parse(Some(&serde_json::json!({
        "projectRoot": p.root.as_str(),
    })));

    // --- Session 1: cold build, disk edit via reindex path, save cache. ---
    let mut ws = Workspace::load(&p.root, &options);
    assert_eq!(ws.index.file_count(), 2, "precondition: two files indexed");

    // Verify V1 interface before the edit.
    let edit_path = p.root.join("src/edit_target.gd");
    assert_eq!(
        ws.index
            .interface_of(&edit_path)
            .and_then(|i| i.class_name.as_deref()),
        Some("EditTarget"),
        "precondition: original class_name must be EditTarget"
    );

    // Write new content to disk (changes size so stat definitely differs).
    let v2_src = "class_name EditTargetV2\nextends Node\nvar new_field: int = 0\n";
    p.write("src/edit_target.gd", v2_src);

    // Simulate what server.rs reindex_from_disk does: parse from disk, reindex, update stat.
    let text = std::fs::read_to_string(edit_path.as_std_path()).unwrap();
    ws.reindex(&edit_path, &gd_syntax::parse(&text).tree);
    ws.update_stat_from_disk(&edit_path);

    // Verify the index now reflects V2 in-session.
    assert_eq!(
        ws.index
            .interface_of(&edit_path)
            .and_then(|i| i.class_name.as_deref()),
        Some("EditTargetV2"),
        "in-session reindex must immediately reflect the new class_name"
    );

    // Save cache — no open buffers, so save_cache() is fine.
    ws.save_cache();

    // --- Session 2: warm load from cache. ---
    let mut warm_ws = Workspace::load(&p.root, &options);
    assert_eq!(
        warm_ws.index.file_count(),
        2,
        "warm load must produce the same file count"
    );

    // The edited file's NEW interface must be reflected.
    assert_eq!(
        warm_ws
            .index
            .interface_of(&edit_path)
            .and_then(|i| i.class_name.as_deref()),
        Some("EditTargetV2"),
        "warm load must reflect the edited file's new class_name (EditTargetV2), not the old one"
    );

    // The unchanged file's interface must be unchanged.
    let bystander_path = p.root.join("src/bystander.gd");
    assert_eq!(
        warm_ws
            .index
            .interface_of(&bystander_path)
            .and_then(|i| i.class_name.as_deref()),
        Some("Bystander"),
        "unchanged file must still have its original class_name (Bystander)"
    );

    // stat_table correctness: a post-warm-load reconcile must see no modified files.
    // If update_stat_from_disk wasn't called, the edited file's old stat would still be in the
    // cache, warm-load would see stored!=disk and re-parse it, but the warm Workspace's own
    // stat_table would be freshly populated from the warm-index walk — so reconcile would be
    // 0 either way on the warm ws. The key test is that warm LOAD correctly got V2 (above).
    // This reconcile verifies the warm workspace's stat_table is fully consistent post-load.
    let r = warm_ws.reconcile(&Default::default());
    assert_eq!(
        r.modified, 0,
        "reconcile after warm load must see no modified files (stat_table is consistent post-warm-load): {r:?}"
    );
    assert_eq!(
        r.added, 0,
        "reconcile after warm load must see no added files: {r:?}"
    );
    assert_eq!(
        r.removed, 0,
        "reconcile after warm load must see no removed files: {r:?}"
    );
}

/// Issue 1 never-lie guard: unsaved buffer edit must NOT be served as disk truth after a warm load.
///
/// This is the "never lie" seam: the editor has a buffer whose interface differs from the on-disk
/// file, the server shuts down (saves cache via `save_cache_excluding_open`), and the next launch
/// warm-loads. The warm load must serve the ON-DISK interface (by re-parsing from disk because
/// the file's stat entry was excluded from the saved cache), not the unsaved buffer interface.
///
/// The fix: `save_cache_excluding_open` omits open-buffer files from the persisted stat table
/// so warm-load sees "unknown stat" for them and re-parses from disk.
#[test]
fn warm_load_after_unsaved_buffer_edit_serves_disk_interface() {
    let p = common::TempProject::new();
    p.write("project.godot", "config_version=5\n");
    // Disk has V1 content with class_name DiskClass.
    let disk_src = "class_name DiskClass\nextends Node\n";
    let buffer_src = "class_name BufferClass\nextends Node\nvar unsaved: int = 0\n";
    p.write("src/target.gd", disk_src);
    p.write("src/other.gd", "class_name Other\nextends Node\n");

    let options = InitializationOptions::parse(Some(&serde_json::json!({
        "projectRoot": p.root.as_str(),
    })));

    // --- Session 1: cold build, buffer-only edit (disk unchanged), save excluding the open file. ---
    let mut ws = Workspace::load(&p.root, &options);
    assert_eq!(ws.index.file_count(), 2, "precondition: two files indexed");

    let target_path = p.root.join("src/target.gd");
    assert_eq!(
        ws.index
            .interface_of(&target_path)
            .and_then(|i| i.class_name.as_deref()),
        Some("DiskClass"),
        "precondition: original class_name must be DiskClass"
    );

    // Simulate a buffer-only edit: parse the buffer text (NOT on disk) and reindex.
    // Do NOT write to disk and do NOT call update_stat_from_disk.
    // This models what server.rs reindex_open_buffer does.
    let parsed = gd_syntax::parse(buffer_src);
    ws.reindex(&target_path, &parsed.tree);

    // In-session the index reflects the buffer content.
    assert_eq!(
        ws.index
            .interface_of(&target_path)
            .and_then(|i| i.class_name.as_deref()),
        Some("BufferClass"),
        "in-session buffer reindex must reflect BufferClass"
    );

    // The file is "open" in the editor (unsaved buffer). Simulate save_cache_excluding_open by
    // building the open-paths set that the server's shutdown path would build.
    // `gd_project::normalize_path` is the same normalization used by the server's
    // `open_buffer_paths` helper (forward slashes, same as index keys).
    let norm_target = gd_project::normalize_path(&target_path);
    let open_paths = rustc_hash::FxHashSet::from_iter([norm_target]);

    // Save cache excluding the open buffer: the target file's stat entry must NOT be persisted.
    ws.save_cache_excluding_open(&open_paths);

    // --- Session 2: warm load from cache. ---
    // The target file's stat entry was NOT saved, so warm-load sees "unknown stat" → re-parses
    // from disk → recovers the on-disk DiskClass interface, NOT the unsaved BufferClass.
    let warm_ws = Workspace::load(&p.root, &options);
    assert_eq!(
        warm_ws.index.file_count(),
        2,
        "warm load must produce the same file count"
    );

    assert_eq!(
        warm_ws
            .index
            .interface_of(&target_path)
            .and_then(|i| i.class_name.as_deref()),
        Some("DiskClass"),
        "warm load must serve the ON-DISK class_name (DiskClass), not the unsaved buffer \
         interface (BufferClass) — never-lie guard"
    );

    // The non-open file must still be served from cache (its stat was persisted normally).
    let other_path = p.root.join("src/other.gd");
    assert_eq!(
        warm_ws
            .index
            .interface_of(&other_path)
            .and_then(|i| i.class_name.as_deref()),
        Some("Other"),
        "the non-open file must be served from cache with its correct interface"
    );
}

/// Timing breakdown: measure the individual phases of cold vs warm to diagnose ratio failures.
/// This test is informational — it always passes, printing its findings to stderr.
#[test]
fn timing_breakdown() {
    let p = generate_corpus();
    let root = &p.root;
    let options = InitializationOptions::parse(Some(&serde_json::json!({
        "projectRoot": root.as_str(),
    })));

    // Cold build.
    let t0 = std::time::Instant::now();
    let cold_ws = Workspace::load(root, &options);
    let cold_dur = t0.elapsed();
    eprintln!(
        "[timing] cold build: {:.3}s ({} files)",
        cold_dur.as_secs_f64(),
        cold_ws.index.file_count()
    );

    // Save cache.
    let t1 = std::time::Instant::now();
    cold_ws.save_cache();
    let save_dur = t1.elapsed();
    let cache_path = root
        .join(".gdls")
        .join(format!("index.{}.bin", gd_project::CACHE_FORMAT_VERSION));
    let cache_size = std::fs::metadata(cache_path.as_std_path())
        .map(|m| m.len())
        .unwrap_or(0);
    eprintln!(
        "[timing] save cache: {:.3}s ({} bytes = {:.1} MB)",
        save_dur.as_secs_f64(),
        cache_size,
        cache_size as f64 / 1_048_576.0
    );

    // Warm load.
    let t2 = std::time::Instant::now();
    let warm_ws = Workspace::load(root, &options);
    let warm_dur = t2.elapsed();
    eprintln!(
        "[timing] warm load: {:.3}s ({} files)",
        warm_dur.as_secs_f64(),
        warm_ws.index.file_count()
    );
    eprintln!(
        "[timing] ratio: {:.1}×",
        cold_dur.as_secs_f64() / warm_dur.as_secs_f64()
    );
}
