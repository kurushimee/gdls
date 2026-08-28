//! `textDocument/signatureHelp` (M8, issue #65 — Phase 5).
//!
//! Shows the active call's signature + which argument the cursor sits in, as the user types
//! arguments. This is `gd_server` glue, NOT a faithful frontend port: Godot's `gdscript_editor.cpp`
//! is the *semantic* reference (`_make_arguments_hint` for the `ret name(p: T = def, …)` label,
//! `_find_call_arguments` for the callee-shape dispatch), and idiomatic Rust is fine here.
//!
//! # Two reused layers
//!
//! - **Call-site detection** is the Phase-2 token layer
//!   ([`crate::completion_context::enclosing_open_bracket`] + [`crate::completion_context::arg_index_after`]):
//!   the nearest unclosed `(`, then the depth-0 comma count to the cursor = the argument index.
//!   Because the lexer makes each string literal a single token, a `)`/`,` inside a string argument
//!   never mis-resolves the active call or parameter (#65) — the same property completion relies on.
//! - **Callee resolution** dispatches like `_find_call_arguments`: a `base.method(` subscript callee
//!   resolves the base's type from the analysis (native / builtin / project-script method); a
//!   `ClassName.new(` resolves the class's `_init`; a bare `name(` is a `@GlobalScope` utility, a
//!   builtin constructor, or a method on the implicit-self class. One gdls addition on top of
//!   `_find_call_arguments`: a `Callable`-typed base whose name is pinned to a lambda literal shows
//!   THAT lambda's parameters for `.call(` / `.call_deferred(` — see [`lambda_call_sig`] for the
//!   fail-closed rule and for why `.bind(` is left alone (#193).
//!
//! # Where parameter NAMES + DEFAULTS come from (the load-bearing constraint)
//!
//! A **native** method / utility / constructor gets parameter names, types, and default literals
//! straight from the API dump ([`gd_types::Param`]). A **project-script** method's label is built
//! from the **declaring file's parse tree** ([`gd_syntax::ast::FunctionNode`]): the cross-file
//! [`gd_project::Interface`] carries parameter names and types, but *not* the default-value
//! expressions — only a `required_params` count — and Godot's `_make_arguments_hint` reads
//! `parameter->initializer` directly for exactly that reason. So a script callee is pinpointed by
//! its declaring `FileId` + `MemberDecl.name_span` (never a name-only walk, which could grab a
//! same-named function in an inner class — a "never lie" wrong-signature), the declaring file is
//! parsed (a one-shot `gd_syntax::parse` of just that file, not the whole project), and the default
//! is rendered as the initializer node's **source substring** (honest, cheap, and off the
//! reduce/resolve path).

use lsp_types::{
    Documentation, MarkupContent, MarkupKind, ParameterInformation, ParameterLabel, SignatureHelp,
    SignatureHelpParams, SignatureInformation,
};

use gd_analyze::{AnalysisResult, Binding, CalleeTarget, DataType, DtKind};
use gd_syntax::ast::{NodeKind, ParseTree};
use gd_syntax::token::{Token, TokenKind};

use crate::completion_context::{arg_index_after, enclosing_open_bracket};
use crate::docs::ProseFormat;
use crate::position::PositionMapper;
use crate::server::{ServerState, SignatureHelpCaps};
use crate::uri::CanonicalKey;

/// `textDocument/signatureHelp`: classify the enclosing call at the cursor, resolve its callee to
/// one or more signatures, and project them as a [`SignatureHelp`]. Mirrors the
/// [`crate::completion::completion`] preamble (VFS rope → cached parse → `tokenize` →
/// `analyze_if_gd` → [`PositionMapper`] → `position_to_byte`). Returns `None` — never an error —
/// for a missing buffer, a non-`.gd` file, a cursor in no call, or an unresolvable callee
/// ("never crash, never lie": no applicable signature is `null`, not a guess).
#[must_use]
pub fn signature_help(
    state: &mut ServerState,
    params: SignatureHelpParams,
) -> Option<SignatureHelp> {
    let tdp = params.text_document_position_params;
    let uri = tdp.text_document.uri.clone();
    let text = state.vfs.get(uri.as_str()).map(|d| d.text())?;
    let parsed = state.workspace.parse(&CanonicalKey::for_uri(&uri), &text);
    let (tokens, _lex_errors) = gd_syntax::tokenize(&text);
    // Analysis resolves a `base.method(` receiver's type (the subscript arm). `None` for a
    // non-`.gd` buffer; the bare-call / utility / constructor arms still resolve without it.
    let analyzed = crate::handlers::analyze_if_gd(state, &uri, &parsed.tree, &text);

    let mapper = {
        let doc = state.vfs.get(uri.as_str())?;
        PositionMapper::new(&doc.rope, state.encoding)
    };
    let byte = mapper.position_to_byte(tdp.position);

    // The enclosing unclosed call paren + the argument index. `None` ⇒ the cursor is in no call.
    let (open_idx, arg_index) = enclosing_call(&tokens, byte)?;

    // The requesting file's id (for the bare-self method chain walk); `None` for a buffer outside
    // the project index.
    let fid = crate::uri::uri_to_path(&uri).and_then(|p| state.workspace.index.file_id(&p));

    // Resolve the callee to its signature(s).
    let sigs = resolve_signatures(
        state,
        &parsed.tree,
        analyzed.as_deref(),
        &text,
        &tokens,
        open_idx,
        arg_index,
        fid,
    )?;
    if sigs.is_empty() {
        return None;
    }

    let caps = &state.caps.signature_help;
    // `activeSignature` stays stable across a retrigger: if the client echoed the prior
    // `SignatureHelp` (its `activeSignature` tracks the user's manual navigation) and that index is
    // still in range for the new signature set, keep it; otherwise default to 0.
    let active_signature = retained_active_signature(&params.context, sigs.len());

    // The top-level `activeParameter` is the active signature's parameter index, **clamped** to that
    // signature's parameter count (so a vararg call past its last declared argument still highlights
    // the vararg slot, and an over-supplied fixed call highlights the last parameter rather than an
    // out-of-range index the client would silently drop to 0). It must match the per-signature value
    // a 3.16 client sees — capability gating changes richness, never which parameter is active.
    let top_active = sigs[active_signature as usize].active_parameter(arg_index);

    let signatures = sigs
        .into_iter()
        .map(|sig| sig.into_lsp(arg_index, caps))
        .collect();

    Some(SignatureHelp {
        signatures,
        active_signature: Some(active_signature),
        // Carried per-signature (gated on `activeParameterSupport`) AND at the top level so a
        // pre-3.16 client still highlights the same parameter.
        active_parameter: Some(top_active),
    })
}

/// The enclosing unclosed `(` call paren and the 0-based argument index the cursor sits in, or
/// `None` when the cursor is not inside a `(`-delimited call. Only `(` openers count — a `[`/`{`
/// enclosing bracket is a subscript / collection literal, never a call. The anchor for the backward
/// scan is the last token ending at or before the cursor (an unanchored cursor is never in a call).
fn enclosing_call(tokens: &[Token], byte: usize) -> Option<(usize, usize)> {
    let anchor = anchor_index(tokens, byte);
    match enclosing_open_bracket(tokens, anchor)? {
        (open_idx, TokenKind::ParenthesisOpen) => {
            Some((open_idx, arg_index_after(tokens, open_idx, byte)))
        }
        _ => None,
    }
}

/// Index of the anchor token: the last token whose `span.end <= byte`, skipping layout / error
/// tokens (the standalone lexer emits `Newline`/`Indent`/`Dedent`/`Eof` inside brackets, and an
/// `Error` marker often shares a span with a real token). Mirrors `completion_context`'s private
/// `anchor_index`; re-derived here so the call-site scan has the same start point completion uses.
fn anchor_index(tokens: &[Token], byte: usize) -> Option<usize> {
    tokens
        .iter()
        .enumerate()
        .rev()
        .find(|(_, t)| !is_skippable(t.kind) && t.span.end <= byte)
        .map(|(i, _)| i)
}

/// The non-layout / non-error token immediately before index `i`, if any — the callee token sits
/// just before its `(`.
fn prev_meaningful(tokens: &[Token], i: usize) -> Option<usize> {
    tokens[..i]
        .iter()
        .enumerate()
        .rev()
        .find(|(_, t)| !is_skippable(t.kind))
        .map(|(j, _)| j)
}

/// Tokens the anchor / neighbor scan treats as "not really there": the standalone lexer's layout
/// tokens plus the `Error` diagnostic marker (see `completion_context::is_anchor_skippable`).
fn is_skippable(kind: TokenKind) -> bool {
    matches!(
        kind,
        TokenKind::Newline
            | TokenKind::Indent
            | TokenKind::Dedent
            | TokenKind::Eof
            | TokenKind::Error
    )
}

// ===================================================================================================
// Callee resolution — the `_find_call_arguments` dispatch, idiomatic-Rust.
// ===================================================================================================

/// Resolve the callee whose `(` is at `open_idx` to its signature(s). Returns `None` when the token
/// before the `(` is not a callee (a grouping paren, a `func` parameter list, an annotation) — those
/// are not calls. Dispatch order mirrors Godot's `_find_call_arguments`:
///
/// 1. `base.method(` (subscript callee) → the method on `base`'s resolved type (native / builtin /
///    project script), or — when the member name is `new` and `base` is a type — the class `_init`.
/// 2. a bare `name(` → a `@GlobalScope` utility, else a builtin **constructor** (`Vector2(`), else a
///    method on the implicit-self class (own / inherited project method, else the native root).
#[allow(clippy::too_many_arguments)] // the resolved call-site (tokens + tree + analysis + file id)
fn resolve_signatures(
    state: &ServerState,
    tree: &ParseTree,
    analyzed: Option<&AnalysisResult>,
    text: &str,
    tokens: &[Token],
    open_idx: usize,
    arg_index: usize,
    fid: Option<gd_project::FileId>,
) -> Option<Vec<Sig>> {
    let callee_idx = prev_meaningful(tokens, open_idx)?;
    let callee = &tokens[callee_idx];

    // A `func <name>(` parameter list and an `@annotation(` are not calls.
    if callee.kind == TokenKind::Annotation {
        return None;
    }
    let is_func_params = callee.kind == TokenKind::Identifier
        && prev_meaningful(tokens, callee_idx).is_some_and(|p| tokens[p].kind == TokenKind::Func);
    if is_func_params {
        return None;
    }

    // Is the callee a `base.method` attribute access, or a bare name? The token before a member
    // callee is a `.`; resolve the member name + the base expression either way.
    let dot_before =
        prev_meaningful(tokens, callee_idx).filter(|&p| tokens[p].kind == TokenKind::Period);

    let name = callee_token_name(callee)?;

    if let Some(dot_idx) = dot_before {
        // `Type.new(` — a constructor. Resolve by the base TOKEN name (a type reference, which the
        // analyzer pins as a meta type that the value-typed path below won't see as `is_set()`).
        // Tried first so `Hero.new(` / `Node.new(` resolve deterministically to the class `_init`.
        if name == "new" {
            if let Some(base_name) = base_token_name_before(tokens, dot_idx) {
                if let Some(sig) = resolve_typed_new(state, text, &base_name) {
                    return Some(sig);
                }
            }
        }
        return resolve_attribute_call(
            state, tree, analyzed, text, tokens, dot_idx, open_idx, &name,
        );
    }
    resolve_bare_call(state, text, fid, &name, arg_index)
}

/// The identifier text of the simple-name token immediately before the dot at `dot_idx` (the base
/// in `Base.method(`), or `None` when it isn't a plain name (a `)`/`]` chained base, etc.).
fn base_token_name_before(tokens: &[Token], dot_idx: usize) -> Option<String> {
    let base_idx = prev_meaningful(tokens, dot_idx)?;
    let base = &tokens[base_idx];
    (base.kind == TokenKind::Identifier || base.kind.is_identifier())
        .then(|| base.source.to_string())
}

/// The identifier text of a callee token, when it is a simple name (identifier or contextual
/// keyword used as a name). `None` for a `)`/`]` callee (a call on a call/subscript result — gdls
/// can't name a signature there) or `super`.
fn callee_token_name(callee: &Token) -> Option<String> {
    (callee.kind == TokenKind::Identifier || callee.kind.is_identifier())
        .then(|| callee.source.to_string())
}

/// `base.method(` — resolve `base`'s type from the analysis and look up `method` on it. A `new` on a
/// type base routes to the class `_init` (the constructor). The base expression is the typed node
/// whose span ends at the dot (`smallest_typed_ending_at`, the completion ATTRIBUTE convention).
#[allow(clippy::too_many_arguments)] // the resolved call-site (tree + tokens + analysis + dot/open idx)
fn resolve_attribute_call(
    state: &ServerState,
    tree: &ParseTree,
    analyzed: Option<&AnalysisResult>,
    text: &str,
    tokens: &[Token],
    dot_idx: usize,
    open_idx: usize,
    method: &str,
) -> Option<Vec<Sig>> {
    let analyzed = analyzed?;
    let dot_start = tokens[dot_idx].span.start;
    let base_dt = smallest_typed_ending_at(tree, analyzed, dot_start)?;

    match base_dt.kind {
        DtKind::Native if !base_dt.native_type.is_empty() => {
            native_method_sig(state, &base_dt.native_type, method)
        }
        DtKind::Builtin => {
            let bt = gd_analyze::data_type::variant_type_name(base_dt.builtin_type);
            // A lambda-valued name's `.call(` shows the LAMBDA's parameters (#193); the native
            // `Callable.call(...)` vararg signature it otherwise falls back to says nothing about
            // what the call actually takes.
            if bt == "Callable" {
                if let Some(sig) = lambda_call_sig(tree, text, tokens, dot_idx, method) {
                    return Some(sig);
                }
            }
            builtin_method_sig(state, bt, method)
        }
        DtKind::Script => {
            let sr = base_dt.script_type.as_ref()?;
            if method == "new" {
                // `ScriptInstance.new(` is unusual but valid; resolve the chain's `_init`.
                return script_init_sig(state, text, sr.file);
            }
            // Prefer the analyzer's resolved callee for THIS call: it carries the inner-class
            // `class_path` precisely, which the base VALUE's `ScriptRef` does not (an inner-class
            // instance is stored on the value node as the root class — #113). Fall back to the base
            // value's own (file, inner) when no Script call binding was recorded for this paren.
            if let Some(CalleeTarget::Script { file, class_path }) =
                call_target_at_open_paren(analyzed, tokens[open_idx].span.start)
            {
                if let Some(sig) = script_method_sig(state, text, file, &class_path, method) {
                    return Some(sig);
                }
            }
            script_method_sig(state, text, sr.file, &sr.inner, method)
        }
        // A `Class.new(` where the base is a *meta* type the analyzer left as a script/native ref is
        // handled by the meta-base name path in `resolve_bare_call`; nothing else resolves here.
        _ => None,
    }
}

/// The [`CalleeTarget`] the analyzer resolved for the call whose `(` is at `open_paren_byte`. Unlike
/// inlayHint's exact whole-span match, signatureHelp fires MID-call — often an incomplete call whose
/// recorded `Binding::Call.call_site` ends right AT the `(` — so match the INNERMOST call binding that
/// BRACKETS the open paren (`call_site.start < open_paren_byte <= call_site.end`). This carries the
/// inner-class `class_path` that the base value's `ScriptRef` does not (#113); `None` when the call
/// isn't analyzer-resolved (the caller then falls back to the base value's own type).
fn call_target_at_open_paren(
    analysis: &AnalysisResult,
    open_paren_byte: usize,
) -> Option<CalleeTarget> {
    analysis
        .bindings()
        .iter()
        .filter_map(|b| match b {
            Binding::Call {
                call_site, callee, ..
            } if call_site.start < open_paren_byte && open_paren_byte <= call_site.end => {
                Some((call_site.start, callee))
            }
            _ => None,
        })
        .max_by_key(|(start, _)| *start)
        .map(|(_, callee)| callee.clone())
}

// ===================================================================================================
// Lambda callees (#193).
// ===================================================================================================

/// `f.call(` / `f.call_deferred(` where `f` names a lambda — the signature of the LAMBDA the call
/// forwards its arguments to, rendered as `ret call(p: T = def, …)` like any other callee.
///
/// The originating lambda is pinned **syntactically**, in the requesting file only. Godot's analyzer
/// types every lambda as a bare `Callable` carrying no method info (`gdscript_analyzer.cpp:4683`,
/// `reduce_lambda`) and gdls' port is faithful to that, so the base value's [`DataType`] cannot hold
/// the lambda's identity — the same division of labour as Godot's own editor layer, which guesses an
/// expression's shape in `gdscript_editor.cpp` rather than in the analyzer.
///
/// Fail-closed ("never lie"): the base must be a plain name whose binding at the call site is a `var`
/// initialized with a lambda literal, and which no assignment ever rebinds — see [`lambda_bound_to`]
/// for how the binding is pinned. Every refusal falls back to the honest native `Callable`
/// signature: a parameter list belonging to some OTHER lambda is exactly the lie to avoid.
///
/// `bind` is deliberately NOT handled: `Callable.bind` appends its arguments to the END of the
/// lambda's parameter list, so which parameter the cursor sits in depends on how many arguments the
/// user will eventually bind — unknowable mid-typing, and a wrong `activeParameter` is a lie. Same
/// for `callv`/`bindv`, whose single argument is an `Array`, not the lambda's parameter list.
fn lambda_call_sig(
    tree: &ParseTree,
    text: &str,
    tokens: &[Token],
    dot_idx: usize,
    method: &str,
) -> Option<Vec<Sig>> {
    if !matches!(method, "call" | "call_deferred") {
        return None;
    }
    let base_idx = prev_meaningful(tokens, dot_idx)?;
    let base = base_token_name_before(tokens, dot_idx)?;
    let func = lambda_bound_to(tree, &base, tokens[base_idx].span.start)?;
    Some(vec![Sig::from_lambda(tree, text, method, func)])
}

/// The lambda `name` holds at `base_byte`, under the fail-closed rule documented on
/// [`lambda_call_sig`]. A LOCAL name resolves through the tree's scope-correct binding resolver
/// (`ParseTree::resolve_local_binding_at`, the primitive the rename firewall trusts), so two
/// functions each declaring their own `var f := func …` each get their own lambda; a name with no
/// local binding in scope falls back to a class-level `var`, which must be the file's only member of
/// that name. Either way a name the code REBINDS by assignment is refused: gdls does not track which
/// assignment reaches the cursor, so the declared lambda may not be the one being called.
fn lambda_bound_to<'a>(
    tree: &'a ParseTree,
    name: &str,
    base_byte: usize,
) -> Option<&'a gd_syntax::ast::FunctionNode> {
    let decl_ident = match tree.resolve_local_binding_at(base_byte, name) {
        Some(decl) => {
            // Binding-correct: only THIS binding's own occurrences count as rebinds; a same-named
            // local in a sibling block is a different symbol and is excluded by re-resolution.
            let scope = crate::handlers::enclosing_function_span(tree, tree.get(decl).span.start)?;
            let occurrences = tree.local_binding_occurrences(decl, scope);
            if occurrences
                .iter()
                .any(|&span| is_assignment_target(tree, span))
            {
                return None;
            }
            decl
        }
        None => class_level_var_ident(tree, name)?,
    };
    let var = variable_declaring(tree, decl_ident)?;
    let NodeKind::Lambda(l) = &tree.get(var.initializer?).kind else {
        return None;
    };
    match &tree.get(l.function?).kind {
        NodeKind::Function(f) => Some(f),
        _ => None,
    }
}

/// The declaration identifier of a class-level `var name` — the file's ONLY member of that name,
/// declared outside every function (a `var` inside a function is a local, which the caller resolved
/// first). `None` when the name isn't a unique class-level `var`, or when any assignment in the file
/// rebinds it (`name = …` / `self.name = …` / `other.name = …`, all conservatively refused: a member
/// has no scope narrower than the file to bound the scan).
fn class_level_var_ident(tree: &ParseTree, name: &str) -> Option<gd_syntax::ast::NodeId> {
    let mut found = None;
    for id in tree.iter_ids() {
        match &tree.get(id).kind {
            NodeKind::Variable(v) => {
                let Some(ident) = v.identifier else { continue };
                if ident_text(tree, ident) != name {
                    continue;
                }
                if crate::handlers::enclosing_function_span(tree, tree.get(id).span.start).is_some()
                {
                    return None; // a same-named local elsewhere — ambiguous, refuse
                }
                if found.is_some() {
                    return None; // declared twice at class level (in an inner class, say)
                }
                found = Some(ident);
            }
            NodeKind::Assignment(a) if assignee_name(tree, a.assignee).as_deref() == Some(name) => {
                return None
            }
            _ => {}
        }
    }
    found
}

/// The `VariableNode` whose declaration identifier is `ident` — the `var` a resolved binding was
/// declared by. `None` for a binding introduced by anything else (a parameter, a `for` variable, a
/// `match` pattern), none of which can hold a lambda literal.
fn variable_declaring(
    tree: &ParseTree,
    ident: gd_syntax::ast::NodeId,
) -> Option<&gd_syntax::ast::VariableNode> {
    tree.iter_ids().find_map(|id| match &tree.get(id).kind {
        NodeKind::Variable(v) if v.identifier == Some(ident) => Some(v),
        _ => None,
    })
}

/// True when the identifier at `span` is the ASSIGNEE of an assignment (`name = …`) — the site that
/// rebinds a name to a value its declaration never saw.
fn is_assignment_target(tree: &ParseTree, span: gd_syntax::ByteSpan) -> bool {
    tree.iter_ids().any(|id| match &tree.get(id).kind {
        NodeKind::Assignment(a) => a.assignee.is_some_and(|aid| tree.get(aid).span == span),
        _ => false,
    })
}

/// The bare name an assignment's assignee writes to: the identifier for a plain write, the attribute
/// for a `self.name` / `obj.name` write, `None` for an index write or a missing assignee.
fn assignee_name(tree: &ParseTree, assignee: Option<gd_syntax::ast::NodeId>) -> Option<String> {
    match &tree.get(assignee?).kind {
        NodeKind::Identifier(i) => Some(i.name.clone()),
        NodeKind::Subscript(sub) => match sub.access {
            Some(gd_syntax::ast::SubscriptAccess::Attribute(Some(attr))) => {
                Some(ident_text(tree, attr))
            }
            _ => None,
        },
        _ => None,
    }
}

/// A bare `name(` — a `@GlobalScope` utility, a builtin constructor (`Vector2(`), or a method on the
/// implicit-self class. (The `Type.new(` constructor form is an *attribute* callee handled earlier
/// in [`resolve_signatures`] via [`resolve_typed_new`], so it never reaches here.)
fn resolve_bare_call(
    state: &ServerState,
    text: &str,
    fid: Option<gd_project::FileId>,
    name: &str,
    arg_index: usize,
) -> Option<Vec<Sig>> {
    // (a) `@GlobalScope` / GDScript utility (`print`, `abs`, `randi`, …).
    if let Some(sig) = utility_sig(state, name) {
        return Some(sig);
    }
    // (b) A builtin type name used as a constructor (`Vector2(`, `Color(`, `Callable(`).
    if let Some(sigs) = builtin_constructor_sigs(state, name, arg_index) {
        return Some(sigs);
    }
    // (c) A method on the implicit-self class (own / inherited project method, else the native
    // root the chain bottoms out in).
    resolve_self_method(state, text, fid, name)
}

/// Per-overload signatures for a bare builtin-type callee (`Vector2(`, `Color(`, `Callable(`) —
/// #257. `None` when `name` isn't a builtin type, so the caller falls through to the self-method
/// arm.
///
/// **A deliberate deviation from Godot, not an oversight.** Godot's own language server returns
/// `null` here (verified against the headless 4.6.3 oracle at every argument position): its
/// constructor-overload arghints live in the COMPLETION path (`gdscript_editor.cpp:3411-3427`),
/// because that is where the Godot editor's call-hint popup is fed from. #194 ported that surface
/// faithfully and it stays exactly as it was. But a generic client renders parameter hints from
/// `signatureHelp` and nowhere else, and `Vector2(` / `Color(` are among the most-typed calls in
/// the language — so under #30 ("generic LSP first; Godot-specific data additive, never instead")
/// the same dump data is served here too. Purely additive: no analyzer behaviour moves, and the
/// completion arghints are untouched.
///
/// Overload selection mirrors Godot's completion filter: skip every overload whose arity the
/// active argument index overruns (`arg_idx >= arguments.size()`, `gdscript_editor.cpp:3417`),
/// which drops the no-arg overload as soon as anything is typed. Where Godot then shows nothing,
/// this keeps the popup alive: if the filter empties the set — the user typed past every
/// overload's arity, an error state mid-edit — every overload is offered with the widest first, so
/// the hint degrades to "here is the closest thing" instead of vanishing.
fn builtin_constructor_sigs(state: &ServerState, name: &str, arg_index: usize) -> Option<Vec<Sig>> {
    let db = &state.workspace.native;
    let builtin = db.builtin_named(name)?;
    if builtin.constructors.is_empty() {
        return None;
    }
    let surviving: Vec<&gd_types::Constructor> = builtin
        .constructors
        .iter()
        .filter(|ctor| arg_index < ctor.params.len())
        .collect();
    let sigs: Vec<Sig> = if surviving.is_empty() {
        let mut all: Vec<&gd_types::Constructor> = builtin.constructors.iter().collect();
        // Widest first so `activeSignature` = 0 points at the closest match to what was typed.
        all.sort_by_key(|ctor| std::cmp::Reverse(ctor.params.len()));
        all.into_iter()
            .map(|ctor| Sig::from_builtin_constructor(db, name, ctor))
            .collect()
    } else {
        surviving
            .into_iter()
            .map(|ctor| Sig::from_builtin_constructor(db, name, ctor))
            .collect()
    };
    Some(sigs)
}

/// `Type.new(` — the constructor of a native class or a project `class_name`, resolved from the
/// **type name** before the dot (not a value type). Tried before the value-typed attribute path so
/// a `Hero.new(` resolves to `Hero`'s `_init` even though `Hero` (a meta type) carries no instance
/// members. `None` when the name isn't a known type.
fn resolve_typed_new(state: &ServerState, text: &str, base_name: &str) -> Option<Vec<Sig>> {
    // A native class → a constructor signature (the engine `_init` is not a dumped member, so the
    // truthful shape is `ClassName(...)`).
    if state.workspace.native.class_named(base_name).is_some() {
        return Some(vec![Sig::constructor(base_name)]);
    }
    // A project `class_name` → the declaring file's `_init` (or a no-arg constructor).
    if let Some(entry) = state.workspace.index.registry().get(base_name) {
        if let Some(fid) = state.workspace.index.file_id(&entry.path) {
            return script_init_sig(state, text, fid)
                .or_else(|| Some(vec![Sig::constructor(base_name)]));
        }
    }
    None
}

/// A method on the implicit-`self` class for a bare `name(`: walk the requesting file's `extends`
/// chain ([`gd_project::Index::extends_chain_files`], the hover/definition bare-call convention). A
/// project member (own or inherited script) shadows a native — its real `(params)` come from the
/// declaring file's tree; otherwise the native root's method from the DB. `None` without a known
/// requesting file id (a buffer outside the project index).
fn resolve_self_method(
    state: &ServerState,
    text: &str,
    fid: Option<gd_project::FileId>,
    name: &str,
) -> Option<Vec<Sig>> {
    let fid = fid?;
    let (chain, root) = state
        .workspace
        .index
        .extends_chain_files(fid, &state.workspace.native);
    // The nearest chain file declaring `name` as a function — its declaring tree carries the real
    // parameter names + defaults.
    for f in &chain {
        if let Some(iface) = state.workspace.index.interface(*f) {
            if iface
                .members
                .iter()
                .any(|m| m.name == name && m.kind == gd_project::MemberKind::Func)
            {
                return script_method_sig(state, text, *f, &[], name);
            }
        }
    }
    // Not a project member — the native root the chain bottoms out in.
    native_method_sig(state, &root?, name)
}

// ===================================================================================================
// Native signature sourcing — methods / utilities / constructors from the API dump.
// ===================================================================================================

/// The signature of a native class method (incl. inherited), from the DB. `None` when the class /
/// method is unknown, or the member is not a method (a property named the same isn't callable).
fn native_method_sig(state: &ServerState, class: &str, method: &str) -> Option<Vec<Sig>> {
    let db = &state.workspace.native;
    let (decl, member) = db.lookup_member(class, method)?;
    let declaring = db.name_of(decl.name);
    let gd_types::NativeMember::Method(m) = member else {
        return None;
    };
    Some(vec![Sig::from_native_method(db, declaring, method, m)])
}

/// The signature of a builtin-type method (`Vector2.lerp`, `Array.append`), from the DB.
fn builtin_method_sig(state: &ServerState, builtin: &str, method: &str) -> Option<Vec<Sig>> {
    let db = &state.workspace.native;
    let (_bt, member) = db.lookup_builtin_member(builtin, method)?;
    let gd_types::NativeMember::Method(m) = member else {
        return None;
    };
    Some(vec![Sig::from_native_method(db, builtin, method, m)])
}

/// A `@GlobalScope` / GDScript utility's signature (`print`, `abs`, …), from the DB.
fn utility_sig(state: &ServerState, name: &str) -> Option<Vec<Sig>> {
    let db = &state.workspace.native;
    let u = db.utility(name)?;
    Some(vec![Sig::from_utility(db, name, u)])
}

// ===================================================================================================
// Script signature sourcing — from the DECLARING file's parse tree (names + types + defaults).
// ===================================================================================================

/// The signature of a project-script method `name` declared in file `fid` under `class_path` (the
/// analyzer's inner-class chain for the callee — empty = the file's root class), built from that
/// file's **parse tree** so parameter names, written types, AND default-value expressions are real
/// (the cross-file [`gd_project::Interface`] has names + types but not defaults — see the module
/// docs). The `FunctionNode` is pinpointed by the OWNING class's interface member `name_span` (never
/// a name-only walk), so an inner method that name-collides with a root method is never confused for
/// it — mirroring inlayHint's `script_parameter_names`.
fn script_method_sig(
    state: &ServerState,
    text: &str,
    fid: gd_project::FileId,
    class_path: &[String],
    name: &str,
) -> Option<Vec<Sig>> {
    let root = state.workspace.index.interface(fid)?;
    // Walk the inner-class chain to the OWNING class (each segment matches an inner class's
    // `class_name`). An unresolvable segment → `None` (never fall through to the root and risk a
    // same-named method's signature).
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
        .find(|m| m.name == name && m.kind == gd_project::MemberKind::Func)?;
    let name_span = decl.name_span;
    // The method's `##` doc comment (BBCode), from the OWNING class's interface member — the parse
    // tree carries no doc, so it rides alongside the signature (#97). Empty description ⇒ no popup.
    let doc = decl
        .doc
        .as_ref()
        .map(|d| d.description.clone())
        .filter(|s| !s.is_empty());

    let path = state.workspace.index.path(fid)?;
    let decl_text = file_text(state, path);
    let decl_src = decl_text.as_deref().unwrap_or(text);
    // A one-shot parse of just the declaring file (not the cached `Workspace::parse`, which needs
    // `&mut` the analysis borrow already holds). Parameter names + defaults need the parse *tree*;
    // the index interface alone can't supply the default expressions.
    let parsed = gd_syntax::parse(decl_src);
    let func = function_at_name_span(&parsed.tree, name_span, name)?;
    Some(vec![Sig::from_function_node(
        &parsed.tree,
        decl_src,
        name,
        func,
        doc,
    )])
}

/// The `_init` constructor signature of the script class in file `fid` (or the head of its chain),
/// built from the declaring file's tree. Falls back to a no-arg `_init()` shape when the class
/// declares no explicit `_init`.
fn script_init_sig(state: &ServerState, text: &str, fid: gd_project::FileId) -> Option<Vec<Sig>> {
    // `_init` may be declared on the class itself or inherited; the head interface is the common
    // case (a constructor is rarely inherited as-is). Try the head file's `_init` first.
    if let Some(sig) = script_method_sig(state, text, fid, &[], "_init") {
        return Some(sig);
    }
    // No explicit `_init` — the implicit no-arg constructor.
    let class = state
        .workspace
        .index
        .interface(fid)
        .and_then(|i| i.class_name.clone())
        .unwrap_or_else(|| "_init".to_string());
    Some(vec![Sig::constructor(&class)])
}

/// The `FunctionNode` whose identifier span equals `name_span` AND whose identifier text equals
/// `name` — the precise declaring function, found by the name token (so an inner-class same-named
/// function is never confused for the outer one). The name re-check is cheap insurance against a
/// *coincidental* span collision: `name_span` comes from the cached interface, but the tree is a
/// fresh re-parse of the (possibly on-disk-newer) declaring file, so a span that now lands on a
/// different identifier must not be accepted (`MemberDecl::name_span`'s "validate against live
/// text" contract). `None` when no function matches (a defensively-empty `name_span`, or a stale
/// interface vs the live text).
fn function_at_name_span<'a>(
    tree: &'a ParseTree,
    name_span: gd_syntax::ByteSpan,
    name: &str,
) -> Option<&'a gd_syntax::ast::FunctionNode> {
    tree.iter_ids().find_map(|id| {
        let NodeKind::Function(f) = &tree.get(id).kind else {
            return None;
        };
        let ident = f.identifier?;
        (tree.get(ident).span == name_span && ident_text(tree, ident) == name).then_some(f)
    })
}

// ===================================================================================================
// Cross-file helpers.
// ===================================================================================================

/// The text of file `path`: the live VFS buffer if open, else the on-disk contents. `None` when the
/// file is neither open nor readable (the caller falls back to the requesting buffer's text).
fn file_text(state: &ServerState, path: &camino::Utf8Path) -> Option<String> {
    let uri = crate::uri::path_to_file_uri(path)?;
    if let Some(d) = state.vfs.get(uri.as_str()) {
        return Some(d.text());
    }
    std::fs::read_to_string(path.as_std_path()).ok()
}

/// The smallest-span node whose span **ends exactly** at `end` and carries a resolved type — the
/// base expression in `base.method(` (its span ends at the dot). Linear over the arena, like
/// completion's `smallest_typed_ending_at`; adequate for a one-shot request.
fn smallest_typed_ending_at<'a>(
    tree: &ParseTree,
    analyzed: &'a AnalysisResult,
    end: usize,
) -> Option<&'a DataType> {
    let mut best: Option<(gd_syntax::ast::NodeId, usize)> = None;
    for id in tree.iter_ids() {
        let span = tree.get(id).span;
        if span.end == end && analyzed.types.get(id).is_set() {
            let width = span.end - span.start;
            match best {
                Some((_, bw)) if width > bw => {}
                _ => best = Some((id, width)),
            }
        }
    }
    best.map(|(id, _)| analyzed.types.get(id))
}

// ===================================================================================================
// `Sig` — the source-agnostic signature, projected into LSP with caps gating.
// ===================================================================================================

/// One built signature: the full label, the half-open `[start, end)` byte ranges of each parameter
/// within that label, whether the call is variadic, and the optional BBCode documentation. Built by
/// the native-method / utility / function-node / constructor sources, then projected into
/// [`SignatureInformation`] by [`Sig::into_lsp`] under the client's capability gates.
struct Sig {
    label: String,
    /// `(start, end)` byte offsets of each parameter's slice within `label` — UTF-16 / UTF-8 aware
    /// conversion happens in `into_lsp` only when `labelOffsetSupport` is on. A trailing varargs
    /// pseudo-parameter (`...args: Array`) is included so the cursor past the last real argument
    /// still highlights it (which is also what makes `active_parameter`'s clamp-to-last correct for
    /// a variadic callee — no separate vararg flag is needed).
    params: Vec<(usize, usize)>,
    /// The callee's BBCode documentation (a native member/utility description, or a script `##`
    /// doc), rendered + format-gated in `into_lsp`. `None` when no doc source exists.
    doc: Option<String>,
}

impl Sig {
    /// Project into an LSP [`SignatureInformation`] for the active argument `arg_index`, applying
    /// the capability gates: `[start, end)` parameter offsets behind `labelOffsetSupport` (substring
    /// labels otherwise), a per-signature `activeParameter` behind `activeParameterSupport`, and the
    /// documentation flavor from `documentationFormat`.
    fn into_lsp(self, arg_index: usize, caps: &SignatureHelpCaps) -> SignatureInformation {
        let active = self.active_parameter(arg_index);
        let parameters = self.build_parameters(caps);
        let documentation = self
            .doc
            .as_deref()
            .map(|bb| render_doc(caps.documentation_format, bb));
        SignatureInformation {
            label: self.label,
            documentation,
            parameters: Some(parameters),
            // Per-signature activeParameter only when the client opted in; otherwise the top-level
            // `SignatureHelp.activeParameter` carries it (pre-3.16 shape).
            active_parameter: caps.active_parameter_support.then_some(active),
        }
    }

    /// The active-parameter index clamped into this signature's parameter count. A variadic callee
    /// past its last declared parameter clamps to the varargs slot (the last `params` entry); a
    /// fixed callee clamps to the last real parameter; a no-parameter callee is `0`.
    fn active_parameter(&self, arg_index: usize) -> u32 {
        if self.params.is_empty() {
            return 0;
        }
        let last = self.params.len() - 1;
        (arg_index.min(last)) as u32
    }

    /// The [`ParameterInformation`] list: `[start, end)` offsets when `labelOffsetSupport` is on,
    /// else the substring of the label (which LSP requires to be a literal substring — it is, since
    /// the offsets index `self.label`).
    fn build_parameters(&self, caps: &SignatureHelpCaps) -> Vec<ParameterInformation> {
        self.params
            .iter()
            .map(|&(start, end)| {
                let label = if caps.label_offset_support {
                    // LSP offsets are in UTF-16 code units by default; gdls signature labels are
                    // ASCII-dominant but a type/param name could be non-ASCII, so convert the byte
                    // offsets to UTF-16 units against the label.
                    let s = utf16_len(&self.label[..start]);
                    let e = utf16_len(&self.label[..end]);
                    ParameterLabel::LabelOffsets([s, e])
                } else {
                    ParameterLabel::Simple(self.label[start..end].to_string())
                };
                ParameterInformation {
                    label,
                    documentation: None,
                }
            })
            .collect()
    }

    /// Build a signature from a native method's `MethodInfo`-shaped data (Godot's
    /// `_make_arguments_hint(MethodInfo)`): `ret name(p0: T0, p1: T1 = def, …)`, a vararg appending
    /// `...args: Array`. `declaring` trims an enum scope in type rendering (the editor convention).
    fn from_native_method(
        db: &gd_types::NativeDb,
        declaring: &str,
        name: &str,
        m: &gd_types::Method,
    ) -> Sig {
        let ret = db.display_type(&m.return_type, Some(declaring));
        let mut b = LabelBuilder::new(format!("{ret} {name}("));
        let def_start = m.params.len().saturating_sub(default_count(&m.params));
        for (i, p) in m.params.iter().enumerate() {
            b.sep(i);
            let pname = db.name_of(p.name);
            let pty = db.display_type(&p.ty, Some(declaring));
            let mut frag = format!("{pname}: {pty}");
            if i >= def_start {
                if let Some(def) = &p.default_value {
                    frag.push_str(" = ");
                    frag.push_str(db.name_of(*def));
                }
            }
            b.param(&frag);
        }
        if m.is_vararg {
            b.vararg();
        }
        b.finish(native_method_doc(m))
    }

    /// Build a signature from a `@GlobalScope` / GDScript utility (`MethodInfo` shape, like a
    /// native method). The dump carries no per-utility description, so `doc` is `None`.
    fn from_utility(db: &gd_types::NativeDb, name: &str, u: &gd_types::UtilityFn) -> Sig {
        let ret = db.display_type(&u.return_type, None);
        let mut b = LabelBuilder::new(format!("{ret} {name}("));
        let def_start = u.params.len().saturating_sub(default_count(&u.params));
        for (i, p) in u.params.iter().enumerate() {
            b.sep(i);
            let pname = db.name_of(p.name);
            let pty = db.display_type(&p.ty, None);
            let mut frag = format!("{pname}: {pty}");
            if i >= def_start {
                if let Some(def) = &p.default_value {
                    frag.push_str(" = ");
                    frag.push_str(db.name_of(*def));
                }
            }
            b.param(&frag);
        }
        if u.is_vararg {
            b.vararg();
        }
        b.finish(None)
    }

    /// Build a signature from one builtin-type constructor overload (#257). `Variant::get_constructor_list`
    /// sets `mi.name = mi.return_val.type = type`, so the faithful `_make_arguments_hint` label
    /// reads `Type Type(args)` — the same shape #194 renders on the completion arghint surface, so
    /// the two never disagree about an overload. Constructors carry no per-overload description in
    /// the dump, so `doc` is `None` (as with `from_utility`), and none is a vararg.
    fn from_builtin_constructor(
        db: &gd_types::NativeDb,
        type_name: &str,
        ctor: &gd_types::Constructor,
    ) -> Sig {
        let mut b = LabelBuilder::new(format!("{type_name} {type_name}("));
        for (i, p) in ctor.params.iter().enumerate() {
            b.sep(i);
            let pname = db.name_of(p.name);
            let pty = db.display_type(&p.ty, None);
            b.param(&format!("{pname}: {pty}"));
        }
        b.finish(None)
    }

    /// Build a signature from a project-script `FunctionNode` (Godot's
    /// `_make_arguments_hint(FunctionNode)`): `ret name(p: T = def, …)` where the return type reads
    /// `void` for an absent / void return, each parameter's type is its written annotation (else
    /// `Variant`), and the default is the **initializer node's source substring**. `src` is the
    /// declaring file's text the tree was parsed from. `doc` is the method's `##` BBCode description
    /// (from the declaring interface's `MemberDecl`), rendered to the client's prose flavor at
    /// `finish` time — the parse tree alone carries no doc, so the caller supplies it.
    fn from_function_node(
        tree: &ParseTree,
        src: &str,
        name: &str,
        func: &gd_syntax::ast::FunctionNode,
        doc: Option<String>,
    ) -> Sig {
        let ret = return_type_label(tree, src, func.return_type);
        Sig::from_function_node_returning(tree, src, &ret, name, func, doc)
    }

    /// Build a lambda's signature under the callee name the call site used (`call` /
    /// `call_deferred`, see [`lambda_call_sig`]). An UNANNOTATED lambda return renders as `Variant`,
    /// not `_make_arguments_hint`'s `void` default: `Callable.call` yields whatever the lambda
    /// returns, so claiming `void` would be a lie about the call's value. A lambda carries no `##`
    /// doc, so there is no documentation to attach.
    fn from_lambda(
        tree: &ParseTree,
        src: &str,
        method: &str,
        func: &gd_syntax::ast::FunctionNode,
    ) -> Sig {
        let ret = match func.return_type {
            Some(_) => return_type_label(tree, src, func.return_type),
            None => "Variant".to_string(),
        };
        Sig::from_function_node_returning(tree, src, &ret, method, func, None)
    }

    /// The shared body of [`Sig::from_function_node`] / [`Sig::from_lambda`]: the `ret name(…)`
    /// label over `func`'s parameters, with the return label supplied by the caller.
    fn from_function_node_returning(
        tree: &ParseTree,
        src: &str,
        ret: &str,
        name: &str,
        func: &gd_syntax::ast::FunctionNode,
        doc: Option<String>,
    ) -> Sig {
        let mut b = LabelBuilder::new(format!("{ret} {name}("));
        for (i, &pid) in func.parameters.iter().enumerate() {
            b.sep(i);
            let NodeKind::Parameter(p) = &tree.get(pid).kind else {
                b.param("Variant");
                continue;
            };
            let pname = p
                .identifier
                .map(|id| ident_text(tree, id))
                .unwrap_or_default();
            let pty = type_label(tree, src, p.datatype_specifier);
            let mut frag = format!("{pname}: {pty}");
            if let Some(init) = p.initializer {
                frag.push_str(" = ");
                frag.push_str(node_source(tree, src, init));
            }
            b.param(&frag);
        }
        // A `func f(...args)` rest parameter (the variadic slot) is appended as a `...name: T`
        // pseudo-parameter; being in `params`, it makes `active_parameter` clamp to it past the
        // last declared argument (the variadic-call behavior).
        if let Some(rest) = func.rest_parameter {
            if let NodeKind::Parameter(p) = &tree.get(rest).kind {
                let pname = p
                    .identifier
                    .map(|id| ident_text(tree, id))
                    .unwrap_or_default();
                let pty = type_label(tree, src, p.datatype_specifier);
                b.sep_force();
                b.param(&format!("...{pname}: {pty}"));
            }
        }
        b.finish(doc)
    }

    /// A bare constructor signature `Name(...)` — used for engine-class / builtin construction and a
    /// scriptless `_init`, where gdls' data model carries no typed parameter list. One varargs-style
    /// "..." pseudo-parameter so the cursor highlights *something* while typing arguments, without
    /// fabricating named parameters (never lie).
    fn constructor(name: &str) -> Sig {
        let mut b = LabelBuilder::new(format!("{name}("));
        b.param("...");
        b.finish(None)
    }
}

/// Incrementally assembles a signature label while recording each parameter's `[start, end)` byte
/// span within it — so the parameter offsets and the label can never drift out of sync.
struct LabelBuilder {
    label: String,
    params: Vec<(usize, usize)>,
}

impl LabelBuilder {
    fn new(prefix: String) -> Self {
        LabelBuilder {
            label: prefix,
            params: Vec::new(),
        }
    }

    /// Append `", "` before every parameter after the first.
    fn sep(&mut self, i: usize) {
        if i > 0 {
            self.label.push_str(", ");
        }
    }

    /// Append `", "` unconditionally (the separator before a varargs / rest pseudo-parameter that
    /// follows real parameters). A no-op when the label has no parameters yet.
    fn sep_force(&mut self) {
        if !self.params.is_empty() {
            self.label.push_str(", ");
        }
    }

    /// Append one parameter fragment (`name: T` / `name: T = def` / `...`), recording its span.
    fn param(&mut self, frag: &str) {
        let start = self.label.len();
        self.label.push_str(frag);
        let end = self.label.len();
        self.params.push((start, end));
    }

    /// Append the varargs pseudo-parameter `...args: Array` (Godot's `MethodInfo` vararg render),
    /// recording its span so the cursor past the last real argument highlights it.
    fn vararg(&mut self) {
        self.sep_force();
        self.param("...args: Array");
    }

    /// Close the label with `)` and produce the [`Sig`].
    fn finish(mut self, doc: Option<String>) -> Sig {
        self.label.push(')');
        Sig {
            label: self.label,
            params: self.params,
            doc,
        }
    }
}

// ===================================================================================================
// Small renderers.
// ===================================================================================================

/// The return-type label for a script function: `void` for an absent or `void` return annotation,
/// else the written type's name. Mirrors `_make_arguments_hint(FunctionNode)`'s `void`-default.
fn return_type_label(
    tree: &ParseTree,
    src: &str,
    return_type: Option<gd_syntax::ast::NodeId>,
) -> String {
    match return_type.map(|id| clean_type_text(node_source(tree, src, id))) {
        Some(t) if !t.is_empty() => t,
        _ => "void".to_string(),
    }
}

/// A parameter type-annotation's label: the written type's name, or `Variant` when there is no
/// annotation (Godot renders an un-hard-typed parameter as `Variant`).
fn type_label(tree: &ParseTree, src: &str, ty: Option<gd_syntax::ast::NodeId>) -> String {
    match ty.map(|id| clean_type_text(node_source(tree, src, id))) {
        Some(t) if !t.is_empty() => t,
        _ => "Variant".to_string(),
    }
}

/// Strip the leading annotation punctuation a `Type` node's source carries: the parser anchors a
/// parameter/variable type node at the preceding `:` and a return type at the preceding `->`, so the
/// node's source reads `": String"` / `"-> int"`. The label wants just the type name (`String` /
/// `int`), so drop a leading `->` or `:` and surrounding whitespace.
fn clean_type_text(s: &str) -> String {
    let t = s.trim();
    let t = t.strip_prefix("->").unwrap_or(t);
    let t = t.strip_prefix(':').unwrap_or(t);
    t.trim().to_string()
}

/// The source substring a node spans, clamped to the text bounds (never panics on a stale span).
fn node_source<'a>(tree: &ParseTree, src: &'a str, id: gd_syntax::ast::NodeId) -> &'a str {
    let span = tree.get(id).span;
    let start = span.start.min(src.len());
    let end = span.end.min(src.len());
    if start <= end {
        src.get(start..end).unwrap_or("")
    } else {
        ""
    }
}

/// The identifier text of an `Identifier` node, or `""` for any other kind.
fn ident_text(tree: &ParseTree, id: gd_syntax::ast::NodeId) -> String {
    match &tree.get(id).kind {
        NodeKind::Identifier(i) => i.name.clone(),
        _ => String::new(),
    }
}

/// The number of trailing parameters that carry a default value, in the dump's contiguous-trailing
/// model (defaults are always the last N parameters).
fn default_count(params: &[gd_types::Param]) -> usize {
    params.iter().filter(|p| p.default_value.is_some()).count()
}

/// A native method's BBCode description, or `None` when empty.
fn native_method_doc(m: &gd_types::Method) -> Option<String> {
    (!m.description.is_empty()).then(|| m.description.clone())
}

/// Render a BBCode doc string to the client's negotiated prose flavor as LSP [`Documentation`].
fn render_doc(format: ProseFormat, bbcode: &str) -> Documentation {
    let kind = match format {
        ProseFormat::Markdown => MarkupKind::Markdown,
        ProseFormat::PlainText => MarkupKind::PlainText,
    };
    Documentation::MarkupContent(MarkupContent {
        kind,
        value: crate::docs::bbcode_to(format, bbcode),
    })
}

/// The UTF-16 code-unit length of `s` — LSP's default offset unit for parameter label offsets.
fn utf16_len(s: &str) -> u32 {
    s.chars().map(char::len_utf16).sum::<usize>() as u32
}

/// Keep `activeSignature` stable across a retrigger. When the client echoes the prior
/// [`SignatureHelp`] in the request context (its `activeSignature` tracks the user's manual
/// up/down navigation) and that index is still valid for the new signature set, retain it;
/// otherwise default to the first signature. `count` is the new signature count (≥ 1).
fn retained_active_signature(
    context: &Option<lsp_types::SignatureHelpContext>,
    count: usize,
) -> u32 {
    let prior = context
        .as_ref()
        .filter(|c| c.is_retrigger)
        .and_then(|c| c.active_signature_help.as_ref())
        .and_then(|h| h.active_signature);
    match prior {
        Some(idx) if (idx as usize) < count => idx,
        _ => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// FIX 5 (review): `function_at_name_span` must reject a span that matches but whose identifier
    /// text differs from the requested name — the coincidental-collision path that could otherwise
    /// render a different function's signature under the requested name (a "never lie" hole).
    #[test]
    fn function_at_name_span_rejects_span_match_with_wrong_name() {
        let src = "func alpha(a: int):\n\tpass\nfunc bravo(b: int):\n\tpass\n";
        let tree = gd_syntax::parse(src).tree;
        let alpha_span = tree
            .iter_ids()
            .find_map(|id| {
                let NodeKind::Function(f) = &tree.get(id).kind else {
                    return None;
                };
                let ident = f.identifier?;
                (ident_text(&tree, ident) == "alpha").then(|| tree.get(ident).span)
            })
            .expect("alpha function present");
        // Span lands on alpha, but the requested name is bravo → the guard rejects it.
        assert!(function_at_name_span(&tree, alpha_span, "bravo").is_none());
        // Span and name both alpha → resolves.
        assert!(function_at_name_span(&tree, alpha_span, "alpha").is_some());
    }
}
