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
    fn autoload_file(&self, _name: &str) -> Option<FileId> {
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
