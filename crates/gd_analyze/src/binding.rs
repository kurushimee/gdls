//! Per-occurrence resolution records.
//!
//! gdls's analyzer records what each resolved call site / identifier resolved to, so M4's nav
//! handlers (`references`, `implementation`, `prepareCallHierarchy`, `workspace/symbol`) can
//! answer their LSP requests by projecting these records — instead of recomputing resolution at
//! the LSP boundary the way Godot's editor LSP does.
//!
//! There's no Godot analog: the C++ `GDScriptAnalyzer` is invoked fresh on every editor
//! interaction. gdls separates "resolve types" (the analyzer, M3) from "answer LSP queries" (the
//! server, M4) — these records are the seam. The recording is *additive*: per the WP-N1b
//! discipline, pushing a [`Binding`] never changes another diagnostic or type, so the conformance
//! ratchet is preserved across the recording rollout.

use std::borrow::Borrow;

use gd_project::FileId;
use gd_syntax::ByteSpan;

use crate::context::AnalysisResult;

/// WP-RD15: a class member's name. Newtype over `String` so the cross-file member-xref map keys on
/// a self-documenting type rather than a bare `String`. Borrows as `&str` so a `&str` lookup still
/// hits a `FxHashMap<MemberName, _>` without allocating a key.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct MemberName(pub String);

impl MemberName {
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Borrow<str> for MemberName {
    fn borrow(&self) -> &str {
        &self.0
    }
}

impl From<&str> for MemberName {
    fn from(s: &str) -> Self {
        MemberName(s.to_string())
    }
}

impl From<String> for MemberName {
    fn from(s: String) -> Self {
        MemberName(s)
    }
}

/// WP-RD15: one cross-file member-initializer reference — the `target_member` on `target_file` that
/// some FROM member reads through a `preload`-const attribute chain. Replaces the former
/// stringly-typed `(FileId, String)` pair so the two positions can't be transposed and each reads
/// at the use site. Consumed by the WP-R2 cross-file mutual-member cycle check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemberXref {
    pub target_file: FileId,
    pub target_member: MemberName,
}

/// What kind of declaration a [`Binding::Use`] targets. Distinct from [`gd_syntax::SymbolKind`]
/// (the parser's outline-symbol vocabulary): the analyzer's binding kinds are coarser.
///
/// Emitted today: `Class` and `Member` from `reduce_identifier` (in-file members, cross-file
/// `class_name`s, autoloads) and from `reduce_identifier_from_base`'s in-file CLASS branch;
/// the precise kinds (`Variable` / `Constant` / `Function` / `Signal` / `Enum` / `EnumValue`)
/// from `record_member_use` for every cross-file script-chain member hit; `EnumValueLocal`
/// (qualified name) from `reduce_identifier_from_base`'s in-file ENUM-meta value arm. `Parameter`
/// stays reserved (locals/params are function-scoped and never cross-file). The enum stays
/// `#[non_exhaustive]` so a handler match on it remains correct when new variants land.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum BindingTargetKind {
    Class,
    Function,
    Variable,
    Constant,
    Signal,
    Enum,
    EnumValue,
    Parameter,
    Member,
    /// A value of a NAMED enum declared in THIS file (`enum Direction { NORTH }`, use site
    /// `Direction.NORTH`). Distinct from [`Self::EnumValue`] (which is the cross-file / anonymous-enum
    /// hoist kind, deliberately excluded from the kind-precise reference filter because its uses also
    /// live in annotations/match-patterns the reducer doesn't record).
    ///
    /// **Composite-identity convention (deliberate):** a [`Binding::Use`] of this kind carries the
    /// QUALIFIED `target_name` `"<EnumName>.<value>"` (e.g. `"Direction.NORTH"`), NOT the bare value
    /// name. Both halves are dot-free GDScript identifiers, so the dotted join is unambiguous. This
    /// keys two same-named values in different enums of one file (`enum A { X }` + `enum B { X }`,
    /// both legal) apart, AND makes the binding structurally INVISIBLE to every bare-name matcher
    /// (`push_binding_locations`, `push_use_binding_locations_for`, [`Binding::matches_use`]) — so a
    /// method/signal/member that happens to be named `NORTH` can never collect an enum value's use
    /// under a mutating rename. The qualified name is consumed only by the enum-qualified collector;
    /// no consumer DISPLAYS it.
    EnumValueLocal,
}

/// What a resolved call site dispatches to — the callee classification the reducer derives from
/// the resolution the call actually used. A CLOSED concept (project script / engine class /
/// don't-know): `Unresolved` is the catch-all, so no `#[non_exhaustive]`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum CalleeTarget {
    /// A project script declares the callee. `class_path` is the inner-class chain WITHIN the
    /// file ([`crate::data_type::ScriptRef::inner`]'s vocabulary; empty = the file's root
    /// class) — the owning class, so same-named methods in one file stop sharing call-site
    /// sets.
    Script {
        file: FileId,
        class_path: Vec<String>,
    },
    /// The callee bound to a native engine method. `class` is the class the signature lookup
    /// ran against (the chain's native root, or the subscript base's resolved native type);
    /// consumers resolve the DECLARING class via `NativeDb::lookup_member`, which walks
    /// `inherits`.
    Native { class: String },
    /// The callee couldn't be pinned: value-callables, dynamic dispatch through
    /// `Variant`/`Callable`, lambdas, builtin-value methods, trimmed-DB misses.
    Unresolved,
}

impl CalleeTarget {
    /// The declaring project file for a [`Self::Script`] callee, `None` otherwise — the
    /// file-level view most nav handlers filter on.
    #[must_use]
    pub fn script_file(&self) -> Option<FileId> {
        match self {
            CalleeTarget::Script { file, .. } => Some(*file),
            _ => None,
        }
    }
}

/// One per-occurrence resolution record. Pushed onto [`AnalysisResult::bindings`] by the reducer
/// (WP-N1b) at every resolved call site and identifier/member use. `#[non_exhaustive]` so a
/// future `Binding::Define` variant (declaration sites) doesn't break handler matches.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum Binding {
    /// A call site that the analyzer resolved to a concrete callee.
    Call {
        /// What the call dispatches to — see [`CalleeTarget`].
        callee: CalleeTarget,
        /// The callee identifier as written (or the resolved qualified name where the analyzer
        /// knows it). Used as the primary key for `callHierarchy/incomingCalls`.
        callee_name: String,
        /// The call expression's source span. LSP `Range` derives from this.
        call_site: ByteSpan,
        /// Bare identifier of the enclosing function (e.g. `attack`, **never** `Hero::attack`),
        /// or `None` for top-level / outside-fn calls. Drives `outgoingCalls` grouping. Bare by
        /// construction — the owning class rides alongside in [`Self::Call::caller_class_path`],
        /// which is what actually disambiguates two same-named methods.
        caller_function: Option<String>,
        /// Inner-class chain WITHIN this file where the CALLER is declared (empty = the file's
        /// root class) — [`crate::data_type::ScriptRef::inner`]'s vocabulary, the
        /// [`CalleeTarget::Script::class_path`] pattern carried onto the caller side (#360).
        ///
        /// Without it a root `func tick()` and an inner class's `func tick()` are one
        /// `outgoingCalls` key, and each answers with the union of both methods' calls — a
        /// well-formed, plausible, wrong tree.
        caller_class_path: Vec<String>,
    },
    /// An identifier or member-access that the analyzer resolved to a named declaration. Surfaced
    /// by `textDocument/references`, which in v1 matches by `target_name` across both Call and Use
    /// (see `handlers::push_binding_locations`); `target_kind` is recorded for the reserved
    /// kind-aware filter — see [`Binding::matches_use`].
    Use {
        /// The file declaring the target. `None` for native / unresolved / builtin.
        target_file: Option<FileId>,
        /// Inner-class chain WITHIN `target_file` where the target is declared
        /// ([`crate::data_type::ScriptRef::inner`]'s vocabulary; empty = the file's root class).
        /// Keeps same-named members of different inner classes in one file distinct under
        /// `references`/`rename` — the [`CalleeTarget::Script::class_path`] pattern, carried onto
        /// the non-call member-use surface (#153).
        target_class_path: Vec<String>,
        target_kind: BindingTargetKind,
        target_name: String,
        site: ByteSpan,
    },
}

impl Binding {
    /// WP-RD14: smart constructor for a resolved call site. The reducer's sole producer of
    /// [`Self::Call`] — centralizing construction here is where the field invariant lives and (in
    /// debug) is checked: `caller_function` is the enclosing function's **bare** identifier
    /// (`attack`, never the class-qualified `Hero::attack`), since `outgoingCalls` groups on it.
    pub(crate) fn call(
        callee: CalleeTarget,
        callee_name: String,
        call_site: ByteSpan,
        caller_function: Option<String>,
        caller_class_path: Vec<String>,
    ) -> Self {
        debug_assert!(
            caller_function.as_deref().is_none_or(|c| !c.contains("::")),
            "Binding::call: caller_function must be a BARE identifier, never class-qualified; \
             got {caller_function:?}"
        );
        Binding::Call {
            callee,
            callee_name,
            call_site,
            caller_function,
            caller_class_path,
        }
    }

    /// WP-RD14: smart constructor for a resolved identifier / member use — the reducer's sole
    /// producer of [`Self::Use`]. Centralizes construction alongside [`Self::call`].
    pub(crate) fn use_(
        target_file: Option<FileId>,
        target_class_path: Vec<String>,
        target_kind: BindingTargetKind,
        target_name: String,
        site: ByteSpan,
    ) -> Self {
        Binding::Use {
            target_file,
            target_class_path,
            target_kind,
            target_name,
            site,
        }
    }

    /// True when this binding is a [`Self::Call`] declared in `class_path` whose
    /// `caller_function` matches `bare_caller`. Used by `outgoingCalls`. The name argument is a
    /// **bare** function name (e.g. `attack`, never `Hero::attack`), matching how
    /// `caller_function` is recorded; the owning class is the separate `class_path` argument
    /// (empty = the file's root class).
    pub fn matches_caller(&self, class_path: &[String], bare_caller: &str) -> bool {
        matches!(
            self,
            Binding::Call {
                caller_function: Some(c),
                caller_class_path,
                ..
            } if c == bare_caller && caller_class_path.as_slice() == class_path
        )
    }

    /// The Script-declaring file of a [`Self::Call`]'s callee — `None` for Native/Unresolved
    /// callees and for non-Call bindings. The file-level view nav handlers filter on.
    #[must_use]
    pub fn callee_script_file(&self) -> Option<FileId> {
        match self {
            Binding::Call { callee, .. } => callee.script_file(),
            _ => None,
        }
    }

    /// True when this binding is a [`Self::Call`] whose callee matches the given
    /// (file, class path, name). `file = None` matches every NON-project callee (`Native` and
    /// `Unresolved` alike) — preserving the pre-`CalleeTarget` `callee_file: None` matching that
    /// `incomingCalls`' degrade paths rely on, and the class path is not compared there since a
    /// non-project callee has none.
    ///
    /// #360: for a project callee the class path IS compared, so an inner class's `tick` and the
    /// root class's `tick` no longer share one set of call sites.
    pub fn matches_callee(&self, file: Option<FileId>, class_path: &[String], name: &str) -> bool {
        let Binding::Call {
            callee,
            callee_name,
            ..
        } = self
        else {
            return false;
        };
        if callee.script_file() != file || callee_name != name {
            return false;
        }
        match callee {
            CalleeTarget::Script { class_path: cp, .. } => cp.as_slice() == class_path,
            _ => true,
        }
    }

    /// True when this binding is a [`Self::Use`] targeting `(kind, name)`.
    ///
    /// **Reserved, not yet wired.** The v1 `references` handler matches by bare name across both
    /// [`Self::Call`] and [`Self::Use`] (`handlers::push_binding_locations`) — intentionally loose,
    /// so it never consults `target_kind`. This kind-aware predicate exists for the M5 kind-precise
    /// `references` pass; `target_kind` is recorded now so that pass is a server-side change only
    /// (the "additive recording" discipline).
    pub fn matches_use(&self, kind: BindingTargetKind, name: &str) -> bool {
        matches!(
            self,
            Binding::Use { target_kind, target_name, .. }
            if *target_kind == kind && target_name == name
        )
    }
}

/// Filter `result.bindings` to entries whose [`Binding::Use`] target matches `(kind, name)`.
///
/// **Reserved, not yet wired** (see [`Binding::matches_use`]): the v1 `references` handler matches
/// by bare name across Call + Use rather than by kind. Kept — with the `target_kind` the reducer
/// records — so the M5 kind-precise `references` pass is a server-side change only.
#[must_use = "iterators are lazy and do nothing unless consumed"]
pub fn find_use_bindings<'a>(
    result: &'a AnalysisResult,
    kind: BindingTargetKind,
    name: &'a str,
) -> impl Iterator<Item = &'a Binding> + 'a {
    result
        .bindings()
        .iter()
        .filter(move |b| b.matches_use(kind, name))
}

/// Filter `result.bindings` to call-bindings whose callee matches `(file, class path, name)`.
/// Used by `callHierarchy/incomingCalls`.
#[must_use = "iterators are lazy and do nothing unless consumed"]
pub fn find_incoming_calls<'a>(
    result: &'a AnalysisResult,
    callee_file: Option<FileId>,
    callee_class_path: &'a [String],
    callee_name: &'a str,
) -> impl Iterator<Item = &'a Binding> + 'a {
    result
        .bindings()
        .iter()
        .filter(move |b| b.matches_callee(callee_file, callee_class_path, callee_name))
}

/// Filter `result.bindings` to call-bindings declared in `caller_class_path` whose caller matches
/// the bare caller name `bare_caller` (never class-qualified — see
/// [`Binding::Call::caller_function`]). Used by `callHierarchy/outgoingCalls`.
#[must_use = "iterators are lazy and do nothing unless consumed"]
pub fn find_outgoing_calls<'a>(
    result: &'a AnalysisResult,
    caller_class_path: &'a [String],
    bare_caller: &'a str,
) -> impl Iterator<Item = &'a Binding> + 'a {
    result
        .bindings()
        .iter()
        .filter(move |b| b.matches_caller(caller_class_path, bare_caller))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{FoldTable, TypeTable};

    fn empty(bindings: Vec<Binding>) -> AnalysisResult {
        AnalysisResult::new_for_test(
            TypeTable::new(0),
            FoldTable::new(0),
            Vec::new(),
            rustc_hash::FxHashMap::default(),
            bindings,
        )
    }

    /// WP-RD14: the `Binding::call` smart constructor enforces that `caller_function` is a BARE
    /// identifier (never class-qualified) via a `debug_assert!`, since `outgoingCalls` groups on it.
    #[cfg(debug_assertions)]
    #[test]
    #[should_panic(expected = "must be a BARE identifier")]
    fn binding_call_rejects_qualified_caller() {
        let _ = Binding::call(
            CalleeTarget::Unresolved,
            "attack".into(),
            ByteSpan { start: 0, end: 6 },
            Some("Hero::combo".into()),
            Vec::new(),
        );
    }

    #[test]
    fn find_use_bindings_filters_by_kind_and_name() {
        let result = empty(vec![
            Binding::Use {
                target_file: Some(FileId::new(1)),
                target_class_path: Vec::new(),
                target_kind: BindingTargetKind::Function,
                target_name: "attack".into(),
                site: ByteSpan { start: 0, end: 6 },
            },
            Binding::Use {
                target_file: Some(FileId::new(2)),
                target_class_path: Vec::new(),
                target_kind: BindingTargetKind::Variable,
                target_name: "attack".into(),
                site: ByteSpan { start: 10, end: 16 },
            },
            Binding::Use {
                target_file: Some(FileId::new(1)),
                target_class_path: Vec::new(),
                target_kind: BindingTargetKind::Function,
                target_name: "flee".into(),
                site: ByteSpan { start: 20, end: 24 },
            },
        ]);
        let hits: Vec<&Binding> =
            find_use_bindings(&result, BindingTargetKind::Function, "attack").collect();
        assert_eq!(hits.len(), 1);
    }

    fn script(file: u32) -> CalleeTarget {
        CalleeTarget::Script {
            file: FileId::new(file),
            class_path: Vec::new(),
        }
    }

    #[test]
    fn find_outgoing_calls_groups_by_caller() {
        let result = empty(vec![
            Binding::Call {
                callee: script(2),
                callee_name: "flee".into(),
                call_site: ByteSpan { start: 0, end: 6 },
                caller_function: Some("attack".into()),
                caller_class_path: Vec::new(),
            },
            Binding::Call {
                callee: CalleeTarget::Unresolved,
                callee_name: "print".into(),
                call_site: ByteSpan { start: 10, end: 15 },
                caller_function: Some("attack".into()),
                caller_class_path: Vec::new(),
            },
            Binding::Call {
                callee: script(2),
                callee_name: "flee".into(),
                call_site: ByteSpan { start: 20, end: 26 },
                caller_function: Some("other".into()),
                caller_class_path: Vec::new(),
            },
        ]);
        let attack_calls: Vec<&Binding> = find_outgoing_calls(&result, &[], "attack").collect();
        assert_eq!(attack_calls.len(), 2);
    }

    #[test]
    fn find_incoming_calls_filters_by_callee() {
        let result = empty(vec![
            Binding::Call {
                callee: script(2),
                callee_name: "flee".into(),
                call_site: ByteSpan { start: 0, end: 6 },
                caller_function: Some("attack".into()),
                caller_class_path: Vec::new(),
            },
            Binding::Call {
                callee: CalleeTarget::Unresolved,
                callee_name: "print".into(),
                call_site: ByteSpan { start: 10, end: 15 },
                caller_function: Some("attack".into()),
                caller_class_path: Vec::new(),
            },
            Binding::Call {
                callee: CalleeTarget::Native {
                    class: "Node".into(),
                },
                callee_name: "queue_free".into(),
                call_site: ByteSpan { start: 20, end: 30 },
                caller_function: Some("attack".into()),
                caller_class_path: Vec::new(),
            },
        ]);
        let into_flee: Vec<&Binding> =
            find_incoming_calls(&result, Some(FileId::new(2)), &[], "flee").collect();
        assert_eq!(into_flee.len(), 1);
        // `None` matches every NON-project callee — Unresolved and Native alike (the
        // pre-CalleeTarget degrade semantics incomingCalls relies on).
        let into_print: Vec<&Binding> = find_incoming_calls(&result, None, &[], "print").collect();
        assert_eq!(into_print.len(), 1);
        let into_queue_free: Vec<&Binding> =
            find_incoming_calls(&result, None, &[], "queue_free").collect();
        assert_eq!(into_queue_free.len(), 1);
    }
}
