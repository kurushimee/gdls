//! #555 — `Preload file "X" does not exist.`, end to end through the real project view.
//!
//! The analyzer only relays what [`gd_analyze::CrossFileQuery::preload_missing_path`] testifies;
//! `WorkspaceXFileQuery` is the one impl that answers, and it answers from a live `stat`. These
//! tests pin what it will and will not claim, and the invalidation half — a row has to appear the
//! moment the target goes and clear the moment it comes back.
//!
//! Every reported row is verbatim `Godot_v4.7.2-stable --headless --check-only` output. The four
//! deliberate silences (a `uid://`, a `user://`, an existing file with no loader, a directory) are
//! where gdls under-reports on purpose — see `WorkspaceXFileQuery::preload_missing_path`.

mod common;

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

fn project(script: &str) -> TempProject {
    let p = TempProject::new();
    p.write(
        "project.godot",
        "config_version=5\n\n[application]\nconfig/features=PackedStringArray(\"4.7\")\n",
    );
    p.write("src/user.gd", script);
    p
}

/// Analyze `src/user.gd` and return its error messages.
fn errors(ws: &mut Workspace, p: &TempProject) -> Vec<String> {
    let uri = path_to_file_uri(&p.root.join("src/user.gd")).expect("valid file uri");
    let key = CanonicalKey::for_uri(&uri);
    let path = p.root.join("src/user.gd");
    let text = std::fs::read_to_string(path.as_std_path()).expect("read script");
    let tree = ws.parse(&key, &text).tree.clone();
    ws.analyze(&key, &path, &tree, &text)
        .diagnostics
        .iter()
        .filter(|d| d.severity() == gd_analyze::Severity::Error)
        .map(|d| d.message().to_owned())
        .collect()
}

#[test]
fn a_missing_res_script_reports() {
    let p = project("extends Node\n\nconst A = preload(\"res://src/gone.gd\")\n");
    let mut ws = Workspace::load(&p.root, &options(&p));
    assert_eq!(
        errors(&mut ws, &p),
        vec![r#"Preload file "res://src/gone.gd" does not exist."#.to_owned()]
    );
}

/// Godot relativizes then simplifies, so the row names `res://gone.tres`, not `res://src/../gone.tres`.
#[test]
fn a_relative_path_reports_in_godots_simplified_form() {
    let p = project("extends Node\n\nconst A = preload(\"../gone.tres\")\n");
    let mut ws = Workspace::load(&p.root, &options(&p));
    assert_eq!(
        errors(&mut ws, &p),
        vec![r#"Preload file "res://gone.tres" does not exist."#.to_owned()]
    );
}

/// A resource that is really there, whatever its kind, draws nothing.
#[test]
fn an_existing_target_is_silent() {
    let p = project(
        "extends Node\n\nconst A = preload(\"res://src/other.gd\")\nconst B = preload(\"res://data/t.tres\")\nconst C = preload(\"other.gd\")\n",
    );
    p.write("src/other.gd", "extends Node\n");
    p.write("data/t.tres", "[gd_resource type=\"Resource\"]\n");
    let mut ws = Workspace::load(&p.root, &options(&p));
    assert_eq!(errors(&mut ws, &p), Vec::<String>::new());
}

/// The four deliberate under-reports, each for its own reason: an unresolved `uid://` can mean a
/// lagging uid map rather than a missing file; `user://` is outside the project tree; a file that
/// exists but no loader claims is Godot's OTHER message, which gdls does not port; and a directory
/// reads the same as a half-typed path.
#[test]
fn the_shapes_gdls_will_not_claim_stay_silent() {
    let p = project(
        "extends Node\n\nconst A = preload(\"uid://bogus123\")\nconst B = preload(\"user://x.gd\")\nconst C = preload(\"res://notes.txt\")\nconst D = preload(\"res://src\")\n",
    );
    p.write("notes.txt", "hello\n");
    let mut ws = Workspace::load(&p.root, &options(&p));
    assert_eq!(errors(&mut ws, &p), Vec::<String>::new());
}

/// A `res://` that climbs out of the tree is not a path Godot accepts at all, and gdls must not
/// reinterpret it as a relative one on the way to a claim.
#[test]
fn a_traversing_res_path_stays_silent() {
    let p = project("extends Node\n\nconst A = preload(\"res://../../etc/passwd\")\n");
    let mut ws = Workspace::load(&p.root, &options(&p));
    assert_eq!(errors(&mut ws, &p), Vec::<String>::new());
}

/// An imported asset is remapped: `ResourceLoader::exists` follows the `.import`, so the source
/// counts as present even when it was never checked in. Godot fails later with a different
/// message, so a row here would be the wrong one.
#[test]
fn an_import_sidecar_makes_the_source_count_as_present() {
    let p = project("extends Node\n\nconst A = preload(\"res://art/tex.svg\")\n");
    p.write(
        "art/tex.svg.import",
        "[remap]\n\nimporter=\"texture\"\ntype=\"CompressedTexture2D\"\n",
    );
    let mut ws = Workspace::load(&p.root, &options(&p));
    assert_eq!(errors(&mut ws, &p), Vec::<String>::new());
}

/// A script the index holds but the disk does not is an unsaved buffer, not a missing file.
#[test]
fn an_unsaved_buffer_counts_as_present() {
    let p = project("extends Node\n\nconst A = preload(\"res://src/draft.gd\")\n");
    let mut ws = Workspace::load(&p.root, &options(&p));
    let draft = p.root.join("src/draft.gd");
    let tree = ws.parse_source("extends Node\n").tree;
    ws.reindex(&draft, &tree);
    assert_eq!(errors(&mut ws, &p), Vec::<String>::new());
}

/// Without a `project.godot` there is no `res://` and the root is whatever folder was opened, so
/// every negative claim about the tree is off — the fail-closed gate.
#[test]
fn a_project_less_root_claims_nothing() {
    let p = TempProject::new();
    p.write(
        "src/user.gd",
        "extends Node\n\nconst A = preload(\"res://src/gone.gd\")\n",
    );
    let mut ws = Workspace::load(&p.root, &options(&p));
    assert_eq!(errors(&mut ws, &p), Vec::<String>::new());
}

/// The invalidation half for a script target: deleting it must surface the row, and recreating it
/// must clear it, without the referencing file itself being touched.
#[test]
fn deleting_and_recreating_a_script_target_moves_the_row() {
    let p = project("extends Node\n\nconst A = preload(\"res://src/other.gd\")\n");
    p.write("src/other.gd", "extends Node\n");
    let mut ws = Workspace::load(&p.root, &options(&p));
    assert_eq!(errors(&mut ws, &p), Vec::<String>::new());

    let target = p.root.join("src/other.gd");
    std::fs::remove_file(target.as_std_path()).expect("remove target");
    ws.remove(&target);
    assert_eq!(
        errors(&mut ws, &p),
        vec![r#"Preload file "res://src/other.gd" does not exist."#.to_owned()],
        "the row must appear without the referencing file being edited"
    );

    p.write("src/other.gd", "extends Node\n");
    let tree = ws.parse_source("extends Node\n").tree;
    ws.reindex(&target, &tree);
    assert_eq!(
        errors(&mut ws, &p),
        Vec::<String>::new(),
        "and clear again when the target comes back"
    );
}

/// The same for a non-script resource, which carries no `FileId` and so no dependency edge — the
/// path-reference table is the only thing that can reach its consumers.
#[test]
fn deleting_and_recreating_a_resource_target_moves_the_row() {
    let p = project("extends Node\n\nconst A = preload(\"res://data/t.tres\")\n");
    p.write("data/t.tres", "[gd_resource type=\"Resource\"]\n");
    let mut ws = Workspace::load(&p.root, &options(&p));
    assert_eq!(errors(&mut ws, &p), Vec::<String>::new());

    let target = p.root.join("data/t.tres");
    std::fs::remove_file(target.as_std_path()).expect("remove target");
    ws.remove_asset(&target);
    ws.relink_resource_path(&target);
    assert_eq!(
        errors(&mut ws, &p),
        vec![r#"Preload file "res://data/t.tres" does not exist."#.to_owned()]
    );

    p.write("data/t.tres", "[gd_resource type=\"Resource\"]\n");
    ws.reindex_asset(&target);
    ws.relink_resource_path(&target);
    assert_eq!(errors(&mut ws, &p), Vec::<String>::new());
}
