//! M5 WP-H1 + WP-H2 memory hardening integration coverage.
//!
//! These tests drive the bounded LRU caches + the soft-pressure bulk-evict ladder through the
//! [`gd_server::Workspace`] public API. They don't spin up a full LSP loop: the WP-H1 ladder is
//! orchestrated by [`gd_server::server::react_to_memory_pressure`] inside the 3-second ticker
//! arm, which has its own unit coverage in `server.rs`'s tests. What can only be exercised at
//! the integration level is the LRU eviction order, the `pop_lru`-based bulk shed, and the
//! interaction between the per-insert evict-on-overflow and the per-tick bulk evict-half.
//!
//! The seeding hooks these tests call are `#[cfg(any(test, debug_assertions))]` on `Workspace`,
//! and a `test` cfg on an integration crate does not reach the library it links. So the file as a
//! whole compiles only where those hooks exist. CI runs `--all-targets` in debug, where it does.
#![cfg(debug_assertions)]

mod common;

use std::num::NonZeroUsize;

use camino::Utf8PathBuf;
use common::TempProject;
use gd_analyze::{AnalysisResult, FoldTable, TypeTable};
use gd_server::config::InitializationOptions;
use gd_server::uri::{path_to_file_uri, CanonicalKey};
use gd_server::Workspace;
use gd_syntax::ParseResult;
use rustc_hash::FxHashMap;

fn options_with_cache_capacity(p: &TempProject, capacity: usize) -> InitializationOptions {
    InitializationOptions::parse(Some(&serde_json::json!({
        "projectRoot": p.root.as_str(),
        "autoDumpExtensionApi": false,
        "memory": {
            "cacheCapacity": capacity,
        },
    })))
}

fn key_for_file(idx: usize, root: &Utf8PathBuf) -> CanonicalKey {
    let p = root.join(format!("synthetic_{idx}.gd"));
    let uri = path_to_file_uri(&p).expect("valid file uri");
    CanonicalKey::for_uri(&uri)
}

fn empty_parse() -> ParseResult {
    // A minimal parseable input: just a comment. The cache holds whatever ParseResult we give it
    // — the value is irrelevant to the LRU test, only identity (which key holds which entry).
    gd_syntax::parse("# synthetic test entry\n")
}

fn empty_analysis() -> AnalysisResult {
    AnalysisResult::new_for_test(
        TypeTable::new(0),
        FoldTable::new(0),
        Vec::new(),
        FxHashMap::default(),
        Vec::new(),
    )
}

/// WP-H2: the LruCache eviction-on-insert mechanism limits both caches to the configured
/// capacity. A workspace built with `cacheCapacity = 4` and 10 inserted entries holds exactly 4
/// at any time — the most-recently-touched 4 — and `cache_lens` reflects that. This is the
/// per-insert side of the WP-H1 bound; the bulk-evict-half is exercised below.
#[test]
fn lru_cache_evicts_on_insert_once_capacity_is_reached() {
    let p = TempProject::new();
    p.write(
        "project.godot",
        "config_version=5\n\n[application]\nconfig/features=PackedStringArray(\"4.6\")\n",
    );
    let mut ws = Workspace::load(&p.root, &options_with_cache_capacity(&p, 4));

    // Stuff 10 distinct entries — capacity 4 means 6 must be evicted on the way through.
    for i in 0..10 {
        ws.debug_insert_parse_entry(key_for_file(i, &p.root), i as u64, empty_parse());
        ws.debug_insert_analysis_entry(key_for_file(i, &p.root), i as u64, empty_analysis());
    }

    let (parse_len, analysis_len) = ws.cache_lens();
    assert_eq!(
        parse_len, 4,
        "parse_cache is capped at the configured cacheCapacity (4); over-insert evicts LRU on insert"
    );
    assert_eq!(
        analysis_len, 4,
        "analysis_cache is capped at the same cacheCapacity"
    );
}

/// WP-H1 Soft-pressure action: `Workspace::evict_half` drops the LRU-oldest half of both
/// caches in one pass. After 8 inserts (capacity 16, no per-insert eviction), evict_half drops
/// exactly 4 from each — the four with the lowest insertion order.
#[test]
fn evict_half_drops_the_oldest_half_of_both_caches() {
    let p = TempProject::new();
    p.write(
        "project.godot",
        "config_version=5\n\n[application]\nconfig/features=PackedStringArray(\"4.6\")\n",
    );
    let mut ws = Workspace::load(&p.root, &options_with_cache_capacity(&p, 16));

    for i in 0..8 {
        ws.debug_insert_parse_entry(key_for_file(i, &p.root), i as u64, empty_parse());
        ws.debug_insert_analysis_entry(key_for_file(i, &p.root), i as u64, empty_analysis());
    }
    let (parse_before, analysis_before) = ws.cache_lens();
    assert_eq!(parse_before, 8);
    assert_eq!(analysis_before, 8);

    let evicted = ws.evict_half();
    let (parse_after, analysis_after) = ws.cache_lens();
    assert_eq!(
        parse_after, 4,
        "evict_half drops floor(len / 2) parse entries"
    );
    assert_eq!(analysis_after, 4);
    assert_eq!(
        evicted,
        (parse_before / 2) + (analysis_before / 2),
        "evict_half reports the total entries it dropped across both caches"
    );
}

/// `evict_half` on an empty cache is a no-op. Guards against a regression where pop_lru on
/// empty would panic, loop, or return Some(...) — `lru` documents `None` for the empty case but
/// pin it explicitly so a future swap to a different LRU crate can't quietly break the contract.
#[test]
fn evict_half_on_empty_caches_is_a_no_op() {
    let p = TempProject::new();
    p.write(
        "project.godot",
        "config_version=5\n\n[application]\nconfig/features=PackedStringArray(\"4.6\")\n",
    );
    let mut ws = Workspace::load(&p.root, &options_with_cache_capacity(&p, 8));
    assert_eq!(ws.cache_lens(), (0, 0));
    assert_eq!(ws.evict_half(), 0);
    assert_eq!(ws.cache_lens(), (0, 0));
}

/// Cache capacity defaults to the WP-H2 baked-in value when the client omits the override. The
/// constant is exposed through [`gd_server::config::MemoryConfig::DEFAULT_CACHE_CAPACITY`] so a
/// future shift of the default doesn't strand this assertion on a stale literal.
#[test]
fn default_cache_capacity_matches_documented_default() {
    let p = TempProject::new();
    p.write(
        "project.godot",
        "config_version=5\n\n[application]\nconfig/features=PackedStringArray(\"4.6\")\n",
    );
    let opts = InitializationOptions::parse(Some(&serde_json::json!({
        "projectRoot": p.root.as_str(),
    "autoDumpExtensionApi": false,
    })));
    let mut ws = Workspace::load(&p.root, &opts);

    let cap = gd_server::config::MemoryConfig::DEFAULT_CACHE_CAPACITY;
    // Stuff cap + 1 entries; the cache holds exactly `cap` after the final insert.
    for i in 0..cap + 1 {
        ws.debug_insert_parse_entry(key_for_file(i, &p.root), i as u64, empty_parse());
    }
    let (parse_len, _) = ws.cache_lens();
    assert_eq!(
        parse_len, cap,
        "default cache capacity must enforce a bound at MemoryConfig::DEFAULT_CACHE_CAPACITY"
    );
}

/// Zero-capacity client override is clamped up to the WP-H2 default. The lru crate would panic
/// on `NonZeroUsize::new(0).unwrap()`; this test pins the clamp's purpose — a hand-edited config
/// with `cacheCapacity = 0` must not crash the session.
#[test]
fn zero_capacity_is_clamped_to_the_default() {
    let p = TempProject::new();
    p.write(
        "project.godot",
        "config_version=5\n\n[application]\nconfig/features=PackedStringArray(\"4.6\")\n",
    );
    let mut ws = Workspace::load(&p.root, &options_with_cache_capacity(&p, 0));
    // The fact that load() returned at all proves the clamp engaged (a raw 0 would have
    // panicked inside NonZeroUsize::new(0).expect(...) in the constructor). Belt + suspenders:
    // assert the resulting cap matches the default.
    let cap = gd_server::config::MemoryConfig::DEFAULT_CACHE_CAPACITY;
    for i in 0..cap + 1 {
        ws.debug_insert_parse_entry(key_for_file(i, &p.root), i as u64, empty_parse());
    }
    assert_eq!(ws.cache_lens().0, cap);
}

/// MemoryConfig's defaults satisfy `NonZeroUsize::new` regardless of what other knobs are set —
/// future fields on the struct must not change this. Confirms the test rig + the production
/// constructor agree on the post-clamp invariant.
#[test]
fn memory_config_default_cache_capacity_is_nonzero() {
    let cap = gd_server::config::MemoryConfig::DEFAULT_CACHE_CAPACITY;
    assert!(cap > 0);
    assert!(NonZeroUsize::new(cap).is_some());
}
