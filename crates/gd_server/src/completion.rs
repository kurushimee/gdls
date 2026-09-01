//! `textDocument/completion` + `completionItem/resolve` (M8, issue #64 — Phase 3).
//!
//! This is the LSP *handler* layer over the Phase 2 context engine
//! ([`crate::completion_context::classify`]) and the Phase 1 enumeration APIs
//! ([`gd_analyze::enumerate`]). It is `gd_server` glue, NOT a faithful frontend port: Godot's
//! `gdscript_editor.cpp` is the *semantic* reference ("what to suggest where"), and idiomatic Rust
//! is fine here (`THINKING.md`). This phase renders only the two highest-value contexts —
//! **IDENTIFIER** (the bare-name in-scope set) and **ATTRIBUTE** (`expr.<cursor>` member access);
//! every other [`CompletionKind`] returns an empty (but well-formed) list, deferred to Phase 4.
//!
//! # Generic-LSP contract (anti-catalog W18, `docs/09 §3`)
//!
//! - The response is always a [`CompletionList`], **never a bare array** — the type is fixed at the
//!   handler signature so the array mistake is unrepresentable.
//! - Every item carries a single-line [`lsp_types::TextEdit`] over the typed-prefix span (so the
//!   client replaces exactly the word under the cursor), a **fixed-width** `sort_text` (gopls
//!   `%05d` style, so a lexicographic client sort equals the rank order), a `filter_text` aligned
//!   to the item name, and a `kind` clamped to the client's `completionItemKind.valueSet`.
//! - `documentation`/`detail` are left `None` and filled lazily by [`completion_item_resolve`]; the
//!   `data` field is a **compact, self-sufficient key** ([`CompletionData`]) — a file + symbol path,
//!   never the request params.
//!
//! # Capability gating (every projection names its gate)
//!
//! - Snippet placeholders (`($0)`) only when the client advertises `completionItem.snippetSupport`
//!   AND `initializationOptions.completion.snippets` is on; otherwise a bare-name plain-text edit.
//! - `InsertReplaceEdit` only when `completionItem.insertReplaceSupport`; otherwise a plain
//!   `TextEdit`.
//! - `commitCharacters` only when `completionItem.commitCharactersSupport`, and even then suppressed
//!   for the string-valued annotation-argument context (a `.`/`(` commit mid-string is a wart);
//!   member / identifier / type / keyword items keep them.

use lsp_types::{
    CompletionItem, CompletionItemKind, CompletionList, CompletionParams, CompletionTextEdit,
    Documentation, InsertReplaceEdit, InsertTextFormat, MarkupContent, MarkupKind, Position, Range,
    TextEdit,
};
use serde::{Deserialize, Serialize};

use gd_analyze::enumerate::{self, MemberItem, MemberItemKind, MemberOwner};
use gd_analyze::{AnalysisResult, DataType, DtKind, FoldedValue};
use gd_syntax::ast::{NodeId, ParseTree};
use gd_syntax::ByteSpan;

use crate::completion_context::{self, classify, CompletionKind, DeferredReason, NodePathSigil};
use crate::config::{CallArgumentStyle, CompletionConfig};
use crate::docs::ProseFormat;
use crate::position::PositionMapper;
use crate::server::{CompletionCaps, ServerState};
use crate::uri::CanonicalKey;

/// The default `CompletionItemKind` set a client supports when it advertises no
/// `completionItemKind.valueSet`: the original-protocol range `Text`(1)..=`Reference`(18). Per LSP
/// 3.17 an absent value-set means exactly this range — NOT "all kinds" and NOT "none", so clamping
/// against it (rather than an empty set) is what keeps a minimal client from losing every icon.
fn default_kind_value_set() -> [CompletionItemKind; 18] {
    [
        CompletionItemKind::TEXT,
        CompletionItemKind::METHOD,
        CompletionItemKind::FUNCTION,
        CompletionItemKind::CONSTRUCTOR,
        CompletionItemKind::FIELD,
        CompletionItemKind::VARIABLE,
        CompletionItemKind::CLASS,
        CompletionItemKind::INTERFACE,
        CompletionItemKind::MODULE,
        CompletionItemKind::PROPERTY,
        CompletionItemKind::UNIT,
        CompletionItemKind::VALUE,
        CompletionItemKind::ENUM,
        CompletionItemKind::KEYWORD,
        CompletionItemKind::SNIPPET,
        CompletionItemKind::COLOR,
        CompletionItemKind::FILE,
        CompletionItemKind::REFERENCE,
    ]
}

/// An empty, well-formed list — the response for every context this phase does not render and for
/// the "no analysis / nothing here" paths. `is_incomplete: false` tells the client the set is
/// complete for the current prefix (it need not re-query on the next keystroke beyond re-filtering).
fn empty_list() -> CompletionList {
    CompletionList {
        is_incomplete: false,
        items: Vec::new(),
    }
}

/// `textDocument/completion`: classify the cursor, then render IDENTIFIER + ATTRIBUTE contexts as a
/// [`CompletionList`]. Mirrors the [`crate::handlers::hover`] preamble (VFS rope → cached parse →
/// `analyze_if_gd` → [`PositionMapper`] → `position_to_byte`). Returns an empty list — never an
/// error — for a missing buffer, a non-`.gd` file, a deferred/unhandled context, or an unresolved
/// base type ("never crash, never lie": missing analysis is silence, not a failure).
#[must_use]
pub fn completion(state: &mut ServerState, params: CompletionParams) -> CompletionList {
    let tdp = params.text_document_position;
    let uri = tdp.text_document.uri.clone();
    let Some(text) = state.vfs.get(uri.as_str()).map(|d| d.text()) else {
        return empty_list();
    };
    let parsed = state.workspace.parse(&CanonicalKey::for_uri(&uri), &text);
    // The token stream `classify` needs (the standalone lexer output for the same source).
    let (tokens, _lex_errors) = gd_syntax::tokenize(&text);
    // Analyze for the ATTRIBUTE arm's base-type resolution; `None` for a non-`.gd` buffer. The
    // IDENTIFIER arm degrades gracefully without it (locals + class members + globals don't need a
    // resolved type), so a `None` here still yields a useful list.
    let analyzed = crate::handlers::analyze_if_gd(state, &uri, &parsed.tree, &text);

    let Some(doc) = state.vfs.get(uri.as_str()) else {
        return empty_list();
    };
    let mapper = PositionMapper::new(&doc.rope, state.encoding);
    let byte = mapper.position_to_byte(tdp.position);

    let ctx = classify(&parsed.tree, &tokens, byte);
    // The single-line replace range = the typed-prefix span (empty ⇒ a zero-width range at the
    // cursor, so the client inserts without deleting). Override stubs are the one exception: accepting
    // a full signature over an existing `func name():` skeleton must consume that stale same-line
    // signature tail too, or the client applies source-invalid `func name(...):\n\t$0():`.
    let edit_range = if matches!(ctx.kind, CompletionKind::OverrideMethod { .. }) {
        override_method_range(&mapper, &tokens, &text, ctx.prefix, byte, tdp.position)
    } else {
        prefix_range(&mapper, ctx.prefix, tdp.position)
    };

    let render = RenderCtx {
        caps: &state.caps.completion,
        config: &state.options.completion,
        edit_range,
        index: &state.workspace.index,
        suppress_commit: suppress_commit_for(&ctx.kind),
    };

    let items = match &ctx.kind {
        CompletionKind::Attribute { base } => attribute_items(
            state,
            &uri,
            &parsed.tree,
            analyzed.as_deref(),
            *base,
            &tokens,
            byte,
            &render,
        ),
        CompletionKind::Identifier => {
            identifier_items(state, &parsed.tree, analyzed.as_deref(), byte, &render)
        }
        // --- Phase 4 contexts ---
        CompletionKind::Annotation => annotation_items(&render),
        CompletionKind::AnnotationArguments {
            annotation_name,
            arg_index,
        } => annotation_argument_items(state, annotation_name.as_deref(), *arg_index, &render),
        // Type positions. `void` only for the `-> ` return slot; `extends` excludes
        // builtins/enums/void (a class only); the rest list the full type set.
        CompletionKind::TypeName => type_name_items(
            state,
            &parsed.tree,
            analyzed.as_deref(),
            TypePos::Type,
            &render,
        ),
        // Class-body `var x: <cursor>` — available types, then the `get`/`set` accessor keywords
        // (Godot `COMPLETION_PROPERTY_DECLARATION_OR_TYPE`, `gdscript_editor.cpp:3535`: types listed
        // first, then `get`, then `set`).
        CompletionKind::PropertyDeclarationOrType => {
            let mut items = type_name_items(
                state,
                &parsed.tree,
                analyzed.as_deref(),
                TypePos::Type,
                &render,
            );
            let base_rank = items.len();
            for (offset, kw) in ["get", "set"].into_iter().enumerate() {
                items.push(keyword_item(
                    kw,
                    CompletionItemKind::KEYWORD,
                    CompletionData::Keyword,
                    base_rank + offset,
                    &render,
                ));
            }
            items
        }
        CompletionKind::TypeNameOrVoid => type_name_items(
            state,
            &parsed.tree,
            analyzed.as_deref(),
            TypePos::OrVoid,
            &render,
        ),
        CompletionKind::InheritType => type_name_items(
            state,
            &parsed.tree,
            analyzed.as_deref(),
            TypePos::Inherit,
            &render,
        ),
        CompletionKind::TypeAttribute { base } => type_attribute_items(
            state,
            &parsed.tree,
            analyzed.as_deref(),
            *base,
            &tokens,
            byte,
            &render,
        ),
        CompletionKind::CallArguments {
            callee,
            callee_name,
            arg_index,
        } => call_argument_items(
            state,
            &uri,
            &parsed.tree,
            analyzed.as_deref(),
            *callee,
            callee_name.as_deref(),
            *arg_index,
            byte,
            &render,
        ),
        CompletionKind::Subscript => subscript_items(
            state,
            &parsed.tree,
            analyzed.as_deref(),
            &tokens,
            byte,
            &render,
        ),
        CompletionKind::Assign => assign_items(
            state,
            &parsed.tree,
            analyzed.as_deref(),
            &tokens,
            byte,
            &render,
        ),
        CompletionKind::SuperMethod => {
            super_method_items(state, &parsed.tree, analyzed.as_deref(), &render)
        }
        CompletionKind::PropertyMethod => {
            property_method_items(state, &parsed.tree, analyzed.as_deref(), &render)
        }
        CompletionKind::PropertyAccessor => property_accessor_items(&render),
        CompletionKind::OverrideMethod { is_static } => {
            let own_file =
                crate::uri::uri_to_path(&uri).and_then(|p| state.workspace.index.file_id(&p));
            override_method_items(
                state,
                &parsed.tree,
                analyzed.as_deref(),
                own_file,
                *is_static,
                &render,
            )
        }
        // Deferred (`$`/`%`/`get_node`/path) — scene-aware node-path + resource-path completion
        // (M11 Phase 3). Each arm returns an empty list when nothing concrete is known (no scene
        // attached, no match) — never a project-wide guess (anti-catalog W10).
        CompletionKind::Deferred(reason) => {
            deferred_items(state, &uri, &tokens, byte, *reason, &render)
        }
        // None: a well-formed empty list.
        CompletionKind::None => Vec::new(),
    };

    // A path context's candidate set genuinely changes as the user types past a `/` (each segment
    // re-roots the listing), and `/` is NOT a trigger character (it is also division) — so mark these
    // results incomplete to make the client RE-QUERY on the next keystroke rather than filter a stale
    // segment's list. Every other context is complete for its prefix (a re-filter suffices). This
    // only ever tells the client to ask again; it carries no edit, so it cannot corrupt.
    let is_incomplete = matches!(ctx.kind, CompletionKind::Deferred(_));

    CompletionList {
        is_incomplete,
        items,
    }
}

/// Everything an item-builder needs that does not change between items in one request.
struct RenderCtx<'a> {
    caps: &'a CompletionCaps,
    config: &'a CompletionConfig,
    /// The single-line range every item's `TextEdit` replaces (the typed-prefix span).
    edit_range: Range,
    /// The project index — maps a declaring [`gd_project::FileId`] to its URI for the resolve key.
    index: &'a gd_project::Index,
    /// Whether to drop commit characters for this whole request — set for the string-valued
    /// annotation-argument context, where a `.`/`(` commit mid-string is a UX wart. One request is
    /// one [`CompletionKind`], so a per-request flag is exact (see [`suppress_commit_for`]).
    suppress_commit: bool,
}

impl RenderCtx<'_> {
    /// The URI of a declaring file id (carry-forward (b)'s owner key), or `None` when the index has
    /// no path for it / the path can't be rendered as a `file://` URI.
    fn file_uri(&self, file: gd_project::FileId) -> Option<String> {
        let path = self.index.path(file)?;
        crate::uri::path_to_file_uri(path).map(|u| u.as_str().to_string())
    }

    /// #258: whether the declaring script marked this member `## @deprecated`. Read from the
    /// DECLARING file's interface — the same precise owner key `completionItem/resolve` uses, never
    /// a name-only search. Native members are always `false`: `extension_api.json` (4.6.3, with or
    /// without docs) carries no deprecation field at all, so gdls has no source to claim one from.
    fn member_is_deprecated(&self, owner: &MemberOwner, name: &str) -> bool {
        let MemberOwner::Script { file, inner } = owner else {
            return false;
        };
        let Some(mut iface) = self.index.interface(*file) else {
            return false;
        };
        for seg in inner {
            match iface
                .inner
                .iter()
                .find(|c| c.class_name.as_deref() == Some(seg.as_str()))
            {
                Some(i) => iface = i,
                None => return false,
            }
        }
        iface
            .members
            .iter()
            .find(|m| m.name == name)
            .and_then(|m| m.doc.as_deref())
            .is_some_and(|d| d.is_deprecated)
    }
}

/// #258: stamp the LSP deprecation signal on an item whose declaring `##` doc says `@deprecated`.
/// A client that advertised `completionItem.tagSupport` with `Deprecated` gets `tags: [Deprecated]`
/// (LSP 3.15+); one that did not gets the older `deprecated: true` boolean, which is what a minimal
/// client still understands. Never both — `tags` supersedes the boolean.
fn mark_deprecated(
    mut item: CompletionItem,
    owner: &MemberOwner,
    name: &str,
    render: &RenderCtx,
) -> CompletionItem {
    if !render.member_is_deprecated(owner, name) {
        return item;
    }
    if render.caps.tag_support_deprecated {
        item.tags = Some(vec![lsp_types::CompletionItemTag::DEPRECATED]);
    } else {
        #[allow(deprecated)]
        // The pre-3.15 signal, sent only to clients that never advertised tags.
        {
            item.deprecated = Some(true);
        }
    }
    item
}

// ===================================================================================================
// ATTRIBUTE — `expr.<cursor>` member access.
// ===================================================================================================

/// Render member completions for `base.<cursor>`. Resolve the base expression's [`DataType`] from
/// the analysis (the `base` node id when the AST preserved it, else the smallest typed node ending
/// at the dot), then dispatch through [`enumerate::members_of_type`]. An unresolved base ⇒ empty
/// (offer nothing rather than a wrong set) — including the top-level `local.` case where
/// `base: None` and no typed node is recoverable, an honest Phase-3 gap.
#[allow(clippy::too_many_arguments)] // the resolved call-site (tree + tokens + analysis + render ctx)
fn attribute_items(
    state: &ServerState,
    uri: &lsp_types::Uri,
    tree: &ParseTree,
    analyzed: Option<&AnalysisResult>,
    base: Option<NodeId>,
    tokens: &[gd_syntax::token::Token],
    byte: usize,
    render: &RenderCtx,
) -> Vec<CompletionItem> {
    let Some(analyzed) = analyzed else {
        return Vec::new();
    };
    let Some(dt) = resolve_base_type(state, uri, tree, analyzed, base, tokens, byte) else {
        return Vec::new();
    };
    let dt = &dt;
    // `Color.` / `Vector2.` — a builtin **meta-type** (the type itself, not an instance). Godot's
    // `COMPLETION_BUILT_IN_TYPE_CONSTANT_OR_STATIC_METHOD`: only that type's constants + STATIC
    // methods, never its instance methods (offering `Color.lerp` as a static would be a "never lie"
    // breach). The context engine routes this through `Attribute`; `is_meta_type` is the split.
    if dt.kind == DtKind::Builtin && dt.is_meta_type {
        return builtin_static_items(state, dt, render);
    }
    let members = enumerate_members(state, tree, dt);
    // #306: `MyClass.<cursor>` is the CLASS, not an instance of it. Godot's
    // `COMPLETION_TYPE_ATTRIBUTE` on a script meta type offers the type-scoped surface only, so
    // the instance set here was both wrong (`Inventory.add_item` cannot be called) and missing the
    // one thing anyone actually types after a class name.
    let members = if dt.is_meta_type && matches!(dt.kind, DtKind::Script | DtKind::Class) {
        script_meta_items(members)
    } else {
        members
    };
    members
        .into_iter()
        .enumerate()
        .map(|(rank, m)| member_item(&m, rank, render))
        .collect()
}

/// The completion surface of a script META type (`MyClass.<cursor>`) — the constructor plus
/// everything reachable without an instance, in the order a user wants them: `new` first, then the
/// type-scoped names, then the statics.
///
/// A GDScript class object exposes `new()`, its `const`s, `enum`s (and their values), its inner
/// classes, and its `static func`s / `static var`s, all inherited down the `extends` chain. Plain
/// `var`s and instance methods need an instance and are dropped — offering `Inventory.add_item`
/// is the same "never lie" breach as offering `Color.lerp` as a static, which the builtin arm
/// right above has always refused. #306.
fn script_meta_items(members: Vec<MemberItem>) -> Vec<MemberItem> {
    // `new` is not an interface member — it is the class object's constructor, synthesized here the
    // way Godot's editor offers it. Skipped when the chain already declares a `new` of its own, so
    // the user's entry (with its detail and doc) wins over the synthetic one.
    let declares_new = members.iter().any(|m| m.name == "new");
    let mut out: Vec<MemberItem> = Vec::new();
    if !declares_new {
        out.push(MemberItem::constructor());
    }
    out.extend(members.into_iter().filter(|m| match m.kind {
        // Type-scoped: reachable through the class object, no instance needed.
        MemberItemKind::Constant
        | MemberItemKind::Enum
        | MemberItemKind::EnumValue
        | MemberItemKind::Class => true,
        // Only the static half of the callable/value surface.
        MemberItemKind::Method | MemberItemKind::Property => m.is_static,
        // A signal is emitted and connected on an instance.
        MemberItemKind::Signal => false,
    }));
    out
}

/// Enumerate the members of a resolved type through the project-backed cross-file query.
/// `members_of_type` never calls `autoload_file`, so the autoload map is empty (the shape
/// `xfile.rs`'s own tests use). The `Rc<AnalysisResult>` the caller holds keeps the analysis alive
/// independently of these shared borrows.
fn enumerate_members(state: &ServerState, tree: &ParseTree, dt: &DataType) -> Vec<MemberItem> {
    let xfile = crate::xfile::WorkspaceXFileQuery::new(
        &state.workspace.index,
        &state.workspace.native,
        &state.workspace.analysis_cache,
        crate::xfile::AutoloadEnv::default(),
        &state.workspace.scenes,
        &state.workspace.project.root,
    );
    enumerate::members_of_type(dt, &state.workspace.native, &xfile, tree)
}

/// `Color.<cursor>` — the builtin type's **constants + static methods** (Godot
/// `COMPLETION_BUILT_IN_TYPE_CONSTANT_OR_STATIC_METHOD`, `gdscript_editor.cpp:3488`). Instance
/// members are dropped (`is_static` filter on methods); constants/enums are kept. A static method is
/// `callable` so it gets the call-paren snippet under the same gates as any other call.
fn builtin_static_items(
    state: &ServerState,
    dt: &DataType,
    render: &RenderCtx,
) -> Vec<CompletionItem> {
    let name = gd_analyze::data_type::variant_type_name(dt.builtin_type);
    let Some(members) = enumerate::builtin_members(&state.workspace.native, name) else {
        return Vec::new();
    };
    members
        .into_iter()
        .filter(|m| match m.kind {
            // Static methods only — never an instance method as a static.
            MemberItemKind::Method => m.is_static,
            // Constants / named enums / enum values are type-scoped: keep them.
            MemberItemKind::Constant | MemberItemKind::Enum | MemberItemKind::EnumValue => true,
            // A builtin has no signals/inner-classes; properties are instance-only — drop.
            _ => false,
        })
        .enumerate()
        .map(|(rank, m)| member_item(&m, rank, render))
        .collect()
}

/// Resolve the [`DataType`] to enumerate members of for an ATTRIBUTE context. Prefers the captured
/// `base` node id; when the AST didn't preserve it (`None`), falls back to the smallest typed node
/// whose span ends at the cursor's dot — covering `base.partial` shapes where the base survived as
/// some other node. Returns a set type only (`is_set()`), never `Unresolved`/`Resolving`.
fn resolve_base_type(
    state: &ServerState,
    uri: &lsp_types::Uri,
    tree: &ParseTree,
    analyzed: &AnalysisResult,
    base: Option<NodeId>,
    tokens: &[gd_syntax::token::Token],
    byte: usize,
) -> Option<DataType> {
    // #349: a `$Path` / `%Name` / `get_node("…")` base has a PRECISE scene type that the analyzer
    // deliberately does not carry — it types every such access as a hard bare `Node`, faithful to
    // `gdscript_analyzer.cpp:3866-3886`, because a precise type in the diagnostic path would
    // false-positive on the downcasts Godot tolerates. Hover, definition, and typeDefinition
    // already project the precise type onto the access itself; completion did not, so `$HUD/Label`
    // hovered as `Label` and `$HUD/Label.` one character later offered bare `Node`'s members.
    // Navigation-only by construction — see `crate::scene_nav`.
    let end = match base {
        Some(id) => Some(tree.get(id).span.end),
        None => nearest_dot_start(tokens, byte),
    };
    if let Some(dt) = end.and_then(|e| crate::scene_nav::scene_type_ending_at(state, uri, tree, e))
    {
        return Some(dt);
    }
    analyzed_base_type(tree, analyzed, base, tokens, byte).cloned()
}

/// The analyzer's own type for a `<base>.<cursor>` base expression, or `None` when it has none.
fn analyzed_base_type<'a>(
    tree: &ParseTree,
    analyzed: &'a AnalysisResult,
    base: Option<NodeId>,
    tokens: &[gd_syntax::token::Token],
    byte: usize,
) -> Option<&'a DataType> {
    if let Some(id) = base {
        let dt = analyzed.types.get(id);
        if dt.is_set() {
            return Some(dt);
        }
    }
    // Fallback: the base expression is whatever typed node ends just before the dot. Find the dot
    // by walking back from the cursor over the partial word, then pick the smallest typed node
    // ending at the dot's start.
    let dot_start = nearest_dot_start(tokens, byte)?;
    smallest_typed_ending_at(tree, analyzed, dot_start)
}

/// The byte offset of the `.` nearest to (and at or before) the cursor — the end of the base
/// expression in `base.<cursor>` / `base.partial`. `None` when no dot precedes the cursor on the
/// member-access path.
fn nearest_dot_start(tokens: &[gd_syntax::token::Token], byte: usize) -> Option<usize> {
    use gd_syntax::token::TokenKind;
    tokens
        .iter()
        .rev()
        .filter(|t| t.span.end <= byte)
        .find(|t| t.kind == TokenKind::Period)
        .map(|t| t.span.start)
}

/// The smallest-span node whose span **ends exactly** at `end` and carries a resolved type — the
/// base expression in `base.<cursor>` (its span ends at the dot). Linear over the arena, like
/// hover's `smallest_typed_containing`; adequate for a one-shot completion request.
fn smallest_typed_ending_at<'a>(
    tree: &ParseTree,
    analyzed: &'a AnalysisResult,
    end: usize,
) -> Option<&'a DataType> {
    let mut best: Option<(NodeId, usize)> = None;
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

/// Build one `CompletionItem` from an enumerated [`MemberItem`] at rank `rank`. `detail` stays
/// `None` (resolve fills it); the member's source-derived `detail` rides along in the
/// [`CompletionData`] so resolve doesn't re-enumerate. `data.owner` carries the **declaring** class
/// / file (carry-forward (b)) so resolve fetches the long-form documentation deterministically —
/// from the native DB for a native member, or from the declaring file's interface for a script
/// member (which may differ from the requesting buffer when it is inherited).
fn member_item(m: &MemberItem, rank: usize, render: &RenderCtx) -> CompletionItem {
    let kind = member_kind(m.kind);
    let data = CompletionData::Member {
        owner: data_owner(&m.owner, render),
        name: m.name.clone(),
        detail: m.detail.clone(),
    };
    let callable = matches!(m.kind, MemberItemKind::Method);
    let item = build_item(&m.name, kind, callable, data, rank, render);
    mark_deprecated(item, &m.owner, &m.name, render)
}

/// Translate an enumeration [`MemberOwner`] into the serializable resolve key
/// [`CompletionDataOwner`]: a native member keeps its declaring class name; a script member's
/// declaring [`gd_project::FileId`] is rendered to that file's URI (so resolve is a direct lookup,
/// no nondeterministic name-only search); an unknown owner falls back to the requesting buffer.
fn data_owner(owner: &MemberOwner, render: &RenderCtx) -> CompletionDataOwner {
    match owner {
        MemberOwner::Native(class) => CompletionDataOwner::NativeClass {
            class: class.clone(),
        },
        MemberOwner::Script { file, inner } => match render.file_uri(*file) {
            Some(uri) => CompletionDataOwner::ScriptFile {
                uri,
                inner: inner.clone(),
            },
            None => CompletionDataOwner::Unknown,
        },
        MemberOwner::Unknown => CompletionDataOwner::Unknown,
    }
}

// ===================================================================================================
// IDENTIFIER — the bare-name in-scope set.
// ===================================================================================================

/// Render the bare-identifier completion set: locals + parameters (innermost-first), then the
/// implicit-self class members through the `extends` chain (incl. inherited natives), then the
/// globals — GDScript/`@GlobalScope` utilities, global enum values, global constants, autoload
/// singletons, the project `class_name` registry, and native class names. Ordering follows Godot's
/// `complete_code` priority (locals shadow members shadow globals); `sort_text` encodes it.
///
/// De-duplicates by name keeping the first (highest-priority) occurrence, so a local shadowing a
/// class member appears once, ranked as the local.
fn identifier_items(
    state: &ServerState,
    tree: &ParseTree,
    analyzed: Option<&AnalysisResult>,
    byte: usize,
    render: &RenderCtx,
) -> Vec<CompletionItem> {
    let mut items: Vec<CompletionItem> = Vec::new();
    let mut seen: rustc_hash::FxHashSet<String> = rustc_hash::FxHashSet::default();
    let mut rank: usize = 0;
    let push = |name: &str,
                kind: CompletionItemKind,
                callable: bool,
                data: CompletionData,
                items: &mut Vec<CompletionItem>,
                seen: &mut rustc_hash::FxHashSet<String>,
                rank: &mut usize| {
        if !seen.insert(name.to_string()) {
            return;
        }
        items.push(build_item(name, kind, callable, data, *rank, render));
        *rank += 1;
    };

    // (1) Locals + parameters in the cursor's scope (innermost-first, declaration order). Both
    // locals and parameters render as `VARIABLE` (LSP has no dedicated parameter kind).
    for local in tree.locals_in_scope_at(byte) {
        push(
            &local.name,
            CompletionItemKind::VARIABLE,
            false,
            CompletionData::Local,
            &mut items,
            &mut seen,
            &mut rank,
        );
    }

    // (2) Class members through the implicit `self` extends chain (incl. inherited natives). The
    // finished analysis has rewritten the in-file class to a `Script` ref, so enumerate that type
    // when we can find it — the type pinned on the file's root class node.
    if let Some(analyzed) = analyzed {
        for m in self_chain_members(state, tree, analyzed) {
            let callable = matches!(m.kind, MemberItemKind::Method);
            let before = items.len();
            push(
                &m.name,
                member_kind(m.kind),
                callable,
                CompletionData::Member {
                    owner: data_owner(&m.owner, render),
                    name: m.name.clone(),
                    detail: m.detail.clone(),
                },
                &mut items,
                &mut seen,
                &mut rank,
            );
            // #258: the closure de-duplicates, so stamp only the item it actually appended.
            if items.len() > before {
                let last = items.len() - 1;
                items[last] = mark_deprecated(items[last].clone(), &m.owner, &m.name, render);
            }
        }
    }

    // (3) Globals — order mirrors Godot's tail: utilities, then enum values, then constants, then
    // autoloads, then class_name registry, then native class names.
    let native = &state.workspace.native;
    for util in native.utilities() {
        let name = native.name_of(util.name).to_string();
        push(
            &name,
            CompletionItemKind::FUNCTION,
            true,
            CompletionData::Global { name: name.clone() },
            &mut items,
            &mut seen,
            &mut rank,
        );
    }
    for (_enum_name, value_name, _v) in native.global_enum_values() {
        let name = value_name.to_string();
        push(
            &name,
            CompletionItemKind::ENUM_MEMBER,
            false,
            CompletionData::Global { name: name.clone() },
            &mut items,
            &mut seen,
            &mut rank,
        );
    }
    for (name, _v) in native.global_constants() {
        let name = name.to_string();
        push(
            &name,
            CompletionItemKind::CONSTANT,
            false,
            CompletionData::Global { name: name.clone() },
            &mut items,
            &mut seen,
            &mut rank,
        );
    }
    for autoload in &state.workspace.project.autoloads {
        push(
            &autoload.name,
            CompletionItemKind::VARIABLE,
            false,
            CompletionData::Local,
            &mut items,
            &mut seen,
            &mut rank,
        );
    }
    // Project `class_name` registry — sorted for determinism (the registry is an `FxHashMap`, so
    // `entries()` iterates in nondeterministic file-discovery order, unlike every sibling tier).
    let mut registry_names: Vec<&str> = state
        .workspace
        .index
        .registry()
        .entries()
        .map(|(n, _)| n)
        .collect();
    registry_names.sort_unstable();
    for name in registry_names {
        let name = name.to_string();
        push(
            &name,
            CompletionItemKind::CLASS,
            false,
            CompletionData::NativeClass {
                class: name.clone(),
            },
            &mut items,
            &mut seen,
            &mut rank,
        );
    }
    // (4) Builtin Variant type names (`Vector2`, `Color`, `String`, …) — Godot's
    // `_find_built_in_variants`, called from `_find_identifiers` itself
    // (`gdscript_editor.cpp:1618`), so they belong in EXPRESSION position and not only in a type
    // annotation. Without them `var v := Vec` + invoke-completion offered nothing at all (#308).
    // `Nil` is excluded, exactly as the upstream loop skips `Variant::Type::NIL`.
    for builtin in state.workspace.native.builtin_names() {
        if builtin == "Nil" {
            continue;
        }
        let builtin = builtin.to_string();
        push(
            &builtin,
            CompletionItemKind::CLASS,
            false,
            CompletionData::NativeClass {
                class: builtin.clone(),
            },
            &mut items,
            &mut seen,
            &mut rank,
        );
    }

    // (5) Native engine class names (`Node`, `Timer`, …) — carry-forward (a), M8 Phase 4. Godot's
    // `get_global_map()` includes the native class set in the bare-identifier completion. Lowest
    // priority (after locals, members, and the project registry), via the name-sorted
    // `native_db::class_names()` iterator added in this phase.
    for class in state.workspace.native.class_names() {
        let class = class.to_string();
        push(
            &class,
            CompletionItemKind::CLASS,
            false,
            CompletionData::NativeClass {
                class: class.clone(),
            },
            &mut items,
            &mut seen,
            &mut rank,
        );
    }

    // (5b) #536: a class the project declares through a `.gdextension` that this dump does not
    // carry. The analyzer already refuses to claim such a name is undeclared, and Godot with the
    // extension loaded lists it from ClassDB; leaving it out of the list while staying silent
    // about it is the two halves disagreeing about the same name. The item carries the name and
    // nothing else, which is all gdls knows — `completionItem/resolve` finds no class body and
    // returns it unchanged.
    for class in state.workspace.native.extension_declared_missing_names() {
        let class = class.to_string();
        push(
            &class,
            CompletionItemKind::CLASS,
            false,
            CompletionData::NativeClass {
                class: class.clone(),
            },
            &mut items,
            &mut seen,
            &mut rank,
        );
    }

    // (6) Godot's fixed keyword tier, verbatim and in its own order — the last thing
    // `_find_identifiers` appends (`gdscript_editor.cpp:1620-1631`). `PI`/`TAU`/`INF`/`NAN` are
    // constants and `self`/`super` are neither constants nor keywords, but upstream emits all
    // fourteen through one `CODE_COMPLETION_KIND_KEYWORD` list, so the port mirrors that rather
    // than reclassifying them. #318.
    for kw in [
        "true",
        "false",
        "PI",
        "TAU",
        "INF",
        "NAN",
        "null",
        "self",
        "super",
        "break",
        "breakpoint",
        "continue",
        "pass",
        "return",
    ] {
        push(
            kw,
            CompletionItemKind::KEYWORD,
            false,
            CompletionData::Keyword,
            &mut items,
            &mut seen,
            &mut rank,
        );
    }

    items
}

/// The implicit-`self` member set for IDENTIFIER completion: the members of the type the analyzer
/// pinned on the file's root class node, enumerated through its `extends` chain. Empty when the
/// root class has no resolved type yet (e.g. an empty native DB).
fn self_chain_members(
    state: &ServerState,
    tree: &ParseTree,
    analyzed: &AnalysisResult,
) -> Vec<MemberItem> {
    let Some(root_id) = tree.root_id() else {
        return Vec::new();
    };
    // The root node of a parsed `.gd` is its implicit class; guard the kind defensively.
    if !matches!(tree.get(root_id).kind, gd_syntax::ast::NodeKind::Class(_)) {
        return Vec::new();
    }
    let dt = analyzed.types.get(root_id);
    if !dt.is_set() {
        return Vec::new();
    }
    let xfile = crate::xfile::WorkspaceXFileQuery::new(
        &state.workspace.index,
        &state.workspace.native,
        &state.workspace.analysis_cache,
        crate::xfile::AutoloadEnv::default(),
        &state.workspace.scenes,
        &state.workspace.project.root,
    );
    enumerate::members_of_type(dt, &state.workspace.native, &xfile, tree)
}

// ===================================================================================================
// ANNOTATION — `@<cursor>` (the annotation list) and `@export_range(<cursor>` (its argument words).
// ===================================================================================================

/// `@<cursor>` — the annotation name list (Godot `COMPLETION_ANNOTATION`,
/// `gdscript_editor.cpp:3468`, which iterates `get_annotation_list` = the parser's
/// `valid_annotations` registry). The leading `@` is stripped from the label (the `@` is already
/// typed); an annotation that takes arguments gets a trailing `(` appended (matching
/// `gdscript_editor.cpp:3473`). The single source of truth is
/// [`gd_syntax::parser::REGISTERED_ANNOTATIONS`]; sorted by name for deterministic ranking.
fn annotation_items(render: &RenderCtx) -> Vec<CompletionItem> {
    let mut names: Vec<(&str, bool)> = gd_syntax::parser::REGISTERED_ANNOTATIONS
        .iter()
        .map(|a| (a.name.trim_start_matches('@'), a.takes_arguments()))
        .collect();
    names.sort_unstable_by(|a, b| a.0.cmp(b.0));
    names
        .into_iter()
        .enumerate()
        .map(|(rank, (name, takes_args))| {
            // An annotation taking arguments inserts `name(`; the label keeps the bare name so the
            // typed prefix (after `@`) still filters. Not a snippet — the `(` is a plain hint.
            let insert = if takes_args {
                format!("{name}(")
            } else {
                name.to_string()
            };
            build_item_with(
                ItemText {
                    label: name,
                    filter: name,
                },
                CompletionItemKind::KEYWORD,
                ItemInsert {
                    plain: insert,
                    snippet: None,
                },
                CompletionData::Keyword,
                rank,
                render,
            )
        })
        .collect()
}

/// `@export_range(<cursor>` / `@rpc(<cursor>` / `@warning_ignore(<cursor>` — the per-annotation
/// special argument words (Godot's `_find_annotation_arguments`, `gdscript_editor.cpp:913`). Only
/// the non-editor cases are served (W17 skips the `@export_tool_button` icon list and the
/// `@export_custom` theme-enum args). Each word inserts a quoted string (these are string
/// arguments). `arg_index` gates the slider/easing/rpc words to the slots Godot offers them at.
fn annotation_argument_items(
    state: &ServerState,
    annotation_name: Option<&str>,
    arg_index: usize,
    render: &RenderCtx,
) -> Vec<CompletionItem> {
    let Some(name) = annotation_name else {
        return Vec::new();
    };
    // The special words for this (annotation, argument-index) pair, derived mechanically from
    // `_find_annotation_arguments`. Editor-only icon/theme cases are intentionally omitted (W17).
    let words: Vec<String> = match name {
        // `min, max, step, extra_hints…` — the slider words live at the `extra_hints` slots (3/4/5).
        "@export_range" if matches!(arg_index, 3..=5) => vec![
            "or_greater".into(),
            "or_less".into(),
            "prefer_slider".into(),
            "hide_control".into(),
        ],
        "@export_exp_easing" if matches!(arg_index, 0..=1) => {
            vec!["attenuation".into(), "inout".into()]
        }
        // `@rpc` modes at the first three slots (mode / sync / transfer_mode).
        "@rpc" if matches!(arg_index, 0..=2) => vec![
            "call_local".into(),
            "call_remote".into(),
            "any_peer".into(),
            "authority".into(),
            "reliable".into(),
            "unreliable".into(),
            "unreliable_ordered".into(),
        ],
        // `@warning_ignore*` → the non-deprecated warning code names, lowercased
        // (`gdscript_editor.cpp:1007`; deprecated codes are skipped — they are never produced).
        "@warning_ignore" | "@warning_ignore_start" | "@warning_ignore_restore" => {
            warning_code_words()
        }
        // `@export_node_path(<type>)` → `Node` + every Node subclass / Node-derived global class.
        "@export_node_path" => node_path_type_words(state),
        // Any other annotation/slot: no special words (a generic string argument).
        _ => Vec::new(),
    };
    words
        .into_iter()
        .enumerate()
        .map(|(rank, word)| {
            // Annotation arguments are string literals: insert the word double-quoted (canonical,
            // W17 — no editor quote-style coupling). The label/filter stay the bare word.
            let quoted = format!("\"{word}\"");
            build_item_with(
                ItemText {
                    label: &word,
                    filter: &word,
                },
                CompletionItemKind::VALUE,
                ItemInsert {
                    plain: quoted,
                    snippet: None,
                },
                CompletionData::Keyword,
                rank,
                render,
            )
        })
        .collect()
}

/// The non-deprecated warning code names, lowercased — the `@warning_ignore` argument set. Godot
/// stops at `FIRST_DEPRECATED_WARNING` (the deprecated codes are never produced), which in gdls'
/// [`gd_analyze::warnings`] table is the `PropertyUsedAsFunction` index; the live set is the names
/// before it. Already in code order (deterministic).
fn warning_code_words() -> Vec<String> {
    use gd_analyze::warnings::{WarningCode, ALL, WARN_NAMES};
    ALL.iter()
        .zip(WARN_NAMES.iter())
        .take_while(|(code, _)| **code != WarningCode::PropertyUsedAsFunction)
        .map(|(_, name)| name.to_lowercase())
        .collect()
}

/// `@export_node_path(<type>)` → `Node` plus every Node-derived native class and Node-derived
/// project `class_name` (Godot offers `Node` + `ClassDB::get_inheriters_from_class("Node")` +
/// Node-rooted global classes). Sorted, deterministic.
fn node_path_type_words(state: &ServerState) -> Vec<String> {
    let native = &state.workspace.native;
    let mut out: Vec<String> = Vec::new();
    if native.class_named("Node").is_some() {
        out.push("Node".to_string());
    }
    for class in native.class_names() {
        if class != "Node" && native.is_subclass_of_named(class, "Node") {
            out.push(class.to_string());
        }
    }
    out.sort_unstable();
    out.dedup();
    out
}

// ===================================================================================================
// TYPE positions — TypeName / TypeNameOrVoid / InheritType (available types) and TypeAttribute.
// ===================================================================================================

/// Which type position is being completed — governs whether `void` and the builtin/enum/Variant
/// set are offered, mirroring Godot's `_list_available_types(p_inherit_only)` + the `void` prepend.
#[derive(Clone, Copy, PartialEq, Eq)]
enum TypePos {
    /// `var x: <cursor>` / `Array[<cursor>` / `x as <cursor>` — the full type set, no `void`.
    Type,
    /// `-> <cursor>` return position — the full type set **plus** `void`.
    OrVoid,
    /// `extends <cursor>` — a class only: native classes + project `class_name`, **no**
    /// builtins / enums / `void` / `Variant` (`_list_available_types(true)`, minus the builtins
    /// gdls deliberately excludes per the M8 acceptance criterion).
    Inherit,
}

/// The available-types set for a type position (Godot `_list_available_types`,
/// `gdscript_editor.cpp:1049`): builtins + `Variant` + native classes + project `class_name`
/// registry (+ `void` only for `-> `). `extends` ([`TypePos::Inherit`]) is restricted to classes.
/// De-duplicated by name; ordering is `void`?, builtins, Variant, native classes, then the
/// registry — each tier internally sorted for determinism.
fn type_name_items(
    state: &ServerState,
    tree: &ParseTree,
    analyzed: Option<&AnalysisResult>,
    pos: TypePos,
    render: &RenderCtx,
) -> Vec<CompletionItem> {
    let native = &state.workspace.native;
    let mut items: Vec<CompletionItem> = Vec::new();
    let mut seen: rustc_hash::FxHashSet<String> = rustc_hash::FxHashSet::default();
    let mut rank: usize = 0;
    let mut push = |name: &str, kind: CompletionItemKind, data: CompletionData| {
        if seen.insert(name.to_string()) {
            items.push(keyword_item(name, kind, data, rank, render));
            rank += 1;
        }
    };

    // `void` only at a return position.
    if pos == TypePos::OrVoid {
        push("void", CompletionItemKind::KEYWORD, CompletionData::Keyword);
    }

    // Builtins + `Variant` — excluded for `extends` (a class can't extend a builtin/Variant).
    if pos != TypePos::Inherit {
        for b in native.builtin_names() {
            push(
                b,
                CompletionItemKind::CLASS,
                CompletionData::NativeClass {
                    class: b.to_string(),
                },
            );
        }
        push(
            "Variant",
            CompletionItemKind::CLASS,
            CompletionData::Keyword,
        );
    }

    // Native engine classes.
    for class in native.class_names() {
        push(
            class,
            CompletionItemKind::CLASS,
            CompletionData::NativeClass {
                class: class.to_string(),
            },
        );
    }

    // #536: extension-declared classes this dump lacks. A type position is where they are written,
    // and the analyzer accepts them there, so this is the tier that most needed them.
    for class in native.extension_declared_missing_names() {
        push(
            class,
            CompletionItemKind::CLASS,
            CompletionData::NativeClass {
                class: class.to_string(),
            },
        );
    }

    // Project `class_name` registry (user-declared global classes) — sorted for determinism (the
    // registry `FxHashMap` iterates in nondeterministic order, unlike the sibling tiers above).
    let mut registry_names: Vec<&str> = state
        .workspace
        .index
        .registry()
        .entries()
        .map(|(n, _)| n)
        .collect();
    registry_names.sort_unstable();
    for name in registry_names {
        push(
            name,
            CompletionItemKind::CLASS,
            CompletionData::NativeClass {
                class: name.to_string(),
            },
        );
    }

    // In-file types declared on the current class (inner classes always; named enums only when not
    // an `extends` position) — `_list_available_types`' current-class walk.
    for m in self_chain_type_members(state, tree, analyzed, pos) {
        let kind = match m.kind {
            MemberItemKind::Enum => CompletionItemKind::ENUM,
            _ => CompletionItemKind::CLASS,
        };
        // Inlined rather than routed through `push`: #258 stamps the deprecation marker on the
        // item it just built, and the shared closure holds `items` borrowed for its own lifetime.
        if seen.insert(m.name.clone()) {
            let item = keyword_item(
                &m.name,
                kind,
                CompletionData::Member {
                    owner: data_owner(&m.owner, render),
                    name: m.name.clone(),
                    detail: m.detail.clone(),
                },
                rank,
                render,
            );
            items.push(mark_deprecated(item, &m.owner, &m.name, render));
            rank += 1;
        }
    }

    items
}

/// The type-like members (inner classes, named enums, type constants) of the current class's
/// `extends` chain — the in-file slice of `_list_available_types`. Named enums are dropped for an
/// `extends` position (`p_inherit_only`). Empty without analysis.
fn self_chain_type_members(
    state: &ServerState,
    tree: &ParseTree,
    analyzed: Option<&AnalysisResult>,
    pos: TypePos,
) -> Vec<MemberItem> {
    let Some(analyzed) = analyzed else {
        return Vec::new();
    };
    self_chain_members(state, tree, analyzed)
        .into_iter()
        .filter(|m| match m.kind {
            MemberItemKind::Class => true,
            MemberItemKind::Enum => pos != TypePos::Inherit,
            _ => false,
        })
        .collect()
}

/// `var x: Foo.<cursor>` — the nested types / enums / constants of the type `Foo` (Godot
/// `COMPLETION_TYPE_ATTRIBUTE`, `gdscript_editor.cpp:3642`), NOT `Foo`'s instance members. Keep only
/// the type-scoped member kinds.
///
/// The base type is resolved in two steps: first the ATTRIBUTE path (a typed node ending at the
/// dot); failing that — the common case, since the analyzer pins the *final* resolved type on the
/// whole `Type` node, not a per-segment type ending at the dot — the base **token name** before the
/// dot is resolved directly (a native class / a project `class_name`). This best-effort covers
/// `Class.<cursor>`; a multi-segment `Outer.Inner.<cursor>` whose intermediate type isn't a typed
/// node is the documented limit (it falls through to empty rather than a wrong set).
fn type_attribute_items(
    state: &ServerState,
    tree: &ParseTree,
    analyzed: Option<&AnalysisResult>,
    base: Option<NodeId>,
    tokens: &[gd_syntax::token::Token],
    byte: usize,
    render: &RenderCtx,
) -> Vec<CompletionItem> {
    let members = type_attribute_members(state, tree, analyzed, base, tokens, byte);
    members
        .into_iter()
        // Type-scoped members only: inner classes, named enums, and constants (incl. enum values).
        .filter(|m| {
            matches!(
                m.kind,
                MemberItemKind::Class
                    | MemberItemKind::Enum
                    | MemberItemKind::Constant
                    | MemberItemKind::EnumValue
            )
        })
        .enumerate()
        .map(|(rank, m)| member_item(&m, rank, render))
        .collect()
}

/// The members to enumerate for a `Foo.<cursor>` type-attribute base — via the typed-node path, then
/// the base-token-name fallback. Empty when neither resolves.
fn type_attribute_members(
    state: &ServerState,
    tree: &ParseTree,
    analyzed: Option<&AnalysisResult>,
    base: Option<NodeId>,
    tokens: &[gd_syntax::token::Token],
    byte: usize,
) -> Vec<MemberItem> {
    // Path 1: a typed node ending at the dot (when the AST/analysis preserved the base type).
    if let Some(analyzed) = analyzed {
        if let Some(dt) = analyzed_base_type(tree, analyzed, base, tokens, byte) {
            return enumerate_members(state, tree, dt);
        }
    }
    // Path 2: resolve the dotted type chain before the dot by NAME (the chain's segment types aren't
    // pinned ending-at-the-dot). The FIRST segment is a native class or a project `class_name`; any
    // remaining segments (`Outer.Inner.<cursor>`) descend the project class's inner-class chain
    // (Godot's `COMPLETION_TYPE_ATTRIBUTE` segment-by-segment walk, `gdscript_editor.cpp:3652-3663`).
    // Pure type-NAME resolution — degrade to empty on any unresolved segment (never a wrong set).
    let Some(dot_start) = nearest_dot_start(tokens, byte) else {
        return Vec::new();
    };
    let chain = dotted_type_chain_before(tokens, dot_start);
    let Some((head, rest)) = chain.split_first() else {
        return Vec::new();
    };
    let native = &state.workspace.native;
    if native.class_named(head).is_some() {
        // A native class. Multi-segment native chains (a native nested type) are not modeled here;
        // only the single-segment `Native.<cursor>` case resolves (faithful: never wrong, just
        // incomplete past the first native segment).
        if rest.is_empty() {
            return enumerate::native_class_members(native, head);
        }
        return Vec::new();
    }
    // A project `class_name` head → enumerate the declaring file's script type, descending the
    // remaining segments as the inner-class path.
    if let Some(entry) = state.workspace.index.registry().get(head) {
        if let Some(fid) = state.workspace.index.file_id(&entry.path) {
            let dt = DataType {
                kind: DtKind::Script,
                type_source: gd_analyze::TypeSource::AnnotatedExplicit,
                script_type: Some(gd_analyze::ScriptRef {
                    file: fid,
                    inner: rest.to_vec(),
                }),
                ..Default::default()
            };
            return enumerate_members(state, tree, &dt);
        }
    }
    Vec::new()
}

/// The dotted type-name chain immediately before `dot_start` (`["Outer", "Inner"]` for
/// `Outer.Inner.<cursor>`), reading contiguous `name (. name)*` tokens leftward. Stops at the first
/// non-identifier / non-`.` token (so `foo().Inner.` or `a + B.` only yields the trailing run), and
/// drops a chain whose run is interrupted by a non-`.` separator. Returns the segments in source
/// order. Empty when the token before the dot isn't a simple name.
fn dotted_type_chain_before(tokens: &[gd_syntax::token::Token], dot_start: usize) -> Vec<String> {
    use gd_syntax::token::TokenKind;
    let mut segments: Vec<String> = Vec::new();
    // Index of the token whose end is at-or-before `dot_start` — walk leftward from there.
    let mut idx = tokens
        .iter()
        .rposition(|t| t.span.end <= dot_start && is_meaningful(t.kind));
    // Alternate: expect a name, then a `.`, then a name, … leftward.
    let mut expect_name = true;
    while let Some(i) = idx {
        let t = &tokens[i];
        if expect_name {
            if t.kind == TokenKind::Identifier || t.kind.is_identifier() {
                segments.push(t.source.to_string());
                expect_name = false;
            } else {
                break;
            }
        } else if t.kind == TokenKind::Period {
            expect_name = true;
        } else {
            break;
        }
        idx = tokens[..i].iter().rposition(|t| is_meaningful(t.kind));
    }
    segments.reverse();
    segments
}

/// Whether a token is a meaningful (non-layout, non-error) token for a leftward name-chain scan.
fn is_meaningful(kind: gd_syntax::token::TokenKind) -> bool {
    use gd_syntax::token::TokenKind;
    !matches!(
        kind,
        TokenKind::Newline
            | TokenKind::Indent
            | TokenKind::Dedent
            | TokenKind::Eof
            | TokenKind::Error
    )
}

// ===================================================================================================
// CALL_ARGUMENTS — enum/bitfield candidates for an enum-typed parameter (else identifier fallback).
// ===================================================================================================

/// Inside a call's argument list. When the active parameter (callee + `arg_index`) is enum- or
/// bitfield-typed — resolvable from a native method's argument info — suggest that enum's constants
/// (Godot's `_find_enumeration_candidates` analog). Otherwise fall back to the bare-identifier set
/// (Godot's `_find_identifiers` tail), so a generic call argument still completes. Node-path /
/// file-path argument strings are deferred (M11).
#[allow(clippy::too_many_arguments)] // dispatch fan-out: the classified call-site payload + render ctx
fn call_argument_items(
    state: &ServerState,
    uri: &lsp_types::Uri,
    tree: &ParseTree,
    analyzed: Option<&AnalysisResult>,
    callee: Option<NodeId>,
    callee_name: Option<&str>,
    arg_index: usize,
    byte: usize,
    render: &RenderCtx,
) -> Vec<CompletionItem> {
    // Try the enum-candidate refinement first; fall back to identifiers when the parameter type
    // isn't a resolvable enum/bitfield.
    if let Some(analyzed) = analyzed {
        if let Some(candidates) =
            call_arg_enum_candidates(state, uri, tree, analyzed, callee, callee_name, arg_index)
        {
            return candidates
                .into_iter()
                .enumerate()
                .map(|(rank, name)| {
                    keyword_item(
                        &name,
                        CompletionItemKind::ENUM_MEMBER,
                        CompletionData::Global { name: name.clone() },
                        rank,
                        render,
                    )
                })
                .collect();
        }
    }
    // A bare builtin-type callee (`Vector2(`, `Color(`) gets per-overload constructor arghints, ADDED
    // TO the identifier set (Godot computes these in the completion path, `gdscript_editor.cpp:3411`).
    let mut items =
        builtin_constructor_items(state, callee_name, arg_index, render).unwrap_or_default();
    // Generic call argument → the full in-scope identifier set (reuse the IDENTIFIER renderer). The
    // constructor arghints occupy the leading `sort_text` band (ranks `0..ctor_count`); offset the
    // identifier ranks past it so the client's lexicographic `sort_text` sort keeps the arghints
    // ahead of the candidates (both lists otherwise start their ranks at 0 → a tie the client breaks
    // arbitrarily). A no-op when there are no constructor items.
    let ctor_count = items.len();
    let mut identifiers = identifier_items(state, tree, analyzed, byte, render);
    if ctor_count > 0 {
        for (offset, item) in identifiers.iter_mut().enumerate() {
            item.sort_text = Some(format!("{:05}", ctor_count + offset));
        }
    }
    items.extend(identifiers);
    items
}

/// Per-overload constructor arghints for a bare builtin-type callee (`Vector2(`, `Color(`). Mirrors
/// Godot's "Complete constructor." branch (`gdscript_editor.cpp:3411-3427`): iterate
/// `Variant::get_constructor_list`, skip every overload the active argument index overruns
/// (`arg_idx >= arguments.size()`), and render each survivor via the `_make_arguments_hint`
/// `Type Type(args)` shape — `get_constructor_list` sets `mi.name = mi.return_val.type = type`, so
/// both the name and return read as the type. `None` when the callee isn't a builtin type name (the
/// caller then offers the identifier set alone). These are display-only arghints: the item inserts
/// nothing (an empty edit over the prefix span), so committing one is inert.
fn builtin_constructor_items(
    state: &ServerState,
    callee_name: Option<&str>,
    arg_index: usize,
    render: &RenderCtx,
) -> Option<Vec<CompletionItem>> {
    let name = callee_name?;
    let db = &state.workspace.native;
    let builtin = db.builtin_named(name)?;
    let items: Vec<CompletionItem> = builtin
        .constructors
        .iter()
        // Godot's `if (p_argidx >= E.arguments.size()) continue;` — an overload whose arity the
        // cursor's argument index overruns can't be the one being typed (the no-arg overload is
        // dropped at arg index 0 here).
        .filter(|ctor| arg_index < ctor.params.len())
        .enumerate()
        .map(|(rank, ctor)| {
            let args = ctor
                .params
                .iter()
                .map(|p| format!("{}: {}", db.name_of(p.name), db.display_type(&p.ty, None)))
                .collect::<Vec<_>>()
                .join(", ");
            // `Type Type(args)`: the faithful `_make_arguments_hint` label for a constructor
            // MethodInfo whose name and return type are both the builtin type.
            let detail = format!("{name} {name}({args})");
            constructor_arghint_item(name, &detail, &format!("({args})"), rank, render)
        })
        .collect();
    (!items.is_empty()).then_some(items)
}

/// One display-only constructor-overload arghint item: labelled with the builtin type name, the
/// `Type Type(args)` shape in `detail` (renders on every client) plus a structured
/// `labelDetails.detail` (`(args)`) for clients that advertise `labelDetailsSupport`. The insert is
/// EMPTY — an arghint is informational (Godot surfaces it as a call-hint popup, not a selectable
/// completion), so committing one types nothing rather than re-inserting the type name mid-call.
fn constructor_arghint_item(
    label: &str,
    detail: &str,
    arg_detail: &str,
    rank: usize,
    render: &RenderCtx,
) -> CompletionItem {
    let mut item = build_item_with(
        ItemText {
            label,
            filter: label,
        },
        CompletionItemKind::CONSTRUCTOR,
        // Empty insert: an arghint commits to nothing (the edit replaces the empty prefix span with
        // the empty string). `data` is `Keyword` so `completionItem/resolve` leaves it untouched.
        ItemInsert {
            plain: String::new(),
            snippet: None,
        },
        CompletionData::Keyword,
        rank,
        render,
    );
    // The `Type Type(args)` detail (universal) + structured labelDetails (the client advertised
    // `labelDetailsSupport`); set post-build since `build_item_with` fixes `detail: None` for the
    // lazy-resolve path the doc-bearing contexts use. Mirrors `NodeCandidates::into_items`.
    item.detail = Some(detail.to_string());
    item.label_details = Some(lsp_types::CompletionItemLabelDetails {
        detail: Some(arg_detail.to_string()),
        description: Some(label.to_string()),
    });
    // A display-only arghint inserts nothing; a `.`/`(` commit on it would type the punctuation over
    // an empty edit (harmless but pointless), so it carries no commit characters.
    item.commit_characters = None;
    item
}

/// The enum/bitfield constant names for the active call argument, or `None` when the parameter is
/// not a resolvable enum (caller falls back to identifiers). Resolves the callee to a **native**
/// method via the base type the callee is accessed on (`obj.method(` → type of `obj`; a bare
/// `method(` → the implicit-self type), reads the parameter at `arg_index`, and — when its
/// [`gd_types::TypeRef`] is an enum/bitfield — enumerates that enum's values.
fn call_arg_enum_candidates(
    state: &ServerState,
    uri: &lsp_types::Uri,
    tree: &ParseTree,
    analyzed: &AnalysisResult,
    callee: Option<NodeId>,
    callee_name: Option<&str>,
    arg_index: usize,
) -> Option<Vec<String>> {
    let method_name = callee_name?;
    // The class the method is resolved on: the callee's base (for `base.method(`), else the
    // implicit-self class (for a bare `method(`).
    let class = call_arg_receiver_class(state, uri, tree, analyzed, callee)?;
    let native = &state.workspace.native;
    let (_decl, member) = native.lookup_member(&class, method_name)?;
    let gd_types::NativeMember::Method(m) = member else {
        return None;
    };
    let param = m.params.get(arg_index)?;
    let (scope, name) = match &param.ty {
        gd_types::TypeRef::Enum { scope, name } | gd_types::TypeRef::Bitfield { scope, name } => {
            (scope.map(|s| native.name_of(s)), native.name_of(*name))
        }
        _ => return None,
    };
    let values = native.enum_constants(scope, name);
    if values.is_empty() {
        None
    } else {
        Some(values.into_iter().map(str::to_string).collect())
    }
}

/// The native class a call's method is resolved on. `base.method(` → the resolved native type of
/// `base` (recovered from the callee identifier's enclosing `Subscript`'s base); a bare `method(`
/// (no captured callee, or a non-attribute callee) → the implicit-self class's native type. `None`
/// when no native receiver can be determined.
fn call_arg_receiver_class(
    state: &ServerState,
    uri: &lsp_types::Uri,
    tree: &ParseTree,
    analyzed: &AnalysisResult,
    callee: Option<NodeId>,
) -> Option<String> {
    // `base.method(` — the callee identifier's enclosing attribute Subscript carries the base.
    if let Some(callee_id) = callee {
        if let Some(base_dt) = attribute_callee_base_type(state, uri, tree, analyzed, callee_id) {
            return native_class_of(&base_dt);
        }
    }
    // Bare `method(` — resolve against the implicit-self type (the file's root class), chasing its
    // native root so a script `self` still finds inherited native methods.
    let root_id = tree.root_id()?;
    let dt = analyzed.types.get(root_id);
    native_class_of(dt).or_else(|| {
        let sr = dt.script_type.as_ref()?;
        let xfile = crate::xfile::WorkspaceXFileQuery::new(
            &state.workspace.index,
            &state.workspace.native,
            &state.workspace.analysis_cache,
            crate::xfile::AutoloadEnv::default(),
            &state.workspace.scenes,
            &state.workspace.project.root,
        );
        enumerate::script_chain_native_root(&xfile, &state.workspace.native, sr)
    })
}

/// The native type name a base [`DataType`] denotes for a call receiver — directly for a `Native`
/// kind, otherwise `None` (a `Script`/`Builtin` receiver's enum-param resolution is out of scope —
/// the caller chases the script's native root separately).
fn native_class_of(dt: &DataType) -> Option<String> {
    (dt.kind == DtKind::Native && !dt.native_type.is_empty()).then(|| dt.native_type.clone())
}

/// The resolved type of the **base** of an attribute call: given the callee identifier node of
/// `base.method(`, find its enclosing `Subscript{Attribute}` and return the base expression's type.
/// `None` for a bare callee (no enclosing attribute access).
fn attribute_callee_base_type(
    state: &ServerState,
    uri: &lsp_types::Uri,
    tree: &ParseTree,
    analyzed: &AnalysisResult,
    callee_id: NodeId,
) -> Option<DataType> {
    use gd_syntax::ast::{NodeKind, SubscriptAccess};
    // The callee identifier's enclosing parent is the attribute Subscript whose base we type.
    for id in tree.iter_ids() {
        if let NodeKind::Subscript(s) = &tree.get(id).kind {
            if matches!(s.access, Some(SubscriptAccess::Attribute(Some(a))) if a == callee_id) {
                let base = s.base?;
                // #349: the scene-precise type first — see `crate::scene_nav`.
                if let Some(dt) = crate::scene_nav::scene_type_of_base(state, uri, tree, base) {
                    return Some(dt);
                }
                let dt = analyzed.types.get(base);
                return dt.is_set().then(|| dt.clone());
            }
        }
    }
    None
}

// ===================================================================================================
// SUBSCRIPT / ASSIGN — refinements over the identifier set.
// ===================================================================================================

/// `d[<cursor>` — the literal string keys of a `const` Dictionary base (quoted; Godot's
/// `CODE_COMPLETION_KIND_MEMBER`, emitted here as the standard-legend LSP `PROPERTY` kind this
/// codebase uses for members), ADDED TO the bare-identifier set (Godot `COMPLETION_SUBSCRIPT`,
/// `gdscript_editor.cpp:3613-3624`).
/// Godot offers the keys when `base.value` is a folded `DICTIONARY` constant: it iterates the dict's
/// property list, inserting each key `quote(quote_style)` with `CODE_COMPLETION_KIND_MEMBER`, THEN
/// (additively, when the index is not a literal) calls `_find_identifiers`. gdls mirrors that: it
/// resolves the base identifier to a `const` whose initializer is a Dictionary literal, offers each
/// key that folded to a `FoldedValue::String` (double-quoted, the W17 canonical quote — no editor
/// quote-style coupling), then appends the identifier fallback. A base that does not positively
/// resolve to a `const` dict literal yields the identifier set only (never lie).
fn subscript_items(
    state: &ServerState,
    tree: &ParseTree,
    analyzed: Option<&AnalysisResult>,
    tokens: &[gd_syntax::token::Token],
    byte: usize,
    render: &RenderCtx,
) -> Vec<CompletionItem> {
    let mut items: Vec<CompletionItem> = Vec::new();
    let mut rank: usize = 0;

    // Dict keys first — only when the base positively resolves to a `const` dict literal whose keys
    // folded to strings (Godot's `base.value.get_type() == DICTIONARY` guard; `base.value` is set
    // only for a folded constant). The analysis (and its FoldTable) come from the last good analyze.
    if let Some(analyzed) = analyzed {
        if let Some(keys) = const_dict_subscript_keys(tree, analyzed, tokens, byte) {
            for key in keys {
                let quoted = format!("\"{key}\"");
                items.push(keyword_item(
                    &quoted,
                    CompletionItemKind::PROPERTY,
                    CompletionData::Local,
                    rank,
                    render,
                ));
                rank += 1;
            }
        }
    }

    // Identifier fallback — additive, matching Godot's `_find_identifiers` tail. The identifier
    // builder ranks from 0, so its `sort_text` would tie the dict keys' ranks; that is harmless for
    // a client that re-sorts by `sort_text` then label, and the dict keys (quoted) never collide
    // with bare identifiers. Keys lead because they are pushed first.
    items.extend(identifier_items(state, tree, analyzed, byte, render));
    items
}

/// The literal string keys of the `const` Dictionary that the subscript base in `BASE[<cursor>`
/// resolves to, or `None` when the base is not a `const` dict literal (caller falls back to the
/// identifier set). The base is the identifier token immediately before the `[` nearest at-or-before
/// the cursor; it is resolved to a `const` declaration (a func-local in scope, else a class-level
/// member), whose initializer must be a Dictionary literal. Only keys that folded to a
/// `FoldedValue::String` are returned, in source order (Godot iterates the dict's property list).
fn const_dict_subscript_keys(
    tree: &ParseTree,
    analyzed: &AnalysisResult,
    tokens: &[gd_syntax::token::Token],
    byte: usize,
) -> Option<Vec<String>> {
    let base = subscript_base_identifier(tokens, byte)?;
    // Resolve the const's dict initializer: the scope-aware path first (precise — a func-local in the
    // cursor's suite shadows a class member), then the collapse fallback (#221). When the cursor's `[`
    // is unclosed mid-edit, the pull lexer suppresses the newline, the enclosing function suite never
    // closes, and `locals_in_scope_at` finds nothing — but the `const NAME = {…}` declaration node
    // still exists in the arena, so an arena scan recovers it.
    let init = resolve_const_dict_initializer(tree, byte, &base)
        .or_else(|| arena_scan_const_dict_initializer(tree, &base))?;
    let dict = match &tree.get(init).kind {
        gd_syntax::ast::NodeKind::Dictionary(d) => d,
        _ => return None,
    };
    // De-duplicate: Godot iterates the folded Dictionary's property list (one entry per key), so a
    // literal with a repeated key (itself an analyzer error) still offers that key once. We iterate
    // AST elements, so de-dup here to match Godot's effective set; source order is preserved.
    let mut seen: rustc_hash::FxHashSet<String> = rustc_hash::FxHashSet::default();
    let keys: Vec<String> = dict
        .elements
        .iter()
        .filter_map(|kv| kv.key)
        .filter_map(|key_id| dict_key_string(tree, analyzed, dict.style, key_id))
        .filter(|s| seen.insert(s.clone()))
        .collect();
    if keys.is_empty() {
        None
    } else {
        Some(keys)
    }
}

/// The string value of a const-dict key node, for subscript-key completion. The analyzer's
/// [`FoldedValue::String`] is consulted FIRST (the precise, intact-parse path — a key that folds to a
/// string through const-reference / concatenation is offered exactly as before). When the fold is
/// absent — the #221 unclosed-bracket collapse leaves the func body unreduced, so key nodes never
/// fold — the string is derived from the AST key node, faithful to how Godot folds the two
/// syntactic string-key shapes:
///   - a LUA-style key (`{a = 1}`, `DictStyle::LuaTable` or the single-element ambiguous `None`) is a
///     bare `Identifier` whose name IS the string key;
///   - a PYTHON-style key (`{"a": 1}`) is a string `Literal`.
///
/// Any other key shape (a Python-style non-string literal `{1: x}`, an expression) is NOT a statically
/// known string key and yields `None` — the same exclusion the `FoldedValue::String` filter applied,
/// so the collapse fallback never offers a key Godot's fold would not (never lie).
fn dict_key_string(
    tree: &ParseTree,
    analyzed: &AnalysisResult,
    style: Option<gd_syntax::ast::DictStyle>,
    key_id: NodeId,
) -> Option<String> {
    use gd_syntax::ast::{DictStyle, NodeKind};
    // Fold-first: preserve the intact-parse path byte-for-byte.
    // A Lua-style key folds as a `StringName` (gdscript_parser.cpp:3331-3336), a Python-style one
    // as a `String`. Both are reachable through `d.key`, which is a `get_named` on the dictionary;
    // a `NodePath` key is not, so it is deliberately not offered.
    if let Some(FoldedValue::String(s) | FoldedValue::StringName(s)) = analyzed.folds.get(key_id) {
        return Some(s.clone());
    }
    // AST fallback (collapse): derive the key string syntactically, style-aware.
    match (&tree.get(key_id).kind, style) {
        // Lua-style (and the single-element ambiguous `None`, parsed Lua-style) identifier key.
        (NodeKind::Identifier(i), Some(DictStyle::LuaTable) | None) => Some(i.name.clone()),
        // Python-style quoted string-literal key.
        (NodeKind::Literal(l), Some(DictStyle::PythonDict)) => match &l.value {
            gd_syntax::Literal::String(s) => Some(s.clone()),
            _ => None,
        },
        _ => None,
    }
}

/// Collapse fallback for [`resolve_const_dict_initializer`] (#221): the cursor's `[` is unclosed, so
/// the enclosing function suite never closed and the func-local `const` is invisible to
/// `locals_in_scope_at` — but its declaration node survives in the arena. Scan for a `const` named
/// `name` whose initializer is a Dictionary literal. To stay never-lie under a (pathological)
/// same-name collision, return the initializer ONLY when exactly one such const-dict declaration
/// exists; two same-named const dicts with different keys are genuinely ambiguous under the degraded
/// parse, so we offer no keys (the caller falls back to the identifier set).
fn arena_scan_const_dict_initializer(tree: &ParseTree, name: &str) -> Option<NodeId> {
    use gd_syntax::ast::NodeKind;
    let mut found: Option<NodeId> = None;
    for id in tree.iter_ids() {
        let NodeKind::Constant(c) = &tree.get(id).kind else {
            continue;
        };
        let Some(ident) = c.identifier else { continue };
        let NodeKind::Identifier(i) = &tree.get(ident).kind else {
            continue;
        };
        if i.name != name {
            continue;
        }
        let Some(init) = c.initializer else { continue };
        if !matches!(tree.get(init).kind, NodeKind::Dictionary(_)) {
            continue;
        }
        if found.is_some() {
            return None; // ambiguous same-name const dicts under the collapse → never lie
        }
        found = Some(init);
    }
    found
}

/// The base identifier text in `BASE[<cursor>` — the bare `Identifier` token immediately before the
/// `[` nearest at-or-before the cursor. `None` when no `[` precedes the cursor, the token before it
/// is not an identifier (`f()[`, `arr[0][` → `)`/`]`), or that identifier is the tail of an
/// attribute chain (`a.b[` — `b` is a member access, NOT a resolvable bare `const` name; offering
/// keys for it would lie about a base that did not resolve to the const). Such bases fall through to
/// the identifier set.
fn subscript_base_identifier(tokens: &[gd_syntax::token::Token], byte: usize) -> Option<String> {
    use gd_syntax::token::TokenKind;
    let bracket_idx = tokens
        .iter()
        .enumerate()
        .rev()
        .filter(|(_, t)| t.span.end <= byte)
        .find(|(_, t)| t.kind == TokenKind::BracketOpen)
        .map(|(i, _)| i)?;
    let prev_idx = tokens[..bracket_idx]
        .iter()
        .rposition(|t| is_meaningful(t.kind))?;
    if tokens[prev_idx].kind != TokenKind::Identifier {
        return None;
    }
    // Reject an attribute tail (`a.b[`): a `.` immediately before the identifier means it is a member
    // access, not a bare name resolvable to a `const` declaration.
    if let Some(before_idx) = tokens[..prev_idx]
        .iter()
        .rposition(|t| is_meaningful(t.kind))
    {
        if tokens[before_idx].kind == TokenKind::Period {
            return None;
        }
    }
    Some(tokens[prev_idx].source.to_string())
}

/// The initializer `NodeId` of the `const` named `name` visible at `byte`: a func-local `const` in
/// the cursor's scope (shadowing-correct via [`ParseTree::locals_in_scope_at`]), else a class-level
/// `const` member. `None` when no such `const` exists (a `var`, a same-named non-const member, or an
/// unknown name). The returned node is the const's initializer expression — the caller checks it is a
/// Dictionary literal.
fn resolve_const_dict_initializer(tree: &ParseTree, byte: usize, name: &str) -> Option<NodeId> {
    use gd_syntax::ast::{LocalKind, Member, NodeKind};
    // Func-local `const` in scope (innermost-first, already-declared) shadows a class member.
    for local in tree.locals_in_scope_at(byte) {
        if local.name == name && local.kind == LocalKind::Constant {
            if let NodeKind::Constant(c) = &tree.get(local.source).kind {
                return c.initializer;
            }
        }
    }
    // Class-level `const` member, by name (verify the resolved member is actually a Constant —
    // `members_indices` keys by name across every member kind).
    let root_id = tree.root_id()?;
    let NodeKind::Class(class) = &tree.get(root_id).kind else {
        return None;
    };
    let idx = *class.members_indices.get(name)?;
    if let Member::Constant(cid) = class.members.get(idx)? {
        if let NodeKind::Constant(c) = &tree.get(*cid).kind {
            return c.initializer;
        }
    }
    None
}

/// `x = <cursor>` / `x += <cursor>` — enum members when the assignee is enum-typed (Godot
/// `COMPLETION_ASSIGN` → `_find_enumeration_candidates`), else the full in-scope identifier set.
/// The assignee's type comes from the smallest typed node ending at the `=`.
fn assign_items(
    state: &ServerState,
    tree: &ParseTree,
    analyzed: Option<&AnalysisResult>,
    tokens: &[gd_syntax::token::Token],
    byte: usize,
    render: &RenderCtx,
) -> Vec<CompletionItem> {
    if let Some(analyzed) = analyzed {
        if let Some(values) = assign_enum_candidates(state, tree, analyzed, tokens, byte) {
            return values
                .into_iter()
                .enumerate()
                .map(|(rank, name)| {
                    keyword_item(
                        &name,
                        CompletionItemKind::ENUM_MEMBER,
                        CompletionData::Global { name: name.clone() },
                        rank,
                        render,
                    )
                })
                .collect();
        }
    }
    identifier_items(state, tree, analyzed, byte, render)
}

/// The enum constant names for an `x = ` assignee that is enum-typed, or `None` (caller falls back
/// to identifiers). The assignee is the expression ending at the assignment operator; its type is
/// taken from the smallest typed node ending there. Only a native-rooted enum type resolves to a
/// candidate list (the value names of `DataType::enum_values`).
fn assign_enum_candidates(
    state: &ServerState,
    tree: &ParseTree,
    analyzed: &AnalysisResult,
    tokens: &[gd_syntax::token::Token],
    byte: usize,
) -> Option<Vec<String>> {
    let _ = state;
    // The assignee is the token immediately before the assignment operator; its span END is where
    // the assignee node ends (anchoring on the operator's start would miss the gap of whitespace
    // before `=` — the assignee node ends at the *name*, not at the operator).
    let assignee_end = assignee_token_end(tokens, byte)?;
    let dt = smallest_typed_ending_at(tree, analyzed, assignee_end)?;
    if dt.kind != DtKind::Enum || dt.enum_values.is_empty() {
        return None;
    }
    let mut names: Vec<String> = dt.enum_values.keys().cloned().collect();
    names.sort_unstable();
    Some(names)
}

/// The byte offset where the **assignee** expression ends in `assignee = <cursor>` — the end of the
/// token immediately before the assignment operator nearest at-or-before the cursor. `None` when no
/// assignment operator precedes the cursor, or it has no preceding token (a malformed `= x`).
fn assignee_token_end(tokens: &[gd_syntax::token::Token], byte: usize) -> Option<usize> {
    use gd_syntax::token::TokenKind::*;
    let op_idx = tokens
        .iter()
        .enumerate()
        .rev()
        .filter(|(_, t)| t.span.end <= byte)
        .find(|(_, t)| {
            matches!(
                t.kind,
                Equal
                    | PlusEqual
                    | MinusEqual
                    | StarEqual
                    | StarStarEqual
                    | SlashEqual
                    | PercentEqual
                    | LessLessEqual
                    | GreaterGreaterEqual
                    | AmpersandEqual
                    | PipeEqual
                    | CaretEqual
            )
        })
        .map(|(i, _)| i)?;
    // The assignee token sits just before the operator (skipping layout).
    tokens[..op_idx]
        .iter()
        .rev()
        .find(|t| !matches!(t.kind, Newline | Indent | Dedent | Eof))
        .map(|t| t.span.end)
}

// ===================================================================================================
// SUPER_METHOD / PROPERTY_METHOD — the class's own / parent methods.
// ===================================================================================================

/// `super.<cursor>` — the **parent** class's methods (Godot `COMPLETION_SUPER_METHOD`,
/// `gdscript_editor.cpp:3843`, `_find_identifiers_in_class(p_parent_only = true, only_functions)`).
/// Enumerated from the implicit-self type's **parent** chain ([`enumerate::script_parent_members`]),
/// so a method the current class overrides is still offered (its parent declares it — the very
/// method `super.method()` targets) and a brand-new own method is correctly absent. Restricted to
/// the `Method` kind.
fn super_method_items(
    state: &ServerState,
    tree: &ParseTree,
    analyzed: Option<&AnalysisResult>,
    render: &RenderCtx,
) -> Vec<CompletionItem> {
    let Some(analyzed) = analyzed else {
        return Vec::new();
    };
    let Some(sr) = self_script_ref(tree, analyzed) else {
        return Vec::new();
    };
    let xfile = crate::xfile::WorkspaceXFileQuery::new(
        &state.workspace.index,
        &state.workspace.native,
        &state.workspace.analysis_cache,
        crate::xfile::AutoloadEnv::default(),
        &state.workspace.scenes,
        &state.workspace.project.root,
    );
    enumerate::script_parent_members(&xfile, &state.workspace.native, &sr)
        .into_iter()
        .filter(|m| matches!(m.kind, MemberItemKind::Method))
        .enumerate()
        .map(|(rank, m)| member_item(&m, rank, render))
        .collect()
}

/// The implicit-`self` type's [`gd_analyze::ScriptRef`] — the file's root class as a script ref
/// (the analyzer rewrites an in-file class to a `Script` type before the result escapes). `None`
/// when the root has no resolved script type.
fn self_script_ref(tree: &ParseTree, analyzed: &AnalysisResult) -> Option<gd_analyze::ScriptRef> {
    let root_id = tree.root_id()?;
    let dt = analyzed.types.get(root_id);
    if dt.kind != DtKind::Script {
        return None;
    }
    dt.script_type.clone()
}

/// `var x: int:\n\tget = <cursor>` / `set = <cursor>` — the class's own non-static methods (Godot
/// `COMPLETION_PROPERTY_METHOD`, `gdscript_editor.cpp:3550`), the getter/setter binds a method by
/// name. Enumerated from the implicit-self class, restricted to methods.
fn property_method_items(
    state: &ServerState,
    tree: &ParseTree,
    analyzed: Option<&AnalysisResult>,
    render: &RenderCtx,
) -> Vec<CompletionItem> {
    let Some(analyzed) = analyzed else {
        return Vec::new();
    };
    self_chain_members(state, tree, analyzed)
        .into_iter()
        .filter(|m| matches!(m.kind, MemberItemKind::Method))
        .enumerate()
        .map(|(rank, m)| {
            // The accessor binds a bare method name (no call parens) — never a snippet.
            let item = keyword_item(
                &m.name,
                CompletionItemKind::METHOD,
                CompletionData::Member {
                    owner: data_owner(&m.owner, render),
                    name: m.name.clone(),
                    detail: m.detail.clone(),
                },
                rank,
                render,
            );
            mark_deprecated(item, &m.owner, &m.name, render)
        })
        .collect()
}

/// `var x: T:\n\t<cursor>` — the bare property-accessor keywords `get`/`set` (Godot
/// `COMPLETION_PROPERTY_DECLARATION`, `gdscript_editor.cpp:3543`, which inserts exactly those two as
/// plain text). A fixed two-item list; the keyword inserts the bare word (no parens — the accessor
/// block continues with `:` or `=` after it, which the user types).
fn property_accessor_items(render: &RenderCtx) -> Vec<CompletionItem> {
    ["get", "set"]
        .into_iter()
        .enumerate()
        .map(|(rank, kw)| {
            keyword_item(
                kw,
                CompletionItemKind::KEYWORD,
                CompletionData::Keyword,
                rank,
                render,
            )
        })
        .collect()
}

// ===================================================================================================
// OVERRIDE_METHOD — `[static ]func <cursor>` in a class body: overridable methods with a stub.
// ===================================================================================================

/// `func <cursor>` — or `static func <cursor>` — at class-body statement start: the overridable
/// parent methods, each rendered as a full signature stub (Godot `COMPLETION_OVERRIDE_METHOD`,
/// `gdscript_editor.cpp:3681`).
///
/// **Two sources, in Godot's order** (`gdscript_editor.cpp:3685-3759`):
/// - **Script-parent methods.** Godot's CLASS branch (`:3688-3708`) walks the `extends` chain
///   inserting **every** inherited `FUNCTION` member (not just virtuals — any parent `func` is
///   overridable), skipping ones already seen or already defined in the current class. The
///   `static`-ness must match the cursor's (`:3701`), which the classifier reports as
///   `OverrideMethod { is_static }`: a bare `func` offers only non-`static` parent funcs, a `static
///   func` only `static` ones. The stub is rendered from the
///   declaring file's real parsed signature (params with their **written** default text, never a
///   fabricated default — see [`script_override_stub_item`]).
/// - **Native virtuals** (the chain's native tail, `:3729+`): the `is_virtual` methods (`_ready`,
///   `_process`, …), with their real `(params) -> Ret` from the native DB. At a `static func` cursor
///   Godot builds no virtual list at all and pushes one synthetic `_static_init` in its place
///   (`:3742`), so that single entry is the whole native tail there.
///
/// **A method the class already overrides is skipped** (Godot's `has_function(...) continue`,
/// `:3697`/`:3744`). `self_chain_members` yields the chain **name-first** (the in-file override's own
/// `_ready` before the inherited one), so a first-wins name-dedup makes the own/inherited method
/// shadow the same-named parent method — an already-overridden `_ready` is not re-offered while a
/// fresh `_process` is. The dedup runs across the whole chain (script members + the native tail).
fn override_method_items(
    state: &ServerState,
    tree: &ParseTree,
    analyzed: Option<&AnalysisResult>,
    own_file: Option<gd_project::FileId>,
    is_static: bool,
    render: &RenderCtx,
) -> Vec<CompletionItem> {
    let Some(analyzed) = analyzed else {
        return Vec::new();
    };
    let mut seen: rustc_hash::FxHashSet<String> = rustc_hash::FxHashSet::default();
    let mut items: Vec<CompletionItem> = self_chain_members(state, tree, analyzed)
        .into_iter()
        // First-wins name-dedup across the whole chain (script members + the native tail): an
        // own/inherited method shadows the same-named parent method (the already-overridden skip).
        // The in-file class's own members come first, so a method it already defines is consumed
        // here (the `seen.insert`) and never re-offered.
        .filter(|m| seen.insert(m.name.clone()))
        .filter_map(|m| match &m.owner {
            // Native virtuals: `is_virtual` is set only for native members. Rendered from the
            // member's `detail` (`(params) -> Ret`).
            // A `static func` cursor drops the whole virtual list — see the `_static_init` tail
            // appended below (`gdscript_editor.cpp:3742`).
            MemberOwner::Native(_)
                if !is_static && matches!(m.kind, MemberItemKind::Method) && m.is_virtual =>
            {
                Some(OverrideStub::Native(m))
            }
            // The class's OWN methods (declared in this very file) are not override candidates —
            // they are already defined here (Godot's `current_class->has_function` skip,
            // `gdscript_editor.cpp:3697`). They still consumed their name in `seen` above, so an
            // inherited same-named method is shadowed; they are simply not offered.
            MemberOwner::Script { file, .. } if Some(*file) == own_file => None,
            // Script-PARENT methods: any inherited `func`. `is_static` is not tracked on the
            // enumerated `MemberItem` (always `false`), so the declaring file's interface is the
            // source of truth for the `static`-match filter and the `name_span` the stub renders
            // from. A method whose declarer can't be resolved is dropped (never a fabricated stub).
            // The owner's `inner` chain is intentionally ignored (`..`): override stubs are resolved
            // by file + name (`script_override_stub`), not per-inner-class.
            MemberOwner::Script { file, .. } if matches!(m.kind, MemberItemKind::Method) => {
                let stub = script_override_stub(state, *file, &m.name, is_static)?;
                Some(OverrideStub::Script(m, stub))
            }
            _ => None,
        })
        .enumerate()
        .map(|(rank, stub)| match stub {
            OverrideStub::Native(m) => override_stub_item(&m, rank, render),
            OverrideStub::Script(m, signature) => {
                script_override_stub_item(&m.name, &signature, rank, render)
            }
        })
        .collect();
    // The static tail. Godot swaps the native virtual list for one synthetic entry at a `static func`
    // cursor — `_static_init` is not truly virtual, but it is the one native-side name a static
    // function can "override" (`gdscript_editor.cpp:3742`). It carries no parameters and returns
    // nothing, so it renders as `_static_init() -> void:`. The chain's own dedup set decides whether
    // to offer it: a class that already declares `_static_init` consumed the name above.
    if is_static && !seen.contains(STATIC_INIT) {
        items.push(script_override_stub_item(
            STATIC_INIT,
            &format!("{STATIC_INIT}() -> void"),
            items.len(),
            render,
        ));
    }
    items
}

/// The one name a `static func` cursor can override (`gdscript_editor.cpp:3745`).
const STATIC_INIT: &str = "_static_init";

/// One resolved override-completion entry, tagged by source so the rank-and-render pass can build
/// the right item (a native virtual from its `detail`, a script parent from its reparsed signature).
enum OverrideStub {
    Native(MemberItem),
    Script(MemberItem, String),
}

/// The `name<signature>` override-stub text for a script-parent method (e.g.
/// `do_it(times: int, who, loud: bool = true) -> String`), or `None` when the method's staticness
/// does not match the cursor's (Godot's `is_static != member.function->is_static` skip,
/// `gdscript_editor.cpp:3701`), or when the declaring file / its `func` node / its signature span
/// can't be resolved.
///
/// The signature is the **verbatim source substring** the author wrote — Godot renders
/// `identifier->name + member.function->signature + ":"`, where `signature` is the literal source
/// from the parameter list through the return type, captured by `substr` (`gdscript_parser.cpp:1736`,
/// `gdscript_editor.cpp:3705`). So an untyped parameter stays bare (no synthesized `: Variant`), an
/// absent return annotation appends nothing (no synthesized `-> void`), and every default expression
/// is exactly as typed. The trailing block-opening `:` is dropped here ([`script_override_stub_item`]
/// re-appends it).
fn script_override_stub(
    state: &ServerState,
    file: gd_project::FileId,
    name: &str,
    is_static: bool,
) -> Option<String> {
    let iface = state.workspace.index.interface(file)?;
    let decl = iface
        .members
        .iter()
        .find(|m| m.name == name && m.kind == gd_project::MemberKind::Func)?;
    // A `static func` cursor offers only the parent's `static` methods, and a bare `func` cursor
    // only the non-`static` ones (the `is_static !=` gate).
    if decl.flags.is_static != is_static {
        return None;
    }
    let name_span = decl.name_span;
    let path = state.workspace.index.path(file)?;
    let src = file_text_at(state, path)?;
    let parsed = state.workspace.parse_source(&src);
    let (func_id, func) = function_at_name_span(&parsed.tree, name_span, name)?;
    let signature = verbatim_signature(&parsed.tree, &src, func_id, func)?;
    Some(format!("{name}{signature}"))
}

/// The verbatim `(params) -> Ret` signature source of a `FunctionNode` — the substring from just
/// after the name (the `(`) through the return type, with the block-opening `:` and trailing layout
/// removed. Mirrors Godot's `signature` capture (`gdscript_parser.cpp:1733-1737`): the literal
/// author text, never a reconstruction. `None` when the spans are unusable.
fn verbatim_signature(
    tree: &ParseTree,
    src: &str,
    func_id: NodeId,
    func: &gd_syntax::ast::FunctionNode,
) -> Option<String> {
    let name_id = func.identifier?;
    let after_name = tree.get(name_id).span.end;
    // The slice `name.end..end` is `(params) -> Ret:<layout>` (the parser re-anchors the body Suite
    // *after* the block colon + newline + indent, even for an `@abstract` func that has no real
    // body, so `body.start` always lands past the signature). `func_id` span end is the fallback.
    let end = func
        .body
        .map(|b| tree.get(b).span.start)
        .unwrap_or_else(|| tree.get(func_id).span.end);
    if after_name > end || end > src.len() {
        return None;
    }
    let slice = src.get(after_name..end)?;
    // Cut at the block-opening `:` — the FIRST `:` at bracket depth 0 (a parameter type colon is
    // inside the `(...)` at depth ≥ 1; a dict-literal default `{"a": 1}` is at depth ≥ 1 too; the
    // return arrow `->` carries no colon). An `@abstract` func has NO block colon (no depth-0 `:`),
    // so the whole structural slice is kept — never truncated at a param colon.
    let cut = block_colon_offset(slice).unwrap_or(slice.len());
    Some(slice[..cut].trim_end().to_string())
}

/// The byte offset of the first bracket-depth-0 `:` in a function signature slice (`(params) -> Ret:`)
/// — the block-opening colon — or `None` when there is none (an abstract func). Tracks `()`/`[]`/`{}`
/// depth so a parameter-type colon or a dict-literal-default colon (both at depth ≥ 1) is skipped,
/// and string literals (with `\`-escapes) so a `:`/bracket/quote inside a string default never
/// confuses the scan. A `#` line comment inside a multi-line signature is NOT special-cased (rare;
/// Godot's `substr` would keep the comment verbatim too) — a `:` in such a comment is a known gap.
fn block_colon_offset(slice: &str) -> Option<usize> {
    let mut depth: i32 = 0;
    let mut in_str: Option<char> = None;
    let mut escaped = false;
    for (i, c) in slice.char_indices() {
        match in_str {
            Some(q) => {
                // A `\`-escaped char (incl. an escaped quote `\"`) does not terminate the string —
                // skip it so an odd number of escaped quotes can't corrupt the in-string state.
                if escaped {
                    escaped = false;
                } else if c == '\\' {
                    escaped = true;
                } else if c == q {
                    in_str = None;
                }
            }
            None => match c {
                '"' | '\'' => in_str = Some(c),
                '(' | '[' | '{' => depth += 1,
                ')' | ']' | '}' => depth -= 1,
                ':' if depth == 0 => return Some(i),
                _ => {}
            },
        }
    }
    None
}

/// One script-parent override-method stub item. Mirrors [`override_stub_item`] but takes a
/// pre-rendered `name(params) -> Ret` signature (the native path reads it from `detail`); the label
/// and plain insert are `signature:` and the snippet drops the cursor into an indented body.
fn script_override_stub_item(
    name: &str,
    signature: &str,
    rank: usize,
    render: &RenderCtx,
) -> CompletionItem {
    let display = format!("{signature}:");
    let snippet =
        (render.caps.snippet_support && render.config.snippets).then(|| format!("{display}\n\t$0"));
    build_item_with(
        ItemText {
            label: &display,
            filter: name,
        },
        CompletionItemKind::METHOD,
        ItemInsert {
            plain: display.clone(),
            snippet,
        },
        CompletionData::Keyword,
        rank,
        render,
    )
}

// ---------------------------------------------------------------------------------------------------
// Declaring-file lookup helpers (override stubs). `file_text_at` / `function_at_name_span` /
// `ident_text_node` mirror small private equivalents in `signature_help.rs`; kept local so the two
// cursor features stay decoupled (signature_help is a separate concern). The signature itself is
// the VERBATIM source substring (see `verbatim_signature`), not a reconstruction.
// ---------------------------------------------------------------------------------------------------

/// The text of file `path`: the live VFS buffer if open, else the on-disk contents. `None` when the
/// file is neither open nor readable.
fn file_text_at(state: &ServerState, path: &camino::Utf8Path) -> Option<String> {
    let uri = crate::uri::path_to_file_uri(path)?;
    if let Some(d) = state.vfs.get(uri.as_str()) {
        return Some(d.text());
    }
    std::fs::read_to_string(path.as_std_path()).ok()
}

/// The `FunctionNode` whose identifier span equals `name_span` AND whose identifier text equals
/// `name` (the precise declaring function — the name re-check guards against a coincidental span
/// collision after a re-parse of the possibly-newer declaring file, per `MemberDecl::name_span`'s
/// "validate against live text" contract). `None` when no function matches.
fn function_at_name_span<'a>(
    tree: &'a ParseTree,
    name_span: gd_syntax::ByteSpan,
    name: &str,
) -> Option<(NodeId, &'a gd_syntax::ast::FunctionNode)> {
    use gd_syntax::ast::NodeKind;
    tree.iter_ids().find_map(|id| {
        let NodeKind::Function(f) = &tree.get(id).kind else {
            return None;
        };
        let ident = f.identifier?;
        (tree.get(ident).span == name_span && ident_text_node(tree, ident) == name)
            .then_some((id, f))
    })
}

/// The identifier text of an `Identifier` node, or `""` for any other kind.
fn ident_text_node(tree: &ParseTree, id: gd_syntax::ast::NodeId) -> String {
    match &tree.get(id).kind {
        gd_syntax::ast::NodeKind::Identifier(i) => i.name.clone(),
        _ => String::new(),
    }
}

/// One override-method stub item. The label is `name(params) -> Ret:` (the Godot
/// `COMPLETION_OVERRIDE_METHOD` display, built from the native member's `detail` signature); the
/// insert is the same signature with a snippet body (`…:\n\t$0`) when the snippet gates pass, else
/// just the signature line. `filter_text` is the bare name so the typed `func <prefix>` still
/// filters. The caller offers only native virtuals, whose `detail` is always `(params) -> Ret`.
fn override_stub_item(m: &MemberItem, rank: usize, render: &RenderCtx) -> CompletionItem {
    // The native virtual's `detail` is `(params) -> Return`; build `name(params) -> Ret:`. A
    // defensive `name():` fallback covers a (not-expected) missing detail.
    let signature = match &m.detail {
        Some(d) if d.starts_with('(') => format!("{}{}:", m.name, d),
        _ => format!("{}():", m.name),
    };
    // Snippet body: drop the cursor into an indented body. Canonical one-tab indent (W17).
    let snippet = (render.caps.snippet_support && render.config.snippets)
        .then(|| format!("{signature}\n\t$0"));
    build_item_with(
        ItemText {
            label: &signature,
            filter: &m.name,
        },
        CompletionItemKind::METHOD,
        ItemInsert {
            plain: signature.clone(),
            snippet,
        },
        CompletionData::Keyword,
        rank,
        render,
    )
}

// ===================================================================================================
// DEFERRED — scene-aware `$`/`%`/`get_node` node paths + `load`/`preload` resource paths (M11 P3).
// ===================================================================================================

/// Dispatch a deferred completion context (M11 Phase 3). `NodePath` (`$Rel/Path`, `get_node("…")`,
/// `NodePath("…")`) and `UniqueNodePath` (`%Name`) suggest the scene's node names from the scene(s)
/// that ATTACH the current `.gd` (anti-catalog W10: no scene attached ⇒ empty, never a project-wide
/// guess). `ResourcePath` (`load`/`preload`) suggests `res://` project paths (scripts + scenes). The
/// dormant gd_project resolvers ([`SceneIndex::children_relative_from`] /
/// [`SceneIndex::unique_nodes_in`] / [`SceneIndex::resolve_relative_from`]) do the scene-graph walk.
fn deferred_items(
    state: &ServerState,
    uri: &lsp_types::Uri,
    tokens: &[gd_syntax::token::Token],
    byte: usize,
    reason: DeferredReason,
    render: &RenderCtx,
) -> Vec<CompletionItem> {
    match reason {
        DeferredReason::NodePath => node_path_items(state, uri, tokens, byte, render),
        DeferredReason::UniqueNodePath => unique_node_path_items(state, uri, tokens, byte, render),
        DeferredReason::ResourcePath => resource_path_items(state, tokens, byte, render),
    }
}

/// The current `.gd` buffer's `res://` path — the key into the scene reverse map
/// ([`SceneIndex::scenes_attaching_script`]). `None` for a buffer outside the project root or a
/// non-`file://` URI (then the deferred arms degrade to empty — permissive, never a guess).
fn current_script_res(state: &ServerState, uri: &lsp_types::Uri) -> Option<String> {
    let path = crate::uri::uri_to_path(uri)?;
    let root = gd_project::normalize_path(&state.workspace.project.root);
    let norm = gd_project::normalize_path(&path);
    gd_project::path_to_res(&root, &norm)
}

/// The scenes that attach the current script, sorted for deterministic union ordering
/// ([`SceneIndex::scenes_attaching_script`] iterates a `FxHashSet`). Empty ⇒ no scene attaches this
/// script ⇒ the caller returns an empty completion list (W10).
fn attaching_scenes_sorted(state: &ServerState, script_res: &str) -> Vec<String> {
    let mut scenes: Vec<String> = state
        .workspace
        .scenes
        .scenes_attaching_script(script_res)
        .map(str::to_string)
        .collect();
    scenes.sort_unstable();
    scenes
}

/// The single node `script_res` attaches at within `scene` — the relative base a `$X` resolves
/// against. `None` (skip this scene) when the script attaches at zero or MORE THAN ONE node (the
/// relative base would be ambiguous), mirroring `xfile::resolve_one_scene`.
fn unique_attachment_path<'a>(scene: &'a gd_project::Scene, script_res: &str) -> Option<&'a str> {
    let mut attachment: Option<&str> = None;
    for node in &scene.nodes {
        if node.script.as_deref() == Some(script_res) {
            if attachment.is_some() {
                return None; // multiple attachment nodes — ambiguous relative base
            }
            attachment = Some(&node.path);
        }
    }
    attachment
}

/// `$Rel/Path/<cursor>` / `get_node("Rel/Path/<cursor>")` — the child node names reachable from the
/// access base, UNIONED across every scene that attaches the current script. A `%`-rooted string
/// (`get_node("%Name")`) is routed to the unique-name set. Empty when no scene attaches the script
/// (W10) or the base path resolves to nothing.
fn node_path_items(
    state: &ServerState,
    uri: &lsp_types::Uri,
    tokens: &[gd_syntax::token::Token],
    byte: usize,
    render: &RenderCtx,
) -> Vec<CompletionItem> {
    // The committed directory (path up to the last `/`) tells us which node's children to list. Bare
    // `$…` reads it from the token stream; a `get_node("…")` string reads it from inside the literal.
    let committed_dir: String = if let Some((sigil, dir)) =
        completion_context::bare_node_path_committed_dir(tokens, byte)
    {
        // A bare `%…` reaching here (the classifier tags `%` as UniqueNodePath, so this is only the
        // `$` sigil in practice); route a `%` defensively to the unique set.
        if sigil == NodePathSigil::Unique {
            return unique_node_path_items(state, uri, tokens, byte, render);
        }
        dir
    } else if let Some(s) = completion_context::string_node_path_committed_dir(tokens, byte) {
        if s.unique {
            // `get_node("%Name")` is a unique-name access; `%Name/child` (deeper) is deferred.
            return if s.deeper_unique {
                Vec::new()
            } else {
                unique_items_from(state, uri, render)
            };
        }
        s.committed_dir
    } else {
        return Vec::new();
    };

    let Some(script_res) = current_script_res(state, uri) else {
        return Vec::new();
    };
    // Union children across every attaching scene; annotate a name whose type differs across scenes.
    let mut candidates: NodeCandidates = NodeCandidates::default();
    for scene_res in attaching_scenes_sorted(state, &script_res) {
        let Some(scene) = state.workspace.scenes.scene(&scene_res) else {
            continue;
        };
        let Some(attachment) = unique_attachment_path(scene, &script_res) else {
            continue; // ambiguous (multi-attach) — skip this scene from the union
        };
        for (name, root) in
            state
                .workspace
                .scenes
                .children_relative_from(&scene_res, attachment, &committed_dir)
        {
            candidates.add(name, node_type_label(&root));
        }
    }
    candidates.into_items(CompletionItemKind::FIELD, render)
}

/// `%<cursor>` (and `get_node("%<cursor>")`) — the owner-unique node names, unioned across every
/// attaching scene. Empty when no scene attaches the script (W10). A `%Name/…` deeper traversal is
/// classified as `UniqueNodePath` but lists nothing (a documented Phase-3 deferral).
fn unique_node_path_items(
    state: &ServerState,
    uri: &lsp_types::Uri,
    tokens: &[gd_syntax::token::Token],
    byte: usize,
    render: &RenderCtx,
) -> Vec<CompletionItem> {
    // Guard the deeper `%Name/child` form (the back-scan crosses `/`, so the classifier still tags
    // it UniqueNodePath): list unique names only when no `/` follows the `%`.
    if bare_unique_has_trailing_path(tokens, byte) {
        return Vec::new();
    }
    unique_items_from(state, uri, render)
}

/// The unique-name candidate set itself (shared by the bare `%` arm and the `get_node("%…")` string
/// arm). Unioned across attaching scenes with cross-scene type ambiguity annotated.
///
/// Unlike [`node_path_items`], this does NOT skip a scene where the script attaches at multiple
/// nodes: `%Name` is OWNER-scoped (resolved against the scene's owner-wide unique table, NOT relative
/// to the attachment node — see [`gd_project::SceneIndex::resolve_unique_in`]), so the unique-name
/// set is well-defined regardless of how many nodes a scene attaches the script to. The multi-attach
/// ambiguity that forces a relative-`$`-path skip simply doesn't apply here.
fn unique_items_from(
    state: &ServerState,
    uri: &lsp_types::Uri,
    render: &RenderCtx,
) -> Vec<CompletionItem> {
    let Some(script_res) = current_script_res(state, uri) else {
        return Vec::new();
    };
    let mut candidates: NodeCandidates = NodeCandidates::default();
    for scene_res in attaching_scenes_sorted(state, &script_res) {
        for (name, root) in state.workspace.scenes.unique_nodes_in(&scene_res) {
            candidates.add(name, node_type_label(&root));
        }
    }
    candidates.into_items(CompletionItemKind::FIELD, render)
}

/// Whether a bare `%Name/…` access has a `/` after the `%` sigil at the cursor (the deeper-traversal
/// form to defer). Reuses the classifier's committed-dir walk: a non-empty committed directory for a
/// `%` sigil means a `/` was crossed.
fn bare_unique_has_trailing_path(tokens: &[gd_syntax::token::Token], byte: usize) -> bool {
    matches!(
        completion_context::bare_node_path_committed_dir(tokens, byte),
        Some((NodePathSigil::Unique, dir)) if !dir.is_empty()
    )
}

/// `load("res://…/<cursor>")` / `preload(...)` — `res://` project paths matching the typed directory
/// prefix. The listing is the union of every project resource Godot's `_get_directory_contents`
/// surfaces with no type filter: the `.gd` script index ([`gd_project::Index::iter_interfaces`]) ∪
/// the `.tscn` scene index ([`gd_project::SceneIndex::iter`]) ∪ arbitrary assets — textures, audio,
/// `.tres`, `.scn`, … — held by the [`gd_project::AssetIndex`]. The asset index covers exactly the
/// files the other two don't (it excludes `.gd`/`.tscn`), so the three sources partition the project
/// tree with no double-listing.
///
/// Each item's **insert text is the FULL `res://…` path** (a file's path, or a subdirectory's
/// `res://…/` prefix). The classifier makes the edit span cover the WHOLE typed string content for a
/// resource path ([`completion_context::string_arg_prefix`]), because a `res://` literal has a
/// mandatory scheme: inserting only a tail would drop the scheme while the user is still typing it
/// (`load("re|")` → `load("src/")`). Replacing the whole content with the full path is correct for
/// any amount of typed prefix.
fn resource_path_items(
    state: &ServerState,
    tokens: &[gd_syntax::token::Token],
    byte: usize,
    render: &RenderCtx,
) -> Vec<CompletionItem> {
    let Some(dir) = completion_context::resource_path_committed_dir(tokens, byte) else {
        return Vec::new();
    };
    // The directory the listing is rooted at, always `res://`-rooted. Until the user has typed a
    // committed `res://…/` directory we anchor at the project root (`res://`); a partial scheme
    // (`re`, `res:/`) is still being typed, so it commits to nothing deeper than the root.
    let prefix = if dir.starts_with("res://") {
        dir
    } else {
        "res://".to_string()
    };

    // Collect every project res:// path: scripts (.gd) + scenes (.tscn) + arbitrary assets.
    let root = gd_project::normalize_path(&state.workspace.project.root);
    let mut res_paths: Vec<String> = Vec::new();
    for (fid, _iface) in state.workspace.index.iter_interfaces() {
        if let Some(path) = state.workspace.index.path(fid) {
            if let Some(res) = gd_project::path_to_res(&root, path) {
                res_paths.push(res);
            }
        }
    }
    for (res, _scene) in state.workspace.scenes.iter() {
        res_paths.push(res.to_string());
    }
    for res in state.workspace.assets().iter() {
        res_paths.push(res.to_string());
    }

    // The immediate entries directly under `prefix`, as their FULL res path: a direct file is its own
    // path; a nested file contributes its subdirectory's `res://…/<seg>/` prefix (offered once).
    // De-dup by full path. Only entries actually under `prefix` count.
    let mut seen: rustc_hash::FxHashSet<String> = rustc_hash::FxHashSet::default();
    let mut entries: Vec<(String, bool)> = Vec::new(); // (full_res_path, is_dir)
    for res in &res_paths {
        let Some(rest) = res.strip_prefix(&prefix) else {
            continue;
        };
        if rest.is_empty() {
            continue;
        }
        match rest.find('/') {
            // A nested file: offer the subdirectory's full prefix once (`res://…/<seg>/`).
            Some(slash) => {
                let dir_full = format!("{}{}/", prefix, &rest[..slash]);
                if seen.insert(dir_full.clone()) {
                    entries.push((dir_full, true));
                }
            }
            // A direct file under the prefix: its own full res path.
            None => {
                if seen.insert(res.clone()) {
                    entries.push((res.clone(), false));
                }
            }
        }
    }
    entries.sort_by(|a, b| a.0.cmp(&b.0));

    entries
        .into_iter()
        .enumerate()
        .map(|(rank, (full, is_dir))| {
            // Insert the FULL res path (the edit spans the whole typed content, so any partially-typed
            // scheme/path is replaced wholesale — `load("re|")` accept `res://src/foo.gd` →
            // `load("res://src/foo.gd")`). The label shows the path after `res://` for readability;
            // the filter is the full path so a typed `res://…` prefix still narrows the list.
            let label = full.strip_prefix("res://").unwrap_or(&full).to_string();
            let kind = if is_dir {
                CompletionItemKind::FOLDER
            } else {
                CompletionItemKind::FILE
            };
            build_item_with(
                ItemText {
                    label: &label,
                    filter: &full,
                },
                kind,
                ItemInsert {
                    plain: full.clone(),
                    snippet: None,
                },
                CompletionData::Keyword,
                rank,
                render,
            )
        })
        .collect()
}

/// The type label a [`gd_project::ResolvedRoot`] presents in a node-path item's detail, SCRIPT-FIRST
/// (mirrors `xfile::resolved_root_to_facts`): an attached script's basename (a `.gd` file the node
/// owns — the more precise type) wins over the native `type=`; a node with only a native type shows
/// that; a node with neither shows `Node` (the permissive bare type Godot itself assigns `$`/`%`).
fn node_type_label(root: &gd_project::ResolvedRoot) -> String {
    if let Some(script) = &root.script {
        // The script basename (`res://ui/health_bar.gd` → `health_bar.gd`) — a stable, readable
        // type hint without resolving the script's `class_name` (which may be absent).
        let base = script.rsplit('/').next().unwrap_or(script.as_str());
        return base.to_string();
    }
    root.native_type
        .clone()
        .unwrap_or_else(|| "Node".to_string())
}

/// Accumulates node-path candidates across scenes, keyed by node NAME, collecting the distinct type
/// labels each name resolves to (so a multi-scene union annotates an ambiguous type as `A | B`).
#[derive(Default)]
struct NodeCandidates {
    /// name → the distinct type labels seen for it (a `BTreeSet` so the joined detail is sorted and
    /// deterministic regardless of scene visit order).
    by_name: std::collections::BTreeMap<String, std::collections::BTreeSet<String>>,
}

impl NodeCandidates {
    /// Record that node `name` resolves to type `ty` in some attaching scene.
    fn add(&mut self, name: String, ty: String) {
        self.by_name.entry(name).or_default().insert(ty);
    }

    /// Render the accumulated candidates as completion items, name-sorted (the `BTreeMap` iterates in
    /// key order). Each item's `detail` (and `labelDetails.description`) is the node's type — a
    /// single type, or the distinct types joined ` | ` when they disagree across scenes. The insert
    /// is the bare node name; `data` is [`CompletionData::Keyword`] so `resolve` leaves the detail
    /// intact (it only fills a `None` detail). `kind` is clamped to the client's value set upstream.
    fn into_items(self, kind: CompletionItemKind, render: &RenderCtx) -> Vec<CompletionItem> {
        self.by_name
            .into_iter()
            .enumerate()
            .map(|(rank, (name, types))| {
                let detail = types.into_iter().collect::<Vec<_>>().join(" | ");
                let mut item = build_item_with(
                    ItemText {
                        label: &name,
                        filter: &name,
                    },
                    kind,
                    ItemInsert {
                        plain: name.clone(),
                        snippet: None,
                    },
                    CompletionData::Keyword,
                    rank,
                    render,
                );
                // The node's type as detail + structured labelDetails.description (the client
                // advertised `labelDetailsSupport`); set post-build since `build_item_with` fixes
                // `detail: None` for the lazy-resolve path the doc-bearing contexts use.
                item.detail = Some(detail.clone());
                item.label_details = Some(lsp_types::CompletionItemLabelDetails {
                    detail: None,
                    description: Some(detail),
                });
                item
            })
            .collect()
    }
}

// ===================================================================================================
// Item construction — the shared W18 projection (textEdit, sortText, filterText, kind, gating).
// ===================================================================================================

/// The text an item inserts, resolved by the caller: a plain replacement plus, when the item is a
/// snippet, the `$…`-bearing snippet form (gated separately so a non-snippet client still gets
/// `plain`). Built by [`name_insert`] (the common bare-name / call-paren case) or directly for the
/// special inserts (override stub, annotation `(`, a quoted key).
struct ItemInsert {
    /// What a non-snippet client inserts (and the `label`/`filter_text` base unless overridden).
    plain: String,
    /// The snippet form (with tab-stops), used only when the client advertises `snippetSupport`
    /// AND `completion.snippets` is on. `None` ⇒ never a snippet (a keyword, a plain type name).
    snippet: Option<String>,
}

/// The label-vs-insert split for an item whose label differs from its inserted text (an override
/// stub labels `_ready() -> void:` but inserts the body; an `@export_range(` labels with the `(`).
/// `filter_text` is what the client filters the typed prefix against (the bare name).
struct ItemText<'a> {
    label: &'a str,
    filter: &'a str,
}

/// Build the `CompletionItem` for `name`, applying every capability gate. `callable` requests a
/// snippet call-paren insertion (subject to the snippet gates). `rank` becomes the fixed-width
/// `sort_text` so a lexicographic client sort reproduces the server's priority order. The common
/// case; threads through [`build_item_with`] so every context shares one gating path.
fn build_item(
    name: &str,
    kind: CompletionItemKind,
    callable: bool,
    data: CompletionData,
    rank: usize,
    render: &RenderCtx,
) -> CompletionItem {
    let insert = name_insert(name, callable, render);
    build_item_with(
        ItemText {
            label: name,
            filter: name,
        },
        kind,
        insert,
        data,
        rank,
        render,
    )
}

/// The bare-name / call-paren insert for `name`: a snippet `name($0)` when `callable` and the
/// snippet gates pass, else a plain `name`. The single place the snippet-call gate is decided.
fn name_insert(name: &str, callable: bool, render: &RenderCtx) -> ItemInsert {
    // Snippet placeholders only when the client supports them AND the user left them on AND the item
    // is callable AND the chosen style isn't `NameOnly`.
    let want_snippet = callable
        && render.caps.snippet_support
        && render.config.snippets
        && render.config.call_argument_style != CallArgumentStyle::NameOnly;
    ItemInsert {
        plain: name.to_string(),
        snippet: want_snippet.then(|| snippet_text(name, render.config.call_argument_style)),
    }
}

/// The shared item projection: clamp the kind, pick the snippet-vs-plain text under the gates, wrap
/// it in an `InsertReplaceEdit`/`TextEdit` per capability, and stamp the fixed-width `sort_text`.
/// Every context routes through here so capability gating stays uniform (the Phase-4 constraint).
fn build_item_with(
    text: ItemText,
    kind: CompletionItemKind,
    insert: ItemInsert,
    data: CompletionData,
    rank: usize,
    render: &RenderCtx,
) -> CompletionItem {
    let kind = clamp_kind(kind, render.caps);
    // Use the snippet text only when the client advertises snippetSupport AND the user left snippets
    // on; otherwise the plain replacement (even if a snippet form was offered).
    let snippet_ok = render.caps.snippet_support && render.config.snippets;
    let (new_text, insert_text_format) = match insert.snippet {
        Some(s) if snippet_ok => (s, Some(InsertTextFormat::SNIPPET)),
        _ => (insert.plain, None),
    };

    // The edit: an InsertReplaceEdit when the client supports it (insert == replace == the prefix
    // span here, since a new-identifier completion replaces exactly the typed word), else a plain
    // TextEdit. The `new_text` carries the snippet (or bare name) either way; `insert_text_format`
    // governs interpretation.
    let text_edit = if render.caps.insert_replace_support {
        CompletionTextEdit::InsertAndReplace(InsertReplaceEdit {
            new_text,
            insert: render.edit_range,
            replace: render.edit_range,
        })
    } else {
        CompletionTextEdit::Edit(TextEdit {
            range: render.edit_range,
            new_text,
        })
    };

    CompletionItem {
        label: text.label.to_string(),
        kind,
        // Fixed-width rank: a lexicographic client sort == the priority order (gopls convention).
        sort_text: Some(format!("{rank:05}")),
        // Filter against the bare name (the snippet's placeholders must not pollute filtering).
        filter_text: Some(text.filter.to_string()),
        text_edit: Some(text_edit),
        insert_text_format,
        // Commit characters (`.`/`(`) only when the client supports them AND this context isn't a
        // string-valued one (annotation arguments), where a punctuation commit mid-string would be
        // surprising. Member / identifier / type / keyword items keep them.
        commit_characters: commit_chars(render.caps, render.suppress_commit),
        // Lazy: documentation + detail are filled by `completionItem/resolve`.
        detail: None,
        documentation: None,
        data: serde_json::to_value(&data).ok(),
        ..Default::default()
    }
}

/// A plain keyword/type-name item — no snippet, never callable (`get`/`set`/`void`, a type name).
/// Routes through [`build_item_with`] so gating (kind clamp, edit shape) stays uniform.
fn keyword_item(
    name: &str,
    kind: CompletionItemKind,
    data: CompletionData,
    rank: usize,
    render: &RenderCtx,
) -> CompletionItem {
    build_item_with(
        ItemText {
            label: name,
            filter: name,
        },
        kind,
        ItemInsert {
            plain: name.to_string(),
            snippet: None,
        },
        data,
        rank,
        render,
    )
}

/// The call-paren snippet for a callable, per the configured style. `ParensWithCursor` ⇒
/// `name($0)` (cursor between the parens); `Parens` ⇒ `name()` (cursor after). `NameOnly` never
/// reaches here (its gate falls to the bare-name branch).
fn snippet_text(name: &str, style: CallArgumentStyle) -> String {
    match style {
        CallArgumentStyle::ParensWithCursor => format!("{name}($0)"),
        CallArgumentStyle::Parens => format!("{name}()"),
        // Unreachable under the `want_snippet` gate, but render a safe bare name if it ever is.
        CallArgumentStyle::NameOnly => name.to_string(),
    }
}

/// The commit characters for an item, or `None` when the client can't handle them. Member/identifier
/// completions accept `.` (chain another access) and `(` (call). Suppressed entirely without the
/// `commitCharactersSupport` capability (LSP: a server must not send them otherwise), and when
/// `suppress` is set — the string-valued annotation-argument context, where committing on `.`/`(`
/// mid-string is a UX wart.
fn commit_chars(caps: &CompletionCaps, suppress: bool) -> Option<Vec<String>> {
    if caps.commit_characters_support && !suppress {
        Some(vec![".".to_string(), "(".to_string()])
    } else {
        None
    }
}

/// Whether a completion request's items should drop commit characters, by context. Only the
/// string-valued annotation-argument words (`@export_range("or_greater"` …) qualify: a `.`/`(`
/// commit would land inside the quoted string. Every other context this phase serves is
/// member-/identifier-/type-/keyword-shaped, where `.` and `(` are reasonable commits.
fn suppress_commit_for(kind: &CompletionKind) -> bool {
    matches!(kind, CompletionKind::AnnotationArguments { .. })
}

/// Clamp a server-chosen kind to the client's `completionItemKind.valueSet` (or the LSP-default
/// 1..=18 set when the client advertised none). A kind outside the supported set becomes `None` —
/// the item still completes, it just shows without a type-specific icon — rather than a number the
/// client promised it can't render.
fn clamp_kind(kind: CompletionItemKind, caps: &CompletionCaps) -> Option<CompletionItemKind> {
    match &caps.kind_value_set {
        Some(set) => set.contains(&kind).then_some(kind),
        None => default_kind_value_set().contains(&kind).then_some(kind),
    }
}

/// Map an enumeration [`MemberItemKind`] to its LSP [`CompletionItemKind`]. Signals map to `EVENT`
/// (the closest standard kind), enum *values* to `ENUM_MEMBER`. Several of these (`EVENT`,
/// `CONSTANT`, `ENUM_MEMBER`) fall outside the original 1..=18 range, so a minimal client may clamp
/// them away — intended.
fn member_kind(kind: MemberItemKind) -> CompletionItemKind {
    match kind {
        MemberItemKind::Method => CompletionItemKind::METHOD,
        MemberItemKind::Property => CompletionItemKind::PROPERTY,
        MemberItemKind::Signal => CompletionItemKind::EVENT,
        MemberItemKind::Constant => CompletionItemKind::CONSTANT,
        MemberItemKind::Enum => CompletionItemKind::ENUM,
        MemberItemKind::EnumValue => CompletionItemKind::ENUM_MEMBER,
        MemberItemKind::Class => CompletionItemKind::CLASS,
    }
}

/// The single-line replace [`Range`] an item's `TextEdit` targets: the typed-prefix span when the
/// cursor sits in a partial word, else a zero-width range at the cursor (a pure insertion). Built
/// from the byte span via the [`PositionMapper`] so it is encoding-correct.
fn prefix_range(mapper: &PositionMapper, prefix: Option<ByteSpan>, cursor: Position) -> Range {
    match prefix {
        Some(span) => mapper.span_to_range(span),
        None => Range::new(cursor, cursor),
    }
}

/// Override stubs insert a complete function signature. If the user requests completion inside an
/// existing partial skeleton (`func do():`, `func ():`, `func do() -> void:`), replacing only the
/// typed prefix leaves stale syntax after the snippet. Widen only this context's edit to the
/// same-line signature tail proven to belong to the declaration.
fn override_method_range(
    mapper: &PositionMapper,
    tokens: &[gd_syntax::token::Token],
    text: &str,
    prefix: Option<ByteSpan>,
    byte: usize,
    cursor: Position,
) -> Range {
    let (start, name_end) = override_name_span(tokens, prefix, byte);
    let end = override_signature_tail_end(text, name_end).unwrap_or(name_end);
    if start <= end {
        mapper.span_to_range(ByteSpan::new(start, end))
    } else {
        prefix_range(mapper, prefix, cursor)
    }
}

/// The name fragment to replace for an override-method completion. `prefix` covers the usual
/// cursor-at-end case; the token fallback covers a client invoking completion with the cursor in the
/// middle of the still-current identifier.
fn override_name_span(
    tokens: &[gd_syntax::token::Token],
    prefix: Option<ByteSpan>,
    byte: usize,
) -> (usize, usize) {
    if let Some(span) = prefix {
        return (span.start, span.end);
    }
    if let Some(t) = tokens.iter().find(|t| {
        t.span.start <= byte
            && byte <= t.span.end
            && (t.kind == gd_syntax::token::TokenKind::Identifier || t.kind.is_identifier())
    }) {
        return (t.span.start, t.span.end);
    }
    (byte, byte)
}

/// End byte of a same-line function-signature tail after the name (`()`, `() -> void`, trailing
/// block `:`), or `None` when the text after the name is not a recognizable signature tail. Stops at
/// comments/newlines and tracks strings/bracket depth so default values cannot fake the block colon.
fn override_signature_tail_end(text: &str, from: usize) -> Option<usize> {
    let from = from.min(text.len());
    let line_end = text[from..]
        .find('\n')
        .map(|rel| from + rel)
        .unwrap_or(text.len());
    let slice = text.get(from..line_end)?;
    let mut depth: i32 = 0;
    let mut in_str: Option<char> = None;
    let mut escaped = false;
    let mut saw_tail = false;
    let mut tail_end = from;
    let mut saw_first = false;

    for (rel, c) in slice.char_indices() {
        let abs = from + rel;
        match in_str {
            Some(q) => {
                saw_tail = true;
                tail_end = abs + c.len_utf8();
                if escaped {
                    escaped = false;
                } else if c == '\\' {
                    escaped = true;
                } else if c == q {
                    in_str = None;
                }
            }
            None => {
                if depth == 0 && c == '#' {
                    break;
                }
                if !saw_first {
                    if c.is_whitespace() {
                        continue;
                    }
                    if !matches!(c, '(' | '-' | ':') {
                        return None;
                    }
                    saw_first = true;
                }
                if !c.is_whitespace() {
                    saw_tail = true;
                    tail_end = abs + c.len_utf8();
                }
                match c {
                    '"' | '\'' => in_str = Some(c),
                    '(' | '[' | '{' => depth += 1,
                    ')' | ']' | '}' => depth -= 1,
                    ':' if depth == 0 => return Some(abs + c.len_utf8()),
                    _ => {}
                }
            }
        }
    }

    saw_tail.then_some(tail_end)
}

// ===================================================================================================
// `data` — the compact, self-sufficient resolve key (W18: never the request params).
// ===================================================================================================

/// The payload round-tripped on `CompletionItem.data` between completion and resolve. Compact and
/// self-sufficient: it carries only what resolve needs to re-find the symbol's documentation and
/// detail — a tag plus a symbol path — and deliberately **not** the request position/params
/// (anti-catalog W18). Untagged variants would be ambiguous, so it is internally tagged on `"k"`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "k", rename_all = "snake_case")]
enum CompletionData {
    /// A member of an enumerated type (`expr.member`, or an implicit-`self` member). `owner` is the
    /// **declaring** class/file (carry-forward (b)) so resolve fetches the long-form doc from the
    /// right source — the native DB for a native member, the declaring file's interface for a
    /// script member (which may differ from the requesting buffer when inherited). `detail` is the
    /// source-derived signature carried over so resolve need not re-enumerate.
    Member {
        owner: CompletionDataOwner,
        name: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        detail: Option<String>,
    },
    /// A global symbol resolved by name against the native DB (a utility / global constant / global
    /// enum value).
    Global { name: String },
    /// A native class name or a project `class_name` (resolve renders its brief/description).
    NativeClass { class: String },
    /// A local / parameter / autoload — nothing to resolve (no doc source), but carried so the round
    /// trip is total and resolve has an explicit no-op branch rather than an unknown-tag fallthrough.
    Local,
    /// A keyword / type-name / annotation-word item with no doc source (`get`/`set`/`void`/an
    /// annotation name / an override stub). A no-op for resolve, like [`CompletionData::Local`].
    Keyword,
}

/// The **declaring** owner of a member item, encoded as a serializable resolve key (carry-forward
/// (b)). Either a native class name (resolve via [`gd_types::NativeDb::lookup_member`]) or a
/// declaring file's URI (resolve via that file's interface — **not** the requesting buffer, which
/// was the Phase-3 bug for inherited / cross-file members). `Unknown` ⇒ no doc source.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "o", rename_all = "snake_case")]
enum CompletionDataOwner {
    /// Declared on a native (engine / builtin) class.
    NativeClass { class: String },
    /// Declared in a project GDScript file: the declaring file's URI plus the inner-class chain
    /// the member lives on (`[]` = the file's top-level class, `["Inner"]` = a nested class). Resolve
    /// descends the chain so an inner-class instance member's doc is read from the inner interface,
    /// not the file root (#152). `inner` is absent on the wire when empty (round-trip compatible with
    /// an older client's data that carried only `uri`).
    ScriptFile {
        uri: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        inner: Vec<String>,
    },
    /// No recoverable declarer — resolve has no doc to add.
    Unknown,
}

// ===================================================================================================
// `completionItem/resolve` — fill documentation + detail lazily; leave ranking/edit fields intact.
// ===================================================================================================

/// `completionItem/resolve`: decode the item's [`CompletionData`] and fill `documentation` +
/// `detail`, mutating the incoming item **in place**. Per the LSP spec the ranking/insertion fields
/// (`sort_text`, `filter_text`, `insert_text`, `text_edit`) are the server's earlier promise — they
/// are left byte-for-byte unchanged. An absent / unparseable `data`, or a symbol that can no longer
/// be found, returns the item with its docs simply not filled (never an error).
#[must_use]
pub fn completion_item_resolve(
    state: &mut ServerState,
    mut item: CompletionItem,
) -> CompletionItem {
    let Some(data) = item.data.as_ref().and_then(|v| {
        serde_json::from_value::<CompletionData>(v.clone())
            .map_err(|e| {
                log::debug!("completionItem/resolve: undecodable data ({e}); leaving item as-is")
            })
            .ok()
    }) else {
        return item;
    };

    let format = state.caps.completion.documentation_format;
    let (detail, documentation) = resolve_doc(state, &data, format);
    if item.detail.is_none() {
        item.detail = detail;
    }
    if let Some(doc) = documentation {
        item.documentation = Some(doc);
    }
    item
}

/// Look up the `(detail, documentation)` for a resolved item. Detail prefers the signature carried
/// in `data` (a `Member`'s source-derived type); documentation is the BBCode description rendered
/// to the client's negotiated prose flavor. Either may be `None` when no source is available.
fn resolve_doc(
    state: &ServerState,
    data: &CompletionData,
    format: ProseFormat,
) -> (Option<String>, Option<Documentation>) {
    match data {
        // No doc source — keywords / locals / annotation words.
        CompletionData::Local | CompletionData::Keyword => (None, None),
        CompletionData::Member {
            owner,
            name,
            detail,
        } => {
            let documentation = resolve_member_doc(state, owner, name, format);
            (detail.clone(), documentation)
        }
        CompletionData::Global { name } => resolve_global_doc(state, name, format),
        CompletionData::NativeClass { class } => resolve_class_doc(state, class, format),
    }
}

/// Documentation for an enumerated member, fetched from its **declaring** owner (carry-forward (b),
/// M8 Phase 4): a native member's BBCode description from the native DB, or a script member's `##`
/// doc comment from the **declaring** file's interface (not the requesting buffer — that was the
/// Phase-3 bug for inherited / cross-file members). Deterministic: the owner is a precise class /
/// file key, never a nondeterministic name-only search. `None` when no doc source exists.
fn resolve_member_doc(
    state: &ServerState,
    owner: &CompletionDataOwner,
    name: &str,
    format: ProseFormat,
) -> Option<Documentation> {
    match owner {
        CompletionDataOwner::NativeClass { class } => {
            resolve_native_member_doc(state, class, name, format)
        }
        CompletionDataOwner::ScriptFile { uri, inner } => {
            resolve_script_member_doc(state, uri, inner, name, format)
        }
        CompletionDataOwner::Unknown => None,
    }
}

/// A native member's long-form BBCode description, from the declaring class via
/// [`gd_types::NativeDb::lookup_member`] / [`gd_types::NativeDb::lookup_builtin_member`]. The
/// `Property`/`Method`/`Signal` variants carry a per-member description; `Enum`/`Constant`/
/// `EnumValue` carry none in the dump (returns `None`). Tried as a class member first, then a
/// builtin member (`Color.RED` resolves through the builtin path).
fn resolve_native_member_doc(
    state: &ServerState,
    class: &str,
    name: &str,
    format: ProseFormat,
) -> Option<Documentation> {
    let db = &state.workspace.native;
    let member = db
        .lookup_member(class, name)
        .map(|(_, m)| m)
        .or_else(|| db.lookup_builtin_member(class, name).map(|(_, m)| m))?;
    let description = native_member_description(member)?;
    if description.is_empty() {
        return None;
    }
    Some(prose_doc(format, description))
}

/// The BBCode description carried by a [`gd_types::NativeMember`], if its variant has one.
fn native_member_description(member: gd_types::NativeMember<'_>) -> Option<&str> {
    use gd_types::NativeMember;
    match member {
        NativeMember::Property(p) => Some(&p.description),
        NativeMember::Method(m) => Some(&m.description),
        NativeMember::Signal(s) => Some(&s.description),
        // Constants and enum members carry docs only in a with-docs dump
        // (`--dump-extension-api-with-docs`); the stock fallback leaves them empty (#456).
        NativeMember::Enum(e) => Some(&e.description),
        NativeMember::Constant(k) => Some(&k.description),
        NativeMember::EnumValue { doc, .. } => Some(doc),
    }
}

/// A script member's `##` doc comment, from the **declaring** file's interface (the file the
/// `CompletionDataOwner::ScriptFile` URI names), descending `inner` to the inner class the member
/// lives on (`[]` = the file's top-level class). An inner-class instance member whose name collides
/// with a root member must read its doc from the inner interface, not the file root (#152). `None`
/// when the file isn't indexed, a chain segment is missing, the member isn't found, or it carries no
/// doc.
fn resolve_script_member_doc(
    state: &ServerState,
    uri: &str,
    inner: &[String],
    name: &str,
    format: ProseFormat,
) -> Option<Documentation> {
    let uri = uri.parse::<lsp_types::Uri>().ok()?;
    let path = crate::uri::uri_to_path(&uri)?;
    let fid = state.workspace.index.file_id(&path)?;
    let mut iface = state.workspace.index.interface(fid)?;
    for seg in inner {
        iface = iface
            .inner
            .iter()
            .find(|c| c.class_name.as_deref() == Some(seg.as_str()))?;
    }
    let decl = iface.members.iter().find(|m| m.name == name)?;
    let doc = decl.doc.as_ref()?;
    // #258: the banner is part of the documentation body, so a member whose `##` block carries
    // only `@deprecated: …` still resolves to something instead of `None`.
    let mut body = String::new();
    crate::docs::append_member_doc(&mut body, format, doc);
    if body.is_empty() {
        return None;
    }
    Some(prose_doc_raw(format, &body))
}

/// Documentation + detail for a global symbol (utility / constant). Utilities render their
/// `(params) -> Return` signature as detail; the dump carries no per-utility description, so
/// documentation stays `None`.
fn resolve_global_doc(
    state: &ServerState,
    name: &str,
    _format: ProseFormat,
) -> (Option<String>, Option<Documentation>) {
    let native = &state.workspace.native;
    if let Some(util) = native.utilities().find(|u| native.name_of(u.name) == name) {
        let params: Vec<String> = util
            .params
            .iter()
            .map(|p| {
                format!(
                    "{}: {}",
                    native.name_of(p.name),
                    native.display_type(&p.ty, None)
                )
            })
            .collect();
        let detail = format!(
            "({}) -> {}",
            params.join(", "),
            native.display_type(&util.return_type, None)
        );
        return (Some(detail), None);
    }
    (None, None)
}

/// Documentation for a native class / project `class_name`: its `brief_description`, then the
/// long-form `description` when distinct, rendered to the client's prose flavor.
fn resolve_class_doc(
    state: &ServerState,
    class: &str,
    format: ProseFormat,
) -> (Option<String>, Option<Documentation>) {
    let Some(c) = state.workspace.native.class_named(class) else {
        return (None, None);
    };
    let mut body = String::new();
    crate::docs::append_doc(&mut body, format, &c.brief_description);
    if c.description != c.brief_description {
        crate::docs::append_doc(&mut body, format, &c.description);
    }
    let body = body.trim();
    if body.is_empty() {
        (None, None)
    } else {
        (None, Some(prose_doc_raw(format, body)))
    }
}

/// Render a BBCode doc string to the client's negotiated prose flavor as LSP [`Documentation`].
fn prose_doc(format: ProseFormat, bbcode: &str) -> Documentation {
    prose_doc_raw(format, &crate::docs::bbcode_to(format, bbcode))
}

/// Wrap already-rendered prose as [`Documentation`] with the matching markup kind.
fn prose_doc_raw(format: ProseFormat, rendered: &str) -> Documentation {
    let kind = match format {
        ProseFormat::Markdown => MarkupKind::Markdown,
        ProseFormat::PlainText => MarkupKind::PlainText,
    };
    Documentation::MarkupContent(MarkupContent {
        kind,
        value: rendered.to_string(),
    })
}

#[cfg(test)]
mod tests;
