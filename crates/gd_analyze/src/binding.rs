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
/// (the parser's outline-symbol vocabulary): the analyzer's binding kinds are coarser and only
/// have variants the M4 reducer can actually emit today.
///
/// **M4 emits only `Class` and `Member`.** The other variants are reserved for follow-on
/// recording sites: `reduce_identifier_from_base`'s native-method path could emit `Function`,
/// `reduce_subscript_attribute`'s enum-value path could emit `EnumValue`, etc. (see WP-N1b's
/// "additive recording" discipline). The enum stays `#[non_exhaustive]` so a handler match on
/// it remains correct when new variants land.
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
}

/// One per-occurrence resolution record. Pushed onto [`AnalysisResult::bindings`] by the reducer
/// (WP-N1b) at every resolved call site and identifier/member use. `#[non_exhaustive]` so a
/// future `Binding::Define` variant (declaration sites) doesn't break handler matches.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum Binding {
    /// A call site that the analyzer resolved to a concrete callee.
    Call {
        /// The file declaring the callee. `None` for native methods, builtins, lambdas, dynamic
        /// dispatch through `Variant` / `Callable`, and any other callee gdls can't pin to a
        /// project script.
        callee_file: Option<FileId>,
        /// The callee identifier as written (or the resolved qualified name where the analyzer
        /// knows it). Used as the primary key for `callHierarchy/incomingCalls`.
        callee_name: String,
        /// The call expression's source span. LSP `Range` derives from this.
        call_site: ByteSpan,
        /// Bare identifier of the enclosing function (e.g. `attack`, **never** `Hero::attack`),
        /// or `None` for top-level / outside-fn calls. Drives `outgoingCalls` grouping. NOT
        /// class-qualified in v1: two same-named methods in different classes share a *caller* key
        /// here (the *callee* side is dispatch-resolved by `reducer.rs::resolve_callee_file`).
        caller_function: Option<String>,
    },
    /// An identifier or member-access that the analyzer resolved to a named declaration. Surfaced
    /// by `textDocument/references`, which in v1 matches by `target_name` across both Call and Use
    /// (see `handlers::push_binding_locations`); `target_kind` is recorded for the reserved
    /// kind-aware filter — see [`Binding::matches_use`].
    Use {
        /// The file declaring the target. `None` for native / unresolved / builtin.
        target_file: Option<FileId>,
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
        callee_file: Option<FileId>,
        callee_name: String,
        call_site: ByteSpan,
        caller_function: Option<String>,
    ) -> Self {
        debug_assert!(
            caller_function.as_deref().is_none_or(|c| !c.contains("::")),
            "Binding::call: caller_function must be a BARE identifier, never class-qualified; \
             got {caller_function:?}"
        );
        Binding::Call {
            callee_file,
            callee_name,
            call_site,
            caller_function,
        }
    }

    /// WP-RD14: smart constructor for a resolved identifier / member use — the reducer's sole
    /// producer of [`Self::Use`]. Centralizes construction alongside [`Self::call`].
    pub(crate) fn use_(
        target_file: Option<FileId>,
        target_kind: BindingTargetKind,
        target_name: String,
        site: ByteSpan,
    ) -> Self {
        Binding::Use {
            target_file,
            target_kind,
            target_name,
            site,
        }
    }

    /// True when this binding is a [`Self::Call`] whose `caller_function` matches `bare_caller`.
    /// Used by `outgoingCalls`. The argument is a **bare** function name (e.g. `attack`, never
    /// `Hero::attack`), matching how `caller_function` is recorded (see the field doc above).
    pub fn matches_caller(&self, bare_caller: &str) -> bool {
        matches!(
            self,
            Binding::Call {
                caller_function: Some(c),
                ..
            } if c == bare_caller
        )
    }

    /// True when this binding is a [`Self::Call`] whose callee matches the given (file, name).
    /// `callee_file = None` is allowed and matches when `file` is `None`. Used by
    /// `incomingCalls`.
    pub fn matches_callee(&self, file: Option<FileId>, name: &str) -> bool {
        matches!(
            self,
            Binding::Call { callee_file, callee_name, .. }
            if *callee_file == file && callee_name == name
        )
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

/// Filter `result.bindings` to call-bindings whose callee matches `(file, name)`. Used by
/// `callHierarchy/incomingCalls`.
#[must_use = "iterators are lazy and do nothing unless consumed"]
pub fn find_incoming_calls<'a>(
    result: &'a AnalysisResult,
    callee_file: Option<FileId>,
    callee_name: &'a str,
) -> impl Iterator<Item = &'a Binding> + 'a {
    result
        .bindings()
        .iter()
        .filter(move |b| b.matches_callee(callee_file, callee_name))
}

/// Filter `result.bindings` to call-bindings whose caller matches the bare caller name
/// `bare_caller` (never class-qualified — see [`Binding::Call::caller_function`]). Used by
/// `callHierarchy/outgoingCalls`.
#[must_use = "iterators are lazy and do nothing unless consumed"]
pub fn find_outgoing_calls<'a>(
    result: &'a AnalysisResult,
    bare_caller: &'a str,
) -> impl Iterator<Item = &'a Binding> + 'a {
    result
        .bindings()
        .iter()
        .filter(move |b| b.matches_caller(bare_caller))
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
            None,
            "attack".into(),
            ByteSpan { start: 0, end: 6 },
            Some("Hero::combo".into()),
        );
    }

    #[test]
    fn find_use_bindings_filters_by_kind_and_name() {
        let result = empty(vec![
            Binding::Use {
                target_file: Some(FileId::new(1)),
                target_kind: BindingTargetKind::Function,
                target_name: "attack".into(),
                site: ByteSpan { start: 0, end: 6 },
            },
            Binding::Use {
                target_file: Some(FileId::new(2)),
                target_kind: BindingTargetKind::Variable,
                target_name: "attack".into(),
                site: ByteSpan { start: 10, end: 16 },
            },
            Binding::Use {
                target_file: Some(FileId::new(1)),
                target_kind: BindingTargetKind::Function,
                target_name: "flee".into(),
                site: ByteSpan { start: 20, end: 24 },
            },
        ]);
        let hits: Vec<&Binding> =
            find_use_bindings(&result, BindingTargetKind::Function, "attack").collect();
        assert_eq!(hits.len(), 1);
    }

    #[test]
    fn find_outgoing_calls_groups_by_caller() {
        let result = empty(vec![
            Binding::Call {
                callee_file: Some(FileId::new(2)),
                callee_name: "flee".into(),
                call_site: ByteSpan { start: 0, end: 6 },
                caller_function: Some("attack".into()),
            },
            Binding::Call {
                callee_file: None,
                callee_name: "print".into(),
                call_site: ByteSpan { start: 10, end: 15 },
                caller_function: Some("attack".into()),
            },
            Binding::Call {
                callee_file: Some(FileId::new(2)),
                callee_name: "flee".into(),
                call_site: ByteSpan { start: 20, end: 26 },
                caller_function: Some("other".into()),
            },
        ]);
        let attack_calls: Vec<&Binding> = find_outgoing_calls(&result, "attack").collect();
        assert_eq!(attack_calls.len(), 2);
    }

    #[test]
    fn find_incoming_calls_filters_by_callee() {
        let result = empty(vec![
            Binding::Call {
                callee_file: Some(FileId::new(2)),
                callee_name: "flee".into(),
                call_site: ByteSpan { start: 0, end: 6 },
                caller_function: Some("attack".into()),
            },
            Binding::Call {
                callee_file: None,
                callee_name: "print".into(),
                call_site: ByteSpan { start: 10, end: 15 },
                caller_function: Some("attack".into()),
            },
        ]);
        let into_flee: Vec<&Binding> =
            find_incoming_calls(&result, Some(FileId::new(2)), "flee").collect();
        assert_eq!(into_flee.len(), 1);
        let into_print: Vec<&Binding> = find_incoming_calls(&result, None, "print").collect();
        assert_eq!(into_print.len(), 1);
    }
}
