//! The `AnalysisContext` — gdls's analog of Godot's `GDScriptAnalyzer` object.
//!
//! Godot's analyzer is a class whose members thread the parser, the current-class/lambda cursors, and
//! the static-context flag through ~80 methods, mutating `DataType`s straight onto AST nodes. gdls's
//! AST is engine-free, so the resolved types live in side tables ([`TypeTable`]/[`FoldTable`]) keyed
//! by `NodeId`, and the cursor state + diagnostic sink live here. The resolve/reduce passes
//! ([`crate::resolver`], later `reducer`/`folder`) take `&mut AnalysisContext` exactly as Godot's
//! methods take `this`.
//!
//! Two Godot fields map onto distinct stores, mirroring Godot's two `DataType` slots per class:
//! a class node's **own** (meta) type goes in [`TypeTable`]; its **base** type goes in [`Self::bases`]
//! (Godot's `ClassNode::base_type`, which also carries the `RESOLVING` cycle sentinel).

use gd_project::FileId;
use gd_syntax::ast::{Node, NodeId};
use gd_syntax::{ByteSpan, Dialect, ParseTree};
use gd_types::NativeDb;
use rustc_hash::{FxHashMap, FxHashSet};

use crate::binding::{Binding, MemberName, MemberXref};
use crate::cancellation::CancellationToken;
use crate::cross_file::CrossFileQuery;
use crate::data_type::{DataType, DtKind, ScriptRef};
use crate::diagnostic::{Diagnostic, DiagnosticSink};
use crate::foldtable::FoldTable;
use crate::typetable::TypeTable;
use crate::warn_policy::WarnPolicy;

/// What [`crate::analyze`] yields for one file: the resolved per-node types, folded constants, the
/// emitted diagnostics, and (M4) the per-member cross-file initializer xrefs that drive the live
/// cycle-detection in the LSP path. Independent of who holds the parse `Rc`, so the server can
/// cache it.
#[derive(Debug)]
pub struct AnalysisResult {
    pub types: TypeTable,
    pub folds: FoldTable,
    pub diagnostics: Vec<Diagnostic>,
    /// For each of *this file's* class-member names, the `(target_file, target_member)`
    /// pairs its initializer reads via a Script-meta attribute access. Consumed by
    /// [`crate::cross_file::CrossFileQuery::member_initializer_xrefs`] overrides (e.g.
    /// `WorkspaceXFileQuery`) so cross-file mutual-member cycle detection (the Script-meta
    /// branch of `reducer.rs::reduce_identifier_from_base`, marked
    /// `// WP-R2: cross-file mutual member cycle detection`) fires in the LSP path, not just
    /// the conformance harness.
    ///
    /// Empty in single-file analysis (`NoCrossFile`) or files with no cross-file member
    /// initializers. Recording is additive — never changes any other field.
    ///
    /// **WP-RD1: private.** Read via [`Self::member_xrefs`]; the only write path is
    /// [`AnalysisContext::record_member_xref`] (staged on the context, moved here by
    /// [`AnalysisContext::finish`]). A frozen output — never mutated post-construction.
    member_xrefs: FxHashMap<MemberName, Vec<MemberXref>>,

    /// Per-occurrence resolution records — every resolved call site ([`Binding::Call`]) and
    /// identifier / member-access ([`Binding::Use`]).
    ///
    /// Recording sites in `reducer.rs`:
    /// - [`Binding::Call`] at `reduce_call`'s single post-resolution gate (bare, dotted, and
    ///   super shapes alike), classifying the callee as a [`crate::binding::CalleeTarget`]
    ///   derived from the resolution the dispatch actually used. The Object /
    ///   builtin-constructor / utility-function early returns bail first, so `Vector2()`,
    ///   `print()`, etc. are never recorded.
    /// - [`Binding::Use`] (`BindingTargetKind::Member` / `Class`) in `reduce_identifier` for
    ///   in-file class-member and cross-file `class_name` resolution (autoload sentinels
    ///   included).
    /// - [`Binding::Use`] with PRECISE kinds (`Variable` / `Constant` / `Function` / `Signal` /
    ///   `Enum` / `EnumValue`) via `record_member_use` for every `lookup_script_chain_member`
    ///   hit — cross-file attribute reads (`obj.hp` through a script-typed base) and bare
    ///   inherited members alike.
    /// - [`Binding::Use`] (`BindingTargetKind::Member`) at `reduce_identifier_from_base`'s
    ///   in-file CLASS-branch hit (`self.hp`, attribute reads on a base typed as this file's
    ///   own class) — the in-file twin of `record_member_use`.
    ///
    /// Deliberately NOT recorded: attribute reads on native / builtin / enum bases (the
    /// over-resolution they'd need conflicts with the analyzer's "degrade rather than fail"
    /// rule — recording a guessed target would let nav lie).
    ///
    /// Consumed by the LSP nav handlers in `gd_server`: `textDocument/references` projects `Use`
    /// bindings (`handlers::push_binding_locations` and the file-filtered member path);
    /// `callHierarchy/{incoming,outgoing}Calls` filter `Call` by callee / caller. Recording is
    /// additive — never changes any other field.
    ///
    /// **WP-RD1: private.** Read via [`Self::bindings`]; the only write path is
    /// [`AnalysisContext::record_binding`] (staged on the context, moved here by
    /// [`AnalysisContext::finish`]). A frozen output — never mutated post-construction.
    bindings: Vec<Binding>,

    /// Lambda expressions that use `self` implicitly or explicitly while being reduced.
    ///
    /// Godot stores this as `GDScriptParser::LambdaNode::use_self`, mutated by
    /// `GDScriptAnalyzer::mark_lambda_use_self()` (analyzer.cpp:6364). gdls keeps the parse tree
    /// immutable during analysis, so the analyzer records the same fact in this side table.
    lambda_uses_self: FxHashSet<NodeId>,

    /// `true` when analysis was cut short by the WP-O3 fixpoint governor or a WP-O4 cancellation
    /// token (see [`AnalysisContext::checkpoint`]), so the side tables (`types`, `bindings`,
    /// `member_xrefs`) are **partial**, not authoritative. Callers that cache results keyed by
    /// content must NOT persist a bailed result — re-serving it as if complete would silently under-
    /// report hover types / references / call edges on an unchanged file (the "never lie" rule). The
    /// `diagnostics` still carry the synthetic `analyzer: …` breadcrumb plus everything that fired
    /// before the bail.
    pub bailed: bool,
}

impl AnalysisResult {
    /// The per-occurrence resolution records (every resolved call site / identifier use). The
    /// read side of the WP-RD1 chokepoint — the field is private so the only producer is
    /// [`AnalysisContext::record_binding`] → [`AnalysisContext::finish`].
    #[must_use]
    pub fn bindings(&self) -> &[Binding] {
        &self.bindings
    }

    /// Whether a lambda expression was marked as using `self` during analysis.
    ///
    /// Mirrors Godot's `LambdaNode::use_self` flag without mutating the syntax tree.
    #[must_use]
    pub fn lambda_uses_self(&self, lambda: NodeId) -> bool {
        self.lambda_uses_self.contains(&lambda)
    }

    /// This file's per-member cross-file initializer xrefs (see the field docs). Read side of the
    /// WP-RD1 chokepoint — produced only via [`AnalysisContext::record_member_xref`].
    #[must_use]
    pub fn member_xrefs(&self) -> &FxHashMap<MemberName, Vec<MemberXref>> {
        &self.member_xrefs
    }

    /// Construct a result from explicit parts — **test-only.** Gated behind `cfg(test)` (for
    /// `gd_analyze`'s own unit tests) and the `test-support` feature (so dependent crates'
    /// integration tests — e.g. `gd_server::xfile` — can build literals across the crate
    /// boundary, where a bare `#[cfg(test)]` would not be visible). Production code obtains an
    /// `AnalysisResult` exclusively from [`AnalysisContext::finish`].
    #[cfg(any(test, feature = "test-support"))]
    #[must_use]
    pub fn new_for_test(
        types: TypeTable,
        folds: FoldTable,
        diagnostics: Vec<Diagnostic>,
        member_xrefs: FxHashMap<MemberName, Vec<MemberXref>>,
        bindings: Vec<Binding>,
    ) -> Self {
        AnalysisResult {
            types,
            folds,
            diagnostics,
            member_xrefs,
            bindings,
            lambda_uses_self: FxHashSet::default(),
            bailed: false,
        }
    }
}

/// A lambda whose body is queued for resolution after the enclosing body statement.
#[derive(Clone, Debug)]
pub(crate) struct PendingLambdaBody {
    pub lambda_id: NodeId,
    pub captured_concrete: Option<NodeId>,
    pub captured_static: bool,
    pub captured_suite_stack: Vec<NodeId>,
    pub captured_lambda_stack: Vec<NodeId>,
}

/// The mutable analysis state for one file. Borrows the inputs (tree, native DB, cross-file query,
/// warning policy) for the duration of [`crate::analyze`]; owns the produced side tables and sink.
pub struct AnalysisContext<'a> {
    /// The file under analysis.
    pub tree: &'a ParseTree,
    pub native: &'a NativeDb,
    pub xfile: &'a dyn CrossFileQuery,
    /// This file's id (Godot's `parser->script_path` identity), for self-referential `ScriptRef`s.
    ///
    /// **WP-RD2: `Option`.** `None` for a file the index doesn't know (an `untitled:` buffer or a
    /// `.gd` outside the project). Binding-recording sites read it directly so orphan bindings
    /// record `None` rather than a colliding placeholder id; the only place a concrete id is still
    /// needed — the head class's own [`ScriptRef`] in [`Self::finish`] — falls back to
    /// [`FileId::ORPHAN`], which never escapes to another file's analysis.
    pub file: Option<FileId>,
    /// This file's path (Godot's `parser->script_path`, e.g. `res://path/file.gd`), used as the
    /// head class's `fqcn` when no `class_name` is declared (analyzer.cpp:702). Drives enum-type
    /// rendering (`<file.gd>.<EnumName>`) and the fqcn comparison in `is_type_compatible`. Empty
    /// for isolated unit-test analysis — the head's fqcn then falls back to empty exactly as the
    /// pre-WP-J behaviour, preserving every existing test's diagnostic strings.
    pub script_path: String,
    pub policy: &'a WarnPolicy,

    /// Resolved type per node (Godot's `Node::datatype`); for a class node, its **meta** type.
    pub types: TypeTable,
    /// Folded constant per node (Godot's `ExpressionNode::reduced_value`); filled in WP-E/F.
    pub folds: FoldTable,
    /// A class node's **base** type (Godot's `ClassNode::base_type`), holding the `RESOLVING`
    /// sentinel mid-resolution. Separate from [`Self::types`], which holds the class's own meta type.
    pub bases: FxHashMap<NodeId, DataType>,

    /// The class currently being resolved (Godot's `parser->current_class`).
    pub current_class: Option<NodeId>,
    /// The function currently being resolved (Godot's `parser->current_function`); set in WP-E.
    pub current_function: Option<NodeId>,
    /// Name of the class member whose initializer is currently being reduced. Godot tracks
    /// the same information in-band via `DataType::RESOLVING` on the member's node
    /// (analyzer.cpp:984-991); gdls also stamps `DtKind::Resolving` on the member's type, but
    /// we additionally carry the **name** here so cross-file consumers can check the cycle by
    /// `(ctx.file, current_resolving_member)` without having to walk back from a `NodeId` they
    /// don't own. Set by [`crate::resolver::resolve_class_member`]'s Variable/Constant arms
    /// before the assignable's initializer is reduced; restored on exit. Read by
    /// [`crate::reducer::reduce_identifier_from_base`]'s Script-meta branch.
    pub current_resolving_member: Option<String>,
    /// Whether resolution is in a `static` context (Godot's `static_context`); drives static-access
    /// checks in WP-E.
    pub static_context: bool,
    /// Whether the current `reduce_identifier`/`reduce_subscript` call is reducing the **callee**
    /// of an enclosing Call node. Lets the identifier path skip the
    /// `Cannot access non-static …` check (analyzer.cpp:4464-4490) when the call-version of the
    /// same check (analyzer.cpp:3642-3656, the `Cannot call non-static …` arm) will fire instead.
    /// Mirrors Godot's choice to invoke `reduce_identifier_from_base` (no-access-check) from
    /// `reduce_call` rather than the standalone `reduce_identifier` (with-access-check).
    pub reducing_callee: bool,
    /// The nearest **concrete** (non-lambda) function in the current resolution path. When a
    /// lambda body is being resolved, `current_function` points at the lambda's synthesized
    /// FunctionNode (so identifier / parameter lookup works) while `concrete_function` continues
    /// to point at the outer regular function. The static-context error templates at
    /// analyzer.cpp:3651-3654 / :4470 read this so a `static_func ... var f = func ():
    /// non_static_func()` reports `from the static function "static_func()"` rather than the
    /// lambda's empty name. Godot reaches the same value by walking `source_lambda` →
    /// `parent_function` (analyzer.cpp:3645-3649); gdls's AST doesn't carry those back-pointers
    /// so we track it explicitly.
    pub concrete_function: Option<NodeId>,
    /// Lambda nodes whose bodies are queued for resolution after the enclosing class body
    /// finishes. Each entry snapshots the enclosing concrete function, static context, lexical
    /// suite stack, and parent lambda stack:
    /// `concrete_function` lets the body's static-context errors name the outer concrete (e.g.
    /// `static_func()`) instead of the lambda's empty name, and `static_context` lets the body
    /// inherit static-ness from the surrounding scope — Godot's
    /// `resolve_function_signature` at analyzer.cpp:1749-1751 mutates the lambda's `is_static`
    /// to `= static_context`, but gdls's parse tree is immutable from the analyzer's side, so
    /// we carry the bit through the queue instead. Mirrors Godot's
    /// `pending_body_resolution_lambdas` queue (analyzer.cpp:4684, drained at :6528).
    pub(crate) pending_lambda_bodies: Vec<PendingLambdaBody>,
    /// Lambda cursor stack, outermost-to-innermost. Godot carries `current_lambda` plus
    /// `LambdaNode::parent_lambda`; gdls derives the same chain by pushing around lambda
    /// signature/body reduction.
    pub current_lambda_stack: Vec<NodeId>,
    /// Whether the current `reduce_call` is being called as the target of an enclosing `await`.
    /// Mirrors Godot's `p_is_await` parameter (analyzer.cpp:3231 / 3751-3758): when true, a
    /// coroutine call result does NOT fire MISSING_AWAIT; when false at statement root it fires
    /// the warning, and when false off-root it fires `Function "X()" is a coroutine, so it must
    /// be called with "await".` (Godot escalates the warning to an error in non-root
    /// expression position).
    pub awaiting_call: bool,
    /// Class nodes whose interface has been resolved (Godot's `ClassNode::resolved_interface`),
    /// making `resolve_class_interface` idempotent.
    pub resolved_interfaces: FxHashSet<NodeId>,
    /// Expression nodes already visited by `reduce_expression` (Godot's `ExpressionNode::reduced`
    /// flag, analyzer.cpp:2600-2605): prevents repeated work and cycles.
    pub reduced: FxHashSet<NodeId>,
    /// Class nodes whose body has been resolved (Godot's `ClassNode::resolved_body`,
    /// analyzer.cpp:1365): makes `resolve_class_body` idempotent across recursion.
    pub resolved_bodies: FxHashSet<NodeId>,
    /// Function nodes whose body has been resolved (Godot's `FunctionNode::resolved_body`,
    /// analyzer.cpp:1989): the per-function analog of `resolved_bodies`.
    pub resolved_functions: FxHashSet<NodeId>,
    /// Nodes marked `@abstract` by annotation processing. The parse tree is immutable from the
    /// analyzer's side (`&ParseTree`), so instead of stamping `ClassNode::is_abstract` /
    /// `FunctionNode::is_abstract` inline, the annotation-apply step inserts the NodeId here and
    /// the rest of the analyzer reads it via `ctx.is_abstract(id)`.
    pub abstract_nodes: FxHashSet<NodeId>,
    /// The active suite scope chain — pushed on entry to `resolve_suite`, popped on exit. The
    /// Godot's `IdentifierNode::suite` back-pointer (gdscript_parser.cpp:3097) gives the parser
    /// scope-tracked lookups; gdls's AST keeps `SuiteNode::locals` but not the back-pointer, so
    /// we walk this stack inside `reduce_identifier` instead.
    pub suite_stack: Vec<NodeId>,
    /// Godot's per-variable `assignments` counter, keyed by the declaring node. Upstream keeps
    /// it on `VariableNode` (debug-only): the declaration initializer contributes one
    /// (gdscript_parser.cpp:1261 — folded in lazily by `crate::reducer::assignment_count`) and
    /// `reduce_assignment` increments for local-variable assignees before reducing the assignee
    /// (gdscript_analyzer.cpp:2852-2860). Read by the UNASSIGNED_VARIABLE /
    /// UNASSIGNED_VARIABLE_OP_ASSIGN checks; traversal-order evolution is the point — a read
    /// before the first assignment warns even if a later statement assigns.
    pub assignments: FxHashMap<NodeId, u32>,

    /// Per-member cross-file initializer xrefs, recorded by [`Self::record_member_xref`] from
    /// the reducer's Script-meta attribute-access path. Moved into
    /// [`AnalysisResult::member_xrefs`] by [`Self::finish`].
    ///
    /// **WP-RD1: private staging.** The sole write path is [`Self::record_member_xref`]; the
    /// reducer can never push a raw `(file, member)` pair around it.
    member_xrefs: FxHashMap<MemberName, Vec<MemberXref>>,

    /// Per-occurrence resolution records. Pushed by the reducer at every resolved call site
    /// and identifier / member-access via [`Self::record_binding`]. Moved into
    /// [`AnalysisResult::bindings`] by [`Self::finish`].
    ///
    /// **WP-RD1: private staging.** The sole write path is [`Self::record_binding`].
    bindings: Vec<Binding>,
    lambda_uses_self: FxHashSet<NodeId>,

    /// Byte-position regions in which a given warning is suppressed by an
    /// `@warning_ignore_start("CODE_NAME")` / `@warning_ignore_restore("CODE_NAME")` pair (the
    /// Godot's `warning_ignore_start_lines[code]` at gdscript_parser.cpp:284). One inclusive
    /// `(start_byte, end_byte_or_usize::MAX)` per region; built once at construction by walking
    /// the tree for standalone annotation nodes that the parser doesn't attach to anything.
    warning_ignore_regions: FxHashMap<crate::warnings::WarningCode, Vec<(usize, usize)>>,
    /// Per-line ignored warnings — Godot's `warning_ignored_lines[code]` set
    /// (gdscript_parser.cpp:281). Populated from every node whose `annotations` includes
    /// `@warning_ignore("CODE")`: the annotation's owner's 1-based start_line gets added to
    /// the set for the named code. Suppresses warnings even when the annotation hangs on a
    /// parent node that doesn't match the warning's anchor (e.g. `@warning_ignore("unsafe_cast")`
    /// on a statement whose nested expression is the cast).
    warning_ignored_lines: FxHashMap<crate::warnings::WarningCode, rustc_hash::FxHashSet<u32>>,

    /// The Godot feature release whose analyzer semantics are in force. Set by
    /// [`crate::analyze_with_options`] from `AnalyzeOptions.dialect`; the bare
    /// [`AnalysisContext::new`] path leaves it at [`Dialect::DEFAULT`]. See `gd_syntax::dialect`
    /// for the `DIALECT(...)` guard convention.
    pub dialect: Dialect,

    /// M5 WP-O3: fixpoint loop governor. Incremented at each `reduce_expression` /
    /// `resolve_node` / lambda-drain self-call entry. Crossing [`Self::iter_limit`] bails the
    /// current resolution with a synthetic `analyzer: fixpoint iteration budget exceeded
    /// (limit=N)` error and the partial result the sink has already accumulated.
    pub iter_count: u32,
    /// M5 WP-O3: per-file iteration cap. Set by [`crate::analyze_with_options`] from
    /// `AnalyzeOptions.iter_limit` (defaults to [`crate::DEFAULT_ITER_LIMIT`] = 100 000). A value of
    /// 0 disables the governor (the legacy / fuzz path), but the analyzer's default driver
    /// always sets a finite cap. Once [`Self::iter_count`] reaches this value, [`Self::checkpoint`]
    /// pushes the synthetic budget-exceeded error, latches [`Self::bailed`], and returns `true`,
    /// so the hot reducer / resolver paths bail.
    pub iter_limit: u32,
    /// M5 WP-O4: cooperative cancellation token. Read every 256 nodes inside the reducer /
    /// resolver checkpoints. `None` (the default for tests / fuzz) skips the check entirely.
    pub cancellation: Option<&'a CancellationToken>,
    /// M7 (#57) test governor: sleep this long at every 256-node checkpoint so an analyze pass
    /// is deterministically slow — what makes the cancellation/staleness wire races testable.
    /// `None` (default and production) costs one branch per checkpoint gate.
    pub checkpoint_delay: Option<std::time::Duration>,
    /// Memoized cross-file `extends`-chain resolutions (`crate::script_chain`). One walk per
    /// distinct [`crate::data_type::ScriptRef`] per analysis pass — `is_type_compatible` and the
    /// member walks would otherwise re-resolve the same base chain per argument/identifier.
    /// `RefCell` because several consumers hold `&AnalysisContext`.
    pub(crate) script_chains: std::cell::RefCell<
        FxHashMap<crate::data_type::ScriptRef, std::rc::Rc<crate::script_chain::ResolvedChain>>,
    >,
    /// Memoized decl-name-slot identifier set (see [`Self::decl_ident_ids`]). A pure function of
    /// the immutable tree, built once on first use instead of once per name-set sweep call.
    /// `OnceCell` because the sweep helpers hold `&AnalysisContext`.
    decl_ident_ids: std::cell::OnceCell<FxHashSet<NodeId>>,
    /// M5 WP-O3 / O4: once tripped, every subsequent governor / cancellation checkpoint
    /// short-circuits (the synthetic error has already been pushed; we don't want to spam the
    /// same diagnostic for every remaining call). Toggle is one-way per analyze pass.
    pub bailed: bool,

    sink: DiagnosticSink,
}

impl<'a> AnalysisContext<'a> {
    pub fn new(
        tree: &'a ParseTree,
        native: &'a NativeDb,
        xfile: &'a dyn CrossFileQuery,
        file: Option<FileId>,
        script_path: impl Into<String>,
        policy: &'a WarnPolicy,
    ) -> Self {
        let warning_ignore_regions = build_warning_ignore_regions(tree);
        let warning_ignored_lines = build_warning_ignored_lines(tree);
        let script_path = script_path.into();
        AnalysisContext {
            tree,
            native,
            xfile,
            file,
            script_path,
            policy,
            types: TypeTable::new(tree.len()),
            folds: FoldTable::new(tree.len()),
            bases: FxHashMap::default(),
            current_class: None,
            current_function: None,
            current_resolving_member: None,
            static_context: false,
            reducing_callee: false,
            concrete_function: None,
            pending_lambda_bodies: Vec::new(),
            current_lambda_stack: Vec::new(),
            awaiting_call: false,
            resolved_interfaces: FxHashSet::default(),
            reduced: FxHashSet::default(),
            resolved_bodies: FxHashSet::default(),
            resolved_functions: FxHashSet::default(),
            abstract_nodes: FxHashSet::default(),
            suite_stack: Vec::new(),
            assignments: FxHashMap::default(),
            warning_ignore_regions,
            warning_ignored_lines,
            member_xrefs: FxHashMap::default(),
            bindings: Vec::new(),
            lambda_uses_self: FxHashSet::default(),
            // M5 WP-O3 / O4: governor + cancellation defaults. The bare `AnalysisContext::new`
            // path leaves these at their permissive values (no cap, no token) so the
            // conformance / fuzz path continues exactly as it did pre-M5;
            // [`crate::analyze_with_options`] overrides them from `AnalyzeOptions` for the LSP
            // server's per-request analyze.
            dialect: Dialect::DEFAULT,
            iter_count: 0,
            iter_limit: 0,
            cancellation: None,
            checkpoint_delay: None,
            script_chains: std::cell::RefCell::new(FxHashMap::default()),
            decl_ident_ids: std::cell::OnceCell::new(),
            bailed: false,
            sink: DiagnosticSink::new(),
        }
    }

    /// `GDScriptAnalyzer::mark_lambda_use_self()` (analyzer.cpp:6364): mark the current lambda
    /// and all parent lambdas as using `self`.
    pub(crate) fn mark_lambda_use_self(&mut self) {
        for &lambda in &self.current_lambda_stack {
            self.lambda_uses_self.insert(lambda);
        }
    }

    pub(crate) fn push_current_lambda(&mut self, lambda: NodeId) {
        self.current_lambda_stack.push(lambda);
    }

    pub(crate) fn pop_current_lambda(&mut self) {
        self.current_lambda_stack.pop();
    }

    /// Identifier NodeIds that are the *name slot* of a declaration (Variable / Constant /
    /// Signal / Function / Parameter / Enum / EnumValue / Class) — declaration sites, not
    /// references. Godot's `usages` counter only increments on true references
    /// (`reduce_identifier`), never on the decl identifier itself, so the name-set sweeps
    /// (`referenced_names`, `emit_unused_parameter_warnings`, `warn_unused_local`) all exclude
    /// these ids. Built lazily with one O(nodes) walk on first access and shared by every
    /// sweep in the same analysis pass.
    pub fn decl_ident_ids(&self) -> &FxHashSet<NodeId> {
        self.decl_ident_ids
            .get_or_init(|| build_decl_ident_ids(self.tree))
    }

    /// M5 WP-O3 / O4: per-node governor + cancellation checkpoint. Call once at the entry of
    /// every hot reducer / resolver path the plan calls out (`reduce_expression`'s post-visited
    /// site, `resolve_node`'s dispatcher head, the lambda-drain re-entrant self-call). Returns
    /// `true` if the analyzer should bail this node's resolution (governor exceeded OR
    /// cancellation requested); the diagnostic is pushed exactly once per analyze pass.
    ///
    /// `span` anchors the synthetic diagnostic — the byte range of the node that tripped the
    /// budget / cancel. The check is one branch on the happy path (an `iter_limit == 0` "disabled"
    /// sentinel short-circuits before any token loads), and the cancellation read is gated to
    /// every 256 nodes to keep the `Acquire`-ordered atomic load off the every-node hot path.
    pub fn checkpoint(&mut self, span: ByteSpan) -> bool {
        if self.bailed {
            return true;
        }
        // M7 (#57) test governor: sleep on the same 256-entry gate the cancellation read uses,
        // BEFORE that read — so a cancel arriving during the sleep is observed at this very
        // checkpoint and the cancelled run's latency is ~one delay, while an uncancelled run
        // pays the delay at every gate. Production (`None`) costs one branch here.
        if let Some(delay) = self.checkpoint_delay {
            if self.iter_count & 0xFF == 0 {
                std::thread::sleep(delay);
            }
        }
        // Cancellation check FIRST so a pre-cancel (token cancelled before analyze even starts)
        // is respected immediately on the first checkpoint — the alternative (bump first, then
        // check) would only fire on entries 256/512/... and leave a pre-cancelled token waiting
        // 256 nodes before bailing. The 256-entry gate keeps the Acquire-ordered atomic load
        // off the every-node hot path; iter_count is always bumped below so the gate ticks
        // consistently whether or not the governor is enabled.
        if let Some(tok) = self.cancellation {
            if self.iter_count & 0xFF == 0 && tok.is_cancelled() {
                self.sink
                    .push_error("analyzer: request cancelled".to_string(), span);
                self.bailed = true;
                return true;
            }
        }
        // Always bump so cancellation-gating is consistent. The governor only enforces the cap
        // when `iter_limit > 0` (0 is the "disabled" sentinel — the conformance / fuzz path).
        self.iter_count = self.iter_count.saturating_add(1);
        if self.iter_limit > 0 && self.iter_count >= self.iter_limit {
            let limit = self.iter_limit;
            self.sink.push_error(
                format!("analyzer: fixpoint iteration budget exceeded (limit={limit})"),
                span,
            );
            self.bailed = true;
            return true;
        }
        false
    }

    /// Record that *this file's* member `from_member` (the value of
    /// [`Self::current_resolving_member`] when called) reads `(target_file, target_member)` in
    /// its initializer. Append-only; duplicates within the same analyze pass are intentional
    /// (the downstream cycle check reads via `.iter().any(...)` and doesn't care about
    /// uniqueness).
    pub fn record_member_xref(
        &mut self,
        from_member: &str,
        target_file: FileId,
        target_member: &str,
    ) {
        self.member_xrefs
            .entry(MemberName::from(from_member))
            .or_default()
            .push(MemberXref {
                target_file,
                target_member: MemberName::from(target_member),
            });
    }

    /// Append a per-occurrence resolution record. Used by the reducer at every resolved call
    /// site ([`Binding::Call`]) and identifier / member-access ([`Binding::Use`]). Additive:
    /// never changes any other diagnostic or type.
    pub fn record_binding(&mut self, binding: Binding) {
        self.bindings.push(binding);
    }

    // --- Node / type access ---------------------------------------------------------------------

    pub fn node(&self, id: NodeId) -> &Node {
        self.tree.get(id)
    }

    /// A node's resolved type (Godot's `Node::get_datatype()`).
    pub fn get_type(&self, id: NodeId) -> &DataType {
        self.types.get(id)
    }

    /// Set a node's resolved type (Godot's `Node::set_datatype()`).
    pub fn set_type(&mut self, id: NodeId, dt: DataType) {
        self.types.set(id, dt);
    }

    /// A class node's base type, or a default (`has_no_type()`) one if it has not been resolved yet —
    /// matching Godot, where an unresolved `base_type` reads as `UNDETECTED`.
    pub fn base_type(&self, class_id: NodeId) -> DataType {
        self.bases.get(&class_id).cloned().unwrap_or_default()
    }

    pub fn set_base(&mut self, class_id: NodeId, dt: DataType) {
        self.bases.insert(class_id, dt);
    }

    // --- Diagnostics ----------------------------------------------------------------------------

    /// Godot's `push_error(message, p_origin)`: an unconditional semantic error anchored at a node.
    pub fn push_error(&mut self, message: impl Into<String>, at: NodeId) {
        let span: ByteSpan = self.tree.get(at).span;
        self.sink.push_error(message, span);
    }

    /// Like [`Self::push_error`] but at an explicit byte span (e.g. a synthesized location).
    pub fn push_error_at(&mut self, message: impl Into<String>, span: ByteSpan) {
        self.sink.push_error(message, span);
    }

    /// WP-R3: stamp an explicit 1-based line on the emitted diagnostic. Used by sites that
    /// mirror Godot's null-source `push_error` (gdscript_parser.cpp:241-244 reads
    /// `previous.start_line` / `previous.end_line` when `p_origin == nullptr`). At end-of-parse
    /// the parser's `previous` is at the synthetic post-EOF line stamped on
    /// [`gd_syntax::ParseTree::eof_line`]. The byte span still anchors the diagnostic's column
    /// / span window (used by LSP rendering at the protocol boundary); only the line number
    /// rendered by `.out`-style harnesses changes.
    pub fn push_error_at_line(&mut self, message: impl Into<String>, at: NodeId, line: u32) {
        let span: ByteSpan = self.tree.get(at).span;
        self.sink.push_error_with_line(message, span, line);
    }

    /// Godot's `parser->push_warning(p_source, code, p_symbols)` (gdscript_parser.cpp:257). The
    /// effective level comes from the [`WarnPolicy`] precedence chain; `Ignore` drops silently,
    /// `Warn` / `Error` produce a [`Diagnostic`] anchored at `at_node`'s span (with the anchor's
    /// start line stamped for `finish`'s by-line warning ordering).
    pub fn push_warning(
        &mut self,
        code: crate::warnings::WarningCode,
        symbols: &[String],
        at_node: gd_syntax::ast::NodeId,
    ) {
        self.push_warning_with_related(code, symbols, at_node, Vec::new());
    }

    /// [`Self::push_warning`] with attached [`crate::diagnostic::RelatedInfo`] entries — the
    /// structured twin of a location the message names only in text (the SHADOWED_* family's
    /// shadowed declaration). Shares the `@warning_ignore` suppression.
    pub fn push_warning_with_related(
        &mut self,
        code: crate::warnings::WarningCode,
        symbols: &[String],
        at_node: gd_syntax::ast::NodeId,
        related: Vec<crate::diagnostic::RelatedInfo>,
    ) {
        // Suppression mirrors Godot's `apply_pending_warnings` (gdscript_parser.cpp:269-281):
        // drop when the anchor's 1-based start line is in `warning_ignored_lines[code]` — the
        // per-line set [`build_warning_ignored_lines`] expands from each `@warning_ignore`'s
        // annotation-to-target-header span — or when the anchor falls inside a
        // `@warning_ignore_start`/`_restore` region. There is no node-attached annotation walk:
        // upstream filters purely by line, and the spans already cover every line of the
        // annotated target's header (continuation lines included).
        let line = self.tree.get(at_node).loc.start.line;
        if let Some(lines) = self.warning_ignored_lines.get(&code) {
            if lines.contains(&line) {
                return;
            }
        }
        let span: ByteSpan = self.tree.get(at_node).span;
        if self.is_warning_ignored_in_region(code, span.start) {
            return;
        }
        let level = self.policy.effective_level(code);
        self.sink
            .push_warning_with_related(code, level, symbols, span, line, related);
    }

    /// `True` when the diagnostic byte `pos` falls inside any `@warning_ignore_start(code)` /
    /// `@warning_ignore_restore(code)` region (Godot's `warning_ignore_start_lines[code]` test
    /// at gdscript_parser.cpp:284). gdls's regions are byte-based — start = byte after the
    /// `_start` annotation, end = byte of the matching `_restore` (or `usize::MAX` for "until EOF").
    fn is_warning_ignored_in_region(&self, code: crate::warnings::WarningCode, pos: usize) -> bool {
        let Some(regions) = self.warning_ignore_regions.get(&code) else {
            return false;
        };
        regions.iter().any(|&(s, e)| pos >= s && pos < e)
    }

    pub fn has_errors(&self) -> bool {
        self.sink.has_errors()
    }

    /// Total emitted diagnostics so far. Cheap snapshot for gate-on-error-emission patterns —
    /// see [`crate::diagnostic::DiagnosticSink::diagnostic_count`].
    pub fn diagnostic_count(&self) -> usize {
        self.sink.diagnostic_count()
    }

    // --- Completion -----------------------------------------------------------------------------

    // (helpers below `impl` block)

    /// Consume the context into the analysis result. Rewrites any surviving in-file `Class` type to a
    /// self-referential `Script` ref so that no transient `NodeId` (meaningless in another tree) ever
    /// escapes `analyze()` — the plan's "no `class_node` leaves the result" guard. The inner-class
    /// chain is DERIVED here (pass 1 below) from each `Class` value's `class_node`, so an inner-class
    /// INSTANCE keeps its precise identity for the post-analysis nav/completion consumers (#146).
    pub fn finish(mut self) -> AnalysisResult {
        // WP-RD2: an orphan file (`self.file == None`) still needs a concrete id for its own
        // class types' self-`ScriptRef` so the "no `class_node` leaves the result" rewrite below
        // holds. `FileId::ORPHAN` is that id; it is never recorded in a `Binding` and never
        // escapes to another file's analysis, so it cannot mis-attribute anything.
        let self_file = self.file.unwrap_or(FileId::ORPHAN);
        // Two passes so the inner-class chain is DERIVED (not dropped). Pass 1 (immutable): for each
        // in-file `Class` value, recover its inner-class name chain from `class_node` via the DFS
        // (`in_file_script_ref_of_class`) — root → empty, an inner class → e.g. `["Inner"]`. The old
        // single-pass `unwrap_or_default()` left `inner` EMPTY, so an inner-class INSTANCE value
        // (`var x := Inner.new()`) was rewritten to the file ROOT — a fail-open lie on every value-node
        // consumer (hover/completion/definition resolved root members, not the inner class). Only
        // inner-class instances change; a root instance still yields `inner: []`.
        let class_inner: Vec<(usize, Vec<String>)> = self
            .types
            .iter()
            .enumerate()
            .filter(|(_, dt)| dt.kind == DtKind::Class)
            .map(|(i, dt)| {
                let inner = dt
                    .class_node
                    .and_then(|n| crate::reducer::in_file_script_ref_of_class(&self, n))
                    .map(|sr| sr.inner)
                    .unwrap_or_default();
                (i, inner)
            })
            .collect();
        // Pass 2 (mutable): rewrite each `Class` value to its `Script` ref (the result must not leak
        // `DtKind::Class` — a transient NodeId). Both passes visit `Class` types in index order, so
        // the pass-1 paths drain positionally onto the same types (`TypeTable` is not `Index`able).
        let mut paths = class_inner.into_iter();
        for dt in self.types.iter_mut() {
            if dt.kind != DtKind::Class {
                continue;
            }
            let inner = paths.next().map(|(_, p)| p).unwrap_or_default();
            dt.kind = DtKind::Script;
            dt.class_node = None;
            dt.script_type = Some(ScriptRef {
                file: self_file,
                inner,
            });
        }
        debug_assert!(
            self.types.iter().all(|dt| dt.kind != DtKind::Class),
            "an in-file Class type leaked out of analyze()"
        );
        AnalysisResult {
            types: self.types,
            folds: self.folds,
            diagnostics: self.sink.finish(),
            member_xrefs: self.member_xrefs,
            bindings: self.bindings,
            lambda_uses_self: self.lambda_uses_self,
            bailed: self.bailed,
        }
    }
}

/// Walk the parse tree for every `@warning_ignore_start("CODE_NAME")` and
/// `@warning_ignore_restore("CODE_NAME")` annotation node — Godot's standalone annotations
/// that the parser allocates but doesn't attach to any owner — and build the per-code
/// suppression region map.
///
/// Two simplifying choices vs Godot:
/// * **No nested counting.** Godot resets `warning_ignore_start_lines[code]` to `INT_MAX` on
///   each `@warning_ignore_restore`, so a second `_restore` without a matching `_start` is a
///   no-op. We do the same: an unbalanced `_restore` outside an active region is ignored.
/// * **End-of-file falls back to `usize::MAX`.** A `_start` without a matching `_restore` keeps
///   suppressing through the rest of the file, matching Godot's INT_MAX terminal state.
///
/// Annotations are byte-anchored — the diagnostic's `span.start` is compared against
/// `[region.start, region.end)`. The start byte for a `_start` annotation is **after** the
/// annotation's own span, so the annotation line itself is NOT in the region (matching the
/// Godot's `start_line` semantics which fires from the next line on).
/// One O(nodes) walk collecting every declaration's name-slot identifier id — the backing
/// builder for [`AnalysisContext::decl_ident_ids`]. The kind set is the union of what each
/// name-set sweep excludes: Enum / EnumValue / Class entries are class-body-only declarations
/// (their identifiers can never fall inside a function-body sweep window), so including them
/// is a no-op for `emit_unused_parameter_warnings` / `warn_unused_local` while
/// `referenced_names` needs them.
fn build_decl_ident_ids(tree: &ParseTree) -> FxHashSet<NodeId> {
    use gd_syntax::ast::NodeKind;

    let mut out = FxHashSet::default();
    for id in tree.iter_ids() {
        let name_slot = match &tree.get(id).kind {
            NodeKind::Variable(v) => v.identifier,
            NodeKind::Constant(c) => c.identifier,
            NodeKind::Signal(s) => s.identifier,
            NodeKind::Function(f) => f.identifier,
            NodeKind::Parameter(p) => p.identifier,
            NodeKind::Class(c) => c.identifier,
            NodeKind::Enum(e) => {
                for v in &e.values {
                    if let Some(i) = v.identifier {
                        out.insert(i);
                    }
                }
                e.identifier
            }
            _ => None,
        };
        if let Some(i) = name_slot {
            out.insert(i);
        }
    }
    out
}

fn build_warning_ignore_regions(
    tree: &ParseTree,
) -> FxHashMap<crate::warnings::WarningCode, Vec<(usize, usize)>> {
    use crate::warnings::{code_from_name, WarningCode};
    use gd_syntax::ast::NodeKind;
    use gd_syntax::token::Literal;

    let mut active: FxHashMap<WarningCode, usize> = FxHashMap::default();
    let mut regions: FxHashMap<WarningCode, Vec<(usize, usize)>> = FxHashMap::default();

    // Collect annotations in source order (NodeId order = allocation order ≈ source order for
    // standalone annotations).
    let mut ann_ids: Vec<gd_syntax::ast::NodeId> = tree
        .iter_ids()
        .filter(|&id| matches!(tree.get(id).kind, NodeKind::Annotation(_)))
        .collect();
    ann_ids.sort_by_key(|&id| tree.get(id).span.start);

    for ann_id in ann_ids {
        let span = tree.get(ann_id).span;
        let name = match &tree.get(ann_id).kind {
            NodeKind::Annotation(a) => a.name.clone(),
            _ => continue,
        };
        let (start_marker, restore_marker) = match name.as_str() {
            "@warning_ignore_start" => (true, false),
            "@warning_ignore_restore" => (false, true),
            _ => continue,
        };
        let args: Vec<_> = match &tree.get(ann_id).kind {
            NodeKind::Annotation(a) => a.arguments.clone(),
            _ => continue,
        };
        for arg in args {
            let NodeKind::Literal(lit) = &tree.get(arg).kind else {
                continue;
            };
            let code_name = match &lit.value {
                Literal::String(s) | Literal::StringName(s) | Literal::NodePath(s) => s.clone(),
                _ => continue,
            };
            let Some(code) = code_from_name(&code_name.to_ascii_uppercase()) else {
                continue;
            };
            if start_marker {
                active.entry(code).or_insert(span.end);
            } else if restore_marker {
                if let Some(start) = active.remove(&code) {
                    regions.entry(code).or_default().push((start, span.start));
                }
            }
        }
    }

    // Anything still active runs through the rest of the file.
    for (code, start) in active {
        regions.entry(code).or_default().push((start, usize::MAX));
    }
    regions
}

/// Build the per-line ignored-warnings set — Godot's `warning_ignored_lines[code]` table, filled
/// by `GDScriptParser::warning_ignore_annotation` (gdscript_parser.cpp:5078-5151). For each
/// `@warning_ignore("CODE")` in a node's `annotations`, every 1-based line from the annotation's
/// start line through a target-kind-specific end line is recorded under the code. The end line is
/// the target's *header* end — initializer / list / condition / test end, function signature end,
/// match-branch patterns end — NOT the body end, so a multi-line declaration is covered through
/// its last header line while body lines stay live. Consumed by [`AnalysisContext::push_warning`],
/// which drops a warning whose anchor start line is in the set (Godot's `apply_pending_warnings`
/// line filter, gdscript_parser.cpp:269-281).
fn build_warning_ignored_lines(
    tree: &ParseTree,
) -> FxHashMap<crate::warnings::WarningCode, rustc_hash::FxHashSet<u32>> {
    use crate::warnings::code_from_name;
    use gd_syntax::ast::NodeKind;
    use gd_syntax::token::Literal;

    let mut out: FxHashMap<crate::warnings::WarningCode, rustc_hash::FxHashSet<u32>> =
        FxHashMap::default();

    for id in tree.iter_ids() {
        let owner = tree.get(id);
        if owner.annotations.is_empty() {
            continue;
        }
        for &ann_id in &owner.annotations {
            let ann = match &tree.get(ann_id).kind {
                NodeKind::Annotation(a) => a,
                _ => continue,
            };
            if ann.name != "@warning_ignore" {
                continue;
            }
            // Per-target-kind span (gdscript_parser.cpp:5086-5141): annotation start line
            // through the target's header end.
            let mut start_line = tree.get(ann_id).loc.start.line;
            let mut end_line = owner.loc.end.line;
            // `SIMPLE_CASE`: the named header field's end line, or the target's own start line
            // when the field is absent.
            let header_end = |field: Option<gd_syntax::ast::NodeId>| match field {
                Some(f) => tree.get(f).loc.end.line,
                None => owner.loc.start.line,
            };
            match &owner.kind {
                // Can contain properties (set/get).
                NodeKind::Variable(v) => end_line = header_end(v.initializer),
                // Contain bodies.
                NodeKind::For(f) => end_line = header_end(f.list),
                NodeKind::If(i) => end_line = header_end(i.condition),
                NodeKind::Match(m) => end_line = header_end(m.test),
                NodeKind::While(w) => end_line = header_end(w.condition),
                NodeKind::Class(_) => {
                    // The class *header*, widened over all of its annotations (so a
                    // `@warning_ignore` stacked under `@tool`/`@icon` covers the whole block).
                    end_line = owner.loc.start.line;
                    for &other in &owner.annotations {
                        let other_loc = tree.get(other).loc;
                        start_line = start_line.min(other_loc.start.line);
                        end_line = end_line.max(other_loc.end.line);
                    }
                }
                NodeKind::Function(f) => {
                    // Signature end: max over parameter (+ default initializer) end lines.
                    // Upstream walks only `parameters` — a rest parameter is held outside that
                    // list and does not extend the span; mirrored here.
                    end_line = owner.loc.start.line;
                    for &param in &f.parameters {
                        end_line = end_line.max(tree.get(param).loc.end.line);
                        if let NodeKind::Parameter(p) = &tree.get(param).kind {
                            if let Some(init) = p.initializer {
                                end_line = end_line.max(tree.get(init).loc.end.line);
                            }
                        }
                    }
                }
                NodeKind::MatchBranch(b) => {
                    end_line = owner.loc.start.line;
                    for &pat in &b.patterns {
                        end_line = end_line.max(tree.get(pat).loc.end.line);
                    }
                }
                _ => {}
            }
            let end_line = end_line.max(start_line); // Prevent infinite loop.
            for &arg in &ann.arguments {
                let NodeKind::Literal(lit) = &tree.get(arg).kind else {
                    continue;
                };
                let s = match &lit.value {
                    Literal::String(s) | Literal::StringName(s) | Literal::NodePath(s) => s,
                    _ => continue,
                };
                if let Some(code) = code_from_name(&s.to_ascii_uppercase()) {
                    let lines = out.entry(code).or_default();
                    for line in start_line..=end_line {
                        lines.insert(line);
                    }
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod checkpoint_tests {
    use super::*;
    use crate::{NoCrossFile, StrictSettings};
    use gd_project::WarningConfig;

    fn default_policy() -> WarnPolicy {
        WarnPolicy::build(&WarningConfig::default(), &StrictSettings::default())
    }

    /// A token cancelled before analysis begins must bail on the FIRST checkpoint — not 256 nodes
    /// later. This pins the check-before-bump ordering in [`AnalysisContext::checkpoint`]: were
    /// `iter_count` bumped first, it would read 1 and the `& 0xFF == 0` gate would skip the
    /// cancellation read until the count next landed on a 256 multiple. (The `governor.rs`
    /// full-pass test cannot distinguish the orderings — a long-enough pass eventually trips a 256
    /// gate either way.)
    #[test]
    fn checkpoint_honours_a_pre_cancel_on_the_first_call() {
        let tree = gd_syntax::parse("extends Node\n").tree;
        let native = NativeDb::empty();
        let policy = default_policy();
        let tok = CancellationToken::new();
        tok.cancel();
        let mut ctx = AnalysisContext::new(
            &tree,
            &native,
            &NoCrossFile,
            Some(FileId::new(1)),
            "checkpoint.gd",
            &policy,
        );
        ctx.cancellation = Some(&tok);
        assert!(
            ctx.checkpoint(ByteSpan::new(0, 1)),
            "a pre-cancelled token must bail on the very first checkpoint"
        );
        assert!(ctx.bailed, "the first-checkpoint bail must latch `bailed`");
    }

    /// A cancel that flips while `iter_count` sits OFF a 256 boundary must not be read (the gate
    /// keeps the `Acquire`-ordered atomic load off the every-node hot path); it must be honoured
    /// the moment the count lands on a 256 multiple. Guards both halves of the gate at once.
    #[test]
    fn checkpoint_reads_a_mid_stream_cancel_only_at_the_next_gate() {
        let tree = gd_syntax::parse("extends Node\n").tree;
        let native = NativeDb::empty();
        let policy = default_policy();
        let tok = CancellationToken::new();
        let mut ctx = AnalysisContext::new(
            &tree,
            &native,
            &NoCrossFile,
            Some(FileId::new(1)),
            "checkpoint.gd",
            &policy,
        );
        ctx.cancellation = Some(&tok);

        // Off-gate: iter_count is a non-multiple of 256; the cancel flips but must NOT be read.
        ctx.iter_count = 100;
        tok.cancel();
        assert!(
            !ctx.checkpoint(ByteSpan::new(0, 1)),
            "a cancel observed off the 256-gate must not bail (the gate is shut)"
        );
        assert!(!ctx.bailed);

        // On-gate: align iter_count to a 256 multiple; the next checkpoint reads the token.
        ctx.iter_count = 256;
        assert!(
            ctx.checkpoint(ByteSpan::new(0, 1)),
            "the cancel must be honoured at the next 256-gate"
        );
        assert!(ctx.bailed);
    }
}

#[cfg(test)]
mod warning_ignore_span_tests {
    //! Pins the `@warning_ignore` span model ported from Godot's `warning_ignore_annotation`
    //! (gdscript_parser.cpp:5078-5151): the ignored-lines table runs from the annotation's start
    //! line through the target's *header* end — multi-line headers covered, bodies not.

    use super::*;
    use crate::cross_file::NoCrossFile;
    use crate::warn_policy::StrictSettings;
    use crate::warnings::WarningCode;
    use gd_project::WarningConfig;

    /// The recorded ignore-lines for `code` after parsing `src`, sorted.
    fn span_lines(src: &str, code: WarningCode) -> Vec<u32> {
        let tree = gd_syntax::parse(src).tree;
        let map = build_warning_ignored_lines(&tree);
        let mut lines: Vec<u32> = map
            .get(&code)
            .map(|s| s.iter().copied().collect())
            .unwrap_or_default();
        lines.sort_unstable();
        lines
    }

    fn mini_native() -> NativeDb {
        NativeDb::from_json(
            r#"{
                "header": {"version_major": 4, "version_minor": 6, "version_patch": 3},
                "classes": [
                    {"name": "Object"},
                    {"name": "Node", "inherits": "Object"}
                ]
            }"#,
        )
        .expect("valid mini dump")
    }

    /// Warning codes of all warning diagnostics `src` analyzes to.
    fn warning_codes(src: &str) -> Vec<WarningCode> {
        let tree = gd_syntax::parse(src).tree;
        let policy = WarnPolicy::build(&WarningConfig::default(), &StrictSettings::default());
        let result = crate::analyze(
            &tree,
            Some(FileId::new(1)),
            "t.gd",
            &mini_native(),
            &NoCrossFile,
            &policy,
        );
        result
            .diagnostics
            .iter()
            .filter_map(|d| d.warning_code)
            .collect()
    }

    /// FUNCTION target: the span runs from the annotation through the signature end (max over
    /// parameter end lines) — the multi-line-signature shape from issue #28 — and NOT the body.
    #[test]
    fn function_span_covers_multi_line_signature_not_body() {
        let src = "extends Node\n\nvar health = 3\n\n\n@warning_ignore(\"shadowed_variable\")\nfunc hurt(\n\t\thealth: int) -> void:\n\tprint(health)\n";
        assert_eq!(
            span_lines(src, WarningCode::ShadowedVariable),
            vec![6, 7, 8],
            "annotation line 6 through signature end 8; body line 9 stays live"
        );
    }

    /// VARIABLE target with a multi-line initializer: span reaches the initializer's end line.
    #[test]
    fn variable_span_covers_multi_line_initializer() {
        let src = "extends Node\n\n@warning_ignore(\"unused_private_class_variable\")\nvar _x = [\n\t1,\n\t2,\n]\n";
        assert_eq!(
            span_lines(src, WarningCode::UnusedPrivateClassVariable),
            vec![3, 4, 5, 6, 7],
        );
    }

    /// VARIABLE target without an initializer: span ends at the declaration's own start line.
    #[test]
    fn variable_span_without_initializer_ends_at_declaration() {
        let src = "extends Node\n@warning_ignore(\"unused_private_class_variable\")\nvar _x: int\n";
        assert_eq!(
            span_lines(src, WarningCode::UnusedPrivateClassVariable),
            vec![2, 3],
        );
    }

    /// MATCH_BRANCH target: span covers the (multi-line) patterns, not the branch body.
    #[test]
    fn match_branch_span_covers_patterns_not_body() {
        let src = "extends Node\n\n\nfunc f(v: Array) -> void:\n\tmatch v:\n\t\t@warning_ignore(\"unused_variable\")\n\t\t[1,\n\t\t\t\t2]:\n\t\t\tvar x = 1\n\t\t_:\n\t\t\tpass\n";
        assert_eq!(
            span_lines(src, WarningCode::UnusedVariable),
            vec![6, 7, 8],
            "patterns end line 8; branch body line 9 stays live"
        );
    }

    /// IF target: span covers the (multi-line) condition, not the body.
    #[test]
    fn if_span_covers_condition_not_body() {
        let src = "extends Node\n\n\nfunc f(a: int, b: int) -> void:\n\t@warning_ignore(\"standalone_expression\")\n\tif (a >\n\t\t\tb):\n\t\tpass\n";
        assert_eq!(
            span_lines(src, WarningCode::StandaloneExpression),
            vec![5, 6, 7],
        );
    }

    /// CLASS target: the span is the annotation block plus the class *header* — members are NOT
    /// blanket-covered (Godot's `@warning_ignore_start` regions exist for that).
    #[test]
    fn class_span_covers_header_only() {
        let src =
            "extends Node\n\n\n@warning_ignore(\"unused_signal\")\nclass Inner:\n\tsignal _alarm\n";
        assert_eq!(span_lines(src, WarningCode::UnusedSignal), vec![4, 5]);
    }

    /// End-to-end issue #28 repro: a parameter on a continuation line of an annotated function's
    /// signature must be suppressed (was a false positive vs Godot 4.6.3).
    #[test]
    fn multi_line_signature_shadow_is_suppressed() {
        let src = "extends Node\n\nvar health = 3\n\n\n@warning_ignore(\"shadowed_variable\")\nfunc hurt(\n\t\thealth: int) -> void:\n\thealth += 1\n";
        assert!(
            !warning_codes(src).contains(&WarningCode::ShadowedVariable),
            "the ignore span covers the continuation line of the signature"
        );
    }

    /// End-to-end single-line shape keeps suppressing (regression guard for the span rewrite).
    #[test]
    fn single_line_signature_shadow_is_suppressed() {
        let src = "extends Node\n\nvar health = 3\n\n\n@warning_ignore(\"shadowed_variable\")\nfunc hurt(health: int) -> void:\n\thealth += 1\n";
        assert!(!warning_codes(src).contains(&WarningCode::ShadowedVariable));
    }

    /// End-to-end over-suppression guard: an annotation on the function must NOT swallow a shadow
    /// in the body — the span stops at the signature, exactly like upstream.
    #[test]
    fn function_annotation_does_not_suppress_body_shadow() {
        let src = "extends Node\n\nvar health = 3\n\n\n@warning_ignore(\"shadowed_variable\")\nfunc hurt() -> void:\n\tvar health := 4\n\thealth += 1\n";
        assert!(
            warning_codes(src).contains(&WarningCode::ShadowedVariable),
            "body lines are outside the ignore span and must still warn"
        );
    }
}
