//! Cache-coherence regressions surfaced while hardening M4's cross-file caching.
//!
//! These pin the content-addressed parse/analysis cache (the fix for the closed-file stale-nav
//! bug) and prove the WP-R2 cross-file member-cycle check fires through the *real*
//! `WorkspaceXFileQuery` + `CanonicalKey`-keyed cache — the seam the integration suite previously
//! only claimed to cover.

mod common;

use std::rc::Rc;

use common::TempProject;
use gd_server::config::InitializationOptions;
use gd_server::uri::{path_to_file_uri, CanonicalKey};
use gd_server::Workspace;

fn options(p: &TempProject) -> InitializationOptions {
    InitializationOptions::parse(Some(&serde_json::json!({
        "projectRoot": p.root.as_str(),
    "autoDumpExtensionApi": false,
    })))
}

fn key_for(p: &TempProject, rel: &str) -> CanonicalKey {
    let uri = path_to_file_uri(&p.root.join(rel)).expect("valid file uri");
    CanonicalKey::for_uri(&uri)
}

/// The closed-file nav bug: the parse cache used to key validity on `(uri, version)`
/// and the closed-file disk-read path always passed `version = 0`, so two *different* contents
/// under the same key served the first parse forever. Validity is now content-addressed, so
/// identical text hits the cache and changed text re-parses — even under one stable key.
#[test]
fn parse_cache_is_content_addressed_not_version_keyed() {
    let p = TempProject::new();
    p.write("project.godot", "config_version=5\n");
    let mut ws = Workspace::load(&p.root, &options(&p));
    let key = key_for(&p, "foo.gd");

    // Identical content → cache hit (same Rc, parsed exactly once).
    let first = ws.parse(&key, "var a = 1\n");
    let again = ws.parse(&key, "var a = 1\n");
    assert!(
        Rc::ptr_eq(&first, &again),
        "identical content under the same key must hit the cache"
    );

    // Changed content under the SAME key (the old version=0 collision) → fresh parse, not the
    // stale entry. This is precisely the closed-file-edited-on-disk path that used to lie.
    let changed = ws.parse(&key, "var bbb = 2\n");
    assert!(
        !Rc::ptr_eq(&first, &changed),
        "changed content must re-parse rather than serve the stale cached entry"
    );

    // Reverting to the original content is still a content hit on the now-current entry.
    let reverted = ws.parse(&key, "var bbb = 2\n");
    assert!(Rc::ptr_eq(&changed, &reverted));
}

/// A bailed analysis (WP-O3 governor cap exceeded / WP-O4 cancellation) returns **partial** side
/// tables, so it must NOT be cached: caching it would silently serve a truncated analysis to the
/// next hover/definition/references request on the unchanged file ("never lie"). Two consecutive
/// analyses of the same bailing content therefore each re-run (distinct `Rc`s) and the analysis
/// cache stays empty — unlike a normal result, which is served from cache (see
/// `analysis_cache_is_content_addressed`).
#[test]
fn a_bailed_analysis_is_not_cached() {
    let p = TempProject::new();
    p.write("project.godot", "config_version=5\n");
    let src = "extends Node\nfunc f() -> void:\n\tvar a := 1\n\tvar b := 2\n\tvar c := 3\n";
    p.write("foo.gd", src);
    let mut ws = Workspace::load(&p.root, &options(&p));
    let key = key_for(&p, "foo.gd");
    let path = p.root.join("foo.gd");
    let parsed = ws.parse(&key, src);

    // `iter_limit = 1` forces the fixpoint governor to bail on the first checkpoint → partial result.
    let tiny = || gd_analyze::AnalyzeOptions {
        iter_limit: Some(1),
        ..Default::default()
    };
    let a1 = ws.analyze_with_options(&key, &path, &parsed.tree, src, tiny());
    assert!(
        a1.bailed,
        "iter_limit=1 should trip the governor and flag the result `bailed`"
    );
    let a2 = ws.analyze_with_options(&key, &path, &parsed.tree, src, tiny());
    assert!(
        !Rc::ptr_eq(&a1, &a2),
        "a bailed (partial) analysis must re-run on the next request, not serve a cached partial"
    );
    assert_eq!(
        ws.cache_lens().1,
        0,
        "a bailed analysis must not occupy the analysis cache"
    );
}

/// Same content-addressed validity for the analysis cache (analyze side).
#[test]
fn analysis_cache_is_content_addressed() {
    let p = TempProject::new();
    p.write("project.godot", "config_version=5\n");
    p.write("foo.gd", "var a = 1\n");
    let mut ws = Workspace::load(&p.root, &options(&p));
    let key = key_for(&p, "foo.gd");
    let path = p.root.join("foo.gd");

    let t1 = ws.parse(&key, "var a = 1\n");
    let a1 = ws.analyze(&key, &path, &t1.tree, "var a = 1\n");
    let a2 = ws.analyze(&key, &path, &t1.tree, "var a = 1\n");
    assert!(
        Rc::ptr_eq(&a1, &a2),
        "identical content must hit the analysis cache"
    );

    let t2 = ws.parse(&key, "var zzz = 2\n");
    let a3 = ws.analyze(&key, &path, &t2.tree, "var zzz = 2\n");
    assert!(
        !Rc::ptr_eq(&a1, &a3),
        "changed content must re-analyze rather than serve the stale analysis"
    );
}

/// The headline cross-file coherence bug: a file whose *dependency's interface*
/// changed must NOT re-serve its stale cached analysis just because its own bytes are unchanged.
/// WP-RD8: the dependency change bumps this file's cache epoch (through the reverse-dependency
/// closure), so the cached entry's composite key `(content hash, epoch)` mismatches and
/// `Workspace::analyze` recomputes against the current interfaces. Pre-fix the own-text
/// fingerprint matched and the stale `AnalysisResult` (computed against the OLD dependency) was
/// returned and republished — a phantom/missing cross-file diagnostic that never cleared.
#[test]
fn dirty_dependent_bypasses_stale_analysis_cache() {
    let p = TempProject::new();
    p.write("project.godot", "config_version=5\n");
    p.write(
        "base.gd",
        "class_name Base\nextends Node\nfunc foo() -> int:\n\treturn 1\n",
    );
    p.write("dep.gd", "extends Base\n");
    let mut ws = Workspace::load(&p.root, &options(&p));

    let dep_key = key_for(&p, "dep.gd");
    let dep_path = p.root.join("dep.gd");
    let dep_src = "extends Base\n";

    // Prime the analysis cache for the dependent.
    let dt = ws.parse(&dep_key, dep_src);
    let a1 = ws.analyze(&dep_key, &dep_path, &dt.tree, dep_src);

    // Change Base's *interface* (foo's return type) and reindex it — the watcher's reindex path.
    // `on_file_changed` marks dep.gd dirty via the reverse-dependency closure (dep `extends Base`).
    // Build the new tree through `parse` so the test needs no direct gd_syntax dependency.
    let base_key = key_for(&p, "base.gd");
    let base_path = p.root.join("base.gd");
    let new_base = ws.parse(
        &base_key,
        "class_name Base\nextends Node\nfunc foo() -> String:\n\treturn \"\"\n",
    );
    // WP-RD8: capture dep.gd's cache epoch before the dependency edit so we can assert the
    // interface change advanced it (the self-validating-key analog of the old `is_dirty` check).
    let dep_fid = ws.index.file_id(&dep_path).expect("dep.gd is interned");
    let dep_epoch0 = ws.index.epoch_of(dep_fid);
    ws.reindex(&base_path, &new_base.tree);
    assert!(
        ws.index.epoch_of(dep_fid) > dep_epoch0,
        "an interface change to Base must bump its dependent dep.gd's cache epoch"
    );

    // Re-analyze the dependent with BYTE-IDENTICAL text. The own-text fingerprint still matches, but
    // the advanced epoch makes the cached entry's composite key mismatch → a fresh Rc, not stale a1.
    let a2 = ws.analyze(&dep_key, &dep_path, &dt.tree, dep_src);
    assert!(
        !Rc::ptr_eq(&a1, &a2),
        "a dependent whose dependency's interface changed must recompute even though its own bytes \
         are unchanged (pre-fix this re-served the stale analysis against the old Base)"
    );

    // The recompute re-stamped the entry with the current epoch, so a second analyze is now a
    // normal composite-key hit, not a perpetual recompute (the epoch key self-validates with no
    // clear-after-analyze step).
    let a3 = ws.analyze(&dep_key, &dep_path, &dt.tree, dep_src);
    assert!(
        Rc::ptr_eq(&a2, &a3),
        "once re-stamped, identical content must hit the cache again (no perpetual recompute)"
    );
}

/// Test-gap closure: the dirty-override is pinned above for a *name-based*
/// dependent (`extends Base`), but never for a *path-based* one (`extends "res://base.gd"`), which
/// routes through the separate `path_referencers` reverse index rather than `name_referencers`. This
/// drives the same stale-analysis-cache override through the production `Workspace::reindex` +
/// `analyze` seam for a path-extends dependent, closing the end-to-end gap. (The `Index` unit test
/// `removing_a_path_extends_target_invalidates_its_referencer` covers the delete direction.)
#[test]
fn path_extends_dependent_bypasses_stale_analysis_cache() {
    let p = TempProject::new();
    p.write("project.godot", "config_version=5\n");
    p.write(
        "base.gd",
        "class_name Base\nextends Node\nfunc foo() -> int:\n\treturn 1\n",
    );
    p.write("dep.gd", "extends \"res://base.gd\"\n");
    let mut ws = Workspace::load(&p.root, &options(&p));

    let dep_key = key_for(&p, "dep.gd");
    let dep_path = p.root.join("dep.gd");
    let dep_src = "extends \"res://base.gd\"\n";

    // Prime the analysis cache for the path-extends dependent.
    let dt = ws.parse(&dep_key, dep_src);
    let a1 = ws.analyze(&dep_key, &dep_path, &dt.tree, dep_src);

    // Change Base's *interface* and reindex it (the watcher's reindex path). dep's edge is a PATH
    // edge (`extends "res://base.gd"`), so the reverse-dependency closure that dirties dep is driven
    // by path resolution, exercising path_referencers — not the name machinery the test above covers.
    let base_key = key_for(&p, "base.gd");
    let base_path = p.root.join("base.gd");
    let new_base = ws.parse(
        &base_key,
        "class_name Base\nextends Node\nfunc foo() -> String:\n\treturn \"\"\n",
    );
    // WP-RD8: same epoch-advance check as the name-based test, but the dirtying flows through the
    // path_referencers reverse index (`extends "res://base.gd"`).
    let dep_fid = ws.index.file_id(&dep_path).expect("dep.gd is interned");
    let dep_epoch0 = ws.index.epoch_of(dep_fid);
    ws.reindex(&base_path, &new_base.tree);
    assert!(
        ws.index.epoch_of(dep_fid) > dep_epoch0,
        "an interface change to the res:// target must bump its path-extends dependent's cache epoch"
    );

    // Re-analyze with BYTE-IDENTICAL text: the advanced epoch forces a recompute → fresh Rc.
    let a2 = ws.analyze(&dep_key, &dep_path, &dt.tree, dep_src);
    assert!(
        !Rc::ptr_eq(&a1, &a2),
        "a path-extends dependent whose target's interface changed must recompute even though its \
         own bytes are unchanged"
    );
    // Re-stamped at the current epoch → a second analyze is a normal composite-key hit.
    let a3 = ws.analyze(&dep_key, &dep_path, &dt.tree, dep_src);
    assert!(
        Rc::ptr_eq(&a2, &a3),
        "once re-stamped, identical content must hit the cache again (no perpetual recompute)"
    );
}

/// WP-RD8: the composite cache key must self-invalidate a consumer through a **3-deep** extends
/// chain. `A extends B extends C`: editing C's *interface* bumps C's epoch, which the
/// reverse-dependency closure propagates to B AND transitively to A — so A's cached analysis
/// (whose own bytes never changed) misses on the epoch half of the key and recomputes against the
/// new C. This is the dep-epoch-propagation case the M4 dirty-bit design also handled, re-pinned
/// against the epoch key.
#[test]
fn dep_epoch_propagates_through_three_deep_extends_chain() {
    let p = TempProject::new();
    p.write("project.godot", "config_version=5\n");
    p.write(
        "c.gd",
        "class_name C\nextends Node\nfunc foo() -> int:\n\treturn 1\n",
    );
    p.write("b.gd", "class_name B\nextends C\n");
    p.write("a.gd", "extends B\n");
    let mut ws = Workspace::load(&p.root, &options(&p));

    let a_key = key_for(&p, "a.gd");
    let a_path = p.root.join("a.gd");
    let a_src = "extends B\n";

    // Prime A's analysis cache.
    let at = ws.parse(&a_key, a_src);
    let a1 = ws.analyze(&a_key, &a_path, &at.tree, a_src);

    // Capture A's epoch, then change C's interface two links up the chain.
    let a_fid = ws.index.file_id(&a_path).expect("a.gd interned");
    let a_epoch0 = ws.index.epoch_of(a_fid);
    let c_key = key_for(&p, "c.gd");
    let c_path = p.root.join("c.gd");
    let new_c = ws.parse(
        &c_key,
        "class_name C\nextends Node\nfunc foo() -> String:\n\treturn \"\"\n",
    );
    ws.reindex(&c_path, &new_c.tree);

    assert!(
        ws.index.epoch_of(a_fid) > a_epoch0,
        "an interface change two links up the extends chain must reach A's cache epoch transitively"
    );
    // A's cached analysis (own bytes unchanged) must now miss on the epoch half of the key.
    let a2 = ws.analyze(&a_key, &a_path, &at.tree, a_src);
    assert!(
        !Rc::ptr_eq(&a1, &a2),
        "A must recompute against the new C even though A's own bytes are unchanged (3-deep chain)"
    );
}

/// The cold index must skip the shared exclusion set, not just `.godot/`. Before
/// the fix, `gd_files` filtered only `.godot/`, so a `.gd` under `target/` / `.git/` /
/// `node_modules/` was indexed at startup and could register a `class_name` that shadows a real
/// project class. `Workspace::load` → `Index::build` → `gd_files` → `gd_project::is_excluded`.
#[test]
fn cold_index_skips_excluded_directories() {
    let p = TempProject::new();
    p.write("project.godot", "config_version=5\n");
    p.write("src/real.gd", "class_name Real\nextends Node\n");
    p.write("target/copy.gd", "class_name Copied\nextends Node\n");
    p.write(".git/hook.gd", "class_name Ghost\nextends Node\n");
    p.write("node_modules/dep/x.gd", "class_name Dep\nextends Node\n");

    let ws = Workspace::load(&p.root, &options(&p));
    assert!(
        ws.index.file_id(&p.root.join("src/real.gd")).is_some(),
        "a normal src/ .gd must be cold-indexed"
    );
    for excluded in ["target/copy.gd", ".git/hook.gd", "node_modules/dep/x.gd"] {
        assert!(
            ws.index.file_id(&p.root.join(excluded)).is_none(),
            "{excluded} is under an excluded directory and must NOT be cold-indexed"
        );
    }
}

/// WP-RD9 (Windows residual): a file reached through an NTFS **junction** must resolve to the same
/// `FileId` as its real path — one index entry, not a duplicate. The cold walk reaches the file via
/// its real subtree (WalkDir does not descend a junction), so a client/test accessing it through
/// the junction path relies on `Index::file_id`'s `dunce::canonicalize` slow path to map the
/// junction path back to the interned real path. Skips gracefully if `mklink /J` is unavailable
/// (locked-down CI) rather than failing the suite.
#[cfg(windows)]
#[test]
fn junction_path_resolves_to_single_index_entry() {
    let p = TempProject::new();
    p.write("project.godot", "config_version=5\n");
    p.write("real/x.gd", "class_name JunctionX\nextends Node\n");

    let target = p.root.join("real");
    let link = p.root.join("link");
    let made = std::process::Command::new("cmd")
        .args(["/C", "mklink", "/J"])
        .arg(link.as_std_path())
        .arg(target.as_std_path())
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if !made {
        eprintln!("skipping junction test: `mklink /J` unavailable in this environment");
        return;
    }

    // Canonicalize the root so the only path difference under test is the leaf reparse point
    // (`link` vs `real`); otherwise `std::env::temp_dir()`'s casing (`temp`) diverges from NTFS's
    // real case (`Temp`) and masks what RD9 actually resolves.
    let root = camino::Utf8PathBuf::from_path_buf(dunce::canonicalize(&p.root).unwrap()).unwrap();
    let ws = Workspace::load(&root, &options(&p));
    let via_real = ws.index.file_id(&root.join("real/x.gd"));
    let via_link = ws.index.file_id(&root.join("link/x.gd"));
    assert!(via_real.is_some(), "the real path must be indexed");
    assert_eq!(
        via_real, via_link,
        "a file reached through a junction must resolve to the SAME FileId as its real path \
         (one index entry), not a duplicate or a miss"
    );
}

/// WP-RD9 (Windows residual): NTFS preserves case but matches case-insensitively, so the same file
/// addressed as `Game/x.gd` and `game/x.gd` must resolve to one `FileId`. The cold walk interns the
/// real on-disk case; `Index::file_id`'s `dunce::canonicalize` slow path recovers it for the
/// differently-cased lookup.
#[cfg(windows)]
#[test]
fn differently_cased_path_resolves_to_single_index_entry() {
    let p = TempProject::new();
    p.write("project.godot", "config_version=5\n");
    p.write("Game/x.gd", "class_name CaseX\nextends Node\n");

    // Canonicalize the root so only the leaf component case (`Game` vs `game`) differs — the temp
    // dir's own casing (`temp` vs NTFS `Temp`) would otherwise mask the leaf resolution.
    let root = camino::Utf8PathBuf::from_path_buf(dunce::canonicalize(&p.root).unwrap()).unwrap();
    let ws = Workspace::load(&root, &options(&p));
    let upper = ws.index.file_id(&root.join("Game/x.gd"));
    let lower = ws.index.file_id(&root.join("game/x.gd"));
    assert!(upper.is_some(), "the as-written path must be indexed");
    assert_eq!(
        upper, lower,
        "NTFS matches case-insensitively, so `Game/` and `game/` must resolve to one FileId"
    );
}

/// Deterministic coverage of the `project.godot`-reload path. The integration test
/// `watcher_project_godot_change_reloads_policy` only exercises the watcher→reload *wiring* on a
/// best-effort (real-FS, never-fails) basis; this pins the load-bearing effect — `reload` drops the
/// analysis cache, which is the mechanism by which a changed policy / native lattice takes effect.
#[test]
fn reload_project_and_native_clears_analysis_cache() {
    let p = TempProject::new();
    p.write("project.godot", "config_version=5\n");
    p.write("foo.gd", "var a = 1\n");
    let opts = options(&p);
    let mut ws = Workspace::load(&p.root, &opts);
    let key = key_for(&p, "foo.gd");
    let path = p.root.join("foo.gd");

    let t = ws.parse(&key, "var a = 1\n");
    let a1 = ws.analyze(&key, &path, &t.tree, "var a = 1\n");
    ws.reload_project_and_native(&opts);
    let a2 = ws.analyze(&key, &path, &t.tree, "var a = 1\n");
    assert!(
        !Rc::ptr_eq(&a1, &a2),
        "reload must clear the analysis cache so re-analysis picks up the new policy/native lattice"
    );
}

/// Deterministic coverage of `Workspace::reconcile`'s added/removed accounting: the
/// integration suite reaches reconcile only via the real-FS watcher overflow path. Here we mutate
/// the project on disk and assert the report counts, and that the index membership tracks disk.
#[test]
fn reconcile_tracks_disk_added_and_removed() {
    let p = common::sample_project();
    let mut ws = Workspace::load(&p.root, &common::options_for(&p));

    // A no-op reconcile right after load sees no drift.
    let r = ws.reconcile(&Default::default());
    assert_eq!(
        r.added, 0,
        "fresh load then reconcile should see nothing added"
    );
    assert_eq!(
        r.removed, 0,
        "fresh load then reconcile should see nothing removed"
    );

    // Add a .gd on disk the cold index never saw.
    p.write("src/baddie.gd", "extends Node\n");
    let r = ws.reconcile(&Default::default());
    assert!(
        r.added >= 1,
        "a new on-disk .gd must be counted as added (added={})",
        r.added
    );
    assert!(
        ws.index
            .interface_of(&p.root.join("src/baddie.gd"))
            .is_some(),
        "the added file must now have an interface in the index"
    );

    // Delete a .gd on disk.
    p.remove("src/enemy.gd");
    let r = ws.reconcile(&Default::default());
    assert!(
        r.removed >= 1,
        "a deleted on-disk .gd must be counted as removed (removed={})",
        r.removed
    );
    // Note: `on_file_removed` drops the file's interface but keeps its (stable) FileId interned,
    // so the "still indexed?" check is `interface_of`, not `file_id`.
    assert!(
        ws.index
            .interface_of(&p.root.join("src/enemy.gd"))
            .is_none(),
        "the removed file's interface must be dropped from the index"
    );
}

/// The deterministic twin of the watcher VFS-overlay fix: the open buffer is the
/// source of truth over disk (docs/01, `vfs.rs`), so `reconcile` must NOT drop a file the editor
/// has open even when its on-disk copy was deleted (git stash / branch switch). `apply_reaction`
/// shares the exact `open_paths` mechanism on the live single-event watcher path. Pre-fix
/// `reconcile` was VFS-blind and phantom-removed the stashed-away open file's interface, breaking
/// cross-file resolution for everything depending on it.
#[test]
fn reconcile_keeps_open_buffer_whose_disk_copy_vanished() {
    let p = common::sample_project();
    let mut ws = Workspace::load(&p.root, &common::options_for(&p));
    let hero_path = p.root.join("src/hero.gd");
    let enemy_path = p.root.join("src/enemy.gd");
    assert!(
        ws.index.interface_of(&hero_path).is_some(),
        "precondition: hero.gd is cold-indexed"
    );

    // The editor has hero.gd open. Model the open set the way `open_buffer_paths` builds it
    // (forward-slash-normalized paths). The type is inferred as the `FxHashSet` reconcile wants —
    // `rustc_hash` isn't a dev-dependency, so it can't be named from the integration-test crate.
    let hero_norm = camino::Utf8PathBuf::from(hero_path.as_str().replace('\\', "/"));
    let open = std::iter::once(hero_norm).collect();

    // An external tool (git stash / checkout) deletes BOTH the open file and a closed file on disk.
    p.remove("src/hero.gd");
    p.remove("src/enemy.gd");
    let r = ws.reconcile(&open);

    // The OPEN file survives (its buffer interface is authoritative); the CLOSED file is removed
    // (reconcile still tracks disk for files the editor doesn't own).
    assert!(
        ws.index.interface_of(&hero_path).is_some(),
        "an open buffer whose on-disk copy was deleted must NOT be dropped from the index"
    );
    assert!(
        ws.index.interface_of(&enemy_path).is_none(),
        "a closed file deleted on disk must still be removed by reconcile"
    );
    assert!(
        r.removed >= 1,
        "the closed-file deletion must count toward removed (removed={})",
        r.removed
    );
}

/// WP-RD4 (Windows): the reconcile removal-pass data-safety guard. When the disk walk hits an
/// error (here: a subdirectory denied read via `icacls`), the walk is NOT authoritative, so the
/// "removed = anything in the index but not in the walk" pass MUST be skipped — otherwise a
/// permission glitch on one subdir would phantom-remove every file under it from the index. Cold
/// index sees `locked/secret.gd` (load happens before the deny); the post-deny reconcile can't
/// enumerate it, and the guard must preserve it. Skips gracefully if the deny can't take effect
/// (admin / backup-privilege environment where DENY ACEs are bypassed).
#[cfg(windows)]
#[test]
fn reconcile_skips_removal_pass_when_walk_errors_preserving_indexed_files() {
    let p = TempProject::new();
    p.write("project.godot", "config_version=5\n");
    p.write("src/keep.gd", "class_name Keep\nextends Node\n");
    p.write("locked/secret.gd", "class_name Secret\nextends Node\n");
    let mut ws = Workspace::load(&p.root, &options(&p));
    let secret_path = p.root.join("locked/secret.gd");
    assert!(
        ws.index.interface_of(&secret_path).is_some(),
        "precondition: secret.gd must be cold-indexed before the deny"
    );

    let locked = p.root.join("locked");
    let denied = std::process::Command::new("icacls")
        .args([locked.as_str(), "/inheritance:r", "/deny", "Everyone:(RX)"])
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    // Verify the deny actually blocks enumeration; restore + skip if it didn't (admin bypass).
    if !denied || std::fs::read_dir(locked.as_std_path()).is_ok() {
        let _ = std::process::Command::new("icacls")
            .args([locked.as_str(), "/reset"])
            .status();
        eprintln!(
            "skipping: could not deny read on the subdir (admin/backup-privilege environment)"
        );
        return;
    }

    let report = ws.reconcile(&Default::default());
    // Restore access so the TempProject's Drop can clean the tree.
    let _ = std::process::Command::new("icacls")
        .args([locked.as_str(), "/reset"])
        .status();

    assert!(
        report.had_errors(),
        "the denied subdir must register a walk error (report.walk_errors > 0)"
    );
    assert_eq!(
        report.removed, 0,
        "a walk that hit errors is not authoritative — the removal pass MUST be skipped \
         (report.removed forced to 0), not phantom-delete the files it couldn't enumerate"
    );
    assert!(
        ws.index.interface_of(&secret_path).is_some(),
        "the unwalkable file's interface must be PRESERVED, not removed, when the walk errored"
    );
}

/// WP-R2 end-to-end through the production seam: two files whose member
/// initializers mutually reference each other across a `preload` must surface
/// `Could not resolve external class member "..."`. This is driven by
/// `WorkspaceXFileQuery::member_initializer_xrefs` reading the `CanonicalKey`-keyed analysis
/// cache — the path the corpus harness (which uses `CorpusQuery`) does NOT exercise. Analysing B
/// first caches its recorded xref; analysing A then observes it and fires the cycle.
#[test]
fn cross_file_member_cycle_fires_through_workspace_cache() {
    let p = TempProject::new();
    p.write("project.godot", "config_version=5\n");
    let a_src = "class_name CycA\nconst Other = preload(\"res://b.gd\")\nvar v = Other.v\n";
    let b_src = "class_name CycB\nconst Other = preload(\"res://a.gd\")\nvar v = Other.v\n";
    p.write("a.gd", a_src);
    p.write("b.gd", b_src);

    let mut ws = Workspace::load(&p.root, &options(&p));
    let key_a = key_for(&p, "a.gd");
    let key_b = key_for(&p, "b.gd");
    let path_a = p.root.join("a.gd");
    let path_b = p.root.join("b.gd");

    // Analyse B first so its `B.v -> A.v` member-xref is recorded into the cache.
    let tb = ws.parse(&key_b, b_src);
    let _ = ws.analyze(&key_b, &path_b, &tb.tree, b_src);

    // Now analyse A: it records `A.v -> B.v`, reads B's cached xref via WorkspaceXFileQuery, and
    // detects the cycle.
    let ta = ws.parse(&key_a, a_src);
    let result_a = ws.analyze(&key_a, &path_a, &ta.tree, a_src);

    // Faithful-port discipline (CLAUDE.md): the diagnostic's message string AND source range are
    // both part of the contract, so assert the EXACT Godot message (not a substring that would
    // survive a quoting/interpolation drift) and that the span lands on A's offending member
    // initializer rather than at byte 0 or the wrong line.
    let cycle_diag = result_a
        .diagnostics
        .iter()
        .find(|d| d.message() == r#"Could not resolve external class member "v"."#)
        .unwrap_or_else(|| {
            panic!(
                "expected the exact Godot message `Could not resolve external class member \"v\".` \
                 through the real WorkspaceXFileQuery + CanonicalKey cache; got: {:?}",
                result_a
                    .diagnostics
                    .iter()
                    .map(|d| d.message())
                    .collect::<Vec<_>>()
            )
        });

    // Compute the member-initializer line's byte range from the source rather than hardcoding
    // offsets; any reasonable span for this diagnostic (the `.v` member access or the `var v`
    // declaration) falls inside `var v = Other.v`.
    let line_start = a_src
        .find("var v = Other.v")
        .expect("member-initializer line present in a.gd");
    let line_end = line_start + "var v = Other.v".len();
    let span = cycle_diag.span();
    assert!(
        span.start >= line_start && span.end <= line_end && span.start < span.end,
        "cycle diagnostic span {span:?} must be non-empty and fall inside `var v = Other.v` \
         (bytes {line_start}..{line_end})"
    );
}
