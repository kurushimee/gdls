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
//!   in string / new-identifier contexts.

use lsp_types::{
    CompletionItem, CompletionItemKind, CompletionList, CompletionParams, CompletionTextEdit,
    Documentation, InsertReplaceEdit, InsertTextFormat, MarkupContent, MarkupKind, Position, Range,
    TextEdit,
};
use serde::{Deserialize, Serialize};

use gd_analyze::enumerate::{self, MemberItem, MemberItemKind};
use gd_analyze::{AnalysisResult, DataType};
use gd_syntax::ast::{NodeId, ParseTree};
use gd_syntax::ByteSpan;

use crate::completion_context::{classify, CompletionKind};
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
    // cursor, so the client inserts without deleting). Built once and shared by every item.
    let edit_range = prefix_range(&mapper, ctx.prefix, tdp.position);

    let render = RenderCtx {
        caps: &state.caps.completion,
        config: &state.options.completion,
        edit_range,
        uri: uri.as_str().to_string(),
    };

    let items = match &ctx.kind {
        CompletionKind::Attribute { base } => attribute_items(
            state,
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
        // Phase 4 renders the remaining contexts; until then they are well-formed empty lists.
        _ => Vec::new(),
    };

    CompletionList {
        is_incomplete: false,
        items,
    }
}

/// Everything an item-builder needs that does not change between items in one request.
struct RenderCtx<'a> {
    caps: &'a CompletionCaps,
    config: &'a CompletionConfig,
    /// The single-line range every item's `TextEdit` replaces (the typed-prefix span).
    edit_range: Range,
    /// The buffer URI, embedded into a script item's [`CompletionData`] so resolve is self-sufficient.
    uri: String,
}

// ===================================================================================================
// ATTRIBUTE — `expr.<cursor>` member access.
// ===================================================================================================

/// Render member completions for `base.<cursor>`. Resolve the base expression's [`DataType`] from
/// the analysis (the `base` node id when the AST preserved it, else the smallest typed node ending
/// at the dot), then dispatch through [`enumerate::members_of_type`]. An unresolved base ⇒ empty
/// (offer nothing rather than a wrong set) — including the top-level `local.` case where
/// `base: None` and no typed node is recoverable, an honest Phase-3 gap.
fn attribute_items(
    state: &ServerState,
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
    let Some(dt) = resolve_base_type(tree, analyzed, base, tokens, byte) else {
        return Vec::new();
    };
    // Build a project-backed cross-file query. `members_of_type` never calls `autoload_file`, so the
    // autoload map is empty (the same shape `xfile.rs`'s own tests use). The `Rc<AnalysisResult>`
    // the caller holds keeps the analysis alive independently of these shared borrows.
    let xfile = crate::xfile::WorkspaceXFileQuery::new(
        &state.workspace.index,
        &state.workspace.native,
        &state.workspace.analysis_cache,
        rustc_hash::FxHashMap::default(),
    );
    let members = enumerate::members_of_type(dt, &state.workspace.native, &xfile, tree);
    members
        .into_iter()
        .enumerate()
        .map(|(rank, m)| member_item(&m, rank, render))
        .collect()
}

/// Resolve the [`DataType`] to enumerate members of for an ATTRIBUTE context. Prefers the captured
/// `base` node id; when the AST didn't preserve it (`None`), falls back to the smallest typed node
/// whose span ends at the cursor's dot — covering `base.partial` shapes where the base survived as
/// some other node. Returns a set type only (`is_set()`), never `Unresolved`/`Resolving`.
fn resolve_base_type<'a>(
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
/// [`CompletionData`] so resolve doesn't re-enumerate. `data` is keyed by the member name plus a
/// best-effort owner so resolve can re-find the documentation.
fn member_item(m: &MemberItem, rank: usize, render: &RenderCtx) -> CompletionItem {
    let kind = member_kind(m.kind);
    // A member of an enumerated type: resolve re-finds it through the script interface / native DB
    // by name. We don't have the precise declaring class here cheaply, so the key carries the file
    // (for a script member) and the name; resolve searches the script chain + native DB.
    let data = CompletionData::Member {
        file: render.uri.clone(),
        name: m.name.clone(),
        detail: m.detail.clone(),
    };
    let callable = matches!(m.kind, MemberItemKind::Method);
    build_item(&m.name, kind, callable, data, rank, render)
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
            push(
                &m.name,
                member_kind(m.kind),
                callable,
                CompletionData::Member {
                    file: render.uri.clone(),
                    name: m.name.clone(),
                    detail: m.detail.clone(),
                },
                &mut items,
                &mut seen,
                &mut rank,
            );
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
    for (name, _entry) in state.workspace.index.registry().entries() {
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
    // NB: native engine class names (`Node`, `Timer`, …) are NOT offered in IDENTIFIER position
    // this phase — Phase 1 deliberately did not expose a native class-name iterator, and native
    // class names are load-bearing in TYPE positions (Phase 4: TypeName / InheritType), which is
    // the natural home for that enumeration. The project `class_name` registry above already
    // covers user-declared global classes here.

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
        rustc_hash::FxHashMap::default(),
    );
    enumerate::members_of_type(dt, &state.workspace.native, &xfile, tree)
}

// ===================================================================================================
// Item construction — the shared W18 projection (textEdit, sortText, filterText, kind, gating).
// ===================================================================================================

/// Build the `CompletionItem` for `name`, applying every capability gate. `callable` requests a
/// snippet call-paren insertion (subject to the snippet gates). `rank` becomes the fixed-width
/// `sort_text` so a lexicographic client sort reproduces the server's priority order.
fn build_item(
    name: &str,
    kind: CompletionItemKind,
    callable: bool,
    data: CompletionData,
    rank: usize,
    render: &RenderCtx,
) -> CompletionItem {
    let kind = clamp_kind(kind, render.caps);
    // Snippet placeholders only when the client supports them AND the user left them on AND the item
    // is callable AND the chosen style isn't `NameOnly`. Otherwise a plain bare-name edit.
    let want_snippet = callable
        && render.caps.snippet_support
        && render.config.snippets
        && render.config.call_argument_style != CallArgumentStyle::NameOnly;
    let (new_text, insert_text_format) = if want_snippet {
        (
            snippet_text(name, render.config.call_argument_style),
            Some(InsertTextFormat::SNIPPET),
        )
    } else {
        (name.to_string(), None)
    };

    // The edit: an InsertReplaceEdit when the client supports it (insert == replace == the prefix
    // span here, since a new-identifier completion replaces exactly the typed word), else a plain
    // TextEdit. The `new_text` carries the snippet (or bare name) either way; `insert_text_format`
    // governs interpretation.
    let text_edit = if render.caps.insert_replace_support {
        CompletionTextEdit::InsertAndReplace(InsertReplaceEdit {
            new_text: new_text.clone(),
            insert: render.edit_range,
            replace: render.edit_range,
        })
    } else {
        CompletionTextEdit::Edit(TextEdit {
            range: render.edit_range,
            new_text: new_text.clone(),
        })
    };

    CompletionItem {
        label: name.to_string(),
        kind,
        // Fixed-width rank: a lexicographic client sort == the priority order (gopls convention).
        sort_text: Some(format!("{rank:05}")),
        // Filter against the bare name (the snippet's placeholders must not pollute filtering).
        filter_text: Some(name.to_string()),
        text_edit: Some(text_edit),
        insert_text_format,
        // Commit characters only when supported AND this is a member/identifier (not a string/new
        // identifier where a punctuation commit would be surprising). The identifier/attribute
        // contexts this phase serves are member-shaped, so `.` and `(` are reasonable commits.
        commit_characters: commit_chars(render.caps),
        // Lazy: documentation + detail are filled by `completionItem/resolve`.
        detail: None,
        documentation: None,
        data: serde_json::to_value(&data).ok(),
        ..Default::default()
    }
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
/// `commitCharactersSupport` capability (LSP: a server must not send them otherwise).
fn commit_chars(caps: &CompletionCaps) -> Option<Vec<String>> {
    if caps.commit_characters_support {
        Some(vec![".".to_string(), "(".to_string()])
    } else {
        None
    }
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
    /// A member of an enumerated type (`expr.member`, or an implicit-`self` member). `file` is the
    /// requesting buffer (its script chain is searched first); `detail` is the source-derived
    /// signature carried over so resolve need not re-enumerate.
    Member {
        file: String,
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
        CompletionData::Local => (None, None),
        CompletionData::Member { file, name, detail } => {
            let documentation = resolve_member_doc(state, file, name, format);
            (detail.clone(), documentation)
        }
        CompletionData::Global { name } => resolve_global_doc(state, name, format),
        CompletionData::NativeClass { class } => resolve_class_doc(state, class, format),
    }
}

/// Documentation for an enumerated member: the requesting file's script interface carries the
/// member's own `##` doc comment. `None` when the member isn't a script member or carries no doc.
///
/// Native (engine) member documentation is **deferred this phase**: `data` does not carry the
/// declaring class (adding it would touch the Phase-1 [`MemberItem`]), and a name-only search over
/// the native DB would be nondeterministic (`FxHashMap` order) and could return a same-named member
/// of an unrelated class — a "never lie" breach. The member's *signature* is still surfaced (it
/// rides in `data.detail`, captured at enumeration with the correct declaring class); only the
/// long-form prose is withheld until the declaring class is threaded through (Phase 4).
fn resolve_member_doc(
    state: &ServerState,
    file: &str,
    name: &str,
    format: ProseFormat,
) -> Option<Documentation> {
    let uri = file.parse::<lsp_types::Uri>().ok()?;
    let path = crate::uri::uri_to_path(&uri)?;
    let fid = state.workspace.index.file_id(&path)?;
    let iface = state.workspace.index.interface(fid)?;
    let decl = iface.members.iter().find(|m| m.name == name)?;
    let doc = decl.doc.as_ref()?;
    if doc.description.is_empty() {
        return None;
    }
    Some(prose_doc(format, &doc.description))
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
