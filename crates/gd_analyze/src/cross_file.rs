//! The cross-file resolution seam — the analyzer's window onto the rest of the project.
//!
//! Godot resolves a depended script lazily, re-parsing and shallow-resolving it on demand
//! (`GDScriptParserRef::raise_status(INHERITANCE_SOLVED)`). The parse cache that can do that lives in
//! `gd_server`, so to keep the crate DAG acyclic the analyzer never reaches for it: it depends only on
//! this trait, and `gd_server` implements it over its caches (the deep, re-parsing variant lands with
//! the resolved-interface cache in a later WP).
//!
//! For tests and the lean first cut, [`SyntacticQuery`] backs the trait with M2's *syntactic* [`Index`] — no
//! re-parse. That resolves names and reads `extends`/`class_name` from the eager interface tables,
//! which is everything `resolve_inheritance` needs; deeper cross-file member typing upgrades the impl
//! without touching the analyzer.

use gd_project::{EnumDecl, FileId, Index, Interface, MemberDecl, Resolution};
use gd_types::NativeDb;

use crate::binding::MemberXref;

/// A statically-known `$`/`%` node-path access the NAVIGATION surfaces ask the scene index to
/// resolve (see [`CrossFileQuery::scene_node_facts`]; NOT the diagnostic path).
///
/// Deliberately the SUBSET of `$`/`%` shapes scene resolution handles soundly. A `get_node("A/B")` /
/// `get_node("%Name")` call spells the same access and maps to the same query; every other shape
/// (an absolute `$/root/...` path, an embedded/multi-segment `%`) yields no query and stays
/// unresolved, which the navigation surfaces degrade on gracefully (`gd_server::scene_nav`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NodePathQuery {
    /// A root-relative `$A/B`-style path (no leading `/`, no `%`), resolved RELATIVE to the node the
    /// querying script is attached to. The string is the path text after the `$` (e.g. `"A/B"`).
    RelativePath(String),
    /// A single-segment `%Name` unique-name lookup, resolved in the owner scene's unique-name table.
    /// The string is the bare name (no `%` prefix).
    UniqueName(String),
}

/// What the scene index knows about a `$`/`%` access target — a *fact*, never a `DataType` (the trait
/// stays free of the analyzer's type lattice; a consumer builds a `DataType` from this). Navigation
/// substrate (NOT the diagnostic path — [`CrossFileQuery::scene_node_facts`]).
///
/// Resolution is CONSERVATIVE end-to-end: a `Some` means every attaching scene agreed on this exact
/// fact; any ambiguity (no scene, absent node, instanced sub-scene unresolved, two scenes
/// disagreeing) yields `None` from [`CrossFileQuery::scene_node_facts`] (a missed precise type is a
/// known limitation; a wrong one is a defect).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SceneNodeFacts {
    /// The target node is a native/engine class (e.g. `"Sprite2D"`), with no attached script.
    Native(String),
    /// The target node has an attached GDScript (a `script=` on the node, or the script at the root
    /// of an instanced sub-scene) — typed as a Script INSTANCE of that file.
    Script(FileId),
}

/// What the analyzer can ask about *other* files while resolving the one in hand.
///
/// Deliberately small: it exposes project facts (does this global name exist? what does this file
/// expose? where does this `res://` path point?) and lets the analyzer do the type reasoning. It
/// never returns a `DataType`, so the trait stays free of the lattice and the server impl stays free
/// of analyzer internals.
pub trait CrossFileQuery {
    /// Resolve a bare global class name to the project file that declares it as a `class_name`, if
    /// any. Godot's `ScriptServer::is_global_class` + `get_global_class_path`. `None` covers both
    /// "native class" and "unknown" — the analyzer checks the native DB itself.
    fn global_class_file(&self, name: &str) -> Option<FileId>;

    /// The syntactic [`Interface`] of an indexed file (its `extends`, `class_name`, member
    /// signatures), or `None` if the file is not indexed.
    fn interface(&self, file: FileId) -> Option<&Interface>;

    /// Resolve an `extends "res://path.gd"` literal to an indexed file.
    fn resolve_res_path(&self, path: &str) -> Option<FileId>;

    // ---- WP-P1 cross-file readthroughs ----
    //
    // The following helpers walk an indexed file's [`Interface`] tree by name — they don't run
    // type analysis on the depended file, just navigate its syntactic shape. Godot's analog is
    // `GDScriptParserRef::raise_status(INHERITANCE_SOLVED)` reaching into a depended file's
    // `ClassNode` and walking by `member.identifier` / `inner_classes[i].identifier`.
    //
    // Default impls reach through `Self::interface()` so most impls (incl. `SyntacticQuery`,
    // `NoCrossFile`) get the right behavior for free; a future deep-resolution impl in `gd_server`
    // overrides selectively.

    /// Walk a file's inner class chain by name path (e.g. `["InnerA", "InnerAB"]`). Returns the
    /// innermost matching `Interface`. Used by `resolve_class_inheritance` for path-extends
    /// attribute chains (`extends "foo.gd".InnerA.InnerAB`).
    fn resolve_inner_chain<'a>(&'a self, file: FileId, chain: &[&str]) -> Option<&'a Interface> {
        let root = self.interface(file)?;
        let mut current = root;
        for &name in chain {
            current = current
                .inner
                .iter()
                .find(|c| c.class_name.as_deref() == Some(name))?;
        }
        Some(current)
    }

    /// Look up a top-level member by name in a file's interface. Used by cross-file member-on-Script
    /// attribute walks (e.g. `external_parser.gd`'s `OtherFile.x`).
    fn lookup_file_member<'a>(&'a self, file: FileId, name: &str) -> Option<&'a MemberDecl> {
        self.interface(file)?
            .members
            .iter()
            .find(|m| m.name == name)
    }

    /// Look up a named enum (and its value list) in a file's interface. Used by cross-file
    /// enum-value resolution (e.g. `preload_enum_error.gd`'s `P.Named.VALUE_A`).
    fn lookup_file_enum<'a>(&'a self, file: FileId, name: &str) -> Option<&'a EnumDecl> {
        self.interface(file)?.enums.iter().find(|e| e.name == name)
    }

    /// True iff `name` is a constant hoisted from an unnamed `enum { … }` block in `file`'s head
    /// class. Drives `reduce_identifier_from_base`'s Script-meta anonymous-enum arm: only genuine
    /// hoists may type as an enum *value* (Godot's ENUM_VALUE member arm, analyzer.cpp:4203-4209);
    /// a regular `const` takes the CONSTANT arm (analyzer.cpp:4193-4200) and its declared type.
    fn is_unnamed_enum_value(&self, file: FileId, name: &str) -> bool {
        self.interface(file)
            .is_some_and(|i| i.unnamed_enum_values.iter().any(|v| v == name))
    }

    /// Resolve a script path that may be RELATIVE to the referring file — Godot resolves
    /// `preload("sibling.gd")` / `extends "../base.gd"` against the script's own directory
    /// (analyzer.cpp:437's relativization). The default tries the raw path only (correct for
    /// `NoCrossFile` and for queries whose `resolve_res_path` already handles relative forms);
    /// [`SyntacticQuery`] overrides with a real join against the index's path table.
    fn resolve_path_from(&self, _from: FileId, raw: &str) -> Option<FileId> {
        self.resolve_res_path(raw)
    }

    /// The `res://`-rendered path a `preload`/`load` argument names that this project view can
    /// PROVE holds no file, or `None` when it exists or the impl cannot testify (#555). `from` is
    /// the referring file, which a relative literal needs.
    ///
    /// Fail-closed by construction: the default answers `None`, so `NoCrossFile`, [`SyntacticQuery`],
    /// and every test stub stay silent. Only an impl with a live, watcher-fresh view of the project
    /// tree may answer `Some` — the claim is a negative one, and gdls does not make those on a
    /// partial view (the same discipline the native DB's `ApiProvenance::Exact` gate enforces).
    fn preload_missing_path(&self, _from: Option<FileId>, _raw: &str) -> Option<String> {
        None
    }

    /// `uid://…` → the `res://` path the project's uid map names, or `None` when nothing declares
    /// that uid. Godot 4.4+ rewrites a `preload` argument to the uid form on save, so a modern
    /// project writes `preload("uid://cvc120a27s57m")` where it used to write the path, and every
    /// consumer that reads the argument as a path has to dereference it first. Default `None` keeps
    /// test stubs and `NoCrossFile` permissive — an unresolved uid degrades exactly as an
    /// unresolvable path does.
    fn resolve_uid(&self, _uid: &str) -> Option<String> {
        None
    }

    /// The class a TEXT resource holds, read off its own header line
    /// (`[gd_resource type="Theme" script_class="Foo" …]`). Godot loads the resource and types the
    /// `preload` by what came back (analyzer.cpp:4723's `type_from_variant`); the header carries
    /// that answer without the load, and a `script_class` names the script an instance carries
    /// rather than the script's native base. `raw` is the preload argument as written, already
    /// dereferenced if it was a uid. `None` — a binary `.res`, an unreadable file, no header —
    /// degrades to the caller's `Resource` floor. Candidates come back in priority order, so a
    /// `script_class` the project does not declare still lets `type=` answer. Empty by default,
    /// which keeps test stubs permissive.
    fn text_resource_classes(&self, _from: Option<FileId>, _raw: &str) -> Vec<String> {
        Vec::new()
    }

    /// The resource class an IMPORTED asset preloads as: the `type=` line of the asset's `.import`
    /// sidecar `[remap]` section. Godot types `preload` by the class of the resource the importer
    /// produced (analyzer.cpp:4749-4751 over the loaded Resource), and never guesses from the
    /// extension — a `.png` is `CompressedTexture2D` under the default importer, `Image` under the
    /// "Image" importer. The sidecar is where that class lives on disk.
    ///
    /// `raw` is the preload argument as written (a `res://…` path, or one RELATIVE to the
    /// referring script `from` — analyzer.cpp:437's relativization, mirroring
    /// [`CrossFileQuery::resolve_path_from`]). `None` (no `from` for a relative path, no sidecar,
    /// unreadable file, no `type=` line) degrades to the caller's Variant fallback — no guessing
    /// either way. Default `None` keeps test stubs and `NoCrossFile` permissive.
    fn imported_resource_class(&self, _from: Option<FileId>, _raw: &str) -> Option<String> {
        None
    }

    /// True iff the file's class is `@tool`. Used by MISSING_TOOL emission in resolve_class_inheritance.
    fn is_file_tool(&self, file: FileId) -> bool {
        self.interface(file).is_some_and(|i| i.is_tool)
    }

    /// True iff the file's class is `@abstract`. Used by `reduce_call`'s constructor arm
    /// (the Script-kind variant of WP-N15's native-abstract check).
    fn is_file_abstract(&self, file: FileId) -> bool {
        self.interface(file).is_some_and(|i| i.is_abstract)
    }

    /// The indexed file's source path (as a string). Used by cross-file diagnostic rendering
    /// that needs the file's basename (e.g. the `<file.gd>.<EnumName>` fqcn shape that
    /// Godot's `make_class_enum_type` produces for cross-file enums). `None` when the impl
    /// doesn't track paths (e.g. the `NoCrossFile` test stub).
    fn file_path(&self, _file: FileId) -> Option<&str> {
        None
    }

    /// Resolve a configured autoload singleton NAME to the project file of its script, if any.
    ///
    /// Godot's `ProjectSettings` autoload table → `ScriptServer` singleton. Returning `Some(fid)`
    /// makes `reduce_identifier` type the bare name as a Script INSTANCE (not meta type) pointing
    /// at `fid`, so member access through the singleton (`Global.popup_error(...)`) resolves via
    /// the existing Script-member path. `None` = not an autoload (default; overridden only by
    /// `WorkspaceXFileQuery` in `gd_server`, which has access to the `ProjectModel`).
    /// The file's `res://` path — Godot's head-class `fqcn` spelling (`gdscript_parser.cpp:702`),
    /// used ONLY when rendering a class that has no name of its own (`-self` in a script with no
    /// `class_name` reads as `res://src/probe6.gd`).
    ///
    /// Deliberately separate from [`Self::file_path`]: the server passes a bare BASENAME as
    /// `AnalysisContext::script_path` and basenames the index path for enum fqcns, because the
    /// self-side and cross-side enum spellings have to agree (#286). That machinery must not
    /// change, so the `res://` spelling gets its own accessor. The default is `file_path`
    /// verbatim, which is what every in-tree test query wants.
    fn res_path(&self, file: FileId) -> Option<String> {
        self.file_path(file).map(str::to_owned)
    }

    fn autoload_file(&self, _name: &str) -> Option<FileId> {
        None
    }

    /// The bare native class an autoload `name` should fall back to when it has NO backing script —
    /// i.e. a SCENE autoload whose resolved root node attaches no indexable `.gd`. Godot types such
    /// an autoload as a hard-coded `Node` (`gdscript_analyzer.cpp:4575-4609`: `result.native_type =
    /// "Node"` set before the resource-type checks, kept when the `PackedScene` arm finds no root
    /// script — NOT the root's specific native type), so the returned string is `"Node"` in practice.
    ///
    /// Consulted by `reduce_identifier` ONLY after [`Self::autoload_file`] missed: a script-backed
    /// autoload is typed as that Script instance (the precise case); this supplies the bare-`Node`
    /// floor for the scriptless-scene case so the name doesn't degrade to dynamic — which would let a
    /// lowercase-named scriptless autoload fall through to the "Identifier not declared" error (a
    /// false positive, since Godot always types a registered autoload as at least `Node`).
    ///
    /// **Default `None`** (not an autoload, or script-backed). Overridden only by `gd_server`'s
    /// `WorkspaceXFileQuery`, which owns the [`ProjectModel`](gd_project::ProjectModel) +
    /// [`SceneIndex`](gd_project::SceneIndex). The conformance corpus has no autoloads, so every
    /// in-tree query keeps the default and the ratchets are untouched by construction.
    fn autoload_native_type(&self, _name: &str) -> Option<String> {
        None
    }

    /// Whether `name` is a configured autoload singleton AT ALL — a pure name-table membership check,
    /// independent of whether its target resolves to a script/scene/native type. Used by
    /// `reduce_identifier`'s "Identifier not declared" fallthrough (step 10) to SUPPRESS that error
    /// for any registered autoload whose typing couldn't be resolved (a `uid://` that doesn't
    /// dereference, a scene missing from the index, a script not yet indexed): Godot types EVERY
    /// registered autoload as at least `Node` (`gdscript_analyzer.cpp:4570-4577`), so flagging one as
    /// undeclared is a false positive. Distinct from [`Self::autoload_file`] /
    /// [`Self::autoload_native_type`], which only return `Some` when the typing actually resolved.
    ///
    /// **Default `false`** (no project autoload table). Overridden only by `gd_server`'s
    /// `WorkspaceXFileQuery`. The conformance corpus has no autoloads → always `false` → no
    /// behavioral change to the ratchets by construction.
    fn is_autoload(&self, _name: &str) -> bool {
        false
    }

    /// Resolve a statically-known `$`/`%` node-path access made by the script `script_file` into the
    /// concrete type fact of the target node, reading the `.tscn` scene(s) `script_file` is attached
    /// to.
    ///
    /// **Navigation substrate — NOT the diagnostic path.** This is the seam the precise-HOVER /
    /// -DEFINITION / -COMPLETION features read (resolving `$Foo` to its scene-precise node class for
    /// navigation; `gd_server::scene_nav`). It is **deliberately NOT consulted by `reduce_get_node`**: a valid
    /// `$`/`%` types as bare `NATIVE Node` (faithful to Godot — see `docs/02` §11), because a
    /// scene-PRECISE `DataType` fed into the symmetric compatibility checks would turn the
    /// sibling/subtype downcasts Godot tolerates (`var c: Control = $Node2DChild`) into false
    /// positives. A precise type is safe for navigation (read-only display) but not for diagnostics.
    ///
    /// **Default `None`.** `SyntacticQuery`, `NoCrossFile`, and the analyze-phase conformance
    /// harness's `CorpusQuery` all inherit this default (none can reach a scene index). Only
    /// `gd_server`'s `WorkspaceXFileQuery`, which owns the project's
    /// [`SceneIndex`](gd_project::SceneIndex), overrides it.
    ///
    /// **Conservative contract (no false positives).** An override MUST return `None` for any
    /// uncertainty: no scene attaches `script_file`; the script attaches at MULTIPLE nodes in one
    /// scene (relative resolution ambiguous); the target node is absent; an instanced sub-scene can't
    /// be resolved; or — critically — the script attaches to MULTIPLE scenes that resolve the access
    /// to DIFFERENT facts (return `Some` only on unanimous agreement). A missed precise type is a
    /// known limitation; a wrong one is a release blocker.
    fn scene_node_facts(
        &self,
        _script_file: FileId,
        _query: &NodePathQuery,
    ) -> Option<SceneNodeFacts> {
        None
    }

    /// Cross-file references in `member`'s initializer expression on `file`.
    ///
    /// Each returned `(target_file, target_member)` pair names another file's top-level member
    /// that `member`'s initializer reads via a `preload`-constant chain — i.e. an attribute
    /// access `CONST.NAME` where `CONST` is a `const CONST = preload("…")` in the same class.
    /// Drives the cross-file mutual-cycle detection at
    /// [`crate::reducer::reduce_identifier_from_base`]'s Script-meta branch (the gdls analog of
    /// `gdscript_analyzer.cpp:984-991` + `:1019`'s `Could not resolve external class member`).
    ///
    /// The default impl returns empty, which keeps the cycle check inert on impls that can't
    /// re-parse a depended file ([`NoCrossFile`], [`SyntacticQuery`] over the shallow Index).
    /// Impls that own a re-parse cache (the conformance corpus query, a future deep-resolution
    /// impl in `gd_server`) override this to drive the cycle check. Returning empty is correct
    /// for any single-file analysis (no cross-file cycle is reachable) and matches the pre-
    /// WP-R2 behavior in those configurations.
    fn member_initializer_xrefs(&self, _file: FileId, _member: &str) -> Vec<MemberXref> {
        Vec::new()
    }
}

/// The lean, no-re-parse [`CrossFileQuery`] backed by M2's [`Index`] + native DB. Used by the
/// analyze-phase conformance harness and as `gd_server`'s starting impl.
pub struct SyntacticQuery<'a> {
    pub index: &'a Index,
    pub native: &'a NativeDb,
}

impl<'a> SyntacticQuery<'a> {
    pub fn new(index: &'a Index, native: &'a NativeDb) -> Self {
        SyntacticQuery { index, native }
    }
}

impl CrossFileQuery for SyntacticQuery<'_> {
    fn global_class_file(&self, name: &str) -> Option<FileId> {
        // A project `class_name` shadows a native of the same name, so `Script` is the global-class
        // answer; `Native`/`Unknown` mean "not a project global class".
        match self.index.resolve_name(name, self.native) {
            Resolution::Script(fid) => Some(fid),
            Resolution::Native | Resolution::Unknown => None,
        }
    }

    fn interface(&self, file: FileId) -> Option<&Interface> {
        self.index.interface(file)
    }

    fn resolve_res_path(&self, path: &str) -> Option<FileId> {
        self.index.resolve_res_path(path)
    }

    fn resolve_uid(&self, uid: &str) -> Option<String> {
        self.index.uid_target(uid).map(str::to_owned)
    }

    fn resolve_path_from(&self, from: FileId, raw: &str) -> Option<FileId> {
        if let Some(fid) = self.index.resolve_res_path(raw) {
            return Some(fid);
        }
        if raw.starts_with("res://") || raw.starts_with("user://") || raw.starts_with("uid://") {
            return None; // an absolute form that simply doesn't resolve
        }
        // Relative: join against the referring file's directory and normalize `.`/`..`
        // lexically (the index keys are normalized absolute paths).
        let base = self.index.path(from)?;
        let joined = gd_project::join_lexical(base.parent()?, raw)?;
        self.index.file_id(&joined)
    }

    fn file_path(&self, file: FileId) -> Option<&str> {
        self.index.path(file).map(|p| p.as_str())
    }
}

/// An empty cross-file environment: every query misses. The right `&dyn CrossFileQuery` for analyzing
/// a single isolated file that depends on no project class (most of the corpus), and a safe default
/// before an `Index` is built.
pub struct NoCrossFile;

impl CrossFileQuery for NoCrossFile {
    fn global_class_file(&self, _name: &str) -> Option<FileId> {
        None
    }
    fn interface(&self, _file: FileId) -> Option<&Interface> {
        None
    }
    fn resolve_res_path(&self, _path: &str) -> Option<FileId> {
        None
    }
}
