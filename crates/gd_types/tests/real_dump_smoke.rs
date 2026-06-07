//! Dev-only smoke test against the full, real dump at `api/extension_api.json` (git-ignored).
//! Skips when the file is absent (e.g. CI), so it never blocks a clean checkout — but in a dev tree
//! it exercises the decoder against every real type string in all ~1,198 classes.

use std::path::{Path, PathBuf};

fn dump_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../api/extension_api.json")
}

#[test]
fn real_dump_ingests_if_present() {
    let path = dump_path();
    if !path.exists() {
        eprintln!(
            "skipping real_dump_ingests_if_present: {} not present (dev-only)",
            path.display()
        );
        return;
    }
    let db = gd_types::NativeDb::load(path.to_str().expect("utf-8 path"))
        .expect("real dump loads and parses");
    assert!(
        db.class_count() >= 1000,
        "expected a full editor dump, got {} classes",
        db.class_count()
    );
    assert!(db.is_subclass_of_named("Node", "Object"));
    assert!(db.is_subclass_of_named("Node2D", "Node"));
    assert!(db.content_hash() != 0, "content hash should be set on load");
}
