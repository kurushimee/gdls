//! `ProjectModel::load` against a fixture project that mirrors a real large-project shape:
//! mixed autoload targets (`*res://…gd`, `*uid://…`, non-singleton), a UID sidecar, a `.gdextension`.

use camino::Utf8PathBuf;
use gd_project::{ProjectModel, ResTarget};

fn proj_root() -> Utf8PathBuf {
    Utf8PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/proj")
}

#[test]
fn loads_scalars_autoloads_and_warnings() {
    let m = ProjectModel::load(&proj_root());
    assert_eq!(m.config_version, 5);
    assert_eq!(m.autoloads.len(), 3);
    assert!(m.warnings.enable);
    assert_eq!(
        m.warnings.levels.get("untyped_declaration"),
        Some(&gd_project::WarnLevel::Error)
    );
}

#[test]
fn uid_map_built_from_sidecar() {
    let m = ProjectModel::load(&proj_root());
    assert_eq!(
        m.uids.get("uid://cyftuo4syatlv").map(String::as_str),
        Some("res://src/panku.gd")
    );
}

#[test]
fn uid_autoload_resolves_through_map() {
    let m = ProjectModel::load(&proj_root());
    let panku = m.autoloads.iter().find(|a| a.name == "Panku").unwrap();
    assert!(panku.is_singleton);
    assert_eq!(panku.target, ResTarget::Uid("uid://cyftuo4syatlv".into()));
    // resolve_target follows the uid:// through the map to the concrete script
    assert_eq!(
        m.resolve_target(&panku.target),
        Some(ResTarget::Script("res://src/panku.gd".into()))
    );
}

#[test]
fn main_scene_uid_unresolvable_is_none() {
    let m = ProjectModel::load(&proj_root());
    // main_scene points at uid://mainmainmain, which has no sidecar ⇒ unresolvable (deferred).
    let main = m.main_scene.clone().expect("main_scene present");
    assert_eq!(main, ResTarget::Uid("uid://mainmainmain".into()));
    assert_eq!(m.resolve_target(&main), None);
}

#[test]
fn gdextension_enumerated_with_class_hint() {
    let m = ProjectModel::load(&proj_root());
    assert_eq!(m.gdextensions.len(), 1);
    assert!(m.gdextensions[0].class_hints.iter().any(|c| c == "FooNode"));
}

#[test]
fn res_path_mapping() {
    let m = ProjectModel::load(&proj_root());
    let p = m.res_to_path("res://src/panku.gd").expect("maps");
    assert!(p.as_str().replace('\\', "/").ends_with("proj/src/panku.gd"));
    assert_eq!(m.path_to_res(&p).as_deref(), Some("res://src/panku.gd"));
}
