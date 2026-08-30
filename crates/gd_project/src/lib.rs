//! `gd_project` — the project model and indexer.
//!
//! Owns `project.godot` parsing (autoloads, `res://` root, warning config), the `res://`↔filesystem
//! mapping + UID map, `.gdextension` enumeration, the `class_name` registry, the eager-interface
//! index, the dependency graph + invalidation machinery, and (M4) the `notify` freshness watcher.
//!
//! M2 delivers: project model (this milestone's WP-C), the eager interface index + registry (WP-D),
//! and the dependency graph + `on_file_changed` invalidation (WP-E). The `notify` watcher that drives
//! that invalidation lands in M4 (`docs/03-indexing-freshness.md`).

pub mod asset_index;
pub mod cache;
pub mod depgraph;
pub mod dialect_resolve;
pub mod exclude;
pub mod gdextension;
pub mod index;
pub mod interface;
pub mod model;
pub mod paths;
pub mod project_godot;
pub mod registry;
pub mod scene;
pub mod scene_index;

pub use asset_index::{AssetIndex, AssetIndexCache};
pub use cache::{
    load as cache_load, project_godot_fingerprint, save as cache_save, stat_from_metadata,
    CacheKey, FileStat, LoadedCache, CACHE_FORMAT_VERSION,
};
pub use depgraph::{DepGraph, FileId};
pub use dialect_resolve::{dialect_notice, resolve_dialect, DialectOrigin};
pub use exclude::{is_excluded, ProjectRoot, EXCLUDED_COMPONENTS};
pub use gdextension::GdExtension;
pub use index::{normalize as normalize_path, Index, IndexInvariant, IndexMut, Resolution};
pub use interface::{
    enum_decl, extract as extract_interface, EnumDecl, EnumValueDecl, Extends, InitShape,
    Interface, MemberDecl, MemberFlags, MemberKind, TypeExpr,
};
pub use model::{AutoloadTyping, LoadOutcome, ProjectModel};
pub use paths::{path_to_res, res_to_path};
pub use project_godot::{
    autoload_key_span, classify, parse as parse_project_godot, Autoload, ProjectGodot, ResTarget,
    WarnLevel, WarningConfig,
};
pub use registry::{BaseRef, ClassEntry, ClassNameRegistry};
pub use scene::{
    is_scene_path, normalize_res, parse_scene, ExtResource, NodeType, ResolvedRoot, Scene,
    SceneNode, MAX_INSTANCE_DEPTH,
};
pub use scene_index::{SceneIndex, SceneIndexCache};
