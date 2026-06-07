//! WP-G — cold-index scale gate + degradation.
//!
//! Generates a synthetic project with a realistic `class_name`/`extends` web, cold-indexes it from
//! disk, and asserts (a) it scales — every class registers, the chain resolves end-to-end, no
//! duplicate `FileId`s — and (b) it does so within a *generous* wall-time ceiling that only catches an
//! accidental O(N²) regression. The tight, machine-calibrated ratchet "floor" is a CI-config concern
//! (`docs/07`), deliberately not hard-coded here (CLAUDE.md: targets aren't guessed in source). The
//! `#[ignore]`d 10k variant prints the real timing for a local calibration run.

use std::time::{Instant, SystemTime, UNIX_EPOCH};

use camino::Utf8PathBuf;
use gd_project::{Index, Resolution};
use gd_types::NativeDb;

struct TempDir {
    root: Utf8PathBuf,
}

impl TempDir {
    fn new(tag: &str) -> Self {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = Utf8PathBuf::from_path_buf(std::env::temp_dir())
            .expect("temp dir is UTF-8")
            .join(format!("gdls_{tag}_{}_{nanos}", std::process::id()));
        std::fs::create_dir_all(root.join("src")).unwrap();
        TempDir { root }
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn mini_db() -> NativeDb {
    NativeDb::from_json(
        r#"{"header":{"version_major":4,"version_minor":6,"version_patch":3},
            "classes":[{"name":"Object"},{"name":"Node","inherits":"Object"}]}"#,
    )
    .unwrap()
}

/// Write `n` scripts: `C{i}` extends `C{i-1}` (`C0` extends the native `Node`), each carrying two
/// members typed by other project classes — a connected `extends` spine plus a cross-reference web.
fn generate(dir: &TempDir, n: usize) {
    for i in 0..n {
        let base = if i == 0 {
            "Node".to_string()
        } else {
            format!("C{}", i - 1)
        };
        let ref_a = format!("C{}", (i * 7 + 3) % n);
        let ref_b = format!("C{}", (i * 13 + 5) % n);
        let src = format!(
            "class_name C{i}\nextends {base}\n\nvar a: {ref_a}\nvar b: {ref_b}\n\nfunc tick() -> void:\n\tpass\n"
        );
        std::fs::write(dir.root.join(format!("src/c{i}.gd")), src).unwrap();
    }
}

/// Cold-index `n` files, assert correctness at scale, and return the elapsed cold-index time.
fn index_and_check(n: usize) -> std::time::Duration {
    let dir = TempDir::new("perf");
    generate(&dir, n);
    let db = mini_db();

    let start = Instant::now();
    let index = Index::build(&dir.root);
    let elapsed = start.elapsed();

    // Every file indexed exactly once (path normalization/interning is coherent — no dup FileIds).
    assert_eq!(
        index.file_count(),
        n,
        "every generated file should index once"
    );
    assert_eq!(
        index.registry().len(),
        n,
        "every class_name should register"
    );

    // The base spine resolves: C0 → native Node; the last link → its predecessor script.
    let c0 = index.file_id(&dir.root.join("src/c0.gd")).unwrap();
    assert_eq!(index.resolve_base(c0, &db), Resolution::Native);

    let last = index
        .file_id(&dir.root.join(format!("src/c{}.gd", n - 1)))
        .unwrap();
    let Resolution::Script(prev) = index.resolve_base(last, &db) else {
        panic!("C{} should extend the script class C{}", n - 1, n - 2);
    };
    assert!(index
        .path(prev)
        .is_some_and(|p| p.as_str().ends_with(&format!("c{}.gd", n - 2))));

    // A mid-web name resolves to a project script.
    assert!(matches!(
        index.resolve_name(&format!("C{}", n / 2), &db),
        Resolution::Script(_)
    ));

    elapsed
}

#[test]
fn cold_index_scales_to_500_files() {
    let n = 500;
    let elapsed = index_and_check(n);
    eprintln!("cold-indexed {n} files in {elapsed:?}");
    // Generous ceiling: a correct O(N) cold index of 500 tiny files is far under this; only an
    // accidental O(N²) blowup trips it. (Calibrated ratchet floor lives in CI config — docs/07.)
    assert!(
        elapsed.as_secs() < 20,
        "cold index of {n} files took {elapsed:?} — suspect a super-linear regression"
    );
}

#[test]
fn malformed_file_does_not_break_the_index() {
    let dir = TempDir::new("perf_degrade");
    // A valid base, a syntactically broken file, and a dependent — the broken one must not abort the
    // walk (parser returns a partial tree; extraction yields a partial interface).
    std::fs::write(
        dir.root.join("src/base.gd"),
        "class_name Base\nextends Node\n",
    )
    .unwrap();
    std::fs::write(
        dir.root.join("src/broken.gd"),
        "class_name Broken\nextends ((((\nfunc \n",
    )
    .unwrap();
    std::fs::write(dir.root.join("src/derived.gd"), "extends Base\n").unwrap();

    let index = Index::build(&dir.root);
    assert_eq!(index.file_count(), 3, "all three files are still indexed");
    // The well-formed cross-file relationship still resolves despite the broken sibling.
    let derived = index.file_id(&dir.root.join("src/derived.gd")).unwrap();
    assert!(matches!(
        index.resolve_base(derived, &NativeDb::empty()),
        Resolution::Script(_)
    ));
}

/// Local calibration run (not in the CI default set): prints the 10k cold-index time.
#[test]
#[ignore = "perf calibration — run explicitly: cargo test -p gd_project --test perf_scale -- --ignored --nocapture"]
fn cold_index_perf_10k() {
    let n = 10_000;
    let elapsed = index_and_check(n);
    eprintln!("cold-indexed {n} files in {elapsed:?}");
}
