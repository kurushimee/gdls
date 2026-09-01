//! [`ProjectModel`] — the assembled project environment: `project.godot` + the UID map + the
//! enumerated GDExtensions, rooted at a `res://` directory.

use camino::{Utf8Path, Utf8PathBuf};
use rustc_hash::FxHashMap;

use crate::gdextension::{self, GdExtension};
use crate::paths;
use crate::project_godot::{self, Autoload, ResTarget, WarningConfig};
use crate::scene_index::SceneIndex;

/// How a configured autoload singleton should be TYPED — the gd_project half of Godot's autoload
/// arm (`gdscript_analyzer.cpp:4570-4609`). An autoload is always at least a `Node`; if it can be
/// resolved to a backing GDScript (a `.gd` target directly, or a scene whose root node attaches a
/// `.gd`) it is typed as that script INSTANCE — exactly what Godot does (verified against the
/// 4.6.3-stable binary: a scene autoload completes identically to the same script declared as a
/// direct-script autoload).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AutoloadTyping {
    /// The autoload's backing GDScript, by `res://….gd` path. Direct `.gd` target, a `uid://` that
    /// dereferences to a `.gd`, OR a scene whose resolved root node attaches a `.gd`. The caller
    /// maps this path to a `FileId` and types the name as that Script instance (the #19 seam).
    Script(String),
    /// A scene autoload that resolved to a scene with NO root script (or whose root script isn't an
    /// indexable `.gd`). Godot still types these as a bare native `Node` (hard-coded in the
    /// `PackedScene` arm — NOT the root's specific native type). Carrying it lets the analyzer set
    /// that `Node` floor instead of degrading to dynamic, which would leave a lowercase-named
    /// scriptless autoload tripping the "Identifier not declared" fallthrough (a false positive).
    NativeNode,
}

/// Everything gdls knows about a project's environment, short of the per-script interface index
/// (which the indexer owns). `Clone` because the background auto-dump thread
/// (`gd_server::api_dump`) snapshots the model it was started against.
#[derive(Clone, Debug)]
pub struct ProjectModel {
    pub root: Utf8PathBuf,
    pub config_version: u32,
    /// The Godot feature release declared by `application/config/features`, or `None` when the key
    /// is missing or version-less. See [`crate::ProjectGodot::declared_engine_version`]; resolved
    /// into an actual dialect by [`crate::resolve_dialect`].
    pub declared_engine_version: Option<(u32, u32)>,
    pub main_scene: Option<ResTarget>,
    pub autoloads: Vec<Autoload>,
    pub warnings: WarningConfig,
    pub gdextensions: Vec<GdExtension>,
    /// `uid:// → res://path`, from scanning `*.uid` sidecars.
    pub uids: FxHashMap<String, String>,
    /// The uids two resources claimed, which [`paths::build_uid_map`] refuses to answer for. Godot
    /// still resolves one, so a consumer must not read its absence from `uids` as "no such
    /// resource" (#565).
    pub contested_uids: rustc_hash::FxHashSet<String>,
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
        let (uids, contested_uids) = paths::build_uid_map_checked(root);
        let gdextensions = gdextension::enumerate(root);
        (
            ProjectModel {
                root: root.to_path_buf(),
                config_version: project.config_version,
                declared_engine_version: project.declared_engine_version,
                main_scene: project.main_scene,
                autoloads: project.autoloads,
                warnings: project.warnings,
                gdextensions,
                uids,
                contested_uids,
            },
            outcome,
        )
    }

    /// Whether a real `project.godot` backed this load. `config_version` is the first key Godot
    /// writes and no default supplies it, so a non-zero value means the file was found and parsed —
    /// the positive-project signal a negative claim about the project tree has to clear (#555).
    #[must_use]
    pub fn is_loaded(&self) -> bool {
        self.config_version > 0
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

    /// The `res://` script path a configured autoload `name` points at *directly*, or `None` if
    /// there is no such autoload or its target is not a script (e.g. a scene). `uid://` targets
    /// resolve through [`Self::resolve_target`] first, so an autoload declared as `Name="*uid://…"`
    /// whose sidecar maps to a `.gd` works exactly like a `res://….gd` one — Godot's
    /// `ResourceLoader` dereferences the uid the same way (analyzer.cpp:4579). Scene targets
    /// (direct or uid-resolved) stay `None` here: their root-script resolution needs the scene index
    /// and lives in [`Self::autoload_typing`]. Kept for go-to-definition / references on autoload
    /// names where only a *direct* script target is meaningful.
    #[must_use]
    pub fn autoload_script_path(&self, name: &str) -> Option<String> {
        let autoload = self.autoloads.iter().find(|a| a.name == name)?;
        match self.resolve_target(&autoload.target)? {
            ResTarget::Script(path) => Some(path),
            _ => None,
        }
    }

    /// How a configured autoload `name` should be typed, mirroring Godot's autoload arm
    /// (`gdscript_analyzer.cpp:4570-4609`). Resolves through `uid://` ([`Self::resolve_target`], the
    /// SAME hop the script case uses — no second uid resolver) and, for a scene target, looks the
    /// scene up in `scenes` to find its root node's attached script.
    ///
    /// * direct `.gd` (or `uid://`→`.gd`) → [`AutoloadTyping::Script`] — the #19 path, unchanged.
    /// * `.tscn`/`.scn` (or `uid://`→scene) with an indexed root `.gd` → [`AutoloadTyping::Script`]
    ///   at that root script — precise, matching Godot (which dereferences the singleton's scene
    ///   instance to its root `node->get_script()`).
    /// * a scene with no root script (or a root script that isn't a `.gd` we can name) →
    ///   [`AutoloadTyping::NativeNode`] — Godot's hard-coded bare-`Node` floor for a scene autoload.
    /// * `None` only when `name` is not a configured autoload, or its `uid://` is unresolvable, or
    ///   the scene isn't indexed — i.e. nothing certain to say → the caller degrades to the prior
    ///   generic typing (no false positive).
    ///
    /// The root script is taken via the index-backed root resolution
    /// ([`SceneIndex::resolve_relative_from`] with an empty relative path), so an instanced-root
    /// scene follows its sub-scene chain; any uncertainty there (missing sub-scene, cycle, depth
    /// cap) collapses to `None` from the resolver and we fall back to the bare-`Node` floor.
    #[must_use]
    pub fn autoload_typing(&self, name: &str, scenes: &SceneIndex) -> Option<AutoloadTyping> {
        let autoload = self.autoloads.iter().find(|a| a.name == name)?;
        // Godot gates the WHOLE autoload typing arm on `is_singleton` (gdscript_analyzer.cpp:4572).
        // A `project.godot` entry without the leading `*` (`autoload/Name="res://x.gd"`) registers the
        // path but is NOT instantiated as a global singleton: `has_autoload` is true but
        // `is_singleton` is false, so Godot skips the block — a bare `Name` reference is then
        // `Identifier "Name" not declared`. Refuse to type a non-singleton so gdls matches (and so a
        // non-singleton never feeds a wrong hover/definition/reference on its name).
        if !autoload.is_singleton {
            return None;
        }
        match self.resolve_target(&autoload.target)? {
            ResTarget::Script(path) => Some(AutoloadTyping::Script(path)),
            ResTarget::Scene(scene_res) => {
                // The scene must be indexed to say anything; an un-indexed scene → None (degrade).
                let _ = scenes.scene(&scene_res)?;
                // Resolve the scene's ROOT root-relative (`""`), following an instanced root through
                // the index. `Some(root)` with a `.gd` script → precise; otherwise the bare-`Node`
                // floor Godot uses for a scene autoload.
                let root = scenes.resolve_relative_from(&scene_res, "", "");
                match root.and_then(|r| r.script) {
                    Some(script_res) if script_res.ends_with(".gd") => {
                        Some(AutoloadTyping::Script(script_res))
                    }
                    _ => Some(AutoloadTyping::NativeNode),
                }
            }
            // `uid://` that didn't dereference (handled above as None) or a resource kind we don't
            // type: not a typeable autoload target.
            ResTarget::Uid(_) | ResTarget::Unresolved(_) => None,
        }
    }

    /// The `res://….gd` script an autoload `name` resolves to, covering BOTH a direct-script target
    /// and a SCENE target whose root node attaches a `.gd` (M11 Phase 4). The scene-aware companion
    /// to [`Self::autoload_script_path`] — the navigation handlers (definition / references on an
    /// autoload name) use this so a scene autoload lands on its root script, exactly as its typing
    /// (member hover/completion) already does. Returns `None` for a scriptless scene, an unresolvable
    /// target, or a non-autoload name. Thin projection of [`Self::autoload_typing`]'s `Script` arm.
    #[must_use]
    pub fn autoload_script_path_in_scenes(
        &self,
        name: &str,
        scenes: &SceneIndex,
    ) -> Option<String> {
        match self.autoload_typing(name, scenes)? {
            AutoloadTyping::Script(path) => Some(path),
            AutoloadTyping::NativeNode => None,
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
            declared_engine_version: None,
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
            contested_uids: rustc_hash::FxHashSet::default(),
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
        // `autoload_script_path` is the DIRECT-script accessor: a scene target is None here even
        // when its root attaches a `.gd` — that root-script resolution is `autoload_typing`'s job.
        assert_eq!(model.autoload_script_path("UidScene"), None);
        assert_eq!(model.autoload_script_path("UidUnknown"), None); // no sidecar entry
    }

    /// Build a [`ProjectModel`] with the given autoloads and uid map, rooted at `/tmp/project`.
    fn model_with(autoloads: Vec<Autoload>, uids: FxHashMap<String, String>) -> ProjectModel {
        ProjectModel {
            root: Utf8PathBuf::from("/tmp/project"),
            config_version: 5,
            declared_engine_version: None,
            main_scene: None,
            autoloads,
            warnings: WarningConfig::default(),
            gdextensions: vec![],
            uids,
            contested_uids: rustc_hash::FxHashSet::default(),
        }
    }

    fn autoload(name: &str, target: ResTarget) -> Autoload {
        Autoload {
            name: name.into(),
            target,
            is_singleton: true,
        }
    }

    /// `autoload_typing` mirrors Godot's autoload arm: a scene autoload resolves to its root node's
    /// attached `.gd` (precise Script), uid hops dereference, a scriptless scene is the bare-`Node`
    /// floor, and any un-indexed/unresolvable target degrades to `None`.
    #[test]
    fn autoload_typing_resolves_scene_root_script_and_native_floor() {
        use crate::scene_index::SceneIndex;

        // A scene whose root attaches a `.gd`; a scriptless scene (native Node2D root); and a scene
        // missing from the index entirely.
        let mut scenes = SceneIndex::new();
        scenes.reindex(
            "res://global.tscn",
            "[gd_scene format=3]\n\
             [ext_resource type=\"Script\" path=\"res://global.gd\" id=\"1\"]\n\
             [node name=\"GlobalRoot\" type=\"Node\"]\nscript = ExtResource(\"1\")\n",
        );
        scenes.reindex(
            "res://scriptless.tscn",
            "[gd_scene format=3]\n[node name=\"Root\" type=\"Node2D\"]\n",
        );

        let mut uids = FxHashMap::default();
        uids.insert("uid://scenuid".to_owned(), "res://global.tscn".to_owned());
        uids.insert("uid://scriptuid".to_owned(), "res://lib.gd".to_owned());

        let model = model_with(
            vec![
                autoload("DirectScript", ResTarget::Script("res://save.gd".into())),
                autoload("SceneScript", ResTarget::Scene("res://global.tscn".into())),
                autoload("UidScene", ResTarget::Uid("uid://scenuid".into())),
                autoload("UidScript", ResTarget::Uid("uid://scriptuid".into())),
                autoload(
                    "Scriptless",
                    ResTarget::Scene("res://scriptless.tscn".into()),
                ),
                autoload("MissingScene", ResTarget::Scene("res://nope.tscn".into())),
                autoload("BadUid", ResTarget::Uid("uid://nosidecar".into())),
            ],
            uids,
        );

        // Direct `.gd` target → Script (unchanged #19 path).
        assert_eq!(
            model.autoload_typing("DirectScript", &scenes),
            Some(AutoloadTyping::Script("res://save.gd".to_owned()))
        );
        // Scene whose root attaches `.gd` → precise root script.
        assert_eq!(
            model.autoload_typing("SceneScript", &scenes),
            Some(AutoloadTyping::Script("res://global.gd".to_owned())),
            "a scene autoload resolves to its root node's attached script"
        );
        // uid → scene → root script (the same uid hop the script case uses; no second resolver).
        assert_eq!(
            model.autoload_typing("UidScene", &scenes),
            Some(AutoloadTyping::Script("res://global.gd".to_owned())),
            "uid -> scene -> root script resolves precisely"
        );
        // uid → `.gd` directly.
        assert_eq!(
            model.autoload_typing("UidScript", &scenes),
            Some(AutoloadTyping::Script("res://lib.gd".to_owned()))
        );
        // Scriptless scene → bare native Node floor (Godot's hard-coded `Node`, not `Node2D`).
        assert_eq!(
            model.autoload_typing("Scriptless", &scenes),
            Some(AutoloadTyping::NativeNode),
            "a scriptless scene autoload falls back to the bare-Node floor"
        );
        // Scene not in the index → None (degrade, no false positive).
        assert_eq!(model.autoload_typing("MissingScene", &scenes), None);
        // uid with no sidecar entry → None.
        assert_eq!(model.autoload_typing("BadUid", &scenes), None);
        // Not a configured autoload → None.
        assert_eq!(model.autoload_typing("Nope", &scenes), None);
    }

    /// Godot gates the autoload typing arm on `is_singleton` (analyzer.cpp:4572): a `project.godot`
    /// entry WITHOUT the leading `*` is registered but not a global singleton, so it must NOT be
    /// typed — even when its target is a perfectly resolvable scene-with-root-script or a `.gd`.
    #[test]
    fn non_singleton_autoload_is_not_typed() {
        use crate::scene_index::SceneIndex;
        let mut scenes = SceneIndex::new();
        scenes.reindex(
            "res://global.tscn",
            "[gd_scene format=3]\n\
             [ext_resource type=\"Script\" path=\"res://global.gd\" id=\"1\"]\n\
             [node name=\"GlobalRoot\" type=\"Node\"]\nscript = ExtResource(\"1\")\n",
        );
        let non_singleton_scene = Autoload {
            name: "PlainScene".into(),
            target: ResTarget::Scene("res://global.tscn".into()),
            is_singleton: false, // no leading `*`
        };
        let non_singleton_script = Autoload {
            name: "PlainScript".into(),
            target: ResTarget::Script("res://global.gd".into()),
            is_singleton: false,
        };
        let model = model_with(
            vec![non_singleton_scene, non_singleton_script],
            FxHashMap::default(),
        );
        assert_eq!(
            model.autoload_typing("PlainScene", &scenes),
            None,
            "a non-singleton (no `*`) scene autoload must not be typed (matches Godot's is_singleton gate)"
        );
        assert_eq!(
            model.autoload_typing("PlainScript", &scenes),
            None,
            "a non-singleton (no `*`) script autoload must not be typed either"
        );
        assert_eq!(
            model.autoload_script_path_in_scenes("PlainScene", &scenes),
            None,
            "the nav projection must also refuse a non-singleton"
        );
    }
}
