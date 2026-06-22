//! M10 (#73): `textDocument/inlayHint` + `inlayHint/resolve` + (server→client)
//! `workspace/inlayHint/refresh` for GDScript.
//!
//! Two hint kinds, each independently config-toggleable ([`crate::config::InlayHintConfig`]):
//!
//!   * **[`InlayHintKind::TYPE`]** — the inferred type of a `var x := …` declaration (the walrus
//!     form, where the analyzer infers the type from the initializer) and of an inferred `for` loop
//!     variable (`for item in arr:`). Rendered `: <Type>` after the identifier. Emitted ONLY where
//!     the type is genuinely inferred (no explicit `: T` annotation already present) AND concretely
//!     resolved — a `Variant`/unresolved/meta type yields no hint (never a noise `: Variant`).
//!   * **[`InlayHintKind::PARAMETER`]** — the parameter name before each argument at a resolved call
//!     site (`move(10, 20)` → `x:`/`y:`). **OFF by default for single-argument calls** (a deliberate
//!     noise cut). Driven off the analyzer's own [`gd_analyze::Binding::Call`] resolution (no
//!     re-resolution): the names come from the callee's declaring [`gd_syntax::ast::FunctionNode`]
//!     (project script) or the native [`gd_types::Method`] (engine method).
//!
//! ## Reuse, don't recompute
//!
//! Both kinds read the analyzer's existing per-file [`AnalysisResult`] — the type table for inferred
//! types ([`crate::handlers::human_type_label`] renders the resolved [`gd_analyze::DataType`] exactly
//! as hover does) and the [`gd_analyze::Binding::Call`] set for call-site parameter resolution. A
//! single `analyze` per request feeds both (mirroring `semanticTokens/full`). Analysis-priced, so
//! `inlayHint` sheds at Hard memory pressure (see `server::dispatch_request`).
//!
//! ## The `textEdit` (insert-the-annotation affordance)
//!
//! A TYPE hint carries a `textEdit` that, when accepted, writes the annotation into the source and
//! re-parses + re-analyzes clean (the LSP "the hint becomes part of the document" contract):
//!   * a `for` hint is a plain insert of `: <Type>` at the loop-var identifier's end
//!     (`for i in xs:` → `for i: <Type> in xs:`);
//!   * a `var :=` hint must NEUTRALIZE the inference operator — inserting `: <Type>` alone would
//!     leave `var x: T := …` (contradictory, `:=` *means* "infer"), so the edit REPLACES the
//!     `<ident> .. <initializer>` gap with `: <Type> = `, turning `var x := 5.0` into
//!     `var x: <Type> = 5.0`.
//!
//! The edit is attached ONLY when the resolved type renders as a source-valid type expression (a
//! simple/dotted identifier or an `Array[…]`/`Dictionary[…]` container) — a `<Script #N>` /
//! file-basename render carries the label + tooltip but no edit (never corrupt the file).
//!
//! ## Resolve gating
//!
//! For a client advertising `inlayHint.resolveSupport`, the **tooltip** is deferred: the hint ships
//! without a `tooltip`, carrying a compact `data` blob ([`ResolveData`]) that `inlayHint/resolve`
//! reads to fill it (no re-analyze). For a client without it, the tooltip is embedded EAGERLY in the
//! first response. The `textEdit` is ALWAYS eager either way — an apply never needs a round-trip.

use lsp_types::{
    InlayHint, InlayHintKind, InlayHintLabel, InlayHintParams, InlayHintTooltip, TextEdit,
};
use serde::{Deserialize, Serialize};

use gd_analyze::{AnalysisResult, Binding, CalleeTarget, DataType, DtKind};
use gd_syntax::ast::{NodeKind, ParseTree};
use gd_syntax::ByteSpan;

use crate::position::PositionMapper;
use crate::server::ServerState;
use crate::uri::{uri_to_path, CanonicalKey};

/// The `data` blob carried on a hint between `textDocument/inlayHint` and `inlayHint/resolve` (for a
/// `resolveSupport` client). Holds exactly what resolve needs to fill the tooltip WITHOUT a
/// re-analyze: the tooltip string. Opaque to the client (round-tripped verbatim).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct ResolveData {
    /// The tooltip text resolve attaches. `None` would never be stored (a hint with no tooltip
    /// stores no `data`), so this is always a real string.
    tooltip: String,
}

/// `textDocument/inlayHint`: every inlay hint whose anchor falls in `params.range`.
///
/// Analysis-priced (one `analyze` feeds both hint kinds). Returns `Some(vec)` (possibly empty);
/// never `None` for a valid `.gd` buffer — an unparseable buffer simply yields whatever the
/// tokenizer/analyzer recovered. `None` only for a missing buffer / non-`file://` URI (the LSP
/// `null` wire shape).
#[must_use]
pub fn inlay_hint(state: &mut ServerState, params: InlayHintParams) -> Option<Vec<InlayHint>> {
    let cfg = state.options.inlay_hint.clone();
    // Nothing enabled → an empty set (still a valid response; the client just shows no hints).
    if !cfg.type_hints && !cfg.parameter_hints {
        return Some(Vec::new());
    }

    let uri = params.text_document.uri.clone();
    let text = state.vfs.get(uri.as_str()).map(|d| d.text())?;
    let path = uri_to_path(&uri)?;
    let key = CanonicalKey::for_uri(&uri);
    let enc = state.encoding;
    let parsed = state.workspace.parse(&key, &text);
    let analysis =
        crate::handlers::analyze_with_request_token(state, &key, &path, &parsed.tree, &text);

    let doc = state.vfs.get(uri.as_str())?;
    let mapper = PositionMapper::new(&doc.rope, enc);
    // The requested range, in byte offsets — every hint's ANCHOR byte must fall in `[start, end)`.
    let range_start = mapper.position_to_byte(params.range.start);
    let range_end = mapper.position_to_byte(params.range.end);

    // Build raw hints (byte-anchored) from both kinds against the single analysis, then map + filter
    // to the requested range + project to LSP.
    let mut raw: Vec<RawHint> = Vec::new();
    if cfg.type_hints {
        collect_type_hints(state, &parsed.tree, &analysis, &text, &mut raw);
    }
    if cfg.parameter_hints {
        collect_parameter_hints(state, &parsed.tree, &analysis, &text, &mut raw);
    }

    let resolve_support = state.caps.inlay_hint.resolve_support;
    let hints = raw
        .into_iter()
        .filter(|h| h.anchor >= range_start && h.anchor < range_end)
        .map(|h| h.into_lsp(&mapper, resolve_support))
        .collect();
    Some(hints)
}

/// `inlayHint/resolve`: fill the deferred `tooltip` from the hint's `data` blob.
///
/// Index-/parse-priced — it reads ONLY the round-tripped `data` (never a fresh analyze), so it is not
/// in the Hard-pressure shed set (mirroring `completionItem/resolve`). A hint with no `data` (an
/// eager hint, or a hint with no tooltip) is returned unchanged.
#[must_use]
pub fn inlay_hint_resolve(_state: &mut ServerState, mut hint: InlayHint) -> InlayHint {
    if let Some(data) = hint.data.take() {
        if let Ok(ResolveData { tooltip }) = serde_json::from_value::<ResolveData>(data) {
            hint.tooltip = Some(InlayHintTooltip::String(tooltip));
        }
    }
    hint
}

// ===================================================================================================
// Raw hint — byte-anchored, pre-mapping. Mapped + projected to LSP at the end.
// ===================================================================================================

/// One classified hint before LSP mapping: the anchor byte (its `position`), the label text, the
/// kind, the tooltip, and an optional `textEdit` (byte span + replacement).
struct RawHint {
    /// The byte offset the hint renders at (mapped to the LSP `position`). Also the range-filter key.
    anchor: usize,
    /// The label text (e.g. `: float`, `x:`). Never empty (the LSP forbids an empty label).
    label: String,
    kind: InlayHintKind,
    /// The hover tooltip text. Embedded eagerly, or deferred to resolve via `data` for a
    /// `resolveSupport` client.
    tooltip: Option<String>,
    /// An optional accept-the-hint edit: the byte span to replace and the replacement text.
    text_edit: Option<(ByteSpan, String)>,
}

impl RawHint {
    /// Map to an LSP [`InlayHint`], deferring the tooltip into `data` when `resolve_support` (else
    /// embedding it). The textEdit (when present) is always eager.
    fn into_lsp(self, mapper: &PositionMapper, resolve_support: bool) -> InlayHint {
        let position = mapper.byte_to_position(self.anchor);
        let text_edits = self.text_edit.map(|(span, new_text)| {
            vec![TextEdit {
                range: mapper.span_to_range(span),
                new_text,
            }]
        });
        // Tooltip: eager unless the client resolves lazily, in which case stash it in `data`.
        let (tooltip, data) = match (self.tooltip, resolve_support) {
            (Some(t), true) => (None, serde_json::to_value(ResolveData { tooltip: t }).ok()),
            (Some(t), false) => (Some(InlayHintTooltip::String(t)), None),
            (None, _) => (None, None),
        };
        InlayHint {
            position,
            label: InlayHintLabel::String(self.label),
            kind: Some(self.kind),
            text_edits,
            tooltip,
            padding_left: None,
            padding_right: None,
            data,
        }
    }
}

// ===================================================================================================
// TYPE hints — inferred `var x := …` + inferred `for` loop variables.
// ===================================================================================================

/// Collect TYPE hints: a `var x := …` declaration (walrus inference, no explicit annotation) and an
/// inferred `for` loop variable. The resolved type is read from the analyzer's type table — the
/// `Variable` node carries the var's type; the loop-variable IDENTIFIER node carries the for-var's.
fn collect_type_hints(
    state: &ServerState,
    tree: &ParseTree,
    analysis: &AnalysisResult,
    text: &str,
    out: &mut Vec<RawHint>,
) {
    for id in tree.iter_ids() {
        match &tree.get(id).kind {
            // `var x := <expr>` — the walrus form. Skip a plain `var x = expr` (untyped, the user
            // chose no type) and an explicit `var x: T = expr` (already annotated). The resolved
            // type is pinned on the Variable node itself.
            NodeKind::Variable(v) => {
                if !v.infer_datatype || v.datatype_specifier.is_some() {
                    continue;
                }
                let (Some(ident_id), Some(init_id)) = (v.identifier, v.initializer) else {
                    continue;
                };
                let dt = analysis.types.get(id);
                let Some(label_ty) = hintable_type_label(state, tree, dt) else {
                    continue;
                };
                let ident_end = tree.get(ident_id).span.end;
                let init_start = tree.get(init_id).span.start;
                // The edit NEUTRALIZES `:=`: replace the operator gap (which holds ` := `) with
                // `: <Type> = `, so `var x := 5.0` becomes `var x: <Type> = 5.0`. The replace span
                // must END at the operator's trailing whitespace — NOT at the initializer node's
                // start: a PARENTHESIZED initializer (`var z := (1 + 2)`) is transparent in the AST,
                // so its node span begins INSIDE the parens (at `1`), and replacing up to it would
                // eat the `(` and orphan the `)` (a silent corruption — baseline-clean → syntax
                // error). `walrus_replace_end` finds the byte just past `:=` + its trailing spaces,
                // which for every non-paren initializer equals `init_start` (so the edit is
                // byte-identical to before) and for a paren initializer stops at the `(`.
                let replace_end = walrus_replace_end(text, ident_end, init_start);
                // The edit is attached ONLY when the type yields a source-valid annotation (derived
                // from the DataType, not the display label — so an unnamed-script basename like
                // `a.gd` shows the label but carries NO corrupting edit).
                let text_edit = annotation_type(state, dt).map(|ty| {
                    (
                        ByteSpan {
                            start: ident_end,
                            end: replace_end,
                        },
                        format!(": {ty} = "),
                    )
                });
                out.push(RawHint {
                    anchor: ident_end,
                    label: format!(": {label_ty}"),
                    kind: InlayHintKind::TYPE,
                    tooltip: Some(type_tooltip(&label_ty)),
                    text_edit,
                });
            }
            // `for item in <expr>:` — an inferred loop variable (no explicit `for item: T in …`).
            // The element type is pinned on the loop-variable IDENTIFIER node.
            NodeKind::For(f) => {
                if f.datatype_specifier.is_some() {
                    continue;
                }
                let Some(var_id) = f.variable else {
                    continue;
                };
                let dt = analysis.types.get(var_id);
                let Some(label_ty) = hintable_type_label(state, tree, dt) else {
                    continue;
                };
                let var_end = tree.get(var_id).span.end;
                // A for-var annotation is a plain insert: `for item in …` → `for item: <Type> in …`.
                // Edit attached only for a source-valid annotation (see the `var` arm).
                let text_edit = annotation_type(state, dt).map(|ty| {
                    (
                        ByteSpan {
                            start: var_end,
                            end: var_end,
                        },
                        format!(": {ty}"),
                    )
                });
                out.push(RawHint {
                    anchor: var_end,
                    label: format!(": {label_ty}"),
                    kind: InlayHintKind::TYPE,
                    tooltip: Some(type_tooltip(&label_ty)),
                    text_edit,
                });
            }
            _ => {}
        }
    }
}

/// The END byte of the `var x := …` operator-neutralizing replace span: the first non-whitespace
/// byte AFTER the `:=` operator, clamped to `init_start`.
///
/// The `:=` walrus edit replaces `[ident_end, end)` with `": <T> = "`. The end must NOT be the
/// initializer node's `start`: a parenthesized initializer (`var z := (1 + 2)`) is transparent in
/// the AST — its node span begins at `1`, INSIDE the parens — so replacing up to `init_start` would
/// swallow the `(` and orphan the `)`, turning a clean file into a syntax error. Instead, find the
/// operator (the `:=` always present in this — the `infer_datatype` — form) and skip its trailing
/// ASCII whitespace; the result is the FIRST initializer byte as WRITTEN (the `(` for a paren form,
/// the literal/bracket/call/identifier otherwise). For every non-paren initializer this equals
/// `init_start` (the node already starts at its first written byte), so the edit is byte-identical
/// to the pre-fix behavior; only the paren form is corrected.
///
/// Defensive: if `:=` can't be located in the gap (an unexpected shape — this is only called for an
/// `infer_datatype` variable, where the operator is present by construction), falls back to
/// `init_start` (the prior behavior), never producing an out-of-order or overrun span.
fn walrus_replace_end(text: &str, ident_end: usize, init_start: usize) -> usize {
    // The gap between the identifier end and the initializer node start — holds ` := ` (plus any
    // surrounding whitespace / a line continuation). `init_start >= ident_end` always (node order).
    let gap = text.get(ident_end..init_start).unwrap_or("");
    let Some(op_rel) = gap.find(":=") else {
        return init_start;
    };
    // One past the `:=`, then skip ASCII whitespace (spaces/tabs/newlines from a `\` continuation)
    // to the first written initializer byte. `bytes()` indexing is safe: ASCII whitespace bytes are
    // never a UTF-8 continuation byte, so we always stop on a char boundary (or at `init_start`).
    let mut end = ident_end + op_rel + 2;
    let bytes = text.as_bytes();
    while end < init_start && bytes[end].is_ascii_whitespace() {
        end += 1;
    }
    end
}

/// The label for a resolved type IFF it is worth hinting — `None` for a type that should produce no
/// hint at all. Suppresses `Variant` (dynamic — a `: Variant` hint is pure noise the textEdit-clean
/// test wouldn't catch), `Resolving`/`Unresolved` (no known type), and meta types (a type *value*,
/// e.g. the class itself). Everything concrete (`Builtin`/`Native`/`Script`/`Enum`) renders via
/// [`crate::handlers::human_type_label`] — identical to hover. (`Class` never reaches a finished
/// `AnalysisResult` — the analyzer rewrites it to `Script` — but the arm is harmless if it ever did.)
///
/// This is the INFORMATIONAL label (always shown). Whether the type can also be *inserted* as a
/// source annotation is a SEPARATE, stricter question answered by [`annotation_type`] — so an
/// unnamed-script type renders its `a.gd` basename here as a hint, while carrying no (corrupting)
/// textEdit.
fn hintable_type_label(state: &ServerState, tree: &ParseTree, dt: &DataType) -> Option<String> {
    if !dt.is_set() || dt.kind == DtKind::Variant || dt.is_meta_type {
        return None;
    }
    let label = crate::handlers::human_type_label(state, tree, dt);
    // Defensive: never surface a diagnostic placeholder as a hint label. A `<Script #N>` / `<Class>`
    // / `<unresolved>` render can appear at the top level OR nested in a container element
    // (`Array[<Script #1>]` — `human_type_label`'s name substitution doesn't recurse into element
    // types). No valid GDScript type rendering contains `<`, so a `<` anywhere means a placeholder
    // leaked — drop the whole hint rather than show it.
    if label.is_empty() || label.contains('<') {
        return None;
    }
    Some(label)
}

/// The source-valid GDScript type ANNOTATION for `dt`, or `None` when the type can't be written as a
/// source annotation (so the `textEdit` is omitted — the label still shows). Derived from the
/// [`DataType`] per kind, NOT from the display label, because the display label is a human render
/// that, for an unnamed script, is the file BASENAME (`a.gd`) — which `: a.gd` would re-parse as
/// `type a` member `gd` and corrupt the file. The rules:
///   * `Builtin` → the builtin type name, incl. a parametrized container (`Array[int]`,
///     `Dictionary[String, int]`) when every element type is itself source-valid — taken from
///     `Display` (which renders builtins as their exact source names) and re-validated.
///   * `Native` → the engine class name (`Node2D`).
///   * `Script` → the script's global `class_name` ONLY (no inner-class path) — an unnamed script, or
///     an inner-class type, yields `None`.
///   * anything else (`Enum`/`Class`/unresolved) → `None` (conservative: an enum annotation's source
///     form is subtle; the label still shows, only the auto-insert is withheld).
///
/// Every returned string is finally re-checked by [`is_source_valid_type`] (belt-and-suspenders).
fn annotation_type(state: &ServerState, dt: &DataType) -> Option<String> {
    use gd_analyze::data_type::variant_type_name;
    use gd_analyze::resolver::builtin_type_from_name;
    use gd_analyze::VariantType;

    let candidate: String = match dt.kind {
        DtKind::Builtin => match dt.builtin_type {
            // A parametrized container is insertable ONLY when EVERY element type is itself
            // insertable — recurse on the element `DataType`s rather than trusting `Display` (whose
            // element render can be a source-INVALID basename, e.g. `Array[outer.gd.Inner]` for a
            // project-enum element, which would corrupt the file). An empty / Variant-element
            // container falls through to the bare `Array`/`Dictionary` name (always source-valid).
            VariantType::Array if !dt.container_element_types.is_empty() => {
                let elem = annotation_type(state, &dt.container_element_types[0])?;
                format!("Array[{elem}]")
            }
            VariantType::Dictionary if dt.container_element_types.len() >= 2 => {
                let k = annotation_type(state, &dt.container_element_types[0])?;
                let v = annotation_type(state, &dt.container_element_types[1])?;
                format!("Dictionary[{k}, {v}]")
            }
            // A scalar builtin (or an untyped container) — its name is its exact source form.
            bt => variant_type_name(bt).to_string(),
        },
        DtKind::Native if !dt.native_type.is_empty() => dt.native_type.clone(),
        DtKind::Script => {
            let sr = dt.script_type.as_ref()?;
            // Only a TOP-LEVEL script with a real `class_name` is insertable by its bare name; an
            // inner-class type (`sr.inner` non-empty) or an unnamed script is not (its display is a
            // basename / dotted-basename — not a source type). This gate is load-bearing: since #146,
            // `sr.inner` is reliably populated for an inner-class instance, so this withholds the
            // (would-be source-invalid) auto-insert edit rather than emitting the root `class_name`.
            if !sr.inner.is_empty() {
                return None;
            }
            let name = state
                .workspace
                .index
                .interface(sr.file)?
                .class_name
                .clone()?;
            // Withhold the auto-insert edit when the `class_name` collides with a builtin Variant
            // type (`Array`, `int`, …) or a native engine class (`Node`, …). Godot's analyzer rejects
            // such a `class_name` outright (`gdscript_analyzer.cpp`: "hides a built-in type" / "hides
            // a native class"), so under faithful indexing this name never reaches a usable script
            // type. But the shallow index stores the name as-written and only the M3 analyzer reports
            // the collision as a diagnostic — it is NOT stripped from the interface — so a colliding
            // name CAN surface here. Inserting `: Array = ` would then re-parse as the builtin/native
            // type, not the intended script, silently mis-annotating the file. Fail-closed.
            if builtin_type_from_name(&name).is_some()
                || state.workspace.native.class_named(&name).is_some()
            {
                return None;
            }
            name
        }
        _ => return None,
    };
    is_source_valid_type(&candidate).then_some(candidate)
}

/// Whether `s` is a valid GDScript type EXPRESSION: a dotted chain of identifiers
/// (`float`/`Node2D`/`Outer.Inner`/`Héro`) or a container form (`Array[int]`,
/// `Dictionary[String, int]`, nested). The final guard on [`annotation_type`]'s output — a defensive
/// re-check so a future per-kind derivation that produced a non-source string can't slip a corrupting
/// edit through.
///
/// Each dotted segment must be a valid GDScript identifier under the SAME Unicode XID rule the
/// tokenizer applies (`gd_syntax::lexer`'s `is_identifier_start`/`is_identifier_continue`): the first
/// char is `_` or `XID_Start`, the rest are `XID_Continue` (which already admits `_`). A Unicode
/// `class_name` (e.g. `Héro`) is therefore accepted, matching what the lexer would tokenize — an
/// ASCII-only rule here would withhold the auto-insert edit for a type the source can legally name.
fn is_source_valid_type(s: &str) -> bool {
    fn is_type_ident(s: &str) -> bool {
        // A dotted chain of GDScript identifiers: each non-empty segment starts with `_` or an
        // `XID_Start` char and continues with `XID_Continue` chars — identical to the tokenizer's
        // `is_identifier_start`/`is_identifier_continue` so a Unicode identifier the lexer accepts is
        // accepted here too.
        !s.is_empty()
            && s.split('.').all(|seg| {
                let mut chars = seg.chars();
                matches!(chars.next(), Some(c) if c == '_' || unicode_ident::is_xid_start(c))
                    && chars.all(unicode_ident::is_xid_continue)
            })
    }
    // A container form `Outer[Inner, …]`: the head is a type ident, the bracketed body is a
    // comma-separated list of type expressions (recursively validated).
    if let Some(open) = s.find('[') {
        if s.ends_with(']') {
            let head = &s[..open];
            let body = &s[open + 1..s.len() - 1];
            return is_type_ident(head)
                && body
                    .split(',')
                    .all(|part| is_source_valid_type(part.trim()));
        }
        return false;
    }
    is_type_ident(s)
}

/// The tooltip for a TYPE hint — a short prose line naming the inferred type. (Kept terse; the
/// hover request carries the rich docs.)
fn type_tooltip(label: &str) -> String {
    format!("Inferred type: {label}")
}

// ===================================================================================================
// PARAMETER hints — names before call arguments, off-by-default for single-argument calls.
// ===================================================================================================

/// Collect PARAMETER hints at every resolved multi-argument call site. Driven off the analyzer's
/// [`Binding::Call`] set (no re-resolution): correlate each `Call` AST node to the binding sharing
/// its span, resolve the callee's parameter NAMES (project `FunctionNode` / native `Method`), and
/// place `name:` before each argument. Single-argument calls are skipped (the spec's noise cut).
fn collect_parameter_hints(
    state: &ServerState,
    tree: &ParseTree,
    analysis: &AnalysisResult,
    text: &str,
    out: &mut Vec<RawHint>,
) {
    for id in tree.iter_ids() {
        let NodeKind::Call(call) = &tree.get(id).kind else {
            continue;
        };
        // Off-by-default for single-argument calls (deliberate). Also nothing to label for a 0-arg
        // call.
        if call.arguments.len() < 2 {
            continue;
        }
        let call_span = tree.get(id).span;
        // The analyzer's resolution for THIS call site — matched by the whole-call span (the binding
        // records `call_site = <call node>.span`). No binding ⇒ an unresolved/value callable ⇒ no
        // hints (never fabricate names).
        let Some(callee) = call_binding_target(analysis, call_span) else {
            continue;
        };
        let Some(param_names) = callee_parameter_names(state, &callee, &call.function_name, text)
        else {
            continue;
        };
        // Label each argument with its parameter's name. Guard `i < names.len()` so a vararg /
        // over-supplied call past the last declared parameter gets no (wrong) name.
        for (i, &arg_id) in call.arguments.iter().enumerate() {
            let Some(name) = param_names.get(i) else {
                break;
            };
            if name.is_empty() {
                continue;
            }
            let arg_start = tree.get(arg_id).span.start;
            out.push(RawHint {
                anchor: arg_start,
                label: format!("{name}:"),
                kind: InlayHintKind::PARAMETER,
                tooltip: Some(format!("Parameter: {name}")),
                // Parameter hints carry no textEdit (there is no annotation to insert — the name is
                // purely informational; Godot has no named-argument call syntax).
                text_edit: None,
            });
        }
    }
}

/// The [`CalleeTarget`] the analyzer resolved for the call whose whole-expression span is `span`, or
/// `None` when no `Binding::Call` matches (an unresolved / value-callable call) — then no parameter
/// hints are emitted.
fn call_binding_target(analysis: &AnalysisResult, span: ByteSpan) -> Option<CalleeTarget> {
    analysis.bindings().iter().find_map(|b| match b {
        Binding::Call {
            call_site, callee, ..
        } if *call_site == span => Some(callee.clone()),
        _ => None,
    })
}

/// The callee's parameter names, in order — from the analyzer's resolution. A project-script callee
/// is pinpointed by its declaring `FileId` + inner-class path (the call binding's `CalleeTarget`),
/// then the declaring file's parse tree supplies the `FunctionNode` parameter identifiers; a native
/// callee's names come from the [`gd_types::Method`] in the DB. `None` (no hints) when the callee is
/// `Unresolved` or the method can't be located (never a fabricated name).
fn callee_parameter_names(
    state: &ServerState,
    callee: &CalleeTarget,
    method: &str,
    text: &str,
) -> Option<Vec<String>> {
    match callee {
        CalleeTarget::Script { file, class_path } => {
            script_parameter_names(state, *file, class_path, method, text)
        }
        CalleeTarget::Native { class } => native_parameter_names(state, class, method),
        CalleeTarget::Unresolved => None,
    }
}

/// Parameter names of a native class method `method` (incl. inherited), from the DB. `None` when the
/// class/method is unknown or the member is not a method.
fn native_parameter_names(state: &ServerState, class: &str, method: &str) -> Option<Vec<String>> {
    let db = &state.workspace.native;
    let (_decl, member) = db.lookup_member(class, method)?;
    let gd_types::NativeMember::Method(m) = member else {
        return None;
    };
    Some(
        m.params
            .iter()
            .map(|p| db.name_of(p.name).to_string())
            .collect(),
    )
}

/// Parameter names of a project-script method `method` declared in `file` under `class_path` (the
/// analyzer's inner-class chain for this callee — empty = the file's root class). Built from the
/// declaring file's PARSE TREE (parameter identifiers), pinpointed by the OWNING class's interface
/// member `name_span` so an inner method that shares a name with a root method is never confused for
/// it (the analyzer carries `class_path` precisely to disambiguate — honoring it is what keeps this
/// from "lying" with the wrong signature). `None` when the file / owning class / member can't be
/// located (a miss is acceptable; a wrong name is not).
fn script_parameter_names(
    state: &ServerState,
    file: gd_project::FileId,
    class_path: &[String],
    method: &str,
    text: &str,
) -> Option<Vec<String>> {
    let root = state.workspace.index.interface(file)?;
    // Walk the inner-class chain to the OWNING class (each segment matches an inner class's
    // `class_name`). An unresolvable segment → `None` (never fall through to the root and risk a
    // same-named method's parameters).
    let mut owner = root;
    for seg in class_path {
        owner = owner
            .inner
            .iter()
            .find(|c| c.class_name.as_deref() == Some(seg.as_str()))?;
    }
    let decl = owner
        .members
        .iter()
        .find(|m| m.name == method && m.kind == gd_project::MemberKind::Func)?;
    let name_span = decl.name_span;

    let path = state.workspace.index.path(file)?;
    let decl_src = file_text(state, path);
    let src = decl_src.as_deref().unwrap_or(text);
    let parsed = gd_syntax::parse(src);
    let func = function_at_name_span(&parsed.tree, name_span, method)?;
    Some(
        func.parameters
            .iter()
            .map(|&pid| match &parsed.tree.get(pid).kind {
                NodeKind::Parameter(p) => p
                    .identifier
                    .map(|iid| ident_name(&parsed.tree, iid))
                    .unwrap_or_default(),
                _ => String::new(),
            })
            .collect(),
    )
}

/// The text of file `path`: the live VFS buffer if open, else the on-disk contents. `None` when the
/// file is neither open nor readable (the caller falls back to the requesting buffer's text). Mirrors
/// `signature_help::file_text`.
fn file_text(state: &ServerState, path: &camino::Utf8Path) -> Option<String> {
    let uri = crate::uri::path_to_file_uri(path)?;
    if let Some(d) = state.vfs.get(uri.as_str()) {
        return Some(d.text());
    }
    std::fs::read_to_string(path.as_std_path()).ok()
}

/// The `FunctionNode` whose identifier span equals `name_span` AND whose identifier text equals
/// `name` — the precise declaring function (so a coincidental span collision against a different,
/// re-parsed identifier is rejected). Mirrors `signature_help::function_at_name_span`.
fn function_at_name_span<'a>(
    tree: &'a ParseTree,
    name_span: ByteSpan,
    name: &str,
) -> Option<&'a gd_syntax::ast::FunctionNode> {
    tree.iter_ids().find_map(|id| {
        let NodeKind::Function(f) = &tree.get(id).kind else {
            return None;
        };
        let ident = f.identifier?;
        (tree.get(ident).span == name_span && ident_name(tree, ident) == name).then_some(f)
    })
}

/// The identifier text of an `Identifier` node, or `""` for any other kind.
fn ident_name(tree: &ParseTree, id: gd_syntax::ast::NodeId) -> String {
    match &tree.get(id).kind {
        NodeKind::Identifier(i) => i.name.clone(),
        _ => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_source_valid_type_accepts_idents_and_containers() {
        assert!(is_source_valid_type("float"));
        assert!(is_source_valid_type("Node2D"));
        assert!(is_source_valid_type("Outer.Inner"));
        assert!(is_source_valid_type("Array[int]"));
        assert!(is_source_valid_type("Dictionary[String, int]"));
        assert!(is_source_valid_type("Array[Array[int]]"));
        // A Unicode `class_name` (and a dotted chain through one) is source-valid under the lexer's
        // XID rule — the tokenizer would accept these identifiers, so the auto-insert edit must too.
        assert!(is_source_valid_type("Héro"));
        assert!(is_source_valid_type("Héro.E"));
        assert!(is_source_valid_type("Array[Héro]"));
    }

    #[test]
    fn is_source_valid_type_rejects_non_source_renders() {
        // The `<Script #N>` / `<Class>` / `<unresolved>` placeholders are not source-valid.
        assert!(!is_source_valid_type("<Script #3>"));
        assert!(!is_source_valid_type("<Class>"));
        // A leading-digit segment is rejected.
        assert!(!is_source_valid_type("2d_thing"));
        // A leading combining mark is rejected: it is `XID_Continue` but not `XID_Start`, so the
        // lexer would not start an identifier with it — the accept gate is drawn on the lexer's side.
        assert!(!is_source_valid_type("\u{0301}x"));
        // Empty / bracket-malformed strings are rejected.
        assert!(!is_source_valid_type(""));
        assert!(!is_source_valid_type("Array["));
        assert!(!is_source_valid_type("Array[]"));
    }

    /// The reason `annotation_type` gates on the `DataType` KIND, not on the display string: a
    /// file-basename render like `hero.gd` IS a syntactically valid two-identifier dotted name, so
    /// the string validator alone would wave through a corrupting `: hero.gd` edit. The kind-driven
    /// `annotation_type` (tested end-to-end in `tests/inlay_hint.rs`) is what blocks it.
    #[test]
    fn string_validator_alone_cannot_reject_a_basename() {
        assert!(
            is_source_valid_type("hero.gd"),
            "a basename parses as two idents — so the edit gate MUST be kind-driven, not string-driven"
        );
    }
}
