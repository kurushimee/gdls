//! M10 (#72): `textDocument/semanticTokens/{full,full/delta,range}` +
//! `workspace/semanticTokens/refresh` for GDScript — **standard legend only**.
//!
//! This is the #30 highlighting target made concrete: a generic color scheme colors GDScript via
//! the standard LSP token legend with ZERO theme/client configuration. Every name on the wire is a
//! standard `SemanticTokenType` / `SemanticTokenModifier` constant from LSP 3.17 (pinned by the
//! [`LEGEND_TYPES`] / [`LEGEND_MODIFIERS`] tables + a snapshot test); **no `gdscript/`-prefixed or
//! otherwise custom name is ever emitted.**
//!
//! ## Classification (docs/09 §6-M10 mapping)
//!
//! A document is classified by walking the parse tree ([`classify_document`]) and tagging every
//! relevant identifier-bearing position with a [`TokType`] + a modifier bitset. The walk is the
//! **spine** (token-primary positions via each node's byte span); the analyzer's [`AnalysisResult`]
//! is consulted for use-site precision (member vs local, native vs project, enum value, method):
//!   * `class_name`/types → `class` (natives also get `defaultLibrary`); enums → `enum`; enum values
//!     → `enumMember`; functions → `function`; methods → `method` (+ `static`); signals → `event`;
//!     annotations (`@export`, `@onready`, …) → `decorator`; `const` → `variable` + `readonly`;
//!     parameters → `parameter`; class vars (members) → `property`; locals → `variable`.
//!   * Modifiers: `declaration` (+ `definition`, identical in GDScript) on every declaration site;
//!     `readonly` on `const`; `static` on `static func`/`static var`; `defaultLibrary` on native
//!     classes; `deprecated` is in the legend for completeness (not emitted in v1 — the analyzer has
//!     no per-symbol deprecation source for GDScript).
//!
//! ## Analysis-priced vs parse-priced (the `range` contract)
//!
//! [`classify_document`] takes `analysis: Option<&AnalysisResult>`. `full` / `full/delta` are in the
//! Hard-pressure shed set (they call the analyzer), so when they run, pressure is Normal/Soft and a
//! full analysis is always available. `range` must stay served at Hard pressure, so it passes
//! [`Workspace::cached_analysis`] — an `Option` that is `None` on a cache miss while the server is
//! shedding. The classifier then emits every **structurally** derivable token — declarations,
//! annotations, type positions, and local/parameter USES (resolved from the enclosing function's
//! scope, no analyzer) — and OMITS only the analyzer-dependent uses (cross-file members, enum values)
//! rather than guessing them ("never lie": omit, don't fabricate).
//!
//! ## Wire encoding
//!
//! Tokens are sorted by source position, deduplicated, and LSP delta-encoded ([`encode`]): each
//! token is `(deltaLine, deltaStartChar, length, typeIndex, modifierBitset)` relative to the
//! previous token. `length` is in the negotiated encoding's units (UTF-16 by default) — taken from
//! the [`PositionMapper`]-mapped range width, never from raw byte spans. The full→delta diff
//! ([`diff`]) operates on the flat token array exactly as the client applies it (the integer offsets
//! are `tokenIndex * 5`).

use lsp_types::{
    SemanticToken, SemanticTokenModifier, SemanticTokenType, SemanticTokens, SemanticTokensEdit,
    SemanticTokensLegend,
};

use gd_analyze::{AnalysisResult, Binding, BindingTargetKind, DtKind};
use gd_syntax::ast::{
    DictStyle, EnumValue, LocalKind, Member, NodeId, NodeKind, ParseTree, SubscriptAccess,
};
use gd_syntax::ByteSpan;
use rustc_hash::{FxHashMap, FxHashSet};

use crate::position::PositionMapper;

// ===================================================================================================
// The legend — the SINGLE source of truth. Both the advertised legend and the on-wire type/modifier
// indices derive from these tables, so they can never drift (and the snapshot test pins them).
// ===================================================================================================

/// The standard `SemanticTokenType`s gdls emits, in wire-index order. **STANDARD NAMES ONLY** — a
/// snapshot test fails if a non-standard name is ever added. The index of a type here is its
/// `tokenType` integer on the wire.
pub(crate) const LEGEND_TYPES: &[SemanticTokenType] = &[
    SemanticTokenType::CLASS,       // 0
    SemanticTokenType::ENUM,        // 1
    SemanticTokenType::ENUM_MEMBER, // 2
    SemanticTokenType::FUNCTION,    // 3
    SemanticTokenType::METHOD,      // 4
    SemanticTokenType::PROPERTY,    // 5
    SemanticTokenType::PARAMETER,   // 6
    SemanticTokenType::VARIABLE,    // 7
    SemanticTokenType::EVENT,       // 8
    SemanticTokenType::DECORATOR,   // 9
];

/// The standard `SemanticTokenModifier`s gdls emits, in bit-index order. **STANDARD NAMES ONLY.**
/// The index of a modifier here is its bit position in the `tokenModifiers` bitset.
pub(crate) const LEGEND_MODIFIERS: &[SemanticTokenModifier] = &[
    SemanticTokenModifier::DECLARATION,     // bit 0
    SemanticTokenModifier::DEFINITION,      // bit 1
    SemanticTokenModifier::READONLY,        // bit 2
    SemanticTokenModifier::STATIC,          // bit 3
    SemanticTokenModifier::DEFAULT_LIBRARY, // bit 4
    SemanticTokenModifier::DEPRECATED,      // bit 5
];

/// The advertised [`SemanticTokensLegend`] (a clone of the two const tables). Built once at
/// `capabilities()` time and never per-client — the indices must be stable for delta correlation.
#[must_use]
pub(crate) fn legend() -> SemanticTokensLegend {
    SemanticTokensLegend {
        token_types: LEGEND_TYPES.to_vec(),
        token_modifiers: LEGEND_MODIFIERS.to_vec(),
    }
}

/// A token type, named for the wire index it maps to in [`LEGEND_TYPES`]. `repr(u32)` so the cast to
/// the wire `tokenType` integer is the enum's own discriminant — kept in lockstep with the table by
/// the `debug_assert`s in [`encode`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u32)]
pub(crate) enum TokType {
    Class = 0,
    Enum = 1,
    EnumMember = 2,
    Function = 3,
    Method = 4,
    Property = 5,
    Parameter = 6,
    Variable = 7,
    Event = 8,
    Decorator = 9,
}

// Modifier bit positions (mirrors LEGEND_MODIFIERS order).
const MOD_DECLARATION: u32 = 1 << 0;
const MOD_DEFINITION: u32 = 1 << 1;
const MOD_READONLY: u32 = 1 << 2;
const MOD_STATIC: u32 = 1 << 3;
const MOD_DEFAULT_LIBRARY: u32 = 1 << 4;
#[allow(dead_code)] // In the legend for completeness; GDScript has no per-symbol deprecation source.
const MOD_DEPRECATED: u32 = 1 << 5;

/// A declaration site carries `declaration` + `definition` (GDScript has no separate declare/define).
const MOD_DECL: u32 = MOD_DECLARATION | MOD_DEFINITION;

/// One classified token before delta-encoding: the source byte span, its type, and its modifier
/// bitset (legend-bit positions). Spans are mapped to ranges + sorted at encode time.
#[derive(Clone, Copy, Debug)]
pub(crate) struct RawToken {
    pub span: ByteSpan,
    pub ty: TokType,
    pub modifiers: u32,
}

// ===================================================================================================
// Classification — walk the tree, enrich with the analysis.
// ===================================================================================================

/// Classify every relevant identifier in `tree` into [`RawToken`]s (unsorted, source-byte spans).
///
/// `analysis` is `Some` for the analysis-priced paths (`full`/`full/delta`) and may be `None` for
/// `range` under Hard memory pressure (a [`Workspace::cached_analysis`] miss). With `None`, the
/// structurally-derivable tokens are still emitted — declarations, annotations, type positions, and
/// local/parameter uses (resolved from the enclosing function's scope) — and only the
/// analyzer-dependent uses (cross-file members, enum values) are skipped (never guessed).
#[must_use]
pub(crate) fn classify_document(
    tree: &ParseTree,
    analysis: Option<&AnalysisResult>,
    db: &gd_types::NativeDb,
) -> Vec<RawToken> {
    let mut out: Vec<RawToken> = Vec::new();

    // (1) Use-site span → resolved kind index, built from the analyzer's per-occurrence bindings.
    // `Binding::Use.site` is the resolved identifier/attribute node's own span (see
    // `reducer::record_member_use`), so it keys directly against the identifier nodes we walk.
    // `target_file == None` on a Class use means a native/builtin type (→ defaultLibrary).
    let use_index: FxHashMap<(usize, usize), &Binding> = match analysis {
        Some(a) => a
            .bindings()
            .iter()
            .filter_map(|b| match b {
                Binding::Use { site, .. } => Some(((site.start, site.end), b)),
                // Calls are classified via the AST callee position, not the call-site span; any
                // future binding variant is ignored here (it can't carry an identifier span we own).
                _ => None,
            })
            .collect(),
        None => FxHashMap::default(),
    };

    // (2) Declaration-identifier spans: the identifier child of every declaration node, so the AST
    // walk doesn't also emit a USE token there from a coincident binding (a declaration is tagged
    // once, by its declaration arm). Collected first, then consulted in the identifier arm.
    let mut decl_spans: FxHashMap<(usize, usize), ()> = FxHashMap::default();

    // (3) Spans that are TYPE positions (extends chain, `: T` annotations, `as T` casts, the base of
    // a `Class.member` static access). Identifiers there are `class`/`enum`, classified
    // syntactically (refined to defaultLibrary via the analysis).
    let mut type_spans: FxHashMap<(usize, usize), ()> = FxHashMap::default();

    // (4) Identifier spans that the local-use fallback must NOT treat as a same-named local (the
    // documentHighlight over-capture lesson): attribute positions (`base.x` — a member access the
    // binding pass owns; an unresolved one stays uncolored rather than mis-colored) and Lua-style
    // dict keys (`{ x = v }` — a string literal, not a reference).
    let mut not_a_local_use: FxHashSet<(usize, usize)> = FxHashSet::default();

    // First pass: declarations (structural — always emitted) + collect decl/type/attribute span sets.
    for id in tree.iter_ids() {
        let node = tree.get(id);
        // Annotations attached to this node (`@export`, `@onready`, …) → decorator. Color ONLY the
        // `@name` marker, not the whole annotation node: the node span stretches over the argument
        // list (`@export_range(0, MAX_VAL)`), so spanning it would (a) paint the argument expressions
        // `decorator`, overlapping any token they earn in the use pass, and (b) drop the whole
        // decorator when the arguments wrap across lines (the single-line guard in `encode`). The
        // `@name` is ASCII and always single-line, and the node span starts at the `@` — so the
        // marker is `span.start .. span.start + name.len()` (bytes == columns for ASCII).
        for &ann_id in &node.annotations {
            if let NodeKind::Annotation(a) = &tree.get(ann_id).kind {
                let span = tree.get(ann_id).span;
                let name_span = ByteSpan {
                    start: span.start,
                    end: span.start + a.name.len(),
                };
                push_span(&mut out, name_span, TokType::Decorator, 0);
            }
        }
        match &node.kind {
            NodeKind::Class(c) => {
                // Color the class identifier wherever one exists — including the implicit ROOT
                // class, whose identifier is the file's `class_name X` (the user-facing class name).
                // The root wrapper of a file with no `class_name` has `identifier: None`, so it is
                // naturally skipped; an explicit inner `class Foo:` colors its own `Foo`.
                if let Some(idn) = c.identifier {
                    emit_decl(
                        &mut out,
                        &mut decl_spans,
                        tree,
                        idn,
                        TokType::Class,
                        MOD_DECL,
                    );
                }
                // `extends A.B` / `extends "res://..."` — the extends idents are type positions.
                for &ext in &c.extends {
                    mark_type_chain(tree, ext, &mut type_spans);
                }
            }
            NodeKind::Function(f) => {
                if let Some(idn) = f.identifier {
                    let mods = MOD_DECL | if f.is_static { MOD_STATIC } else { 0 };
                    // A class-member `func` is a `method` (every named GDScript func is a member of
                    // its class — the script IS the root class); only a lambda-body func (reached via
                    // `LambdaNode.function`, not a `Member::Function`) stays `function`. This makes
                    // the DECLARATION agree with the cross-file USE (a resolved method use →
                    // `method`, see `use_token`) and with the M8 completion convention (script
                    // methods → METHOD, native free functions → FUNCTION).
                    let ty = if is_class_member_func(tree, id) {
                        TokType::Method
                    } else {
                        TokType::Function
                    };
                    emit_decl(&mut out, &mut decl_spans, tree, idn, ty, mods);
                }
                // Return type annotation.
                if let Some(rt) = f.return_type {
                    mark_type_node(tree, rt, &mut type_spans);
                }
            }
            NodeKind::Parameter(p) => {
                if let Some(idn) = p.identifier {
                    emit_decl(
                        &mut out,
                        &mut decl_spans,
                        tree,
                        idn,
                        TokType::Parameter,
                        MOD_DECL,
                    );
                }
                if let Some(ts) = p.datatype_specifier {
                    mark_type_node(tree, ts, &mut type_spans);
                }
            }
            NodeKind::Variable(v) => {
                if let Some(idn) = v.identifier {
                    // A class-level var is a `property`; a function-local var is a `variable`. The
                    // enclosing-suite test below (locals) handles locals; here, decide by whether the
                    // var node is a direct class member.
                    let ty = if is_class_member_var(tree, id) {
                        TokType::Property
                    } else {
                        TokType::Variable
                    };
                    let mods = MOD_DECL | if v.is_static { MOD_STATIC } else { 0 };
                    emit_decl(&mut out, &mut decl_spans, tree, idn, ty, mods);
                }
                if let Some(ts) = v.datatype_specifier {
                    mark_type_node(tree, ts, &mut type_spans);
                }
            }
            NodeKind::Constant(c) => {
                if let Some(idn) = c.identifier {
                    emit_decl(
                        &mut out,
                        &mut decl_spans,
                        tree,
                        idn,
                        TokType::Variable,
                        MOD_DECL | MOD_READONLY,
                    );
                }
                if let Some(ts) = c.datatype_specifier {
                    mark_type_node(tree, ts, &mut type_spans);
                }
            }
            NodeKind::Signal(s) => {
                if let Some(idn) = s.identifier {
                    emit_decl(
                        &mut out,
                        &mut decl_spans,
                        tree,
                        idn,
                        TokType::Event,
                        MOD_DECL,
                    );
                }
            }
            NodeKind::Enum(e) => {
                if let Some(idn) = e.identifier {
                    emit_decl(
                        &mut out,
                        &mut decl_spans,
                        tree,
                        idn,
                        TokType::Enum,
                        MOD_DECL,
                    );
                }
                for v in &e.values {
                    emit_enum_value(&mut out, &mut decl_spans, tree, v);
                }
            }
            NodeKind::Cast(c) => {
                if let Some(ct) = c.cast_type {
                    mark_type_node(tree, ct, &mut type_spans);
                }
            }
            NodeKind::TypeTest(t) => {
                if let Some(tt) = t.test_type {
                    mark_type_node(tree, tt, &mut type_spans);
                }
            }
            NodeKind::Subscript(sub) => {
                // `base.attr` — record the attribute identifier's span so the local-use fallback
                // never mistakes it for a same-named local.
                if let Some(SubscriptAccess::Attribute(Some(attr_id))) = sub.access {
                    let s = tree.get(attr_id).span;
                    not_a_local_use.insert((s.start, s.end));
                }
            }
            NodeKind::Dictionary(d) => {
                // A Lua-style key `{ key = value }` is a STRING literal (the analyzer folds it to a
                // string and records no binding), NOT a reference to a same-named local — exclude its
                // identifier span from the local-use fallback. A Python-style key (`{ expr: value }`)
                // IS an expression, so its identifiers are left to normal classification. The
                // single-element ambiguous case (`style == None`) is parsed Lua-style, so treat it so.
                if matches!(d.style, Some(DictStyle::LuaTable) | None) {
                    for kv in &d.elements {
                        if let Some(k) = kv.key {
                            if let NodeKind::Identifier(_) = &tree.get(k).kind {
                                let s = tree.get(k).span;
                                not_a_local_use.insert((s.start, s.end));
                            }
                        }
                    }
                }
            }
            _ => {}
        }
    }

    // Second pass: identifier USES. An identifier node that is NOT a declaration site is a use.
    // Type-position uses are `class`/`enum`; everything else is classified by the analyzer binding
    // (member/enum/method/local). With no analysis, only type-position uses are emitted (they are
    // structurally derivable), everything else is skipped.
    for id in tree.iter_ids() {
        let NodeKind::Identifier(ident) = &tree.get(id).kind else {
            continue;
        };
        let span = tree.get(id).span;
        let key = (span.start, span.end);
        if decl_spans.contains_key(&key) {
            continue; // already emitted as a declaration
        }
        if type_spans.contains_key(&key) {
            // A type reference (extends chain / `: T` / `as T`). A name the native DB knows is a
            // native class or builtin → `class` + defaultLibrary (works WITHOUT analysis, so `range`
            // under Hard pressure still colors native bases). An enum-typed annotation colors as
            // `enum` when the analysis confirms it; otherwise a project type → bare `class`.
            let (ty, modifiers) = type_use_token(&ident.name, analysis, id, db);
            push_span(&mut out, span, ty, modifiers);
            continue;
        }
        // A resolved member/enum/method/cross-file use — from the analyzer binding.
        if let Some(Binding::Use {
            target_kind,
            target_file,
            ..
        }) = use_index.get(&key).copied()
        {
            if let Some((ty, modifiers)) = use_token(*target_kind, target_file.is_none()) {
                push_span(&mut out, span, ty, modifiers);
            }
            continue;
        }
        // A local-variable / parameter / for-var / pattern-bind USE — resolved STRUCTURALLY from the
        // enclosing function's scope (so it colors even with no analysis, i.e. `range` under Hard
        // pressure). Skip idents that aren't a local reference at all (attribute positions `obj.x`,
        // Lua-style dict keys `{ x = v }`) — see `not_a_local_use` (the over-capture lesson).
        if !not_a_local_use.contains(&key) {
            if let Some(ty) = local_use_kind(tree, span.start, &ident.name) {
                push_span(&mut out, span, ty, 0);
            }
        }
    }

    out
}

/// The token type for a bare identifier USE that resolves to a local binding in the enclosing
/// function — `parameter` for a parameter, `variable` for a local var / `const` / `for`-loop var /
/// `match`-pattern bind — or `None` if no enclosing function declares it (then it isn't a local).
///
/// Purely structural (no analyzer): finds the innermost enclosing `Function` span containing `byte`,
/// then the nearest enclosing `Suite` whose `locals` declares `name`. Mirrors the scoping
/// `handlers::enclosing_function_declaring` uses for references' local classification.
fn local_use_kind(tree: &ParseTree, byte: usize, name: &str) -> Option<TokType> {
    // Innermost enclosing function (smallest span containing the byte).
    let mut fn_span: Option<ByteSpan> = None;
    for id in tree.iter_ids() {
        if let NodeKind::Function(_) = &tree.get(id).kind {
            let s = tree.get(id).span;
            if s.start <= byte
                && byte < s.end
                && fn_span.is_none_or(|b| s.end - s.start < b.end - b.start)
            {
                fn_span = Some(s);
            }
        }
    }
    let fn_span = fn_span?;
    // The nearest enclosing Suite (within the function) whose locals declare `name`. A Suite's span
    // contains the use; among those, prefer the innermost (smallest) — GDScript blocks shadow.
    let mut best: Option<(ByteSpan, LocalKind)> = None;
    for id in tree.iter_ids() {
        let NodeKind::Suite(suite) = &tree.get(id).kind else {
            continue;
        };
        let s = tree.get(id).span;
        if s.start < fn_span.start || s.end > fn_span.end {
            continue; // not inside this function
        }
        if !(s.start <= byte && byte < s.end) {
            continue; // the use isn't inside this suite
        }
        if let Some(&idx) = suite.locals_indices.get(name) {
            let kind = suite.locals[idx].kind;
            if best.is_none_or(|(b, _)| s.end - s.start < b.end - b.start) {
                best = Some((s, kind));
            }
        }
    }
    let (_, kind) = best?;
    Some(match kind {
        LocalKind::Parameter => TokType::Parameter,
        // Local vars, consts, for-loop vars, and match-pattern binds all color as `variable` (a
        // local const is still rendered `variable`; the readonly modifier is reserved for the
        // declaration site, where the `Constant` arm sets it).
        LocalKind::Variable
        | LocalKind::Constant
        | LocalKind::ForVariable
        | LocalKind::PatternBind => TokType::Variable,
    })
}

/// The `(TokType, modifiers)` for a resolved [`Binding::Use`], or `None` for kinds with no standard
/// mapping (none currently — every variant maps). `native` adds `defaultLibrary` to type kinds.
fn use_token(kind: BindingTargetKind, native: bool) -> Option<(TokType, u32)> {
    let ty = match kind {
        BindingTargetKind::Class => TokType::Class,
        BindingTargetKind::Enum => TokType::Enum,
        BindingTargetKind::EnumValue => TokType::EnumMember,
        BindingTargetKind::Function => TokType::Method,
        BindingTargetKind::Constant => TokType::Variable,
        BindingTargetKind::Signal => TokType::Event,
        BindingTargetKind::Member => TokType::Property,
        BindingTargetKind::Variable => TokType::Property,
        // Parameters are never recorded as cross-file bindings (function-scoped) — defensive.
        BindingTargetKind::Parameter => TokType::Parameter,
        // `#[non_exhaustive]`: a future binding kind with no standard mapping emits no token rather
        // than a wrong one ("never lie").
        _ => return None,
    };
    let mut modifiers = 0;
    if matches!(kind, BindingTargetKind::Constant) {
        modifiers |= MOD_READONLY;
    }
    if native && matches!(kind, BindingTargetKind::Class) {
        modifiers |= MOD_DEFAULT_LIBRARY;
    }
    Some((ty, modifiers))
}

/// The `(TokType, modifiers)` for an identifier in a TYPE position (`name` is the identifier text).
///
/// Native-vs-project is decided by the native DB — a name it knows as a native class or builtin type
/// → `class` + `defaultLibrary` (this is DB-driven, so it holds even when `analysis` is `None`, e.g.
/// `range` under Hard pressure). The analyzer refines an enum-TYPED annotation (`var x: MyEnum`) to
/// `enum` when its resolved kind says so; otherwise a project type colors as a bare `class`.
fn type_use_token(
    name: &str,
    analysis: Option<&AnalysisResult>,
    id: NodeId,
    db: &gd_types::NativeDb,
) -> (TokType, u32) {
    let native = db.class_named(name).is_some() || db.builtin_named(name).is_some();
    // The analyzer disambiguates an enum-typed annotation; a native/project class stays `class`.
    let is_enum = matches!(analysis, Some(a) if matches!(a.types.get(id).kind, DtKind::Enum));
    let ty = if is_enum {
        TokType::Enum
    } else {
        TokType::Class
    };
    let modifiers = if native { MOD_DEFAULT_LIBRARY } else { 0 };
    (ty, modifiers)
}

/// Emit a declaration token at the identifier child `idn` and record its span so the use pass skips
/// it. `idn` is expected to be an `Identifier` node; a malformed tree where it isn't simply records
/// the span (the push uses the node's own span regardless).
fn emit_decl(
    out: &mut Vec<RawToken>,
    decl_spans: &mut FxHashMap<(usize, usize), ()>,
    tree: &ParseTree,
    idn: NodeId,
    ty: TokType,
    modifiers: u32,
) {
    let span = tree.get(idn).span;
    decl_spans.insert((span.start, span.end), ());
    push_span(out, span, ty, modifiers);
}

/// Emit an enum value declaration (`enumMember` + readonly — enum values are constants).
fn emit_enum_value(
    out: &mut Vec<RawToken>,
    decl_spans: &mut FxHashMap<(usize, usize), ()>,
    tree: &ParseTree,
    v: &EnumValue,
) {
    if let Some(idn) = v.identifier {
        let span = tree.get(idn).span;
        decl_spans.insert((span.start, span.end), ());
        push_span(out, span, TokType::EnumMember, MOD_DECL | MOD_READONLY);
    }
}

/// Mark every identifier in a `TypeNode`'s chain as a type position. The `TypeNode.type_chain` holds
/// the `A.B.C` identifier ids; `container_types` hold the `Array[T]` element type nodes (recursed).
fn mark_type_node(
    tree: &ParseTree,
    type_id: NodeId,
    type_spans: &mut FxHashMap<(usize, usize), ()>,
) {
    match &tree.get(type_id).kind {
        NodeKind::Type(t) => {
            for &c in &t.type_chain {
                mark_type_chain(tree, c, type_spans);
            }
            for &ct in &t.container_types {
                mark_type_node(tree, ct, type_spans);
            }
        }
        // Defensive: an identifier directly in a type slot.
        NodeKind::Identifier(_) => {
            let s = tree.get(type_id).span;
            type_spans.insert((s.start, s.end), ());
        }
        _ => {}
    }
}

/// Mark a single identifier node (an element of a type/extends chain) as a type position.
fn mark_type_chain(tree: &ParseTree, id: NodeId, type_spans: &mut FxHashMap<(usize, usize), ()>) {
    if let NodeKind::Identifier(_) = &tree.get(id).kind {
        let s = tree.get(id).span;
        type_spans.insert((s.start, s.end), ());
    }
}

/// Whether a `Variable` node `var_id` is a direct member of a class (→ `property`) rather than a
/// function-local (→ `variable`). A class member appears in some `ClassNode.members` as
/// `Member::Variable(var_id)`.
fn is_class_member_var(tree: &ParseTree, var_id: NodeId) -> bool {
    tree.iter_ids().any(|cid| {
        if let NodeKind::Class(c) = &tree.get(cid).kind {
            c.members
                .iter()
                .any(|m| matches!(m, Member::Variable(v) if *v == var_id))
        } else {
            false
        }
    })
}

/// Whether a `Function` node `fn_id` is a direct member of a class (→ `method`) rather than a
/// lambda body (→ `function`). A class method appears in some `ClassNode.members` as
/// `Member::Function(fn_id)`; a named-lambda body is reached only via `LambdaNode.function`, so it
/// is absent from every class's member list.
fn is_class_member_func(tree: &ParseTree, fn_id: NodeId) -> bool {
    tree.iter_ids().any(|cid| {
        if let NodeKind::Class(c) = &tree.get(cid).kind {
            c.members
                .iter()
                .any(|m| matches!(m, Member::Function(f) if *f == fn_id))
        } else {
            false
        }
    })
}

/// Push a [`RawToken`] for `span` (skips an empty span — a zero-width identifier from a recovery
/// node carries no color).
fn push_span(out: &mut Vec<RawToken>, span: ByteSpan, ty: TokType, modifiers: u32) {
    if span.end > span.start {
        out.push(RawToken {
            span,
            ty,
            modifiers,
        });
    }
}

// ===================================================================================================
// Encoding — map → sort → dedup → delta-encode → legend-intersect.
// ===================================================================================================

/// Encode classified tokens to the LSP flat-delta form, intersecting with the client's advertised
/// legend.
///
/// Steps (the order is load-bearing): map each byte span to an LSP [`Range`] through `mapper`
/// (encoding-unit columns), drop any token whose type the client didn't advertise and clear modifier
/// bits the client didn't advertise (the legend-intersection contract), sort by start position,
/// dedup coincident (range, type) pairs, then delta-encode (`length` is the mapped range width, in
/// the negotiated encoding's units — never raw bytes). Multi-line spans are skipped unless the client
/// advertised `multilineTokenSupport` (the standard legend produces only single-line identifier
/// tokens, so this is defensive).
#[must_use]
pub(crate) fn encode(
    raw: &[RawToken],
    mapper: &PositionMapper,
    client_legend: &ClientLegend,
) -> Vec<SemanticToken> {
    // Debug-time guard that the enum discriminants still line up with the legend table.
    debug_assert_eq!(LEGEND_TYPES.len(), 10);
    debug_assert_eq!(LEGEND_MODIFIERS.len(), 6);

    // (1) Map to ranges, intersect with the client legend, drop multi-line + undeclared types.
    struct Mapped {
        line: u32,
        start: u32,
        len: u32,
        ty: u32,
        modifiers: u32,
    }
    let mut mapped: Vec<Mapped> = Vec::with_capacity(raw.len());
    for t in raw {
        // The client must advertise this type, else the token is dropped entirely.
        let Some(ty_index) = client_legend.type_index(t.ty) else {
            continue;
        };
        let range = mapper.span_to_range(t.span);
        // Standard identifier tokens are single-line; skip a stray multi-line span (a malformed
        // construct) — the LSP forbids multi-line tokens unless the client opted in.
        if range.start.line != range.end.line {
            continue;
        }
        let len = range.end.character.saturating_sub(range.start.character);
        if len == 0 {
            continue;
        }
        let modifiers = client_legend.project_modifiers(t.modifiers);
        mapped.push(Mapped {
            line: range.start.line,
            start: range.start.character,
            len,
            ty: ty_index,
            modifiers,
        });
    }

    // (2) Sort by source position; (3) dedup by START position alone (keep the first) — a single
    // source position never emits two overlapping tokens, whatever their type/length. The classifier
    // already gates the use-pass on the decl/type span sets, so coincident-but-different tokens
    // shouldn't arise; collapsing by position is the belt-and-suspenders that keeps the stream
    // non-overlapping even for a client without `overlappingTokenSupport`.
    mapped.sort_by_key(|m| (m.line, m.start));
    mapped.dedup_by(|a, b| a.line == b.line && a.start == b.start);

    // (4) Delta-encode relative to the previous token.
    let mut out: Vec<SemanticToken> = Vec::with_capacity(mapped.len());
    let mut prev_line = 0u32;
    let mut prev_start = 0u32;
    for m in mapped {
        let delta_line = m.line - prev_line;
        let delta_start = if delta_line == 0 {
            m.start - prev_start
        } else {
            m.start
        };
        out.push(SemanticToken {
            delta_line,
            delta_start,
            length: m.len,
            token_type: m.ty,
            token_modifiers_bitset: m.modifiers,
        });
        prev_line = m.line;
        prev_start = m.start;
    }
    out
}

/// The client's advertised legend, captured at `initialize` — gdls intersects every emission with
/// it (LSP 3.17: never send a type/modifier index the client didn't declare). When the client
/// advertised an EMPTY legend (or none), [`ClientLegend::full`] is used: gdls's own full legend,
/// every type/modifier permitted (the conventional default — a client that sends no legend still
/// gets colored; rust-analyzer does the same).
#[derive(Clone, Debug)]
pub(crate) struct ClientLegend {
    /// gdls-`TokType`-index → the client's wire index for that type, or `None` if the client did not
    /// advertise it. Indexed by `TokType as usize`.
    type_to_client: [Option<u32>; 10],
    /// gdls-modifier-bit (0..6) → the client's wire bit for that modifier, or `None`.
    mod_to_client: [Option<u32>; 6],
}

impl Default for ClientLegend {
    /// The permissive default — gdls's own full legend. Used when a client never advertised a
    /// semantic-tokens legend (the `ClientCaps::default()` test path, and the empty-legend client).
    fn default() -> Self {
        Self::full()
    }
}

impl ClientLegend {
    /// gdls's own full legend — every type/modifier maps to its own index. Used when the client did
    /// not advertise a semantic-tokens legend (or advertised an empty one).
    #[must_use]
    pub(crate) fn full() -> Self {
        let mut type_to_client = [None; 10];
        for (i, slot) in type_to_client.iter_mut().enumerate() {
            *slot = Some(i as u32);
        }
        let mut mod_to_client = [None; 6];
        for (i, slot) in mod_to_client.iter_mut().enumerate() {
            *slot = Some(i as u32);
        }
        Self {
            type_to_client,
            mod_to_client,
        }
    }

    /// Build from the client's advertised `tokenTypes` / `tokenModifiers` lists. A gdls type/modifier
    /// the client didn't list maps to `None` (dropped at encode time). An empty `types` list is
    /// treated as "no legend advertised" → [`Self::full`] (every client accepts the standard names).
    #[must_use]
    pub(crate) fn from_client(
        types: &[SemanticTokenType],
        modifiers: &[SemanticTokenModifier],
    ) -> Self {
        if types.is_empty() {
            return Self::full();
        }
        let mut type_to_client = [None; 10];
        for (gi, gty) in LEGEND_TYPES.iter().enumerate() {
            if let Some(ci) = types.iter().position(|t| t == gty) {
                type_to_client[gi] = Some(ci as u32);
            }
        }
        let mut mod_to_client = [None; 6];
        for (gi, gmod) in LEGEND_MODIFIERS.iter().enumerate() {
            if let Some(ci) = modifiers.iter().position(|m| m == gmod) {
                mod_to_client[gi] = Some(ci as u32);
            }
        }
        Self {
            type_to_client,
            mod_to_client,
        }
    }

    /// The client's wire index for a gdls token type, or `None` if it wasn't advertised.
    fn type_index(&self, ty: TokType) -> Option<u32> {
        self.type_to_client[ty as usize]
    }

    /// Project a gdls modifier bitset onto the client's advertised modifier bits, dropping any the
    /// client didn't declare.
    fn project_modifiers(&self, gdls_bits: u32) -> u32 {
        let mut out = 0u32;
        for (bit, client_bit) in self.mod_to_client.iter().enumerate() {
            if gdls_bits & (1 << bit) != 0 {
                if let Some(cb) = client_bit {
                    out |= 1 << cb;
                }
            }
        }
        out
    }
}

// ===================================================================================================
// Delta diff — full → delta against the previous token array, on the FLAT integer view.
// ===================================================================================================

/// Compute the `SemanticTokensEdit`s turning `old` into `new`, in the LSP flat-array form the client
/// applies: offsets/counts are over the flat `[deltaLine, deltaStart, length, type, modifiers, …]`
/// integer array (5 ints per token), so a `start` / `deleteCount` is `tokenIndex * 5`.
///
/// A single replace edit spanning the changed middle is emitted (common-prefix / common-suffix
/// trimmed) — correct and minimal enough; the client applies it identically to a multi-edit diff.
#[must_use]
pub(crate) fn diff(old: &[SemanticToken], new: &[SemanticToken]) -> Vec<SemanticTokensEdit> {
    // Common prefix length (in tokens).
    let max = old.len().min(new.len());
    let mut prefix = 0;
    while prefix < max && old[prefix] == new[prefix] {
        prefix += 1;
    }
    // Common suffix length (in tokens), not overlapping the prefix.
    let mut suffix = 0;
    while suffix < (max - prefix) && old[old.len() - 1 - suffix] == new[new.len() - 1 - suffix] {
        suffix += 1;
    }

    let old_changed = old.len() - prefix - suffix; // tokens removed from old's middle
    let new_changed = &new[prefix..new.len() - suffix]; // tokens inserted into the middle

    if old_changed == 0 && new_changed.is_empty() {
        return Vec::new(); // identical
    }

    vec![SemanticTokensEdit {
        start: (prefix * 5) as u32,
        delete_count: (old_changed * 5) as u32,
        data: Some(new_changed.to_vec()),
    }]
}

/// Apply a [`SemanticTokensDelta`]'s edits to `base` over the flat integer view (the reference the
/// round-trip test verifies against — the exact transform a conformant client performs). Used by
/// tests; lives here so it can't drift from [`diff`].
#[cfg(test)]
#[must_use]
pub(crate) fn apply_delta(
    base: &[SemanticToken],
    delta: &lsp_types::SemanticTokensDelta,
) -> Vec<SemanticToken> {
    // Flatten base to the integer array.
    let mut flat: Vec<u32> = Vec::with_capacity(base.len() * 5);
    for t in base {
        flat.extend_from_slice(&[
            t.delta_line,
            t.delta_start,
            t.length,
            t.token_type,
            t.token_modifiers_bitset,
        ]);
    }
    // Apply edits in descending start order so earlier offsets stay valid.
    let mut edits = delta.edits.clone();
    edits.sort_by_key(|e| std::cmp::Reverse(e.start));
    for e in edits {
        let start = e.start as usize;
        let end = start + e.delete_count as usize;
        let mut ins: Vec<u32> = Vec::new();
        if let Some(data) = &e.data {
            for t in data {
                ins.extend_from_slice(&[
                    t.delta_line,
                    t.delta_start,
                    t.length,
                    t.token_type,
                    t.token_modifiers_bitset,
                ]);
            }
        }
        flat.splice(start..end, ins);
    }
    // Re-chunk to tokens.
    flat.chunks_exact(5)
        .map(|c| SemanticToken {
            delta_line: c[0],
            delta_start: c[1],
            length: c[2],
            token_type: c[3],
            token_modifiers_bitset: c[4],
        })
        .collect()
}

/// Wrap a token vec in a `SemanticTokens` with a result id (the `full` / `full/delta` shape, whose
/// id correlates the next delta request).
#[must_use]
pub(crate) fn semantic_tokens(result_id: String, data: Vec<SemanticToken>) -> SemanticTokens {
    SemanticTokens {
        result_id: Some(result_id),
        data,
    }
}

/// Wrap a token vec in a `SemanticTokens` with NO result id (the `range` shape — a partial set is
/// never a delta baseline, so it carries no correlating id).
#[must_use]
pub(crate) fn semantic_tokens_no_id(data: Vec<SemanticToken>) -> SemanticTokens {
    SemanticTokens {
        result_id: None,
        data,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lsp_types::SemanticTokensDelta;

    /// The legend is STANDARD-only: every advertised name is a known LSP 3.17 standard token
    /// type/modifier. This is the #30 generic-LSP guarantee — it FAILS the moment a custom
    /// (`gdscript/`-prefixed or otherwise non-standard) name is added to either table. The exact
    /// legend is pinned so a reviewer sees precisely what goes on the wire.
    #[test]
    fn legend_is_standard_names_only() {
        let types: Vec<&str> = LEGEND_TYPES.iter().map(|t| t.as_str()).collect();
        assert_eq!(
            types,
            vec![
                "class",
                "enum",
                "enumMember",
                "function",
                "method",
                "property",
                "parameter",
                "variable",
                "event",
                "decorator",
            ],
            "the advertised token TYPES must be exactly this standard-only set, in this order"
        );
        let mods: Vec<&str> = LEGEND_MODIFIERS.iter().map(|m| m.as_str()).collect();
        assert_eq!(
            mods,
            vec![
                "declaration",
                "definition",
                "readonly",
                "static",
                "defaultLibrary",
                "deprecated",
            ],
            "the advertised token MODIFIERS must be exactly this standard-only set, in this order"
        );

        // Belt + suspenders: assert every name equals an lsp-types standard constant (so a typo'd
        // literal that merely *looks* standard can't sneak through).
        let standard_types = [
            SemanticTokenType::NAMESPACE,
            SemanticTokenType::TYPE,
            SemanticTokenType::CLASS,
            SemanticTokenType::ENUM,
            SemanticTokenType::INTERFACE,
            SemanticTokenType::STRUCT,
            SemanticTokenType::TYPE_PARAMETER,
            SemanticTokenType::PARAMETER,
            SemanticTokenType::VARIABLE,
            SemanticTokenType::PROPERTY,
            SemanticTokenType::ENUM_MEMBER,
            SemanticTokenType::EVENT,
            SemanticTokenType::FUNCTION,
            SemanticTokenType::METHOD,
            SemanticTokenType::MACRO,
            SemanticTokenType::KEYWORD,
            SemanticTokenType::MODIFIER,
            SemanticTokenType::COMMENT,
            SemanticTokenType::STRING,
            SemanticTokenType::NUMBER,
            SemanticTokenType::REGEXP,
            SemanticTokenType::OPERATOR,
            SemanticTokenType::DECORATOR,
        ];
        for ty in LEGEND_TYPES {
            assert!(
                standard_types.contains(ty),
                "non-standard token type on the wire: {ty:?}"
            );
        }
        let standard_mods = [
            SemanticTokenModifier::DECLARATION,
            SemanticTokenModifier::DEFINITION,
            SemanticTokenModifier::READONLY,
            SemanticTokenModifier::STATIC,
            SemanticTokenModifier::DEPRECATED,
            SemanticTokenModifier::ABSTRACT,
            SemanticTokenModifier::ASYNC,
            SemanticTokenModifier::MODIFICATION,
            SemanticTokenModifier::DOCUMENTATION,
            SemanticTokenModifier::DEFAULT_LIBRARY,
        ];
        for m in LEGEND_MODIFIERS {
            assert!(
                standard_mods.contains(m),
                "non-standard token modifier on the wire: {m:?}"
            );
        }
    }

    /// The `TokType` discriminants line up with their index in `LEGEND_TYPES` (the cast to the wire
    /// `tokenType` is the discriminant). A reorder of one table without the other would silently
    /// mis-color; this pins them together.
    #[test]
    fn toktype_discriminants_match_legend_order() {
        let pairs = [
            (TokType::Class, "class"),
            (TokType::Enum, "enum"),
            (TokType::EnumMember, "enumMember"),
            (TokType::Function, "function"),
            (TokType::Method, "method"),
            (TokType::Property, "property"),
            (TokType::Parameter, "parameter"),
            (TokType::Variable, "variable"),
            (TokType::Event, "event"),
            (TokType::Decorator, "decorator"),
        ];
        for (ty, name) in pairs {
            assert_eq!(
                LEGEND_TYPES[ty as usize].as_str(),
                name,
                "TokType::{name} discriminant must index its legend slot"
            );
        }
    }

    /// `diff` + `apply_delta` round-trip on the FLAT integer view (the exact transform a conformant
    /// client performs): applying the computed edits to `old` reproduces `new` exactly, for a
    /// prefix-change, a suffix-change, an insertion, a deletion, and the identical case.
    #[test]
    fn diff_apply_round_trips_on_flat_array() {
        let tok = |dl, ds, len, ty, m| SemanticToken {
            delta_line: dl,
            delta_start: ds,
            length: len,
            token_type: ty,
            token_modifiers_bitset: m,
        };
        let cases: Vec<(Vec<SemanticToken>, Vec<SemanticToken>)> = vec![
            // identical
            (vec![tok(0, 0, 3, 1, 0)], vec![tok(0, 0, 3, 1, 0)]),
            // middle change
            (
                vec![tok(0, 0, 3, 1, 0), tok(1, 0, 4, 2, 0), tok(1, 0, 5, 3, 0)],
                vec![tok(0, 0, 3, 1, 0), tok(1, 0, 9, 7, 4), tok(1, 0, 5, 3, 0)],
            ),
            // insertion
            (
                vec![tok(0, 0, 3, 1, 0), tok(1, 0, 5, 3, 0)],
                vec![tok(0, 0, 3, 1, 0), tok(1, 0, 4, 2, 0), tok(1, 0, 5, 3, 0)],
            ),
            // deletion
            (
                vec![tok(0, 0, 3, 1, 0), tok(1, 0, 4, 2, 0), tok(1, 0, 5, 3, 0)],
                vec![tok(0, 0, 3, 1, 0), tok(1, 0, 5, 3, 0)],
            ),
            // total replace
            (vec![tok(0, 0, 3, 1, 0)], vec![tok(2, 2, 2, 9, 2)]),
            // empty → some
            (vec![], vec![tok(0, 0, 3, 1, 0)]),
            // some → empty
            (vec![tok(0, 0, 3, 1, 0)], vec![]),
        ];
        for (old, new) in cases {
            let edits = diff(&old, &new);
            let delta = SemanticTokensDelta {
                result_id: Some("x".into()),
                edits,
            };
            let applied = apply_delta(&old, &delta);
            assert_eq!(
                applied, new,
                "diff→apply must reproduce the fresh full token array; old={old:?} new={new:?}"
            );
        }
    }

    /// `diff` offsets are token-index * 5 (flat-array units), not token indices — pin the exact edit
    /// for a known middle change so a regression to token-index units is caught.
    #[test]
    fn diff_offsets_are_flat_array_units() {
        let tok = |dl, ds, len, ty, m| SemanticToken {
            delta_line: dl,
            delta_start: ds,
            length: len,
            token_type: ty,
            token_modifiers_bitset: m,
        };
        let old = vec![tok(0, 0, 3, 1, 0), tok(1, 0, 4, 2, 0), tok(1, 0, 5, 3, 0)];
        let new = vec![tok(0, 0, 3, 1, 0), tok(1, 0, 9, 7, 4), tok(1, 0, 5, 3, 0)];
        let edits = diff(&old, &new);
        assert_eq!(edits.len(), 1);
        // prefix = 1 token, suffix = 1 token → start = 1*5 = 5, delete_count = 1*5 = 5.
        assert_eq!(edits[0].start, 5, "start must be tokenIndex*5");
        assert_eq!(edits[0].delete_count, 5, "deleteCount must be tokenCount*5");
        assert_eq!(edits[0].data.as_ref().unwrap(), &vec![tok(1, 0, 9, 7, 4)]);
    }

    /// The classifier handles `None` analysis (the `range`-under-Hard-pressure / cached-analysis-miss
    /// path): it still emits every STRUCTURALLY-derivable token — declarations, annotations, type
    /// positions, AND local/parameter USES (resolved from the enclosing function's scope, no analyzer
    /// needed) — and never panics. Only analyzer-dependent uses (cross-file members, enum values) are
    /// omitted (never guessed). This is the parse-priced guarantee the `range` handler relies on.
    #[test]
    fn classifier_without_analysis_emits_structural_tokens_only() {
        let src = "extends Node\nclass_name Foo\n\n@export var hp: int = 0\n\nfunc run(n: int) -> void:\n\tvar x = n\n\tprint(x)\n";
        let parsed = gd_syntax::parse(src);
        let db = gd_types::NativeDb::empty();
        let raw = classify_document(&parsed.tree, None, &db);
        assert!(
            !raw.is_empty(),
            "structural classification must still emit tokens with no analysis"
        );
        // The `@export` decorator and the `hp` property declaration are present.
        assert!(
            raw.iter().any(|t| t.ty == TokType::Decorator),
            "the @export annotation must classify as decorator even with no analysis"
        );
        assert!(
            raw.iter()
                .any(|t| t.ty == TokType::Property && t.modifiers & MOD_DECLARATION != 0),
            "the `hp` member declaration must classify as property+declaration with no analysis"
        );
        // The `x` local USE in `print(x)` (line 7, byte at `\tprint(` + 0) is colored `variable`
        // WITHOUT a declaration modifier — resolved structurally from the function scope. The `n`
        // parameter USE in `var x = n` likewise → `parameter`. Locate by byte span: `print(x)`'s `x`.
        let x_use_byte = src.find("print(x)").unwrap() + "print(".len();
        let x_use = raw
            .iter()
            .find(|t| t.span.start == x_use_byte)
            .expect("the `x` local use in print(x) must be colored with no analysis");
        assert_eq!(x_use.ty, TokType::Variable);
        assert_eq!(
            x_use.modifiers & MOD_DECLARATION,
            0,
            "a USE is not a declaration"
        );
    }

    /// A Lua-style dictionary key (`{ name = value }`) is a STRING literal, NOT a reference to a
    /// same-named local — the local-use fallback must not mis-color it. Regression for the dict-key
    /// over-capture (the documentHighlight lesson, extended to dict keys). The Python-style key
    /// (`{ expr: value }`) IS an expression, so a bare-identifier key there is left to normal
    /// classification (not excluded).
    #[test]
    fn lua_dict_key_shadowing_a_local_is_not_colored_as_that_local() {
        // `name` is both a local var AND a Lua-style dict key. The key occurrence must NOT be colored
        // `variable` (it's a string); the declaration + the genuine use stay colored.
        let src = "func f():\n\tvar name = 1\n\treturn { name = name }\n";
        let parsed = gd_syntax::parse(src);
        let db = gd_types::NativeDb::empty();
        let raw = classify_document(&parsed.tree, None, &db);

        // The Lua key `name` (the one right after `{ `) must have NO token.
        let key_byte = src.find("{ name").unwrap() + "{ ".len();
        assert!(
            raw.iter().all(|t| t.span.start != key_byte),
            "a Lua-style dict key must not be colored as a local; got a token at {key_byte}"
        );
        // The value-position `name` (the genuine local use, after `= `) IS colored `variable`.
        let value_byte = src.rfind("name").unwrap();
        let value_tok = raw
            .iter()
            .find(|t| t.span.start == value_byte)
            .expect("the value-position local use must still be colored");
        assert_eq!(value_tok.ty, TokType::Variable);
        // And the declaration `name` (line 1) is colored variable+declaration.
        let decl_byte = src.find("var name").unwrap() + "var ".len();
        let decl_tok = raw
            .iter()
            .find(|t| t.span.start == decl_byte)
            .expect("the local declaration must be colored");
        assert_eq!(decl_tok.ty, TokType::Variable);
        assert_ne!(decl_tok.modifiers & MOD_DECLARATION, 0);
    }

    /// An annotation with an argument list colors ONLY the `@name` marker as `decorator` — not the
    /// whole `@name(args)` node span. Covering the node span would paint the argument expressions
    /// `decorator` (overlapping any token they earn) and would drop the decorator entirely when the
    /// arguments wrap across lines (the single-line guard in `encode`). Here the `@export_range`
    /// marker is 13 chars; the token must be exactly that, leaving the `(0, MAX_VAL)` bytes uncolored.
    #[test]
    fn annotation_with_args_colors_only_the_name_marker() {
        let src = "const MAX_VAL = 100\n@export_range(0, MAX_VAL) var hp: int = 0\n";
        let parsed = gd_syntax::parse(src);
        let db = gd_types::NativeDb::empty();
        let raw = classify_document(&parsed.tree, None, &db);

        let at_byte = src.find("@export_range").unwrap();
        let deco = raw
            .iter()
            .find(|t| t.ty == TokType::Decorator)
            .expect("the annotation must be colored as a decorator");
        assert_eq!(deco.span.start, at_byte, "the decorator starts at the `@`");
        assert_eq!(
            deco.span.end - deco.span.start,
            "@export_range".len(),
            "the decorator must cover ONLY the `@name` marker, not the argument list"
        );
        // No token may cover any byte of the `(0, MAX_VAL)` argument region as a decorator.
        let args_start = at_byte + "@export_range".len();
        assert!(
            raw.iter()
                .all(|t| !(t.ty == TokType::Decorator && t.span.start >= args_start)),
            "the annotation argument list must not be colored as decorator"
        );
    }

    /// A multi-line annotation (`@export_range(` on one line, the args on the next) still emits its
    /// `@name` decorator token through `encode` — the marker is single-line, so it survives the
    /// multi-line guard that (correctly) drops genuinely multi-line spans. Pins the fix for the
    /// silently-dropped decorator on a wrapped annotation.
    #[test]
    fn multiline_annotation_marker_survives_encode() {
        use crate::position::{PositionEncoding, PositionMapper};
        use ropey::Rope;

        let src = "@export_range(\n\t0, 100) var hp: int = 0\n";
        let parsed = gd_syntax::parse(src);
        let db = gd_types::NativeDb::empty();
        let raw = classify_document(&parsed.tree, None, &db);
        let rope = Rope::from_str(src);
        let mapper = PositionMapper::new(&rope, PositionEncoding::Utf16);
        let encoded = encode(&raw, &mapper, &ClientLegend::full());

        // Decode the first token absolutely: it must be the `@export_range` decorator on line 0.
        let first = encoded
            .first()
            .expect("a wrapped annotation must still emit its decorator");
        assert_eq!(first.delta_line, 0);
        assert_eq!(first.delta_start, 0, "the marker starts at column 0");
        assert_eq!(
            first.length as usize,
            "@export_range".len(),
            "the decorator marker length is the `@name` width"
        );
        assert_eq!(
            LEGEND_TYPES[first.token_type as usize],
            SemanticTokenType::DECORATOR
        );
    }

    /// `ClientLegend::from_client` with a reduced legend drops undeclared types entirely and clears
    /// undeclared modifier bits — never emitting an index the client didn't advertise.
    #[test]
    fn reduced_client_legend_drops_undeclared_types_and_modifiers() {
        // A client that supports only `class` and `function` (in its own order: function=0, class=1)
        // and only the `static` modifier (its bit 0).
        let client_types = vec![SemanticTokenType::FUNCTION, SemanticTokenType::CLASS];
        let client_mods = vec![SemanticTokenModifier::STATIC];
        let legend = ClientLegend::from_client(&client_types, &client_mods);

        // `class` maps to the client's index 1; `function` to 0.
        assert_eq!(legend.type_index(TokType::Class), Some(1));
        assert_eq!(legend.type_index(TokType::Function), Some(0));
        // A type the client didn't advertise is dropped.
        assert_eq!(legend.type_index(TokType::Property), None);
        assert_eq!(legend.type_index(TokType::Event), None);

        // `static` (gdls bit 3) projects to the client's bit 0; everything else clears.
        assert_eq!(legend.project_modifiers(MOD_STATIC), 1 << 0);
        assert_eq!(
            legend.project_modifiers(MOD_DECLARATION | MOD_READONLY),
            0,
            "modifiers the client didn't advertise are cleared"
        );
        assert_eq!(
            legend.project_modifiers(MOD_STATIC | MOD_DECLARATION),
            1 << 0,
            "only the advertised `static` survives; `declaration` is dropped"
        );
    }

    /// An empty client legend (a client that advertised semanticTokens but no type list) falls back
    /// to gdls's full legend — every standard name permitted (the conventional default).
    #[test]
    fn empty_client_legend_falls_back_to_full() {
        let legend = ClientLegend::from_client(&[], &[]);
        assert_eq!(legend.type_index(TokType::Class), Some(0));
        assert_eq!(legend.type_index(TokType::Decorator), Some(9));
        assert_eq!(
            legend.project_modifiers(MOD_DECL | MOD_READONLY),
            MOD_DECL | MOD_READONLY
        );
    }
}
