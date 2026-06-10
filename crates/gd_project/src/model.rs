//! [`ProjectModel`] — the assembled project environment: `project.godot` + the UID map + the
//! enumerated GDExtensions, rooted at a `res://` directory.

use camino::{Utf8Path, Utf8PathBuf};
use rustc_hash::FxHashMap;

use crate::gdextension::{self, GdExtension};
use crate::paths;
use crate::project_godot::{self, Autoload, ResTarget, WarningConfig};

/// Everything gdls knows about a project's environment, short of the per-script interface index
/// (which the indexer owns). `Clone` because the background auto-dump thread
/// (`gd_server::api_dump`) snapshots the model it was started against.
#[derive(Clone, Debug)]
pub struct ProjectModel {
    pub root: Utf8PathBuf,
    pub config_version: u32,
    pub main_scene: Option<ResTarget>,
    pub autoloads: Vec<Autoload>,
    pub warnings: WarningConfig,
    pub gdextensions: Vec<GdExtension>,
    /// `uid:// → res://path`, from scanning `*.uid` sidecars.
    pub uids: FxHashMap<String, String>,
}

/// WP-RD13: the outcome of a [`ProjectModel::load_checked`] attempt, fine-grained enough for the
/// reload path to decide whether to adopt the freshly-loaded model or keep its prior one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LoadOutcome {
    /// Read OK and the parse confidence cleared [`ProjectModel::CONFIDENCE_THRESHOLD`].
    Loaded,
    /// `project.godot` is genuinely absent — the documented "treat root as `res://`" degrade.
    /// Rebuild from defaults; nothing was lost.
    Absent,
    /// Present but unreadable (locked mid-save, permission denied, non-UTF-8). A reload should keep
    /// its previously-resolved model rather than reset to defaults on a transient glitch.
    ReadFailed,
    /// Read OK but the parse confidence was below threshold — corrupt-but-parseable garbage the
    /// tolerant parser accepted as a near-default "clean" parse. Same treatment as [`Self::ReadFailed`].
    Corrupt,
}

impl LoadOutcome {
    /// Whether a *reload* caller should keep its previously-resolved model, native DB, and warning
    /// policy rather than adopt this load (the transient-glitch / corruption cases).
    #[must_use]
    pub fn should_preserve_prior(self) -> bool {
        matches!(self, LoadOutcome::ReadFailed | LoadOutcome::Corrupt)
    }
}

impl ProjectModel {
    /// WP-RD13: parse-confidence below this is treated as corrupt-but-parseable — the reload path
    /// then preserves the prior model. 0.5 = "more than half the meaningful lines were garbled".
    pub const CONFIDENCE_THRESHOLD: f32 = 0.5;

    /// Load the model rooted at `root` (the directory containing `project.godot`). A missing or
    /// unreadable `project.godot` degrades to defaults (root treated as `res://`), never an error.
    /// An *actionable* failure (present but locked/permission-denied/non-UTF-8 — common on Windows
    /// during a save) is logged before the fallback; a genuinely absent file is not (that's the
    /// documented standalone-`.gd` case). Callers that must react to the difference (e.g. keep a
    /// prior warning policy rather than reset it) use [`Self::load_checked`].
    pub fn load(root: &Utf8Path) -> Self {
        Self::load_checked(root).0
    }

    /// Like [`Self::load`], but also reports a [`LoadOutcome`] so a reload caller can keep its prior
    /// model when the on-disk `project.godot` was present-but-unreadable (the actionable failure
    /// that already got a `log::warn!`) OR corrupt-but-parseable (WP-RD13 confidence below
    /// [`Self::CONFIDENCE_THRESHOLD`]). A clean read or a genuinely absent file is safe to adopt.
    pub fn load_checked(root: &Utf8Path) -> (Self, LoadOutcome) {
        let path = root.join("project.godot");
        let (project, outcome) = match std::fs::read_to_string(&path) {
            Ok(text) => {
                let (pg, confidence) = project_godot::parse_with_confidence(&text);
                if confidence < Self::CONFIDENCE_THRESHOLD {
                    log::warn!(
                        "project.godot at {path} parsed at low confidence ({confidence:.2} < \
                         {threshold}); treating as corrupt-but-parseable",
                        threshold = Self::CONFIDENCE_THRESHOLD
                    );
                    (pg, LoadOutcome::Corrupt)
                } else {
                    (pg, LoadOutcome::Loaded)
                }
            }
            // A genuinely absent file is the documented "treat root as res://" degrade — expected,
            // stay quiet and don't flag it as a failure.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                (project_godot::ProjectGodot::default(), LoadOutcome::Absent)
            }
            // Present but unreadable (locked mid-save, permission denied, non-UTF-8): falling back
            // to defaults would silently empty the warning configuration, so surface the concrete
            // reason and let the caller decide whether to keep its prior state.
            Err(e) => {
                log::warn!("project.godot at {path} is unreadable ({e}); falling back to defaults");
                (
                    project_godot::ProjectGodot::default(),
                    LoadOutcome::ReadFailed,
                )
            }
        };
        let uids = paths::build_uid_map(root);
        let gdextensions = gdextension::enumerate(root);
        (
            ProjectModel {
                root: root.to_path_buf(),
                config_version: project.config_version,
                main_scene: project.main_scene,
                autoloads: project.autoloads,
                warnings: project.warnings,
                gdextensions,
                uids,
            },
            outcome,
        )
    }

    /// `"res://a/b.gd"` → `<root>/a/b.gd`.
    pub fn res_to_path(&self, res: &str) -> Option<Utf8PathBuf> {
        paths::res_to_path(&self.root, res)
    }

    /// `<root>/a/b.gd` → `"res://a/b.gd"`.
    pub fn path_to_res(&self, path: &Utf8Path) -> Option<String> {
        paths::path_to_res(&self.root, path)
    }

    /// Follow a `uid://` target through the UID map to a concrete `res://` script/scene; pass other
    /// targets through unchanged. Returns `None` only for an unresolvable `uid://`.
    pub fn resolve_target(&self, target: &ResTarget) -> Option<ResTarget> {
        match target {
            ResTarget::Uid(uid) => self.uids.get(uid).map(|res| project_godot::classify(res)),
            other => Some(other.clone()),
        }
    }

    /// The `res://` script path a configured autoload `name` points at, or `None` if there is no
    /// such autoload or its target is not a script (e.g. a scene). `uid://` targets resolve
    /// through [`Self::resolve_target`] first, so an autoload declared as `Name="*uid://…"`
    /// whose sidecar maps to a `.gd` works exactly like a `res://….gd` one — Godot's
    /// `ResourceLoader` dereferences the uid the same way (analyzer.cpp:4579). Scene targets
    /// (direct or uid-resolved) stay `None`: scene-root script typing is the Phase-2 `.tscn`
    /// family. Consumed by go-to-definition / references / singleton typing on autoload names.
    #[must_use]
    pub fn autoload_script_path(&self, name: &str) -> Option<String> {
        let autoload = self.autoloads.iter().find(|a| a.name == name)?;
        match self.resolve_target(&autoload.target)? {
            ResTarget::Script(path) => Some(path),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::project_godot::{Autoload, ResTarget, WarningConfig};
    use rustc_hash::FxHashMap;

    #[test]
    fn autoload_script_path_resolves_script_targets_only() {
        let mut uids = FxHashMap::default();
        uids.insert("uid://gdscript1".to_owned(), "res://via_uid.gd".to_owned());
        uids.insert("uid://scene1".to_owned(), "res://via_uid.tscn".to_owned());
        let model = ProjectModel {
            root: Utf8PathBuf::from("/tmp/project"),
            config_version: 5,
            main_scene: None,
            autoloads: vec![
                Autoload {
                    name: "Save".into(),
                    target: ResTarget::Script("res://save.gd".into()),
                    is_singleton: true,
                },
                Autoload {
                    name: "Music".into(),
                    target: ResTarget::Scene("res://music.tscn".into()),
                    is_singleton: true,
                },
                Autoload {
                    name: "UidScript".into(),
                    target: ResTarget::Uid("uid://gdscript1".into()),
                    is_singleton: true,
                },
                Autoload {
                    name: "UidScene".into(),
                    target: ResTarget::Uid("uid://scene1".into()),
                    is_singleton: true,
                },
                Autoload {
                    name: "UidUnknown".into(),
                    target: ResTarget::Uid("uid://nosidecar".into()),
                    is_singleton: true,
                },
            ],
            warnings: WarningConfig::default(),
            gdextensions: vec![],
            uids,
        };

        assert_eq!(
            model.autoload_script_path("Save"),
            Some("res://save.gd".to_owned())
        );
        assert_eq!(model.autoload_script_path("Music"), None); // non-script (Scene) target
        assert_eq!(model.autoload_script_path("Nope"), None); // unknown name
        assert_eq!(
            model.autoload_script_path("UidScript"),
            Some("res://via_uid.gd".to_owned()),
            "uid -> .gd resolves through the sidecar map"
        );
        assert_eq!(model.autoload_script_path("UidScene"), None); // uid -> scene: Phase 2
        assert_eq!(model.autoload_script_path("UidUnknown"), None); // no sidecar entry
    }
}
