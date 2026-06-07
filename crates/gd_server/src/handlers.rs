//! LSP request handlers.

use gd_analyze::{find_incoming_calls, find_outgoing_calls, AnalysisResult, Binding, DtKind};
use gd_syntax::ast::{
    ClassNode, ConstantNode, FunctionNode, LiteralNode, NodeId, NodeKind, ParseTree, SignalNode,
    SubscriptAccess, VariableNode,
};
use gd_syntax::Literal;
use gd_types::native_db::NativeClass;
use lsp_types::{
    CallHierarchyIncomingCall, CallHierarchyIncomingCallsParams, CallHierarchyItem,
    CallHierarchyOutgoingCall, CallHierarchyOutgoingCallsParams, CallHierarchyPrepareParams,
    DocumentLink, DocumentLinkParams, DocumentSymbolParams, DocumentSymbolResponse,
    GotoDefinitionParams, GotoDefinitionResponse, Hover, HoverContents, HoverParams, Location,
    MarkupContent, MarkupKind, Position, Range, ReferenceParams, SymbolInformation,
    SymbolKind as LspSymbolKind, Uri, WorkspaceSymbolParams, WorkspaceSymbolResponse,
};
use nucleo_matcher::{Config, Matcher, Utf32Str};
use ropey::Rope;
use rustc_hash::FxHashSet;

use crate::position::PositionMapper;
use crate::server::ServerState;
use crate::uri::{path_to_file_uri, uri_to_path, CanonicalKey};

/// `textDocument/documentSymbol`: project the `gd_syntax` symbol outline into LSP's nested
/// [`lsp_types::DocumentSymbol`] tree — kinds plus byte→position ranges, with the full declaration as
/// `range` and the identifier as `selection_range`. Reads the shared cached parse (the same one
/// `publishDiagnostics` uses), so an edit is parsed once.
pub fn document_symbol(
    state: &mut ServerState,
    params: DocumentSymbolParams,
) -> DocumentSymbolResponse {
    let uri = params.text_document.uri;
    let Some(text) = state.vfs.get(uri.as_str()).map(|d| d.text()) else {
        return DocumentSymbolResponse::Nested(Vec::new());
    };
    let parsed = state.workspace.parse(&CanonicalKey::for_uri(&uri), &text);
    let Some(doc) = state.vfs.get(uri.as_str()) else {
        return DocumentSymbolResponse::Nested(Vec::new());
    };
    let mapper = PositionMapper::new(&doc.rope, state.encoding);

    // A1 handoff: `document_symbols` now always returns a single root Class wrapping members.
    // For an unnamed script the root's `name` is `""` (the parser has no path, so it can't fill
    // it). Fill the empty name here with the file's basename from the document URI so editors
    // render a useful symbol name (`"a.gd"`) instead of an empty string.
    let basename = uri
        .path()
        .as_str()
        .rsplit('/')
        .next()
        .unwrap_or("")
        .to_string();

    let symbols = parsed
        .symbols
        .iter()
        .map(|s| {
            let mut lsp = to_lsp_symbol(s, &mapper);
            if lsp.name.is_empty() && !basename.is_empty() {
                lsp.name = basename.clone();
            }
            lsp
        })
        .collect();
    DocumentSymbolResponse::Nested(symbols)
}

/// Map `gd_syntax`'s frontend symbol kind to LSP's. GDScript signals surface as `EVENT` (the LSP
/// kind editors render for signal/event members).
fn symbol_kind(kind: gd_syntax::SymbolKind) -> LspSymbolKind {
    use gd_syntax::SymbolKind::*;
    match kind {
        Class => LspSymbolKind::CLASS,
        Function => LspSymbolKind::FUNCTION,
        Variable => LspSymbolKind::VARIABLE,
        Property => LspSymbolKind::PROPERTY,
        Constant => LspSymbolKind::CONSTANT,
        Signal => LspSymbolKind::EVENT,
        Enum => LspSymbolKind::ENUM,
        EnumMember => LspSymbolKind::ENUM_MEMBER,
    }
}

#[allow(
    deprecated,
    reason = "lsp_types::DocumentSymbol::deprecated is a (deprecated) non-optional field we must set"
)]
fn to_lsp_symbol(
    sym: &gd_syntax::DocumentSymbol,
    mapper: &PositionMapper,
) -> lsp_types::DocumentSymbol {
    let children: Vec<lsp_types::DocumentSymbol> = sym
        .children
        .iter()
        .map(|c| to_lsp_symbol(c, mapper))
        .collect();
    lsp_types::DocumentSymbol {
        name: sym.name.clone(),
        detail: None,
        kind: symbol_kind(sym.kind),
        tags: None,
        deprecated: None,
        range: mapper.span_to_range(sym.span),
        selection_range: mapper.span_to_range(sym.selection_span),
        children: (!children.is_empty()).then_some(children),
    }
}

// =============================================================================================
// M6-C2: textDocument/documentLink.
// =============================================================================================

/// `textDocument/documentLink`: walk the parse tree and emit a [`DocumentLink`] for every
/// `res://`-path string literal (those produced by `preload("res://…")` / `load("res://…")`
/// and any other expression containing a res-path string). The link's `target` is the
/// `file://` URI of the resolved filesystem path; the `range` covers the string token including
/// its surrounding quotes. Only string literals that start with `"res://"` produce links —
/// `user://`, `uid://`, and plain strings are silently skipped.
pub fn document_link(state: &mut ServerState, params: DocumentLinkParams) -> Vec<DocumentLink> {
    let uri = params.text_document.uri;
    let Some(text) = state.vfs.get(uri.as_str()).map(|d| d.text()) else {
        return Vec::new();
    };
    let parsed = state.workspace.parse(&CanonicalKey::for_uri(&uri), &text);
    let Some(doc) = state.vfs.get(uri.as_str()) else {
        return Vec::new();
    };
    let mapper = PositionMapper::new(&doc.rope, state.encoding);

    let mut links = Vec::new();
    for node_id in parsed.tree.iter_ids() {
        let node = parsed.tree.get(node_id);
        let NodeKind::Literal(LiteralNode {
            value: Literal::String(path),
        }) = &node.kind
        else {
            continue;
        };
        if !path.starts_with("res://") {
            continue;
        }
        // Gate on index membership — only emit a link for paths that resolve to an actually
        // existing project file. `res_to_path` is a pure path-join with no existence check;
        // `resolve_res_path` returns Some only for files that are indexed (i.e. on disk at scan
        // time). A link to a non-existent target is a bug (spec: documentLink scope, a3.md §3).
        let Some(fid) = state.workspace.index.resolve_res_path(path) else {
            continue;
        };
        let Some(abs) = state.workspace.index.path(fid).map(|p| p.to_path_buf()) else {
            continue;
        };
        let Some(target) = path_to_file_uri(&abs) else {
            continue;
        };
        links.push(DocumentLink {
            range: mapper.span_to_range(node.span),
            target: Some(target),
            tooltip: None,
            data: None,
        });
    }
    links
}

// =============================================================================================
// WP-H: hover + definition.
// =============================================================================================

/// `textDocument/hover`: render the analyzer's resolved [`DataType`](gd_analyze::DataType) for the
/// node under the cursor, plus any `--dump-extension-api-with-docs` description text the native DB
/// carries for that class or member. Returns `None` if there's nothing meaningful at the position
/// (LSP wire = `null`, which the client renders as "no hover info").
///
/// The hover path runs through the same parse + analyze caches as `publishDiagnostics`, so a hover
/// during a diagnostic publish doesn't re-parse or re-analyze. The cursor's LSP `Position` is
/// converted to a byte through the per-request [`PositionMapper`] (clamped — out-of-range positions
/// degrade rather than panic) and the [`ParseTree`] picks the smallest containing node with
/// [`ParseTree::innermost_node_at`].
pub fn hover(state: &mut ServerState, params: HoverParams) -> Option<Hover> {
    let tdp = params.text_document_position_params;
    let uri = tdp.text_document.uri.clone();
    let text = state.vfs.get(uri.as_str()).map(|d| d.text())?;
    let parsed = state.workspace.parse(&CanonicalKey::for_uri(&uri), &text);
    let analyzed = analyze_if_gd(state, &uri, &parsed.tree, &text);

    let doc = state.vfs.get(uri.as_str())?;
    let mapper = PositionMapper::new(&doc.rope, state.encoding);
    let byte = mapper.position_to_byte(tdp.position);
    let node_id = parsed.tree.innermost_node_at(byte)?;

    // The analyzer pins resolved types on whichever node it "owns" — usually the assignable
    // (`Variable`/`Constant`/`Parameter`) or the expression (`BinaryOp`, `Call`, …), not the inner
    // identifier slot. Hover should land on a node *with* a type when possible, so fall back from
    // the leaf-most node to the smallest containing node whose `TypeTable` entry is set. The leaf
    // node is still the source of the LSP `range` so the editor highlights exactly what the cursor
    // is over.
    let typed_id = analyzed
        .as_deref()
        .and_then(|a| smallest_typed_containing(&parsed.tree, byte, a))
        .unwrap_or(node_id);
    let leaf_node = parsed.tree.get(node_id);

    // M6-F: when the cursor is on an identifier that's the callee of a resolved cross-file call,
    // render the callee's `MemberDecl` signature instead of (or in addition to) the type label.
    // This supersedes the generic type-table hover for call-site identifiers and gives the user
    // a `func helper(...) -> R` line rather than just the return type.
    let member_sig = analyzed
        .as_deref()
        .and_then(|a| hover_member_signature(state, &parsed.tree, node_id, byte, a));

    let markdown = if let Some(sig) = member_sig {
        sig
    } else {
        render_hover(state, &parsed.tree, node_id, typed_id, analyzed.as_deref())
    };
    if markdown.trim().is_empty() {
        return None;
    }

    Some(Hover {
        contents: HoverContents::Markup(MarkupContent {
            kind: MarkupKind::Markdown,
            value: markdown,
        }),
        range: Some(mapper.span_to_range(leaf_node.span)),
    })
}

/// The smallest-span containing node at `byte` whose [`TypeTable`](gd_analyze::TypeTable) entry is
/// set. Linear over the arena, like `innermost_node_at` — adequate for per-keystroke hover (an LSP
/// request, not a hot loop). Returns `None` if no containing node has a resolved type at all
/// (e.g. the byte is inside an `extends` chain on an empty native DB, where every member is
/// `Unresolved`).
///
/// Tie-break is `>` (strictly smaller widths win), matching
/// [`ParseTree::innermost_node_at`](gd_syntax::ast::ParseTree::innermost_node_at)'s convention so
/// that for nodes sharing an identical span (an `Identifier` wrapped by a one-token `Type` with
/// the same byte extent, for instance) both helpers land on the same `NodeId`.
fn smallest_typed_containing(
    tree: &ParseTree,
    byte: usize,
    analyzed: &AnalysisResult,
) -> Option<NodeId> {
    let mut best: Option<(NodeId, u32)> = None;
    for id in tree.iter_ids() {
        let node = tree.get(id);
        if node.span.start <= byte && byte < node.span.end && analyzed.types.get(id).is_set() {
            let width = (node.span.end - node.span.start) as u32;
            match best {
                Some((_, w)) if width > w => {}
                _ => best = Some((id, width)),
            }
        }
    }
    best.map(|(id, _)| id)
}

/// `textDocument/definition`: jump from the identifier under the cursor to its declaration site.
/// Resolution order, mirroring Godot's `gdscript_analyzer.cpp:reduce_identifier` walk
/// (analyzer.cpp:4363 / `reduce_identifier_from_base` at :4024):
///   1. In-file: a class member with that name (variable / constant / function / signal / inner
///      class), found by walking the [`ClassNode::members`] list of the file's root class.
///   2. Cross-file: a project `class_name` registered in the [`gd_project::Index`], resolved via
///      [`gd_project::Index::file_id`] → the indexed file's URI + interface span.
///   3. Native and unknown identifiers return `None` (the LSP wire = `null`); native classes
///      don't have an `extension_api.json`-backed location to jump to.
pub fn definition(
    state: &mut ServerState,
    params: GotoDefinitionParams,
) -> Option<GotoDefinitionResponse> {
    let tdp = params.text_document_position_params;
    let uri = tdp.text_document.uri.clone();
    let text = state.vfs.get(uri.as_str()).map(|d| d.text())?;
    let parsed = state.workspace.parse(&CanonicalKey::for_uri(&uri), &text);

    let doc = state.vfs.get(uri.as_str())?;
    let mapper = PositionMapper::new(&doc.rope, state.encoding);
    let byte = mapper.position_to_byte(tdp.position);
    let node_id = parsed.tree.innermost_node_at(byte)?;

    // (C1) String literal inside preload/load: cursor on `"res://foo.gd"` → jump to foo.gd.
    if let Some(loc) = find_res_path_definition(state, &parsed.tree, node_id) {
        return Some(GotoDefinitionResponse::Scalar(loc));
    }

    let name = cursor_identifier(&parsed.tree, node_id)?;

    // (1) In-file member.
    if let Some(loc) = find_in_file_definition(&parsed.tree, &name, &uri, &mapper) {
        return Some(GotoDefinitionResponse::Scalar(loc));
    }

    // (2) Cross-file `class_name`.
    if let Some(loc) = find_global_class_definition(state, &name) {
        return Some(GotoDefinitionResponse::Scalar(loc));
    }

    // (D) Autoload singleton — last fallback so in-file members and class_name declarations
    // shadow autoload names (Godot's own resolution order: locals → members → class_name → autoload).
    if let Some(loc) = find_autoload_definition(state, &name) {
        return Some(GotoDefinitionResponse::Scalar(loc));
    }

    // (3) Native / unknown: no location.
    None
}

/// Run the analyzer if `uri` points to a `.gd` file. Mirrors the gate used in `publish_diagnostics`
/// — the analyzer is GDScript-specific, and any other extension/scheme just gets parser-only data.
///
/// M5 WP-O4: threads the current request's [`CancellationToken`](gd_analyze::CancellationToken)
/// from [`ServerState::current_token`] into the analyzer's
/// [`gd_analyze::AnalyzeOptions::cancellation`] field via [`analyze_with_request_token`], so a
/// `$/cancelRequest` for this request id flips the token mid-analyze and the analyzer bails on
/// its next 256-node checkpoint.
fn analyze_if_gd(
    state: &mut ServerState,
    uri: &Uri,
    tree: &ParseTree,
    text: &str,
) -> Option<std::rc::Rc<AnalysisResult>> {
    let path = uri_to_path(uri)?;
    if path.extension() != Some("gd") {
        return None;
    }
    Some(analyze_with_request_token(
        state,
        &CanonicalKey::for_uri(uri),
        &path,
        tree,
        text,
    ))
}

/// M5 WP-O4: analyze a `.gd` buffer with the current request's
/// [`CancellationToken`](gd_analyze::CancellationToken) threaded into the analyzer. Cloning the
/// token release the borrow on `state.current_token` before the mutable borrow on
/// `state.workspace` is taken — needed so the two simultaneous &mut borrows on `state` don't
/// alias. Single source of truth for the request-side `analyze_with_options` call shape; all
/// request handlers route through here so a future change to the option set (e.g. wiring in
/// `iter_limit` overrides per-handler) lands in one place.
pub(crate) fn analyze_with_request_token(
    state: &mut ServerState,
    key: &CanonicalKey,
    path: &camino::Utf8Path,
    tree: &ParseTree,
    text: &str,
) -> std::rc::Rc<AnalysisResult> {
    let token = state.current_token.clone();
    state.workspace.analyze_with_options(
        key,
        path,
        tree,
        text,
        gd_analyze::AnalyzeOptions {
            iter_limit: None,
            cancellation: token.as_ref(),
        },
    )
}

/// Render the markdown body of a hover response for the node at `node_id`. Layout:
///   1. fenced `gdscript` code block with the resolved [`DataType`](gd_analyze::DataType) (or, for
///      declaration-anchor identifiers the analyzer doesn't type, the identifier name itself);
///   2. `brief_description` from the native DB, if the resolved type points at one;
///   3. long-form `description` if it adds anything beyond the brief.
fn render_hover(
    state: &ServerState,
    tree: &ParseTree,
    leaf_id: NodeId,
    typed_id: NodeId,
    analyzed: Option<&AnalysisResult>,
) -> String {
    let leaf = tree.get(leaf_id);
    let mut md = String::new();

    // Determine the rendered type label, and the optional class name we'll look up native docs for.
    // The TypeTable entry lookup is on `typed_id` (the widened, type-bearing ancestor), but the
    // declaration-fallback below still uses `leaf` so an `extends Node` keyword span resolves to
    // the `Node` class even when the analyzer pinned the type on the surrounding ClassNode.
    let mut native_lookup: Option<String> = None;

    // Highest precedence — the cursor is directly on a *type name*. The analyzer pins the resolved
    // type on the enclosing class / `extends` node (often the script's own `<Script #N>` meta), not
    // on the hovered type token, so rendering `typed_id`'s type here would surface a useless
    // `<Script #N>` placeholder for `extends Node` / `var x: Condition`. When the leaf identifier
    // names a known native class or a project `class_name`, render that NAME as the signature.
    // (Member-access / preload-path signatures are a richer hover feature tracked as a follow-up in
    // the M5 verification report; those still fall through to the typed-ancestor branch below.)
    let leaf_type_label: Option<String> = match &leaf.kind {
        NodeKind::Identifier(ident) => {
            if state.workspace.native.class_named(&ident.name).is_some() {
                native_lookup = Some(ident.name.clone());
                Some(ident.name.clone())
            } else if state.workspace.index.registry().get(&ident.name).is_some() {
                Some(ident.name.clone())
            } else {
                None
            }
        }
        _ => None,
    };

    if let Some(label) = leaf_type_label {
        md.push_str("```gdscript\n");
        md.push_str(&label);
        md.push('\n');
        md.push_str("```");
    } else if let Some(dt) = analyzed.map(|a| a.types.get(typed_id)) {
        if dt.is_set() {
            md.push_str("```gdscript\n");
            md.push_str(&dt.to_string());
            md.push('\n');
            md.push_str("```");
            if dt.kind == DtKind::Native && !dt.native_type.is_empty() {
                native_lookup = Some(dt.native_type.clone());
            }
        }
    }
    let node = leaf;

    // Declaration-anchor fallback (`extends Node`, the `Node` in `var x: Node`, etc.): if the
    // analyzer didn't put a type on this node but its name matches a known native class, show that
    // class's signature + docs. Catches the most common hover target Claude Code asks about — a
    // type or base-class name in a declaration — that the analyzer pinned on the *parent* node
    // rather than this leaf identifier.
    if native_lookup.is_none() {
        if let NodeKind::Identifier(ident) = &node.kind {
            if state.workspace.native.class_named(&ident.name).is_some() {
                if md.is_empty() {
                    md.push_str("```gdscript\n");
                    md.push_str(&ident.name);
                    md.push('\n');
                    md.push_str("```");
                }
                native_lookup = Some(ident.name.clone());
            }
        }
    }

    if let Some(name) = native_lookup {
        if let Some(class) = state.workspace.native.class_named(&name) {
            append_class_docs(&mut md, class);
        }
    }

    md
}

/// M6-F: when the cursor lands on a Call or Subscript node, resolve the callee's `MemberDecl`
/// from the base expression's type and render its signature. Returns `None` on fall-through
/// (e.g. the base type isn't a project script, or the method isn't in the interface).
///
/// Two cursor positions trigger this path:
/// 1. Cursor at `(` → `innermost_node_at` returns the `Call` node.
/// 2. Cursor inside the callee identifier itself (a child `Identifier`) — handled here by also
///    checking whether the leaf's span is contained in any enclosing Call whose callee is a
///    subscript access with a known base type.
///
/// The signature format is `func name(ParamType, …) -> ReturnType` using the unresolved syntactic
/// type names from [`gd_project::TypeExpr`]; no parameter names are available at the interface
/// level. This matches Godot's `hover` intent for cross-file method calls.
fn hover_member_signature(
    state: &ServerState,
    tree: &ParseTree,
    _leaf_id: NodeId,
    cursor_byte: usize,
    analyzed: &AnalysisResult,
) -> Option<String> {
    use gd_analyze::DtKind;

    // Find a Call node whose span contains the cursor byte. The cursor may be directly on the
    // Call node (at `(`/`)`) or on an inner Identifier child (the callee name) — both cases fall
    // into the same "find the enclosing Call" logic.
    let call_node = tree.iter_ids().find_map(|id| {
        let node = tree.get(id);
        if let NodeKind::Call(c) = &node.kind {
            if node.span.start <= cursor_byte && cursor_byte < node.span.end {
                return Some(c.clone());
            }
        }
        None
    })?;

    // Only subscript calls (`l.helper()`) provide a base whose type we can look up.
    // Bare calls (`helper()`) resolve via the in-class or inherited interface — handled
    // by the existing `render_hover` type-label path; skip them here.
    let callee_id = call_node.callee?;
    let NodeKind::Subscript(sub) = &tree.get(callee_id).kind else {
        return None;
    };
    let base_id = sub.base?;

    // Get the base expression's resolved type — must be a project Script kind.
    let base_dt = analyzed.types.get(base_id);
    if base_dt.kind != DtKind::Script {
        return None;
    }
    let script_ref = base_dt.script_type.as_ref()?;
    let callee_file = script_ref.file;

    // Look up the method name in the callee file's interface.
    let fn_name = &call_node.function_name;
    if fn_name.is_empty() {
        return None;
    }
    let iface = state.workspace.index.interface(callee_file)?;
    let decl = iface.members.iter().find(|m| m.name.as_str() == fn_name)?;

    // Format: `func name(ParamType, …) -> ReturnType`
    let params_str = decl
        .params
        .iter()
        .map(|p| match p {
            gd_project::TypeExpr::Named { path, .. } => path.join("."),
            gd_project::TypeExpr::None => "Variant".to_string(),
        })
        .collect::<Vec<_>>()
        .join(", ");
    let ret_str = match &decl.ty {
        gd_project::TypeExpr::Named { path, .. } => path.join("."),
        gd_project::TypeExpr::None => "void".to_string(),
    };
    let sig = format!("func {}({}) -> {}", fn_name, params_str, ret_str);

    let mut md = String::from("```gdscript\n");
    md.push_str(&sig);
    md.push_str("\n```");
    Some(md)
}

fn append_class_docs(md: &mut String, class: &NativeClass) {
    if !class.brief_description.is_empty() {
        md.push_str("\n\n");
        md.push_str(&class.brief_description);
    }
    // Godot emits `brief_description` and `description` as two distinct strings even when the
    // class has only a short summary; in the with-docs dump they're often equal. Dedupe so the
    // hover doesn't show the same paragraph twice.
    if !class.description.is_empty() && class.description != class.brief_description {
        md.push_str("\n\n");
        md.push_str(&class.description);
    }
}

/// Look for `name` as a member of the file's root class (Godot's `parser->head`). Returns the
/// LSP [`Location`] of the declaration's identifier span — narrower than the whole declaration so
/// the editor's jump lands precisely on the name.
fn find_in_file_definition(
    tree: &ParseTree,
    name: &str,
    uri: &Uri,
    mapper: &PositionMapper,
) -> Option<Location> {
    let root_id = tree.root_id()?;
    let NodeKind::Class(root) = &tree.get(root_id).kind else {
        return None;
    };
    let decl_id = root
        .members
        .iter()
        .find_map(|m| member_named(tree, m, name))?;
    let ident_id = declaration_identifier(tree, decl_id)?;
    Some(Location {
        uri: uri.clone(),
        range: mapper.span_to_range(tree.get(ident_id).span),
    })
}

/// Resolve a class member to a candidate declaration `NodeId` iff its declared name matches.
/// Mirrors [`ClassNode::Member`]'s variant set — every member kind that exposes a name is
/// inspected.
fn member_named(tree: &ParseTree, member: &gd_syntax::ast::Member, name: &str) -> Option<NodeId> {
    use gd_syntax::ast::Member::*;
    let id = match member {
        Class(id) | Constant(id) | Function(id) | Signal(id) | Variable(id) | Enum(id) => *id,
        // `EnumValue` is anonymous-enum housekeeping (no top-level name), `Group` is annotation
        // metadata. Neither is a definition target.
        EnumValue(_) | Group(_) => return None,
    };
    let n = tree.get(id);
    let matches = match &n.kind {
        NodeKind::Class(c) => c.identifier.map(|i| ident_name(tree, i)) == Some(name),
        NodeKind::Constant(ConstantNode {
            identifier: Some(i),
            ..
        }) => ident_name(tree, *i) == name,
        NodeKind::Function(FunctionNode {
            identifier: Some(i),
            ..
        }) => ident_name(tree, *i) == name,
        NodeKind::Signal(SignalNode {
            identifier: Some(i),
            ..
        }) => ident_name(tree, *i) == name,
        NodeKind::Variable(VariableNode {
            identifier: Some(i),
            ..
        }) => ident_name(tree, *i) == name,
        NodeKind::Enum(en) => en.identifier.map(|i| ident_name(tree, i)) == Some(name),
        _ => false,
    };
    matches.then_some(id)
}

/// Pull the identifier `NodeId` out of any member-declaration node — the same one every variant
/// above already accesses, factored so `find_in_file_definition` can hand it to [`PositionMapper`].
fn declaration_identifier(tree: &ParseTree, decl_id: NodeId) -> Option<NodeId> {
    Some(match &tree.get(decl_id).kind {
        NodeKind::Class(ClassNode { identifier, .. })
        | NodeKind::Constant(ConstantNode { identifier, .. })
        | NodeKind::Function(FunctionNode { identifier, .. })
        | NodeKind::Signal(SignalNode { identifier, .. })
        | NodeKind::Variable(VariableNode { identifier, .. })
        | NodeKind::Enum(gd_syntax::ast::EnumNode { identifier, .. }) => (*identifier)?,
        _ => return None,
    })
}

fn ident_name(tree: &ParseTree, id: NodeId) -> &str {
    match &tree.get(id).kind {
        NodeKind::Identifier(i) => &i.name,
        _ => "",
    }
}

/// The identifier name of the node at `id`, or `None` if it isn't an [`NodeKind::Identifier`]. The
/// cursor-resolution gate every position-based nav handler shares (`definition`, `references`,
/// `implementation`, `prepareCallHierarchy`): the request degrades to the LSP `null` wire response
/// when the cursor doesn't land on an identifier.
fn cursor_identifier(tree: &ParseTree, id: NodeId) -> Option<String> {
    match &tree.get(id).kind {
        NodeKind::Identifier(i) => Some(i.name.clone()),
        _ => None,
    }
}

/// Returns `true` when `ident_id` is in a method-or-signal role in `tree`:
/// - The `.identifier` child of a `Function` or `Signal` node (declaration-site click), OR
/// - A `Subscript { access: Attribute(Some(ident_id)) }` operand (call-site attribute click, e.g.
///   `l.helper()` — the cursor lands on `helper`'s Identifier, which is the attribute access node).
///
/// Used to decide whether `textDocument/references` should use the project-wide text scan (correct
/// for method/signal targets that callers reach through body-local typed vars) or the faster
/// `name_referencers` index (correct for class/type/variable targets reachable only via interface-
/// level type annotations). The check is purely structural (O(#nodes), no analyzer involvement)
/// and works identically whether the cursor is on the declaration or a call site.
fn is_method_or_signal_ident(tree: &ParseTree, ident_id: NodeId) -> bool {
    for nid in tree.iter_ids() {
        match &tree.get(nid).kind {
            NodeKind::Function(f) => {
                if f.identifier == Some(ident_id) {
                    return true;
                }
            }
            NodeKind::Signal(s) => {
                if s.identifier == Some(ident_id) {
                    return true;
                }
            }
            NodeKind::Subscript(s) => {
                if matches!(s.access, Some(SubscriptAccess::Attribute(Some(aid))) if aid == ident_id)
                {
                    return true;
                }
            }
            _ => {}
        }
    }
    false
}

/// Resolve a `class_name` to the URI + identifier span of its declaring file's class header. The
/// `class_name` registry tracks the path + identifier name; gdls locates the identifier span by
/// parsing the target's text — but if the target is an *open* buffer, the workspace's parse cache
/// is reused so a navigation hop in a hot file doesn't pay a re-parse. Closed files re-read from
/// disk and parse once (no cache pollution).
///
/// Returns `None` if the name isn't a project class, the path can't be read, or the resulting
/// parse tree has no root identifier (genuinely empty file). I/O failures on the closed path are
/// logged at `warn` so operators can see "definition vanished because the file became unreadable"
/// rather than silently degrading to "no definition found."
fn find_global_class_definition(state: &mut ServerState, name: &str) -> Option<Location> {
    let entry = state.workspace.index.registry().get(name)?;
    let path = entry.path.clone();
    let uri = path_to_file_uri(&path)?;
    let uri_str = uri.as_str().to_owned();

    // Open buffer: reuse the cached parse. Closed file: read + parse directly. The parse cache is
    // content-addressed now (see `workspace::CacheEntry`), so caching this closed-file read would
    // be correct — but a one-shot nav lookup needn't populate the hot open-buffer cache, so we
    // parse directly and leave it uncluttered.
    let (ident_span, text) = if let Some(text) = state.vfs.get(&uri_str).map(|d| d.text()) {
        let parsed = state.workspace.parse(&CanonicalKey::for_uri(&uri), &text);
        (root_class_identifier_span(&parsed.tree)?, text)
    } else {
        let text = match std::fs::read_to_string(path.as_std_path()) {
            Ok(t) => t,
            Err(e) => {
                log::warn!(
                    "could not read {path} for definition lookup of `{name}`: {e}; jump degrades to no-result"
                );
                return None;
            }
        };
        let tree = gd_syntax::parse(&text).tree;
        (root_class_identifier_span(&tree)?, text)
    };

    // Build the location's range against a rope of the target's text. The path may live outside
    // any open buffer, so we materialize a fresh `Rope` for the boundary conversion.
    let rope = ropey::Rope::from_str(&text);
    let mapper = PositionMapper::new(&rope, state.encoding);
    Some(Location {
        uri,
        range: mapper.span_to_range(ident_span),
    })
}

/// M6-C1: resolve a `res://`-path string literal to a [`Location`] at the start of the target
/// file. Called when the cursor's innermost node is a [`NodeKind::Literal`] whose value is a
/// [`Literal::String`] starting with `"res://"`. Non-res strings (e.g. `"user://x"`,
/// format strings, regular text) return `None` — the outer `definition` handler degrades to the
/// normal identifier path.
fn find_res_path_definition(
    state: &ServerState,
    tree: &ParseTree,
    node_id: NodeId,
) -> Option<Location> {
    let NodeKind::Literal(LiteralNode {
        value: Literal::String(path),
    }) = &tree.get(node_id).kind
    else {
        return None;
    };
    if !path.starts_with("res://") {
        return None;
    }
    // Gate on index membership — only emit a Location for paths that resolve to an actually
    // existing, indexed project file. `res_to_path` is a pure path-join with no existence
    // check; `resolve_res_path` returns Some only for indexed (on-disk) files.
    let fid = state.workspace.index.resolve_res_path(path)?;
    let abs = state.workspace.index.path(fid).map(|p| p.to_path_buf())?;
    let uri = path_to_file_uri(&abs)?;
    Some(Location {
        uri,
        range: file_start_range(),
    })
}

/// M6-D: resolve an autoload singleton name to a [`Location`] pointing at the head of the
/// autoload's script file. This is the **last** definition fallback — in-file member lookup and
/// `class_name` registry checks (which honour local shadowing) run first, so `var Save := 1`
/// in the current file correctly shadows an autoload named `Save`.
fn find_autoload_definition(state: &ServerState, name: &str) -> Option<Location> {
    let res_path = state.workspace.project.autoload_script_path(name)?;
    let root = &state.workspace.project.root;
    let abs = gd_project::paths::res_to_path(root, res_path)?;
    let uri = path_to_file_uri(&abs)?;
    Some(Location {
        uri,
        range: file_start_range(),
    })
}

/// The byte span of the root class's identifier, if the tree has one.
fn root_class_identifier_span(tree: &ParseTree) -> Option<gd_syntax::ByteSpan> {
    let root_id = tree.root_id()?;
    let NodeKind::Class(root) = &tree.get(root_id).kind else {
        return None;
    };
    Some(tree.get(root.identifier?).span)
}

// =============================================================================================
// WP-N2: textDocument/references.
// =============================================================================================

/// Load a cross-file candidate the way every analyzing nav handler does: prefer the open buffer,
/// else read disk (logging + returning `None` on an unreadable file), then return the owned text
/// plus the content-addressed cached parse + analysis. Callers build their own `Rope`/`PositionMapper`
/// from the text (the mapper can't outlive a borrow returned from here). Shared by `references`,
/// `incomingCalls`, and `outgoingCalls`; `implementation` parses without analyzing, so it stays
/// separate.
fn load_candidate_analysis(
    state: &mut ServerState,
    path: &camino::Utf8Path,
    uri: &Uri,
    log_ctx: &str,
) -> Option<(
    String,
    std::rc::Rc<gd_syntax::ParseResult>,
    std::rc::Rc<AnalysisResult>,
)> {
    let text = match state.vfs.get(uri.as_str()).map(|d| d.text()) {
        Some(t) => t,
        None => match std::fs::read_to_string(path.as_std_path()) {
            Ok(t) => t,
            Err(e) => {
                log::warn!(
                    "{log_ctx}: skipping candidate {path}: {e}; \
                     cross-file results for that file will be under-reported"
                );
                return None;
            }
        },
    };
    let key = CanonicalKey::for_uri(uri);
    let parsed = state.workspace.parse(&key, &text);
    let analysis = analyze_with_request_token(state, &key, path, &parsed.tree, &text);
    Some((text, parsed, analysis))
}

/// Collect `(path, uri)` for every file the interface-pass index records as referencing `name`,
/// skipping `exclude` (the file the caller already handles) and any path that won't percent-encode
/// into a URI (logged at warn — an unreported reference, same as an unreadable candidate). Shared by
/// `references` and `incomingCalls`.
fn collect_name_referencer_uris(
    index: &gd_project::Index,
    name: &str,
    exclude: Option<&camino::Utf8Path>,
    log_ctx: &str,
) -> Vec<(camino::Utf8PathBuf, Uri)> {
    let mut out = Vec::new();
    for fid in index.name_referencers(name) {
        let Some(p) = index.path(fid).map(|p| p.to_path_buf()) else {
            continue;
        };
        if exclude.is_some_and(|e| normalize_eq(e, &p)) {
            continue;
        }
        match path_to_file_uri(&p) {
            Some(uri) => out.push((p, uri)),
            // warn (not debug) for parity with `load_candidate_analysis`'s unreadable-candidate skip:
            // both silently under-report an otherwise authoritative-looking cross-file result, so an
            // operator on the default log level must see the dropped reference.
            None => log::warn!(
                "{log_ctx}: dropping candidate {p} — path_to_file_uri rejected the path; \
                 cross-file edges from that file will be missing"
            ),
        }
    }
    out
}

/// Group a handler's filtered `Binding::Call` stream into `(key, call-site ranges)` pairs,
/// preserving first-seen order (small N ⇒ linear find). `key_of` derives the grouping key from each
/// binding (callee identity for `outgoingCalls`, caller name for `incomingCalls`) and returns `None`
/// to skip a binding (e.g. a future non-`Call` variant). Shared by both callHierarchy handlers.
fn group_call_ranges<'a, K: PartialEq>(
    bindings: impl Iterator<Item = &'a Binding>,
    mapper: &PositionMapper,
    key_of: impl Fn(&Binding) -> Option<K>,
) -> Vec<(K, Vec<Range>)> {
    let mut groups: Vec<(K, Vec<Range>)> = Vec::new();
    for binding in bindings {
        let Some(key) = key_of(binding) else {
            continue;
        };
        let Binding::Call { call_site, .. } = binding else {
            continue; // key_of only yields Some for Call bindings; defensive for non_exhaustive
        };
        let range = mapper.span_to_range(*call_site);
        if let Some((_, ranges)) = groups.iter_mut().find(|(k, _)| *k == key) {
            ranges.push(range);
        } else {
            groups.push((key, vec![range]));
        }
    }
    groups
}

/// `textDocument/references`: resolve project-wide references for the symbol at the cursor.
///
/// Per LSP 3.17 §textDocument/references: "The references request is sent from the client to the
/// server to resolve project-wide references for the symbol denoted by the given text document
/// position." Returns `Location[]` or `null`. Source: <https://microsoft.github.io/language-server-protocol/specifications/lsp/3.17/specification/#textDocument_references>.
///
/// Algorithm (per the M4 plan §6 + `docs/03 §7.1`, updated for M6-E):
///   1. Resolve cursor → identifier name.
///   2. Choose candidate files:
///      - **Method/signal targets** (M6-E): project-wide textual scan matching Godot's
///        `gdscript_workspace.cpp:472` two-phase strategy — enumerate ALL project files from the
///        index, read text (VFS/disk; no analysis), keep only files whose text contains `name` as a
///        substring. This catches callers that reach the method through a body-local typed var
///        (`var l: Lib = Lib.new(); l.helper()`) that wouldn't appear in `name_referencers`.
///      - **Class/type/variable targets**: `Index::name_referencers(name)` fast-path (interface-pass
///        filter); these can only be reached through interface-level type annotations.
///   3. For each candidate (plus the current buffer): lazy-parse, lazy-analyze, then collect
///      occurrences two ways and de-dupe: (a) the parser-level identifier scan
///      (`push_identifier_locations`) — every `Identifier` node named `name`, covering call-site
///      callees, `extends Foo`, `class_name`, and identifier-typed annotations at the precise
///      identifier range; (b) the analyzer's `Binding::Use` records (`push_binding_locations`) for
///      resolved member/identifier uses (a strict-subset cross-check that de-dupes exactly against
///      the identifier scan). `Binding::Call` is intentionally NOT projected — its span is the whole
///      call expression, and the identifier scan already emits the callee at the correct narrower
///      range, so projecting both double-reported every call site.
///   4. If `params.context.include_declaration`, prepend the declaration site
///      (`find_in_file_definition` / `find_global_class_definition` from the M3 definition path).
///
/// Returns `None` when the cursor doesn't land on an identifier (LSP wire = null). Returns
/// `Some(vec)` (possibly empty) otherwise.
pub fn references(state: &mut ServerState, params: ReferenceParams) -> Option<Vec<Location>> {
    // WP-RD15: the `(uri, text, mapper, name)` prologue this shares with `implementation` is NOT
    // factored into a helper. A shared 4-tuple extractor would hand `implementation` two values it
    // never uses (`text`, and the per-request `mapper` — it works off the resolved class, not the
    // cursor's byte mapping), forcing `_`-prefixed throwaways that read as dead under `-D warnings`.
    // The duplicated handful of lines is clearer than that net-neutral churn; declined permanently.
    let tdp = params.text_document_position;
    let uri = tdp.text_document.uri.clone();
    let text = state.vfs.get(uri.as_str()).map(|d| d.text())?;
    let key = CanonicalKey::for_uri(&uri);
    let parsed = state.workspace.parse(&key, &text);

    let enc = state.encoding;
    // Own the Rope so the mapper doesn't borrow from state.vfs — frees us to call mutating
    // state methods (lazy-analyze, find_global_class_definition) below.
    let rope = Rope::from_str(&text);
    let mapper = PositionMapper::new(&rope, enc);
    let byte = mapper.position_to_byte(tdp.position);
    let node_id = parsed.tree.innermost_node_at(byte)?;
    let name = cursor_identifier(&parsed.tree, node_id)?;

    let mut locations: Vec<Location> = Vec::new();

    if params.context.include_declaration {
        if let Some(loc) = find_in_file_definition(&parsed.tree, &name, &uri, &mapper) {
            locations.push(loc);
        } else if let Some(loc) = find_global_class_definition(state, &name) {
            locations.push(loc);
        }
    }

    // Always scan the current file's bindings — name_referencers is the interface-level filter
    // (cross-file dependents), not the self-references set. The body of the current file may
    // contain many uses of `name` that name_referencers won't surface.
    let current_path = crate::uri::uri_to_path(&uri);
    if let Some(p) = current_path
        .as_ref()
        .filter(|p| p.extension() == Some("gd"))
    {
        let result = analyze_with_request_token(state, &key, p, &parsed.tree, &text);
        push_binding_locations(&mut locations, &result, &name, &uri, &mapper);
        // Also scan parser-level identifier occurrences in the current file (`extends Foo`, type
        // annotations, `class_name`) — the reducer doesn't record these as bindings. Cross-file
        // candidates below already get this scan; without it here, an in-file `extends`/type/
        // `class_name` reference to `name` was silently under-reported. The dedup pass at the end
        // collapses any overlap with the binding scan.
        push_identifier_locations(&mut locations, &parsed.tree, &name, &uri, &mapper);
    }

    // Candidate cross-file referencing files via the interface-pass reverse index (excluding the
    // current file, already scanned above). Collect URIs first so the index borrow doesn't conflict
    // with the lazy-analyze calls below.
    //
    // M6-E: for method/signal targets, callers can reach the method through a body-local typed var
    // (`var l: Lib = Lib.new(); l.helper()`) — the interface pass records `Lib` in the referencing
    // set but not `helper`, so `name_referencers("helper")` misses those callers. This matches
    // Godot's workspace.cpp:472 strategy: a project-wide textual name-scan first (two-phase), then
    // per-hit re-resolve. For method/signal targets we therefore enumerate ALL project files, do a
    // cheap substring pre-filter (`text.contains(name)` — only reads text, not analyze), and pass
    // hits to the existing per-candidate analyze + `push_identifier_locations` loop. For non-method
    // targets (class names, variables) the `name_referencers` fast-path is sufficient: they can
    // only be reached via their interface-level type annotation, which the interface pass records.
    //
    // Cost: one VFS-or-disk read per project file per references request for method/signal targets.
    // This matches Godot's behavior; a future identifier-occurrence index could optimize it. Do NOT
    // full-analyze every file — text-prefilter first, analyze only textual hits.
    let current_fid = current_path
        .as_deref()
        .and_then(|p| state.workspace.index.file_id(p));

    // Detect whether the cursor identifier is in a method-or-signal role. We use a structural AST
    // check (`is_method_or_signal_ident`) rather than an interface lookup because:
    //   1. The interface only contains members of the *declaring* file — a click on a call site in
    //      another file (e.g. `l.helper()` in `a.gd`) won't find `helper` in `a.gd`'s interface.
    //   2. Private (`_`-prefixed) methods appear in the AST regardless of class_name visibility.
    // The check is O(#nodes) on the current file's parse tree — already in cache — with no
    // analyzer call. It handles declaration-click (`func helper():`) and call-site attribute-click
    // (`l.helper()`) identically, so find-references is position-independent (matches Godot).
    let is_method_or_signal = is_method_or_signal_ident(&parsed.tree, node_id);

    // Collect (path, uri) pairs for candidate files. For method/signal targets, enumerate all
    // project files and pre-filter by substring; for others, use the name_referencers fast-path.
    // Either way, exclude the current file (already scanned above).
    let candidates: Vec<(camino::Utf8PathBuf, Uri)> = if is_method_or_signal {
        // Project-wide textual scan: collect all (FileId → path) from the index first (index
        // borrow), then read text separately (VFS / disk borrow) so borrows don't overlap.
        let all_paths: Vec<(gd_project::FileId, camino::Utf8PathBuf)> = state
            .workspace
            .index
            .iter_interfaces()
            .filter_map(|(fid, _)| {
                state
                    .workspace
                    .index
                    .path(fid)
                    .map(|p| (fid, p.to_path_buf()))
            })
            .collect();

        let mut out = Vec::new();
        for (fid, p) in all_paths {
            // Exclude the current file (already scanned above).
            if current_fid.is_some_and(|cfid| cfid == fid) {
                continue;
            }
            let Some(cand_uri) = path_to_file_uri(&p) else {
                log::warn!(
                    "references: dropping candidate {p} — path_to_file_uri rejected the path"
                );
                continue;
            };
            // Pre-filter: only read text (VFS-first, no analysis). If the file's text doesn't
            // contain `name` as a substring, it cannot have a reference — skip it cheaply.
            let text_opt = state
                .vfs
                .get(cand_uri.as_str())
                .map(|d| d.text())
                .or_else(|| std::fs::read_to_string(p.as_std_path()).ok());
            let Some(text) = text_opt else {
                log::warn!(
                    "references: skipping candidate {p} (unreadable); \
                     cross-file results may be under-reported"
                );
                continue;
            };
            if text.contains(name.as_str()) {
                out.push((p, cand_uri));
            }
        }
        out
    } else {
        // Fast-path for class/type/variable names: only files whose interface mentions `name` can
        // reference it; `name_referencers` already has that set.
        let mut candidate_fids: FxHashSet<gd_project::FileId> = FxHashSet::default();
        for fid in state.workspace.index.name_referencers(&name) {
            candidate_fids.insert(fid);
        }
        let mut out = Vec::new();
        for fid in candidate_fids {
            let Some(p) = state.workspace.index.path(fid).map(|p| p.to_path_buf()) else {
                continue;
            };
            if current_path.as_deref().is_some_and(|e| normalize_eq(e, &p)) {
                continue;
            }
            match path_to_file_uri(&p) {
                Some(uri) => out.push((p, uri)),
                None => log::warn!(
                    "references: dropping candidate {p} — path_to_file_uri rejected the path"
                ),
            }
        }
        out
    };

    for (path, cand_uri) in candidates {
        let Some((text, parsed, cand_result)) =
            load_candidate_analysis(state, &path, &cand_uri, "references")
        else {
            continue;
        };
        let rope = Rope::from_str(&text);
        let cand_mapper = PositionMapper::new(&rope, enc);
        push_binding_locations(&mut locations, &cand_result, &name, &cand_uri, &cand_mapper);
        // Also scan identifier-by-name — picks up `extends Foo` and other parser-level refs the
        // reducer doesn't record. De-dupes happen below.
        push_identifier_locations(&mut locations, &parsed.tree, &name, &cand_uri, &cand_mapper);
    }

    // De-duplicate (uri, range) pairs — binding scan + identifier scan can overlap on resolved
    // class/function references that appear in both. Sort by a field-wise tuple key (zero
    // allocation) rather than `format!("{:?}", range)` (two heap Strings per comparison, O(n log n)
    // of them on a common identifier across a 10k-file project).
    let range_key = |r: &Range| (r.start.line, r.start.character, r.end.line, r.end.character);
    locations.sort_by(|a, b| {
        a.uri
            .as_str()
            .cmp(b.uri.as_str())
            .then_with(|| range_key(&a.range).cmp(&range_key(&b.range)))
    });
    locations.dedup_by(|a, b| a.uri.as_str() == b.uri.as_str() && a.range == b.range);

    Some(locations)
}

/// Append a [`Location`] for every [`Binding::Use`] in `result.bindings` whose `target_name` is
/// `name`. [`Binding::Call`] is deliberately excluded (see the body comment): the callee-identifier
/// occurrence of every call is already covered by [`push_identifier_locations`] at the correct,
/// narrower range. The kind filter is intentionally loose for v1 — over-reporting hits for distinct
/// same-named symbols is preferable to under-reporting, and `Index.name_referencers` already
/// narrowed the candidate set before we got here.
fn push_binding_locations(
    out: &mut Vec<Location>,
    result: &AnalysisResult,
    name: &str,
    uri: &Uri,
    mapper: &PositionMapper,
) {
    for binding in result.bindings() {
        // Only `Binding::Use` contributes here. `Binding::Call` is intentionally skipped: its
        // `call_site` spans the WHOLE call expression (`foo(args)`), while
        // `push_identifier_locations` already emits the callee IDENTIFIER (`foo`) at the correct,
        // narrower range — and a `Use` binding is recorded at that same identifier span, so it
        // de-dupes exactly against the identifier scan. Projecting `Call` too added a second, wider
        // Location per call site that the exact-range dedup in `references` could not collapse,
        // double-reporting every call. `call_site` remains the authoritative call
        // range for `callHierarchy` (`from_ranges`), which reads it directly via
        // `find_incoming_calls` — unaffected by this change.
        let Binding::Use {
            target_name, site, ..
        } = binding
        else {
            continue;
        };
        if target_name == name {
            out.push(Location {
                uri: uri.clone(),
                range: mapper.span_to_range(*site),
            });
        }
    }
}

/// Append a [`Location`] for every [`NodeKind::Identifier`] in the parse tree whose name matches.
/// Picks up references the reducer doesn't record as bindings — most importantly `extends Foo`
/// (handled by the resolver, not the reducer), `class_name` declarations, and identifier-typed
/// member declarations (`var x: Foo`). May over-report when an unrelated identifier shares the
/// name; for v1 the over-count is preferable to under-counting.
fn push_identifier_locations(
    out: &mut Vec<Location>,
    tree: &ParseTree,
    name: &str,
    uri: &Uri,
    mapper: &PositionMapper,
) {
    for id in tree.iter_ids() {
        let node = tree.get(id);
        if let NodeKind::Identifier(i) = &node.kind {
            if i.name == name {
                out.push(Location {
                    uri: uri.clone(),
                    range: mapper.span_to_range(node.span),
                });
            }
        }
    }
}

fn normalize_eq(a: &camino::Utf8Path, b: &camino::Utf8Path) -> bool {
    gd_project::normalize_path(a) == gd_project::normalize_path(b)
}

// =============================================================================================
// WP-N3: textDocument/implementation.
// =============================================================================================

/// M6-G: if `fn_name` is a `Func` member of the file identified by `uri`, BFS the inverse-extends
/// graph to find subclasses and return `Location`s for each subclass that also declares a method
/// named `fn_name`. Returns `None` to fall through to the existing class-identifier BFS when the
/// cursor is NOT on a method of the current file's interface (e.g. on a class name or a variable).
fn find_method_overrides(
    state: &mut ServerState,
    fn_name: &str,
    uri: &Uri,
    enc: crate::position::PositionEncoding,
) -> Option<Vec<Location>> {
    // Resolve the current file's FileId and interface.
    let current_path = crate::uri::uri_to_path(uri)?;
    let current_fid = state.workspace.index.file_id(&current_path)?;
    let iface = state.workspace.index.interface(current_fid)?;

    // The cursor must be on a `Func` member of this interface.
    let is_func = iface
        .members
        .iter()
        .any(|m| m.name == fn_name && m.kind == gd_project::MemberKind::Func);
    if !is_func {
        return None;
    }

    // Seed the BFS on the current file's own class_name.
    let seed_name = iface.class_name.clone()?;

    // BFS the inverse-extends graph — same algorithm as the class-identifier branch below.
    let mut known_names: FxHashSet<String> = FxHashSet::default();
    let mut known_files: FxHashSet<gd_project::FileId> = FxHashSet::default();
    known_names.insert(seed_name);
    loop {
        let prev_names = known_names.len();
        let prev_files = known_files.len();
        for (fid, sub_iface) in state.workspace.index.iter_interfaces() {
            if known_files.contains(&fid) {
                continue;
            }
            let parent_name = match &sub_iface.extends {
                gd_project::Extends::Names(parts) => parts.last().map(String::as_str),
                _ => None,
            };
            if parent_name.is_some_and(|p| known_names.contains(p)) {
                known_files.insert(fid);
                if let Some(cn) = &sub_iface.class_name {
                    known_names.insert(cn.clone());
                }
            }
        }
        if known_names.len() == prev_names && known_files.len() == prev_files {
            break;
        }
    }

    // For each known subclass (excluding the cursor's own file), emit a Location for the
    // override's MemberDecl span. If the subclass doesn't override `fn_name`, skip it —
    // method override search only reports files that actually declare the method.
    let mut locations = Vec::new();
    for fid in known_files {
        if fid == current_fid {
            continue;
        }
        let Some(sub_path) = state.workspace.index.path(fid).map(|p| p.to_path_buf()) else {
            continue;
        };
        let Some(cand_uri) = path_to_file_uri(&sub_path) else {
            continue;
        };

        // Check the subclass interface for the override.
        let has_override = state.workspace.index.interface(fid).is_some_and(|i| {
            i.members
                .iter()
                .any(|m| m.name == fn_name && m.kind == gd_project::MemberKind::Func)
        });
        if !has_override {
            continue;
        }

        // Point to the override's span in the subclass file: prefer the MemberDecl.span
        // (the `func` keyword…body start), falling back to file start.
        let override_span = state
            .workspace
            .index
            .interface(fid)
            .and_then(|i| {
                i.members
                    .iter()
                    .find(|m| m.name == fn_name && m.kind == gd_project::MemberKind::Func)
            })
            .map(|m| m.span);

        let range = if let Some(span) = override_span {
            let cand_text = match state.vfs.get(cand_uri.as_str()).map(|d| d.text()) {
                Some(t) => t,
                None => match std::fs::read_to_string(sub_path.as_std_path()) {
                    Ok(t) => t,
                    Err(_) => {
                        locations.push(Location {
                            uri: cand_uri,
                            range: file_start_range(),
                        });
                        continue;
                    }
                },
            };
            let rope = Rope::from_str(&cand_text);
            let cand_mapper = PositionMapper::new(&rope, enc);
            cand_mapper.span_to_range(span)
        } else {
            file_start_range()
        };

        locations.push(Location {
            uri: cand_uri,
            range,
        });
    }

    Some(locations)
}

/// `textDocument/implementation`: list the project classes that extend the class under the cursor
/// (direct + transitive). Per LSP 3.17 §textDocument/implementation, returns
/// `Location | Location[] | LocationLink[] | null`; this impl returns `Location[]`.
///
/// Algorithm (per docs/03 §7.2 — "linear walk over Index.interfaces"):
///   1. Resolve cursor → class name. The `ClassNameRegistry` is consulted **once here** as the
///      name-validity gate (only registered `class_name`s have project subclasses to list); it is
///      not iterated as part of the walk.
///   2. BFS the inverse-of-extends graph over `Index::iter_interfaces` alone:
///      seed `known = {name}`; each pass adds any iface whose extends-chain ends with a known name.
///   3. For each known subclass (excluding the cursor's own class), emit a Location at the file's
///      root class identifier.
///
/// Limitation: when the cursor is on a class member (a `func` / `var`), this v1 still resolves
/// against the enclosing class — finding implementations of the *class*, not overrides of the
/// specific method. Method-level override search is a follow-on; the docs/03 §7.2 design has it
/// scoped to a future WP.
pub fn implementation(
    state: &mut ServerState,
    params: GotoDefinitionParams,
) -> Option<GotoDefinitionResponse> {
    // WP-RD15: the cursor-resolution prologue this shares with `references` is deliberately NOT
    // factored into a shared helper — see the note atop `references`. `implementation` resolves a
    // class and walks the `extends` graph rather than projecting per-request bindings, so a shared
    // `(uri, text, mapper, name)` extractor would force `_`-prefixed throwaways here. Declined.
    let tdp = params.text_document_position_params;
    let uri = tdp.text_document.uri.clone();
    let text = state.vfs.get(uri.as_str()).map(|d| d.text())?;
    let parsed = state.workspace.parse(&CanonicalKey::for_uri(&uri), &text);

    let enc = state.encoding;
    let rope = Rope::from_str(&text);
    let mapper = PositionMapper::new(&rope, enc);
    let byte = mapper.position_to_byte(tdp.position);
    let node_id = parsed.tree.innermost_node_at(byte)?;
    let name = cursor_identifier(&parsed.tree, node_id)?;

    // M6-G: method-override branch — runs BEFORE the class_name gate below so a cursor on a
    // `func` identifier returns override locations rather than null. Detect whether `name` is a
    // `Func` member of the current file's own interface; if so, BFS the subclass graph (seeded on
    // the current file's class_name) and emit a Location for each subclass that overrides `name`.
    if let Some(locs) = find_method_overrides(state, &name, &uri, enc) {
        return Some(GotoDefinitionResponse::Array(locs));
    }

    // Only project class_names participate; native classes have no project subclasses to list.
    state.workspace.index.registry().get(&name)?;

    // BFS the inverse-extends closure over EVERY interface (not just registry entries — a file
    // can extend `Hero` without declaring its own `class_name`). Track known-extender FileIds in
    // a set; class-name keys are still useful for the transitive walk through registered subclasses.
    //
    // WP-RD11 (1) — bench witness, no-op landed. This BFS rescans `iter_interfaces` per fixpoint
    // round (O(depth × files)); a precomputed inverse-of-extends (subclass) index on `Index`,
    // invalidated on mutation, would make it O(subclasses). The Phase-C calibration against a
    // large real-world project (8 GDExtensions) measured `implementation` p99 well under the
    // per-request budget — references (p99 ≈ 310 ms) was the only hot nav handler — so the memoized
    // subclass index is deferred to the first project that actually flags this path, per the plan's
    // "lands OR documented bench witness" rule. `bench/budget.toml` backs the numbers.
    let mut known_names: FxHashSet<String> = FxHashSet::default();
    let mut known_files: FxHashSet<gd_project::FileId> = FxHashSet::default();
    known_names.insert(name.clone());
    loop {
        let prev_names = known_names.len();
        let prev_files = known_files.len();
        for (fid, iface) in state.workspace.index.iter_interfaces() {
            if known_files.contains(&fid) {
                continue;
            }
            // The parent's name is the last identifier in the extends chain (e.g.
            // `extends Outer.Inner` ⇒ parent name = "Inner"; `extends Hero` ⇒ "Hero").
            let parent_name = match &iface.extends {
                gd_project::Extends::Names(parts) => parts.last().map(String::as_str),
                _ => None,
            };
            if parent_name.is_some_and(|p| known_names.contains(p)) {
                known_files.insert(fid);
                if let Some(cn) = &iface.class_name {
                    known_names.insert(cn.clone());
                }
            }
        }
        if known_names.len() == prev_names && known_files.len() == prev_files {
            break;
        }
    }

    // Emit a Location per known subclass file (excluding the cursor's own class file).
    let cursor_fid = state
        .workspace
        .index
        .registry()
        .get(&name)
        .and_then(|e| state.workspace.index.file_id(&e.path));
    let mut locations: Vec<Location> = Vec::new();
    let subclass_paths: Vec<camino::Utf8PathBuf> = known_files
        .iter()
        .filter(|&&fid| Some(fid) != cursor_fid)
        .filter_map(|&fid| state.workspace.index.path(fid).map(|p| p.to_path_buf()))
        .collect();
    for path in subclass_paths {
        let Some(cand_uri) = path_to_file_uri(&path) else {
            log::debug!(
                "implementation: dropping subclass {path} — path_to_file_uri rejected the path; \
                 implementation list under-reports"
            );
            continue;
        };
        let cand_text = match state.vfs.get(cand_uri.as_str()).map(|d| d.text()) {
            Some(t) => t,
            None => match std::fs::read_to_string(path.as_std_path()) {
                Ok(t) => t,
                Err(e) => {
                    log::warn!(
                        "implementation: skipping subclass {path}: {e}; \
                         it will not surface in the result"
                    );
                    continue;
                }
            },
        };
        let cand_parsed = gd_syntax::parse(&cand_text);
        let cand_rope = Rope::from_str(&cand_text);
        let cand_mapper = PositionMapper::new(&cand_rope, enc);
        // Prefer the class-identifier span; fall back to the file start for files without a
        // class_name (most projects don't decorate every script with one). The (0,0)..(0,0)
        // fallback is the documented expected case, not a bug — but log at debug so an
        // operator filing "implementation result clusters at top of file" can correlate.
        let range = match root_class_identifier_span(&cand_parsed.tree) {
            Some(s) => cand_mapper.span_to_range(s),
            None => {
                log::debug!(
                    "implementation: subclass {path} has no root-class identifier; falling \
                     back to range (0,0)..(0,0)"
                );
                file_start_range()
            }
        };
        locations.push(Location {
            uri: cand_uri,
            range,
        });
    }

    Some(GotoDefinitionResponse::Array(locations))
}

// =============================================================================================
// WP-N4: prepareCallHierarchy + incomingCalls + outgoingCalls.
// =============================================================================================

/// `textDocument/prepareCallHierarchy`: identify the function at the cursor and return a
/// [`CallHierarchyItem`] that the client passes back to `callHierarchy/{incoming,outgoing}Calls`.
/// Per LSP 3.17 §textDocument/prepareCallHierarchy, returns `CallHierarchyItem[]` or `null`.
///
/// The `data` field carries the caller function's bare name + this file's URI; the follow-up
/// handlers decode it to look up bindings without re-resolving the cursor position. (Bare, not
/// class-qualified — see `Binding::Call::caller_function`.)
pub fn prepare_call_hierarchy(
    state: &mut ServerState,
    params: CallHierarchyPrepareParams,
) -> Option<Vec<CallHierarchyItem>> {
    // WP-RD7 micro-op — bench witness, no-op landed. The enclosing-function walk below scans the
    // file's `func` declarations once per `prepareCallHierarchy` request; memoizing the file's
    // declared-function-name set per analyze would amortize it. The Phase-C calibration against
    // a large real-world project measured this path well under the per-request budget (references
    // was the only hot nav handler), so the memo is deferred per the plan's "lands OR documented
    // bench witness".
    let tdp = params.text_document_position_params;
    let uri = tdp.text_document.uri.clone();
    let text = state.vfs.get(uri.as_str()).map(|d| d.text())?;
    let parsed = state.workspace.parse(&CanonicalKey::for_uri(&uri), &text);

    let enc = state.encoding;
    let rope = Rope::from_str(&text);
    let mapper = PositionMapper::new(&rope, enc);
    let byte = mapper.position_to_byte(tdp.position);

    // Find the enclosing FunctionNode: walk every node, take the smallest-span Function whose
    // span contains `byte`. Linear over the arena, like `innermost_node_at` and
    // `smallest_typed_containing` above. The natural optimization — `innermost_node_at(byte)`
    // then walk parents — isn't available because `gd_syntax`'s AST is stored as a flat
    // arena without parent back-pointers (an intentional choice for cheap clone + send).
    // O(N) per LSP request is fine for the per-keystroke frequency callers use; M5 may
    // revisit if soak tests reveal it as hot.
    let mut best: Option<(NodeId, u32, &FunctionNode)> = None;
    for id in parsed.tree.iter_ids() {
        let node = parsed.tree.get(id);
        if let NodeKind::Function(f) = &node.kind {
            if node.span.start <= byte && byte < node.span.end {
                let width = (node.span.end - node.span.start) as u32;
                if best.is_none_or(|(_, w, _)| width < w) {
                    best = Some((id, width, f));
                }
            }
        }
    }
    let (fn_id, _w, function) = best?;
    let Some(ident_id) = function.identifier else {
        log::debug!(
            "prepareCallHierarchy: enclosing FunctionNode has no identifier (incomplete \
             `func` header at byte {byte}); returning null"
        );
        return None;
    };
    let fn_name = cursor_identifier(&parsed.tree, ident_id)?;

    let fn_range = mapper.span_to_range(parsed.tree.get(fn_id).span);
    let ident_range = mapper.span_to_range(parsed.tree.get(ident_id).span);

    let data = serde_json::json!({ "uri": uri.as_str(), "name": fn_name });

    #[allow(deprecated)]
    let item = CallHierarchyItem {
        name: fn_name,
        kind: LspSymbolKind::FUNCTION,
        tags: None,
        detail: None,
        uri,
        range: fn_range,
        selection_range: ident_range,
        data: Some(data),
    };
    Some(vec![item])
}

/// Find a function declaration by bare name anywhere in `tree` (root class methods or inner-class
/// methods — the same arena walk [`prepare_call_hierarchy`] uses), returning its full-declaration
/// span (for a [`CallHierarchyItem`]'s `range`) and its identifier span (for `selectionRange`).
/// First match in arena order wins; GDScript has no overloads, and same-named methods across inner
/// classes share the bare-name limitation already documented on [`Binding::Call`]. `None` when no
/// function of that name exists (e.g. the synthetic `<top>` caller for top-level code).
fn function_decl_spans(
    tree: &ParseTree,
    name: &str,
) -> Option<(gd_syntax::ByteSpan, gd_syntax::ByteSpan)> {
    for id in tree.iter_ids() {
        if let NodeKind::Function(f) = &tree.get(id).kind {
            if let Some(ident) = f.identifier {
                if ident_name(tree, ident) == name {
                    return Some((tree.get(id).span, tree.get(ident).span));
                }
            }
        }
    }
    None
}

/// A zero-width LSP range at file start. The documented degrade for a [`CallHierarchyItem`] whose
/// symbol declaration can't be located (native/unresolved callee, the `<top>` caller, or an
/// unreadable file): LSP requires *a* location, and pointing at `(0,0)` is honest ("somewhere in
/// this file") rather than the wrong-but-specific call-site range the pre-fix code shipped.
fn file_start_range() -> Range {
    let zero = Position {
        line: 0,
        character: 0,
    };
    Range {
        start: zero,
        end: zero,
    }
}

/// Resolve a project function's declaration ranges for a [`CallHierarchyItem`]: load the file (open
/// buffer or disk), parse it, locate the `func name` via [`function_decl_spans`], and map its
/// full-declaration span (→ `range`) and identifier span (→ `selectionRange`) through that file's
/// [`PositionMapper`]. Per LSP 3.17 those fields locate the *symbol*, not the call site (call sites
/// are the item's `fromRanges`). Degrades to [`file_start_range`] when the file or function can't be
/// located — never the call-site range, never a panic.
fn resolve_fn_item_ranges(
    state: &mut ServerState,
    path: &camino::Utf8Path,
    uri: &Uri,
    name: &str,
) -> (Range, Range) {
    let text = match state.vfs.get(uri.as_str()).map(|d| d.text()) {
        Some(t) => t,
        None => match std::fs::read_to_string(path.as_std_path()) {
            Ok(t) => t,
            Err(e) => {
                log::debug!(
                    "callHierarchy: cannot read {path} to locate `{name}`'s declaration ({e}); \
                     using a zero-width range at file start"
                );
                return (file_start_range(), file_start_range());
            }
        },
    };
    let parsed = state.workspace.parse(&CanonicalKey::for_uri(uri), &text);
    let Some((decl_span, ident_span)) = function_decl_spans(&parsed.tree, name) else {
        log::debug!(
            "callHierarchy: `{name}` not found as a function declaration in {path}; \
             using a zero-width range at file start"
        );
        return (file_start_range(), file_start_range());
    };
    let rope = Rope::from_str(&text);
    let mapper = PositionMapper::new(&rope, state.encoding);
    (
        mapper.span_to_range(decl_span),
        mapper.span_to_range(ident_span),
    )
}

/// `callHierarchy/outgoingCalls`: read `item.data` to recover (uri, function_name), then filter
/// the analyzed file's bindings for `Binding::Call` entries whose `caller_function == name`.
/// Groups consecutive calls by `(callee_file, callee_name)` so each `CallHierarchyOutgoingCall`
/// reports a `to: CallHierarchyItem` once with every call-site `fromRange`.
///
/// v1 limitation: the group key is `(callee_file, bare callee_name)`, so two distinct in-file
/// functions sharing a name collapse into one `to` group (and resolve to whichever declaration
/// [`function_decl_spans`] finds first). Method-level disambiguation is WP-RD6.
///
/// Per LSP 3.17 §callHierarchy_outgoingCalls the `to` item's `range`/`selectionRange` locate the
/// **callee's declaration** (resolved by loading the callee's file via [`resolve_fn_item_ranges`]),
/// not the call site — call sites are the `from_ranges`. A native/unresolved callee (no project
/// file) degrades to the caller's URI with a zero-width range at file start.
///
/// Returns `Some(vec)` even when empty so the client renders "no outgoing calls" instead of
/// "the request errored".
pub fn outgoing_calls(
    state: &mut ServerState,
    params: CallHierarchyOutgoingCallsParams,
) -> Option<Vec<CallHierarchyOutgoingCall>> {
    let (uri, fn_name) = decode_call_hierarchy_data(&params.item)?;
    let path = crate::uri::uri_to_path(&uri)?;
    let text = match state.vfs.get(uri.as_str()).map(|d| d.text()) {
        Some(t) => t,
        None => {
            // The primary file of the call hierarchy — if it's unreadable we can't compute
            // outgoing calls at all. Log loudly so the user sees "no outgoing calls"
            // wasn't a clean empty answer, and return null to indicate no result. The
            // pre-fix `.ok()?` made this indistinguishable from "function has no calls".
            match std::fs::read_to_string(path.as_std_path()) {
                Ok(t) => t,
                Err(e) => {
                    log::warn!(
                        "outgoingCalls: cannot read primary file {path}: {e}; \
                         returning null instead of an empty result so the client doesn't \
                         render 'no outgoing calls' as a clean answer"
                    );
                    return None;
                }
            }
        }
    };
    let key = CanonicalKey::for_uri(&uri);
    let enc = state.encoding;
    let parsed = state.workspace.parse(&key, &text);
    let rope = Rope::from_str(&text);
    let mapper = PositionMapper::new(&rope, enc);
    let result = analyze_with_request_token(state, &key, &path, &parsed.tree, &text);

    // Group calls by (callee_file, callee_name), preserving first-seen order. `find_outgoing_calls`
    // already filtered to Call bindings whose caller matches `fn_name`.
    type CalleeKey = (Option<gd_project::FileId>, String);
    let groups: Vec<(CalleeKey, Vec<lsp_types::Range>)> = group_call_ranges(
        find_outgoing_calls(&result, fn_name.as_str()),
        &mapper,
        |b| match b {
            Binding::Call {
                callee_file,
                callee_name,
                ..
            } => Some((*callee_file, callee_name.clone())),
            _ => None,
        },
    );

    let mut out = Vec::with_capacity(groups.len());
    for ((callee_file, callee_name), ranges) in groups {
        let (to_uri, to_range, to_selection) = match callee_file {
            Some(fid) => match state.workspace.index.path(fid).map(|p| p.to_path_buf()) {
                Some(path) => match path_to_file_uri(&path) {
                    Some(u) => {
                        // The `to` item locates the callee's DECLARATION (LSP 3.17), not the call
                        // site — load the callee's file and resolve `func callee_name`'s spans.
                        let (range, selection) =
                            resolve_fn_item_ranges(state, &path, &u, &callee_name);
                        (u, range, selection)
                    }
                    None => {
                        log::debug!(
                            "outgoingCalls: dropping callee {callee_name} — \
                             path_to_file_uri({path}) rejected the path"
                        );
                        continue;
                    }
                },
                None => {
                    // A Binding::Call.callee_file was Some(fid) but the Index has no path for
                    // fid. This is NOT an Index-internal invariant — Index::verify() validates
                    // the Index's own structures (interfaces / registry / depgraph /
                    // name_referencers), never the `Binding`s held in an AnalysisResult — so it
                    // can't catch this. It's a stale-analysis-cache artifact: the binding
                    // out-lived the file's removal / quarantine and hasn't been flushed from the
                    // analysis cache yet (a reconcile re-analyzes and re-stamps the bindings).
                    // The on-call's first question is "which fid?" — log loudly.
                    log::warn!(
                        "outgoingCalls: callee {callee_name} bindings reference FileId({fid:?}) \
                         but Index::path returned None — a binding out-lived its file's \
                         removal/quarantine. The analysis cache is stale; re-run \
                         `gdls diagnose --reconcile` to re-analyze and re-stamp the bindings.",
                        fid = fid
                    );
                    continue;
                }
            },
            // Native / unresolved callee: no project declaration to point at. Degrade to the
            // caller's URI with a zero-width range at file start (LSP requires a location), NOT the
            // call-site range the pre-fix code used.
            None => (uri.clone(), file_start_range(), file_start_range()),
        };
        #[allow(deprecated)]
        let to = CallHierarchyItem {
            name: callee_name.clone(),
            kind: LspSymbolKind::FUNCTION,
            tags: None,
            detail: None,
            uri: to_uri,
            range: to_range,
            selection_range: to_selection,
            data: None,
        };
        out.push(CallHierarchyOutgoingCall {
            to,
            from_ranges: ranges,
        });
    }
    Some(out)
}

/// `callHierarchy/incomingCalls`: read `item.data` to recover (uri, function_name), then iterate
/// every file in `Index::name_referencers(name) + the item's own file`, lazy-analyze, and filter
/// for `Binding::Call` entries targeting (target_file, name). Groups by `caller_function`.
///
/// Per LSP 3.17 §callHierarchy_incomingCalls the `from` item's `range`/`selectionRange` locate the
/// **caller's declaration** in the candidate file (resolved via [`function_decl_spans`] on that
/// file's already-parsed tree), not the call site — call sites are the `from_ranges`.
pub fn incoming_calls(
    state: &mut ServerState,
    params: CallHierarchyIncomingCallsParams,
) -> Option<Vec<CallHierarchyIncomingCall>> {
    let (target_uri, target_name) = decode_call_hierarchy_data(&params.item)?;
    let target_path = crate::uri::uri_to_path(&target_uri)?;
    let target_fid = state.workspace.index.file_id(&target_path);
    let enc = state.encoding;

    // Candidate caller files: the target's own file (a function may call itself or other in-file
    // funcs) + every file the interface-pass index records as referencing this name.
    let mut candidates: Vec<(camino::Utf8PathBuf, Uri)> = Vec::new();
    candidates.push((target_path.clone(), target_uri.clone()));
    candidates.extend(collect_name_referencer_uris(
        &state.workspace.index,
        &target_name,
        Some(&target_path),
        "incomingCalls",
    ));

    // For each candidate, lazy-analyze + collect Call bindings matching the callee, grouped by
    // caller_function name within that file.
    let mut out: Vec<CallHierarchyIncomingCall> = Vec::new();
    for (path, cand_uri) in candidates {
        let Some((text, parsed, result)) =
            load_candidate_analysis(state, &path, &cand_uri, "incomingCalls")
        else {
            continue;
        };
        let rope = Rope::from_str(&text);
        let mapper = PositionMapper::new(&rope, enc);

        // Group by caller_function (the synthetic "<top>" for top-level calls). `find_incoming_calls`
        // already filtered to Call bindings whose callee matches (target_fid, target_name).
        let groups = group_call_ranges(
            find_incoming_calls(&result, target_fid, &target_name),
            &mapper,
            |b| match b {
                Binding::Call {
                    caller_function, ..
                } => Some(
                    caller_function
                        .clone()
                        .unwrap_or_else(|| "<top>".to_string()),
                ),
                _ => None,
            },
        );

        for (caller_name, ranges) in groups {
            // The `from` item locates the CALLER's declaration in this candidate file (LSP 3.17),
            // not the call site. `parsed.tree` + `mapper` for this file are already in scope. The
            // synthetic `<top>` caller (top-level code) has no declaration → zero-width at start.
            let (from_range, from_selection) = match function_decl_spans(&parsed.tree, &caller_name)
            {
                Some((decl_span, ident_span)) => (
                    mapper.span_to_range(decl_span),
                    mapper.span_to_range(ident_span),
                ),
                None => (file_start_range(), file_start_range()),
            };
            #[allow(deprecated)]
            let from = CallHierarchyItem {
                name: caller_name.clone(),
                kind: LspSymbolKind::FUNCTION,
                tags: None,
                detail: None,
                uri: cand_uri.clone(),
                range: from_range,
                selection_range: from_selection,
                data: Some(serde_json::json!({
                    "uri": cand_uri.as_str(),
                    "name": caller_name,
                })),
            };
            out.push(CallHierarchyIncomingCall {
                from,
                from_ranges: ranges,
            });
        }
    }
    Some(out)
}

// =============================================================================================
// WP-N5: workspace/symbol.
// =============================================================================================

/// `workspace/symbol`: project-wide fuzzy symbol search over the `class_name` registry + every
/// indexed file's `Interface.members`. Per LSP 3.17 §workspace_symbol, returns
/// `SymbolInformation[]` (the 3.16-compatible shape — every client accepts it).
///
/// Ranking via `nucleo-matcher` 0.3 (same matcher Helix/Zellij use): fuzzy + smart-case + Unicode
/// normalization. Class-name hits sort before member-name hits when scores tie, matching the
/// docs/03 §7.4 design. Results capped at 256 to bound LSP latency on 10k+ symbol projects.
///
/// Builds the flat candidate list on demand (no precomputed flat index per docs/03 §7.4): the
/// registry + per-file interface tables iterate in O(N) once per request. Re-running the query as
/// the user types is the same cost — adequate for v1; M5 can revisit if soak tests reveal it as
/// hot.
pub fn workspace_symbol(
    state: &mut ServerState,
    params: WorkspaceSymbolParams,
) -> Option<WorkspaceSymbolResponse> {
    let query = params.query;
    if query.is_empty() {
        return Some(WorkspaceSymbolResponse::Flat(Vec::new()));
    }

    // WP-RD7 micro-op — bench witness, no-op landed. The flat-candidate list below is rebuilt from
    // `iter_interfaces` on every `workspace/symbol` request; precomputing it on `Index` mutation
    // (and only re-deriving the changed files' rows) would trade per-request CPU for memory + an
    // invalidation hook. The Phase-C calibration on a large real-world project measured this within
    // budget, so the precompute is deferred per the plan's "lands OR documented bench witness" rule.
    // Build the flat candidate list. Each entry is (name, kind, container, path, line, is_class).
    type Candidate = (
        String,
        LspSymbolKind,
        Option<String>,
        camino::Utf8PathBuf,
        u32,
        bool,
    );
    let mut candidates: Vec<Candidate> = Vec::new();

    // Class-name registry entries — top-level class declarations across the project.
    for (name, entry) in state.workspace.index.registry().entries() {
        candidates.push((
            name.to_string(),
            LspSymbolKind::CLASS,
            None,
            entry.path.clone(),
            1,
            true,
        ));
    }
    // Per-file interface members — every Const / Var / Func / Signal / Enum reachable at file
    // scope. Inner classes' members aren't surfaced; M4 limitation, documented for future
    // expansion.
    for (fid, iface) in state.workspace.index.iter_interfaces() {
        let Some(path) = state.workspace.index.path(fid).map(|p| p.to_path_buf()) else {
            continue;
        };
        let container = iface.class_name.clone();
        for member in &iface.members {
            let kind = member_kind_to_lsp(member.kind);
            candidates.push((
                member.name.clone(),
                kind,
                container.clone(),
                path.clone(),
                member.line,
                false,
            ));
        }
    }

    // Fuzzy-match every candidate's name. nucleo's Utf32Str takes a scratch buffer; reuse one
    // for haystack and one for needle to amortize allocation.
    //
    // Use `fuzzy_match_greedy` rather than `fuzzy_match`: the latter has an internal
    // "should have been caught by prefilter" assert that nucleo 0.3.1 can hit on specific
    // short-input combinations (observed for needle "Hr" against a haystack that traversed
    // the prefilter but failed an inner invariant). Greedy is O(n) and may produce slightly
    // non-optimal scoring for very long inputs, but identifiers are short and the LSP code
    // path must never panic — `fuzzy_match_greedy` never asserts on its inputs.
    let mut matcher = Matcher::new(Config::DEFAULT);
    let mut needle_buf: Vec<char> = Vec::new();
    let needle = Utf32Str::new(&query, &mut needle_buf);
    let mut scored: Vec<(u16, Candidate)> = Vec::with_capacity(candidates.len().min(256));
    let mut hay_buf: Vec<char> = Vec::new();
    for cand in candidates {
        // nucleo asserts non-empty input. An empty haystack here would be a registry /
        // interface bug — log loudly so the operator can investigate the bad entry.
        if cand.0.is_empty() {
            log::warn!(
                "workspace_symbol: empty-name candidate at {path} (line {line}); this should be \
                 impossible — Index registry / Interface members carry a name. Investigate the \
                 emit site.",
                path = cand.3,
                line = cand.4
            );
            continue;
        }
        hay_buf.clear();
        let hay = Utf32Str::new(&cand.0, &mut hay_buf);
        if let Some(score) = matcher.fuzzy_match_greedy(hay, needle) {
            scored.push((score, cand));
        }
    }
    // Sort top-256 only: `select_nth_unstable_by` partitions in O(N) so the full
    // candidate set isn't sorted before truncating. The 256 prefix is then sorted in
    // O(K log K) where K=256. Class hits dominate within equal scores (the navigation
    // anchor in nucleo-ranked search). Docstring above promises an LSP-latency cap; the
    // earlier `sort + truncate` did O(N log N) and didn't deliver it on 10k+ symbol
    // projects.
    let cmp = |a: &(u16, Candidate), b: &(u16, Candidate)| {
        b.0.cmp(&a.0).then_with(|| b.1 .5.cmp(&a.1 .5))
    };
    if scored.len() > 256 {
        let _ = scored.select_nth_unstable_by(255, cmp);
        scored.truncate(256);
    }
    scored.sort_by(cmp);

    #[allow(deprecated)]
    let symbols: Vec<SymbolInformation> = scored
        .into_iter()
        .filter_map(|(_score, (name, kind, container, path, line, _))| {
            let uri = match path_to_file_uri(&path) {
                Some(u) => u,
                None => {
                    log::debug!(
                        "workspace_symbol: dropping {name} at {path} — path_to_file_uri \
                         rejected the path; the symbol is invisible to the client"
                    );
                    return None;
                }
            };
            let pos = Position {
                line: line.saturating_sub(1),
                character: 0,
            };
            Some(SymbolInformation {
                name,
                kind,
                tags: None,
                deprecated: None,
                location: Location {
                    uri,
                    range: Range {
                        start: pos,
                        end: pos,
                    },
                },
                container_name: container,
            })
        })
        .collect();
    Some(WorkspaceSymbolResponse::Flat(symbols))
}

fn member_kind_to_lsp(k: gd_project::MemberKind) -> LspSymbolKind {
    use gd_project::MemberKind::*;
    match k {
        Const => LspSymbolKind::CONSTANT,
        Var => LspSymbolKind::VARIABLE,
        Property => LspSymbolKind::PROPERTY,
        Func => LspSymbolKind::FUNCTION,
        Signal => LspSymbolKind::EVENT,
        Enum => LspSymbolKind::ENUM,
    }
}

/// Decode the `data` field a `prepareCallHierarchy` item carries: `{ "uri": ..., "name": ... }`.
/// Returns `None` if the field is absent or malformed.
fn decode_call_hierarchy_data(item: &CallHierarchyItem) -> Option<(Uri, String)> {
    let data = item.data.as_ref()?;
    let uri_str = data.get("uri")?.as_str()?;
    let name = data.get("name")?.as_str()?.to_string();
    let uri: Uri = match uri_str.parse() {
        Ok(u) => u,
        Err(e) => {
            // This URI was server-generated by `prepare_call_hierarchy` and round-tripped through
            // the client. A re-parse failure means the client corrupted the `data` blob — log it
            // so "no calls" is distinguishable from "the data was mangled" rather than a silent `?`.
            log::debug!(
                "callHierarchy: server-issued data.uri {uri_str:?} failed to re-parse ({e}); \
                 treating as no-result"
            );
            return None;
        }
    };
    Some((uri, name))
}
