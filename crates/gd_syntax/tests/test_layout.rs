//! Every crate builds its integration tests as one binary (`autotests = false` plus a single
//! `[[test]]` rooted at `tests/main.rs`), so Cargo no longer discovers `tests/*.rs` on its own. A file
//! dropped into `tests/` without a `mod` line in that crate's `tests/main.rs` would compile into
//! nothing and its tests would silently never run. This walks every workspace crate and fails on
//! any such orphan.

use std::collections::BTreeSet;
use std::path::Path;

#[test]
fn every_test_file_is_a_module_of_the_integration_binary() {
    let crates = Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
    let mut orphans = Vec::new();
    for entry in std::fs::read_dir(&crates).unwrap() {
        let tests = entry.unwrap().path().join("tests");
        let files: BTreeSet<String> = match std::fs::read_dir(&tests) {
            Ok(dir) => dir
                .map(|e| e.unwrap().path())
                .filter(|p| p.extension().is_some_and(|x| x == "rs"))
                .map(|p| p.file_stem().unwrap().to_string_lossy().into_owned())
                .filter(|stem| stem != "main")
                .collect(),
            Err(_) => continue,
        };
        if files.is_empty() {
            continue;
        }
        let main = std::fs::read_to_string(tests.join("main.rs")).unwrap_or_default();
        let declared: BTreeSet<String> = main
            .lines()
            .filter_map(|l| l.trim().strip_prefix("mod ")?.strip_suffix(';'))
            .map(str::to_owned)
            .collect();
        orphans.extend(
            files
                .difference(&declared)
                .map(|stem| format!("{}/{stem}.rs", tests.display())),
        );
    }
    assert!(
        orphans.is_empty(),
        "test files with no `mod` line in their crate's tests/main.rs (they never run): {orphans:?}"
    );
}
