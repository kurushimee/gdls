//! `gd_analyze` — the GDScript analyzer, ported function-for-function from `gdscript_analyzer.cpp`.
//!
//! Implements the staged `resolve_inheritance → resolve_interface → resolve_body` pipeline (mapping
//! onto the eager-interface / lazy-body split), the `reduce_*` / `resolve_*` pass families, gradual
//! (`Variant`) typing with the `UNSAFE_*` warnings, the full **45 active + 3 deprecated-gated**
//! warning set (per `docs/02 §6` and `docs/04`), and the strict-mode policy layer. M3 closed at
//! analyzer-phase conformance 1.0 (300/300); see `docs/04-diagnostics-strict-mode.md` for the
//! diagnostic model.
//!
//! Module map:
//! - [`data_type`] — the resolved type lattice; [`typetable`] / [`foldtable`] — the per-node side
//!   tables that keep `gd_syntax`'s AST engine-free.
//! - [`warnings`] — the 48-code set (45 active + 3 deprecated-gated) + verbatim message
//!   templates; [`diagnostic`] — the emit sink (`push_error` / `push_error_with_line` /
//!   `push_warning`); [`warn_policy`] — the strict-mode precedence layer
//!   (defaults → `project.godot` → profile → fine-grained overrides; inline `@warning_ignore`
//!   layered on at emit time by [`context`]).
//! - [`context`] — the [`AnalysisContext`] cursor + side tables (including the cross-file cycle key
//!   `current_resolving_member`, WP-R2); [`cross_file`] — the [`CrossFileQuery`] seam
//!   (see `docs/02 §10`); [`resolver`] + [`reducer`] — the resolve_* / reduce_* pass bodies.
//!   [`analyze`] drives the passes.

pub mod binding;
pub mod cancellation;
pub mod context;
pub mod cross_file;
pub mod data_type;
pub mod diagnostic;
pub mod foldtable;
pub mod reducer;
pub mod resolver;
pub mod typetable;
pub mod warn_policy;
pub mod warnings;

pub use binding::{
    find_incoming_calls, find_outgoing_calls, find_use_bindings, Binding, BindingTargetKind,
    MemberName, MemberXref,
};
pub use cancellation::CancellationToken;
pub use context::{AnalysisContext, AnalysisResult};
pub use cross_file::{CrossFileQuery, NoCrossFile, SyntacticQuery};
pub use data_type::{DataType, DtKind, MethodSig, ScriptRef, TypeSource, VariantType};
pub use diagnostic::{Diagnostic, DiagnosticSink, Severity};
pub use foldtable::{FoldTable, FoldedValue};
pub use typetable::TypeTable;
pub use warn_policy::{StrictProfile, StrictSettings, WarnPolicy};
pub use warnings::{code_from_name, name_from_code, WarnLevel, WarningCode, WARNING_MAX};

/// Default fixpoint iteration budget for the WP-O3 governor — the per-file cap on
/// `reduce_expression` / `resolve_node` / lambda-drain self-call entries before the analyzer
/// bails with `analyzer: fixpoint iteration budget exceeded` and a partial result.
///
/// The M5 plan §6B suggested 1000 based on a per-file fixpoint-iteration measurement, but the
/// actual checkpoint sites (every once-per-node `reduce_expression`, every `resolve_node`
/// dispatch, every lambda-drain re-entry) tick the counter on EVERY node visited, not just on
/// genuine fixpoint re-iterations. A moderately-sized feature-test file in the conformance
/// corpus (`features/boolean_operators_for_all_types.gd`, ~250 lines) crosses 1000 visits on a
/// clean run, so a limit of 1000 would false-positive on such files. 100 000 leaves ~2 orders of magnitude of
/// headroom over the largest fixture (typical .gd ≤ 5000 nodes, so ≤ 5000 expression bumps + a
/// few thousand resolve_node bumps), keeps a pathological cycle bounded to microseconds of CPU,
/// and still trips long before the LSP's responsiveness budget would otherwise stall.
pub const DEFAULT_ITER_LIMIT: u32 = 100_000;

use gd_project::FileId;
use gd_syntax::ParseTree;
use gd_types::NativeDb;

/// Analyze one parsed file, mirroring `GDScriptAnalyzer::analyze()` (analyzer.cpp:6609): clear, then
/// drive the resolution passes, then hand back the resolved side tables + diagnostics. Cross-file
/// resolution flows in through `&dyn CrossFileQuery` — the analyzer never re-parses a file itself.
///
/// `script_path` is the source file's path as it should appear in fqcn-prefixed messages — the
/// Godot's `parser->script_path` (analyzer.cpp:702, `head->fqcn = canonicalize_path(script_path)`).
/// The conformance harness passes the corpus file's basename (`foo.gd`); the LSP server passes
/// the URI's path. An empty string is the safe default for isolated unit-test analysis: the
/// head class's fqcn then resolves to empty, preserving pre-WP-J diagnostic strings.
///
/// WP-C drives only `resolve_inheritance`; interface/body resolution and warning emission join here in
/// WP-D/E/F. The pass short-circuits exactly as Godot does (an inheritance error skips the later
/// passes), but every diagnostic produced so far is already in the result — analysis never panics on
/// malformed input, it degrades to a partial diagnostic set.
///
/// **WP-RD2: `file` is `Option<FileId>`.** `Some(id)` for a script the index has interned;
/// `None` for a file the index doesn't know (an `untitled:` buffer or a `.gd` outside the
/// project). When `None`, the reducer records `Binding`s with `target_file`/`callee_file = None`
/// (the analyzer never invents a colliding placeholder id — the former `FileId(0)` bug) so the
/// nav handlers correctly answer "don't know" instead of mis-attributing the orphan's references
/// to whichever real script the index interned first.
pub fn analyze(
    tree: &ParseTree,
    file: Option<FileId>,
    script_path: &str,
    native: &NativeDb,
    xfile: &dyn CrossFileQuery,
    policy: &WarnPolicy,
) -> AnalysisResult {
    analyze_with_options(
        tree,
        file,
        script_path,
        native,
        xfile,
        policy,
        AnalyzeOptions::default(),
    )
}

/// Per-call analyzer knobs added in M5 — the WP-O3 fixpoint governor iteration cap and the WP-O4
/// `$/cancelRequest` cooperative-cancellation token. Both are optional and default to the
/// production behaviour: unbounded fixpoint iterations (M3 had no cap) and no cancellation. The
/// LSP server wires both fields through [`analyze_with_options`] from its per-request handler so
/// a slow analyze can be killed before it starves the event loop.
///
/// Kept as a struct (not extra args on `analyze`) so the conformance harness, fuzz targets, and
/// 145+ existing tests can continue calling [`analyze`] unmodified — every new knob lands here
/// without touching their signatures.
#[derive(Default)]
pub struct AnalyzeOptions<'a> {
    /// WP-O3: per-file fixpoint iteration budget. `None` means "use [`DEFAULT_ITER_LIMIT`]"; an
    /// explicit `Some(N)` overrides it (used by the LSP server's `initializationOptions.analyzer.iterLimit`).
    pub iter_limit: Option<u32>,
    /// WP-O4: cooperative cancellation token. Checked every 256 nodes inside the reducer / resolver
    /// hot loops. `None` (the default) skips the check entirely — zero overhead for the
    /// conformance / unit-test path.
    pub cancellation: Option<&'a CancellationToken>,
}

/// `analyze` with per-call knobs — see [`AnalyzeOptions`]. Single source of truth for the analyzer
/// driver; the bare [`analyze`] wrapper is the production / test default. Threads the iteration
/// limit through to [`AnalysisContext`] and stores the cancellation token reference (the analyzer
/// reads via `Self::cancellation` during reducer / resolver checkpoints).
pub fn analyze_with_options<'a>(
    tree: &'a ParseTree,
    file: Option<FileId>,
    script_path: &str,
    native: &'a NativeDb,
    xfile: &'a dyn CrossFileQuery,
    policy: &'a WarnPolicy,
    options: AnalyzeOptions<'a>,
) -> AnalysisResult {
    let mut ctx = AnalysisContext::new(tree, native, xfile, file, script_path, policy);
    ctx.iter_limit = options.iter_limit.unwrap_or(DEFAULT_ITER_LIMIT);
    ctx.cancellation = options.cancellation;
    if resolver::resolve_inheritance(&mut ctx).is_ok() {
        resolver::resolve_interface(&mut ctx);
        // analyzer.cpp:6587 — the third top-level pass. `resolve_body` walks every function body
        // through the `resolve_node` dispatcher; the reducer (WP-E1) runs over every expression
        // it finds. `apply_pending_warnings` lands with the warning emission in WP-F.
        resolver::resolve_body(&mut ctx);
    }
    ctx.finish()
}
