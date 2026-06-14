//! LSP request handlers.

use gd_analyze::{
    find_incoming_calls, find_outgoing_calls, AnalysisResult, Binding, BindingTargetKind,
    CalleeTarget, DataType, DtKind,
};
use gd_syntax::ast::{
    ClassNode, ConstantNode, FunctionNode, LiteralNode, Member, NodeId, NodeKind, ParseTree,
    SignalNode, SubscriptAccess, VariableNode,
};
use gd_syntax::ByteSpan;
use gd_syntax::Literal;
use gd_types::native_db::NativeClass;
use lsp_types::{
    CallHierarchyIncomingCall, CallHierarchyIncomingCallsParams, CallHierarchyItem,
    CallHierarchyOutgoingCall, CallHierarchyOutgoingCallsParams, CallHierarchyPrepareParams,
    DocumentChanges, DocumentHighlight, DocumentHighlightKind, DocumentHighlightParams,
    DocumentLink, DocumentLinkParams, DocumentSymbolParams, DocumentSymbolResponse,
    GotoDefinitionParams, GotoDefinitionResponse, Hover, HoverContents, HoverParams, Location,
    MarkupContent, MarkupKind, OneOf, OptionalVersionedTextDocumentIdentifier, Position,
    PrepareRenameResponse, Range, ReferenceContext, ReferenceParams, RenameParams,
    SymbolInformation, SymbolKind as LspSymbolKind, TextDocumentEdit, TextDocumentPositionParams,
    TextEdit, TypeHierarchyItem, TypeHierarchyPrepareParams, TypeHierarchySubtypesParams,
    TypeHierarchySupertypesParams, Uri, WorkDoneProgressParams, WorkspaceEdit, WorkspaceLocation,
    WorkspaceSymbol, WorkspaceSymbolParams, WorkspaceSymbolResponse,
};
// `Goto{Declaration,TypeDefinition}{Params,Response}` are `lsp_types` aliases of the matching
// `GotoDefinition*` types and live under the `request` submodule (not re-exported at the crate
// root), so they're imported here separately. The aliasing is what lets `declaration` forward its
// params to `definition` with no field translation.
use lsp_types::request::{
    GotoDeclarationParams, GotoDeclarationResponse, GotoTypeDefinitionParams,
    GotoTypeDefinitionResponse,
};
use nucleo_matcher::{Config, Matcher, Utf32Str};
use ropey::Rope;
use rustc_hash::{FxHashMap, FxHashSet};

use crate::position::PositionMapper;
use crate::server::ServerState;
use crate::uri::{path_to_file_uri, uri_to_path, CanonicalKey};

// M8 (#64): the completion + resolve handlers live in `crate::completion` (a sizable module of
// their own) but dispatch addresses them as `handlers::completion` / `handlers::completion_item_resolve`
// alongside every other request handler — re-export so the dispatch table stays one uniform path.
pub(crate) use crate::completion::{completion, completion_item_resolve};

// M8 (#65): the signatureHelp handler lives in `crate::signature_help` (its own module, like
// `crate::completion`) but dispatch addresses it as `handlers::signature_help` alongside every
// other request handler — re-export so the dispatch table stays one uniform path.
pub(crate) use crate::signature_help::signature_help;

// M10 (#74): documentColor + colorPresentation live in `crate::color` (its own module, like
// `crate::completion`) but dispatch addresses them as `handlers::document_color` /
// `handlers::color_presentation` alongside every other request handler — re-export so the dispatch
// table stays one uniform path.
pub(crate) use crate::color::{color_presentation, document_color};

/// `textDocument/documentSymbol`: project the `gd_syntax` symbol outline into LSP's nested
/// [`lsp_types::DocumentSymbol`] tree — kinds plus byte→position ranges, with the full declaration as
/// `range` and the identifier as `selection_range`. Reads the shared cached parse (the same one
/// `publishDiagnostics` uses), so an edit is parsed once.
///
/// The nested shape is opt-in: clients advertise
/// `textDocument.documentSymbol.hierarchicalDocumentSymbolSupport`, and one that didn't (Helix
/// sends an explicit `false`) must get the flat 3.16 `SymbolInformation[]` fallback instead —
/// rust-analyzer/gopls/clangd all downgrade the same way (absent ⇒ flat).
pub fn document_symbol(
    state: &mut ServerState,
    params: DocumentSymbolParams,
) -> DocumentSymbolResponse {
    let hierarchical = state.caps.hierarchical_document_symbols;
    let uri = params.text_document.uri;
    // Single VFS lookup: hold `doc` across the parse and reuse its already-built rope for the
    // mapper (disjoint `&state.vfs` / `&mut state.workspace` borrows compose). Avoids both the
    // redundant hash lookup and re-allocating a rope we already hold.
    let Some(doc) = state.vfs.get(uri.as_str()) else {
        return if hierarchical {
            DocumentSymbolResponse::Nested(Vec::new())
        } else {
            DocumentSymbolResponse::Flat(Vec::new())
        };
    };
    let text = doc.text();
    let parsed = state.workspace.parse(&CanonicalKey::for_uri(&uri), &text);
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

    let mut symbols: Vec<lsp_types::DocumentSymbol> = parsed
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
    // `detail` from a request-time interface extraction over the SAME parse tree — matches the
    // open buffer exactly (no index staleness) and renders through the byte-stable formatters
    // hover already pins. Flat responses drop it naturally (SymbolInformation has no field).
    let iface = gd_project::extract_interface(&parsed.tree);
    annotate_symbol_details(&mut symbols, &iface);
    if hierarchical {
        DocumentSymbolResponse::Nested(symbols)
    } else {
        DocumentSymbolResponse::Flat(flatten_symbols(&symbols, &uri))
    }
}

/// The `extends` clause rendered for a class symbol's `detail` — what reference servers show
/// dimmed next to the class name in the outline.
fn extends_detail(extends: &gd_project::Extends) -> Option<String> {
    match extends {
        gd_project::Extends::None => None,
        gd_project::Extends::Path(p) => Some(format!("extends \"{p}\"")),
        gd_project::Extends::Names(names) => Some(format!("extends {}", names.join("."))),
    }
}

/// Post-pass pairing the built LSP symbol tree with its [`gd_project::Interface`]: every symbol
/// at one level IS the class `iface` describes (the A1 root wrapper, or a recursive inner-class
/// symbol) — its `detail` is the extends clause; member children pair by name against
/// `iface.members` (GDScript has no overloads, so first-match is exact) and render via
/// [`format_member_signature`]; CLASS children recurse into the matching `iface.inner`. Symbols
/// with no interface counterpart (enum values, named enums) keep `detail: None`.
fn annotate_symbol_details(
    symbols: &mut [lsp_types::DocumentSymbol],
    iface: &gd_project::Interface,
) {
    for sym in symbols {
        sym.detail = extends_detail(&iface.extends);
        let Some(children) = sym.children.as_mut() else {
            continue;
        };
        for child in children {
            if child.kind == LspSymbolKind::CLASS {
                if let Some(inner) = iface
                    .inner
                    .iter()
                    .find(|i| i.class_name.as_deref() == Some(child.name.as_str()))
                {
                    annotate_symbol_details(std::slice::from_mut(child), inner);
                }
            } else if let Some(decl) = iface.members.iter().find(|m| m.name == child.name) {
                child.detail = format_member_signature(&child.name, decl);
            }
        }
    }
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

/// The 3.16-compat projection for clients without `hierarchicalDocumentSymbolSupport`: a
/// preorder walk emitting [`SymbolInformation`] with the symbol's FULL `range` (the spec's
/// reveal contract for the flat shape) and `containerName` = the parent symbol's name — the
/// flatten shape all three reference servers ship.
#[allow(
    deprecated,
    reason = "lsp_types::SymbolInformation::deprecated is a (deprecated) non-optional field we must set"
)]
fn flatten_symbols(symbols: &[lsp_types::DocumentSymbol], uri: &Uri) -> Vec<SymbolInformation> {
    fn walk(
        out: &mut Vec<SymbolInformation>,
        symbols: &[lsp_types::DocumentSymbol],
        uri: &Uri,
        container: Option<&str>,
    ) {
        for sym in symbols {
            #[allow(deprecated)]
            out.push(SymbolInformation {
                name: sym.name.clone(),
                kind: sym.kind,
                tags: None,
                deprecated: None,
                location: Location {
                    uri: uri.clone(),
                    range: sym.range,
                },
                container_name: container.map(str::to_owned),
            });
            if let Some(children) = &sym.children {
                walk(out, children, uri, Some(&sym.name));
            }
        }
    }
    let mut out = Vec::new();
    walk(&mut out, symbols, uri, None);
    out
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
    // Single VFS lookup: hold `doc` across the parse and reuse its already-built rope for the
    // mapper. `&state.vfs` and `&mut state.workspace` are disjoint fields, so the borrow composes;
    // building a fresh `Rope::from_str(&text)` would needlessly re-allocate the rope we already own.
    let Some(doc) = state.vfs.get(uri.as_str()) else {
        return Vec::new();
    };
    let text = doc.text();
    let parsed = state.workspace.parse(&CanonicalKey::for_uri(&uri), &text);
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
        // Resolve the literal to an on-disk project file, then link it. Two cases, both keeping
        // the "no link to a non-existent target" guarantee (spec: documentLink scope, a3.md §3):
        //   * a `.gd` script is in the index → use its canonical interned path;
        //   * any other resource (`.tscn`/`.tres`/asset) is NOT indexed — the index holds only
        //     `.gd` (see `gd_files`) — so join it against the project root and confirm it's a real
        //     file on disk. `preload`/`load` routinely target these, so gating purely on index
        //     membership would silently drop every non-GDScript link.
        let abs = match state.workspace.index.resolve_res_path(path) {
            Some(fid) => state.workspace.index.path(fid).map(|p| p.to_path_buf()),
            None => state
                .workspace
                .index
                .res_to_path(path)
                .filter(|p| p.is_file()),
        };
        let Some(abs) = abs else {
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
/// Build hover contents in the client's negotiated format (M7 #62): markdown as assembled, or
/// the plaintext downgrade (`docs::markdown_to_plaintext`) when the client prefers plain text.
fn hover_contents(state: &ServerState, value: String) -> HoverContents {
    match state.caps.hover_format {
        crate::docs::ProseFormat::Markdown => HoverContents::Markup(MarkupContent {
            kind: MarkupKind::Markdown,
            value,
        }),
        crate::docs::ProseFormat::PlainText => HoverContents::Markup(MarkupContent {
            kind: MarkupKind::PlainText,
            value: crate::docs::markdown_to_plaintext(&value),
        }),
    }
}

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

    // M6-Fix2: when the cursor is on a `res://` string literal (e.g. `preload("res://foo.gd")`),
    // render the resolved script's basename. Return early so `render_hover` (which renders `String`
    // for string literal nodes) doesn't shadow this more useful result.
    if let Some(preload_md) = hover_preload_string(state, &parsed.tree, node_id) {
        let leaf_node = parsed.tree.get(node_id);
        return Some(Hover {
            contents: hover_contents(state, preload_md),
            range: Some(mapper.span_to_range(leaf_node.span)),
        });
    }

    // v1.0.2 (issue #26): the cursor is on the NAME of a class-level declaration — render its
    // signature (the analyzer pins no type on the name identifier, so the typed-ancestor
    // fallback below would walk up to the class node and surface its `<Script #N>` meta).
    if let Some(decl_md) =
        hover_declaration_signature(state, &parsed.tree, node_id, &uri, analyzed.as_deref())
    {
        let leaf_node = parsed.tree.get(node_id);
        return Some(Hover {
            contents: hover_contents(state, decl_md),
            range: Some(mapper.span_to_range(leaf_node.span)),
        });
    }

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
    // a `func helper(...) -> R` line rather than just the return type. The attribute fallback
    // covers member identifiers in non-callee position: the signal in `obj.sig.emit()` or an
    // uncalled `obj.method` reference.
    let member_sig = analyzed
        .as_deref()
        .and_then(|a| hover_member_signature(state, &parsed.tree, byte, a))
        .or_else(|| {
            analyzed
                .as_deref()
                .and_then(|a| hover_attribute_member_signature(state, &parsed.tree, byte, a))
        })
        // v1.0.4 (#35): bare calls — an inherited native method / signal through the implicit
        // self (`stop()` under `extends AudioStreamPlayer`), or a `@GlobalScope` utility
        // (`print(...)`). Runs last so any project-script resolution above keeps shadowing.
        .or_else(|| hover_bare_native_signature(state, &parsed.tree, byte, &uri));

    let markdown = if let Some(sig) = member_sig {
        sig
    } else {
        render_hover(state, &parsed.tree, node_id, typed_id, analyzed.as_deref())
    };
    if markdown.trim().is_empty() {
        return None;
    }

    Some(Hover {
        contents: hover_contents(state, markdown),
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
///   3. Native symbols (v1.0.4 #34): the class's API page is materialized as a real document
///      under the user-level stub cache ([`crate::stubs`]) and the Location points into it —
///      the class header for a class name, the member's rendered line for attribute /
///      implicit-self member access. Unknown identifiers return `None` (the LSP wire = `null`).
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

    // (1.5) Cross-file member access (#13): the analyzer records a `Binding::Use` at this exact
    // identifier span with the DECLARING file whenever the cross-file member walk
    // (`lookup_script_chain_member`) resolved it — attribute sites (`obj.sig`, `obj.method`) and
    // bare inherited members alike. Projecting the binding inherits the analyzer's resolution by
    // construction (the same "never lie" shape as the autoload gate below). Class-kind bindings
    // are excluded — class_name/autoload jumps belong to steps (2)/(D) and their own gates.
    {
        let node_span = parsed.tree.get(node_id).span;
        let analyzed = analyze_if_gd(state, &uri, &parsed.tree, &text);
        let target = analyzed.as_deref().and_then(|a| {
            a.bindings().iter().find_map(|b| match b {
                Binding::Use {
                    site,
                    target_file: Some(f),
                    target_kind,
                    target_name,
                } if *site == node_span
                    && *target_kind != BindingTargetKind::Class
                    && target_name == &name =>
                {
                    Some(*f)
                }
                _ => None,
            })
        });
        if let Some(fid) = target {
            if let Some(loc) = member_decl_location(state, fid, &name) {
                return Some(GotoDefinitionResponse::Scalar(loc));
            }
        }
    }

    // (1.6) Cross-file dotted method call (`obj.method()` through a typed var): the attribute
    // identifier inside a call callee records no `Binding::Use` (the reducer's attribute paths
    // are a deliberate recording scope cut — see `AnalysisResult::bindings`), but `reduce_call`
    // recorded a `Binding::Call` whose callee target names the declaring script. Project the
    // call binding whose callee-identifier span contains the cursor — the same projection the
    // references handler's call-site click uses (M6-E) — and jump to the declaration. Hover
    // already resolves these through the type table; definition returning null here was the
    // asymmetry the v1.0.3 real-project walk caught.
    {
        let node_byte = byte;
        let analyzed = analyze_if_gd(state, &uri, &parsed.tree, &text);
        let target = analyzed.as_deref().and_then(|a| {
            let mut spans: Option<FxHashMap<ByteSpan, ByteSpan>> = None;
            a.bindings().iter().find_map(|b| match b {
                Binding::Call {
                    callee,
                    callee_name,
                    call_site,
                    ..
                } if callee_name == &name => {
                    let f = callee.script_file()?;
                    let spans = spans.get_or_insert_with(|| callee_ident_spans(&parsed.tree));
                    let ident = spans.get(call_site).copied()?;
                    (ident.start <= node_byte && node_byte < ident.end).then_some(f)
                }
                _ => None,
            })
        });
        if let Some(fid) = target {
            if let Some(loc) = member_decl_location(state, fid, &name) {
                return Some(GotoDefinitionResponse::Scalar(loc));
            }
        }
    }

    // (2) Cross-file `class_name`.
    if let Some(loc) = find_global_class_definition(state, &name) {
        return Some(GotoDefinitionResponse::Scalar(loc));
    }

    // (D) Autoload singleton — last fallback so in-file members, class_name declarations, AND
    // body-local vars/parameters shadow autoload names.
    //
    // The "never lie" gate: only jump to the autoload script when the analyzer's `reduce_identifier`
    // actually resolved THIS occurrence to the autoload (step 9 — the last fallback in the
    // analyzer's priority chain: local → param → class member → native → class_name → builtin →
    // global-const → autoload). The analyzer records a `Binding::Use { target_file: Some(fid) }`
    // at the cursor's span when and only when steps 1–8 all missed — i.e. exactly when nothing
    // shadows the autoload. Gating here rather than re-implementing scope lookup means the
    // definition handler inherits the analyzer's precedence by construction.
    //
    // Implementation: resolve the autoload name → FileId first (cheap, no analysis), then run
    // the analyzer (cached — same call as hover) and check for the sentinel Use binding at the
    // cursor span. If the binding is absent the identifier was shadowed by a local/param/member
    // and we must NOT jump (return None, not the autoload Location).
    {
        let autoload_fid = state
            .workspace
            .project
            .autoload_script_path(&name)
            .and_then(|p| state.workspace.index.resolve_res_path(&p));
        if let Some(fid) = autoload_fid {
            let node_span = parsed.tree.get(node_id).span;
            let analyzed = analyze_if_gd(state, &uri, &parsed.tree, &text);
            let resolved_to_autoload = analyzed.as_deref().is_some_and(|a| {
                a.bindings().iter().any(|b| {
                    matches!(b,
                        Binding::Use { site, target_file: Some(f), .. }
                            if *site == node_span && *f == fid
                    )
                })
            });
            if resolved_to_autoload {
                if let Some(loc) = find_autoload_definition(state, &name) {
                    return Some(GotoDefinitionResponse::Scalar(loc));
                }
            }
        }
    }

    // (3) Native symbols (v1.0.4 #34): materialize the class's API page as a real read-only
    // document under the user-level stub cache and return a standard `file://` Location into it
    // — LSP ≤ 3.17 has no virtual-document mechanism, and the generic-LSP principle (#30) rules
    // out custom URI schemes. Runs LAST so every project-level resolution keeps shadowing
    // natives. Unknown identifiers still return null.
    native_definition(state, &parsed.tree, byte, &uri, &name, &text)
        .map(GotoDefinitionResponse::Scalar)
}

/// `textDocument/declaration`: in GDScript a declaration and a definition are the same construct —
/// there is no forward-declare/define split (no header/impl, no `extern`), so the declaration of a
/// symbol IS its definition. This handler is therefore a thin wrapper that returns exactly what
/// [`definition`] returns for the same cursor. `GotoDeclarationParams`/`GotoDeclarationResponse`
/// are `lsp_types` aliases of the `GotoDefinition*` types, so the params/response pass straight
/// through with no field translation. (Mirrors how rust-analyzer/clangd treat `declaration` for
/// languages without a separate declaration form.)
pub fn declaration(
    state: &mut ServerState,
    params: GotoDeclarationParams,
) -> Option<GotoDeclarationResponse> {
    definition(state, params)
}

/// `textDocument/typeDefinition`: jump from the symbol under the cursor to the declaration site of
/// its *type* (not its own declaration — that is [`definition`]). E.g. on `e` in `var e := Enemy.new()`
/// this lands on `class_name Enemy`, whereas `definition` lands on the `var e` line.
///
/// Pipeline (mirrors [`hover`]'s borrow order so the analyzer borrow drops before the helper takes
/// `&mut state`): resolve the cursor's `DataType` via [`smallest_typed_containing`] →
/// [`AnalysisResult::types`], **clone** it (releasing the `analyzed` borrow), then map it to a
/// declaring [`Location`] with [`type_decl_location`]. Builtin / Variant / Enum / unresolved types
/// have no declaring source to point at → `None` (LSP `null`); we never guess (W10).
pub fn type_definition(
    state: &mut ServerState,
    params: GotoTypeDefinitionParams,
) -> Option<GotoTypeDefinitionResponse> {
    let tdp = params.text_document_position_params;
    let uri = tdp.text_document.uri.clone();
    let text = state.vfs.get(uri.as_str()).map(|d| d.text())?;
    let parsed = state.workspace.parse(&CanonicalKey::for_uri(&uri), &text);
    let analyzed = analyze_if_gd(state, &uri, &parsed.tree, &text);

    let doc = state.vfs.get(uri.as_str())?;
    let mapper = PositionMapper::new(&doc.rope, state.encoding);
    let byte = mapper.position_to_byte(tdp.position);

    // Clone the DataType so the `analyzed` borrow is released before `type_decl_location` reborrows
    // `state` (the script arm parses the target file; the native arm reads the stub cache). The
    // owned `parsed`/`analyzed` Rcs don't block that reborrow — only `doc`/`mapper`, both dead here.
    let dt = analyzed
        .as_deref()
        .and_then(|a| smallest_typed_containing(&parsed.tree, byte, a).map(|id| a.types.get(id)))?
        .clone();

    type_decl_location(state, &dt).map(GotoTypeDefinitionResponse::Scalar)
}

/// Map a resolved [`DataType`] to the [`Location`] that DECLARES that type — the reusable core of
/// [`type_definition`] (phase-4 `typeHierarchy` anchors supertypes through this same path). Three
/// outcomes, by [`DtKind`]:
///   - [`DtKind::Script`] → the external script's `class_name` site (or its file head if it has no
///     `class_name`), via [`script_decl_location`] keyed on the [`ScriptRef`](gd_analyze::ScriptRef)'s file.
///   - [`DtKind::Native`] → that engine class's stub header, via [`native_class_header_location`].
///   - everything else (`Builtin`/`Variant`/`Enum`/`Resolving`/`Unresolved`) → `None`: these name no
///     single declaring document to jump to, and guessing would violate "never lie" (W10).
///
/// [`DtKind::Class`] (an in-file inner class) is rewritten to `Script` before analysis results
/// escape `analyze`, so it never reaches here — it lands in the catch-all `None` arm rather than
/// being special-cased (matching the upstream invariant noted on `DtKind::Class`).
fn type_decl_location(state: &mut ServerState, dt: &DataType) -> Option<Location> {
    match dt.kind {
        DtKind::Script => script_decl_location(state, dt.script_type.as_ref()?.file),
        DtKind::Native if !dt.native_type.is_empty() => {
            native_class_header_location(state, &dt.native_type)
        }
        _ => None,
    }
}

/// The `class_name` declaration site of the external script `fid` — the `Script`-kind arm of
/// [`type_decl_location`]. Keyed on a [`FileId`](gd_project::FileId) the caller already holds
/// (unlike [`find_global_class_definition`], which resolves a name through the `class_name`
/// registry first); kept separate so that name-keyed fast path stays untouched.
///
/// Prefers the open buffer's cached parse (an edited buffer outranks the index), else reads disk.
/// Anchors at the root class's identifier span; a script with no `class_name` (no root identifier)
/// falls back to a `(0,0)` whole-file [`Location`] — the existing convention for file targets
/// (`find_res_path_definition` / `find_autoload_definition`).
fn script_decl_location(state: &mut ServerState, fid: gd_project::FileId) -> Option<Location> {
    let path = state.workspace.index.path(fid)?.to_path_buf();
    let uri = path_to_file_uri(&path)?;
    let uri_str = uri.as_str().to_owned();
    let text = if let Some(text) = state.vfs.get(&uri_str).map(|d| d.text()) {
        text
    } else {
        match std::fs::read_to_string(path.as_std_path()) {
            Ok(t) => t,
            Err(e) => {
                log::warn!(
                    "could not read {path} for type definition: {e}; jump degrades to no-result"
                );
                return None;
            }
        }
    };
    let parsed = state.workspace.parse(&CanonicalKey::for_uri(&uri), &text);
    let Some(ident_span) = root_class_identifier_span(&parsed.tree) else {
        // No `class_name` → no identifier to anchor; point at the file head (the file-target
        // convention) so the jump still lands in the declaring script.
        return Some(Location {
            uri,
            range: file_start_range(),
        });
    };
    let rope = ropey::Rope::from_str(&text);
    let mapper = PositionMapper::new(&rope, state.encoding);
    Some(Location {
        uri,
        range: mapper.span_to_range(ident_span),
    })
}

/// The native arm of [`definition`] — three cursor shapes, all anchoring into a stub
/// materialized by [`crate::stubs::ensure_class_stub`]:
///   1. the identifier IS a native class name → the stub's `class_name` header;
///   2. a subscript attribute whose base type is Native (`player.stop`) → the member's line in
///      its DECLARING class's stub;
///   3. a bare call callee (`queue_free()` under a Node-rooted script) → the same member anchor
///      through the file's chain native root — definition/hover symmetry (#35's bare-call path).
///
/// Builtin members (`v.length`) stay hover-only: builtin types have no class-page shape to
/// materialize, so `definition` keeps returning null there. And shape 3's project-shadowing
/// check sees interface MEMBERS only — a local `Callable` shadowing an inherited native name
/// still jumps to the native; the interface walk can't see function bodies (accepted gap).
fn native_definition(
    state: &mut ServerState,
    tree: &ParseTree,
    byte: usize,
    uri: &Uri,
    name: &str,
    text: &str,
) -> Option<Location> {
    let stub_root = state.options.stub_cache_dir.clone();

    // 1. Native class name → that class's stub header. The `class_named` guard makes this a
    //    terminal arm (a real class name never falls through to the attribute / bare-call arms,
    //    preserving the original early-return even when stub materialization fails). The header
    //    anchoring itself is shared with `type_definition`'s Native arm (both point at a native
    //    class's `class_name` line) via `native_class_header_location`.
    if state.workspace.native.class_named(name).is_some() {
        return native_class_header_location(state, name);
    }

    // 2. Subscript attribute over a Native-typed base: resolve the member to its declaring
    // class. The analyzer result is cached (same call hover makes).
    let attr_site = tree.iter_ids().find_map(|id| {
        if let NodeKind::Subscript(sub) = &tree.get(id).kind {
            if let Some(SubscriptAccess::Attribute(Some(attr_id))) = sub.access {
                let s = tree.get(attr_id).span;
                if s.start <= byte && byte < s.end && ident_name(tree, attr_id) == name {
                    return Some(sub.base);
                }
            }
        }
        None
    });
    if let Some(Some(base_id)) = attr_site {
        let analyzed = analyze_if_gd(state, uri, tree, text);
        let base_dt = analyzed.as_deref().map(|a| a.types.get(base_id).clone());
        if let Some(base_dt) = base_dt {
            if base_dt.kind == gd_analyze::DtKind::Native && !base_dt.native_type.is_empty() {
                return native_member_stub_location(
                    state,
                    &base_dt.native_type,
                    name,
                    stub_root.as_deref(),
                );
            }
        }
        // Deliberate stop, not a fall-through miss: the cursor names an ATTRIBUTE site, so
        // the bare-call arm below can never describe it — and a non-Native base is project
        // territory the script arms already resolved (or correctly failed to) before this
        // function ran. Falling through could only mis-anchor the name at an unrelated bare
        // call elsewhere in the file.
        return None;
    }

    // 3. Bare call callee through the implicit self — the file's chain native root, with
    // project members shadowing (the hover bare-call rule).
    let is_bare_callee = tree.iter_ids().any(|id| {
        if let NodeKind::Call(c) = &tree.get(id).kind {
            if let Some(callee) = c.callee {
                if let NodeKind::Identifier(i) = &tree.get(callee).kind {
                    let s = tree.get(callee).span;
                    return s.start <= byte && byte < s.end && i.name == name;
                }
            }
        }
        false
    });
    if is_bare_callee {
        let fid = uri_to_path(uri).and_then(|p| state.workspace.index.file_id(&p))?;
        let (chain, root) = state
            .workspace
            .index
            .extends_chain_files(fid, &state.workspace.native);
        let declared_in_project = chain.iter().any(|f| {
            state
                .workspace
                .index
                .interface(*f)
                .is_some_and(|i| i.members.iter().any(|m| m.name == name))
        });
        if declared_in_project {
            return None;
        }
        return native_member_stub_location(state, &root?, name, stub_root.as_deref());
    }
    None
}

/// Materialize the stub of the class DECLARING `member` (found by the chain walk from `class`)
/// and anchor at the member's NAME token on its rendered line.
fn native_member_stub_location(
    state: &ServerState,
    class: &str,
    member: &str,
    stub_root: Option<&str>,
) -> Option<Location> {
    let db = &state.workspace.native;
    let (decl, _) = db.lookup_member(class, member)?;
    let declaring = db.name_of(decl.name).to_owned();
    let (path, stub) =
        crate::stubs::ensure_class_stub(&state.stub_cache, db, &declaring, stub_root)?;
    let anchor = *stub.member_lines.get(member)?;
    stub_token_location(&path, anchor.line, anchor.name_col, anchor.name_len)
}

/// Materialize `class`'s stub and anchor at its `class_name` header token — the native analog of a
/// project script's `class_name` declaration site. Shared by [`native_definition`]'s class-name arm
/// (cursor IS the class name) and [`type_definition`]'s `Native` arm (cursor is a symbol whose
/// resolved type is this native class). Takes `&ServerState`: `ensure_class_stub` only reads the
/// stub cache + native DB, so no analysis (hence no `&mut`) is needed here.
///
/// `ensure_class_stub` re-checks `class_named` internally, so a non-class name returns `None`
/// without a stub being written. `class.len()` is a byte count, safe as a UTF-16/32 column too:
/// `ensure_class_stub` only materializes identifier-shaped (ASCII) class names.
fn native_class_header_location(state: &ServerState, class: &str) -> Option<Location> {
    let (path, stub) = crate::stubs::ensure_class_stub(
        &state.stub_cache,
        &state.workspace.native,
        class,
        state.options.stub_cache_dir.as_deref(),
    )?;
    stub_token_location(
        &path,
        stub.class_line,
        stub.class_name_col,
        class.len() as u32,
    )
}

/// A Location covering a name token on `line` (0-based) of the stub at `path`. The columns are
/// byte offsets within the line, valid under ANY negotiated position encoding: stub declaration
/// lines open with fixed ASCII prefixes and engine names are ASCII identifiers (see
/// [`crate::stubs::MemberAnchor`]).
fn stub_token_location(path: &camino::Utf8Path, line: u32, col: u32, len: u32) -> Option<Location> {
    let uri = path_to_file_uri(path)?;
    Some(Location {
        uri,
        range: lsp_types::Range::new(Position::new(line, col), Position::new(line, col + len)),
    })
}

/// Run the analyzer if `uri` points to a `.gd` file. Mirrors the gate used in `publish_diagnostics`
/// — the analyzer is GDScript-specific, and any other extension/scheme just gets parser-only data.
///
/// M5 WP-O4: threads the current request's [`CancellationToken`](gd_analyze::CancellationToken)
/// from [`ServerState::current_token`] into the analyzer's
/// [`gd_analyze::AnalyzeOptions::cancellation`] field via [`analyze_with_request_token`], so a
/// `$/cancelRequest` for this request id flips the token mid-analyze and the analyzer bails on
/// its next 256-node checkpoint.
pub(crate) fn analyze_if_gd(
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
            checkpoint_delay: None,
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
            if let Some(class) = state.workspace.native.class_named(&ident.name) {
                // v1.0.4 (#35): the editor-LSP declaration line (`<Native> class X extends Y`)
                // instead of the bare name — the docs append below as before.
                native_lookup = Some(ident.name.clone());
                Some(crate::native_render::class_detail(
                    &state.workspace.native,
                    class,
                ))
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
            // Through the human label, never the raw `Display` — a script-typed value must
            // read `ReproEntity`, not the `<Script #N>` diagnostic placeholder (issue #26).
            md.push_str(&human_type_label(state, tree, dt));
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
            if let Some(class) = state.workspace.native.class_named(&ident.name) {
                if md.is_empty() {
                    md.push_str("```gdscript\n");
                    md.push_str(&crate::native_render::class_detail(
                        &state.workspace.native,
                        class,
                    ));
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
/// The signature format is `func name(name: ParamType, …) -> ReturnType` using the parameter
/// names and unresolved syntactic type names from [`gd_project::MemberDecl`]. When a name is
/// unavailable (empty string), the type alone is rendered.
fn hover_member_signature(
    state: &ServerState,
    tree: &ParseTree,
    cursor_byte: usize,
    analyzed: &AnalysisResult,
) -> Option<String> {
    use gd_analyze::DtKind;

    // Find the innermost Call node whose span contains the cursor byte. The cursor is gated below
    // to the callee member-name identifier; this only recovers the enclosing Call so we can reach
    // the callee subscript and its base type.
    //
    // Pick the *innermost* (smallest-span) enclosing Call: DFS pre-order visits an outer call
    // before its inner ones, so for a nested callee — e.g. the cursor on `bar` in
    // `a.foo(b.bar(x))` — a plain first-match would wrongly select `a.foo` and render its
    // signature instead of `b.bar`'s.
    let call_node = tree
        .iter_ids()
        .filter_map(|id| {
            let node = tree.get(id);
            if let NodeKind::Call(c) = &node.kind {
                if node.span.start <= cursor_byte && cursor_byte < node.span.end {
                    return Some((node.span.end - node.span.start, c.clone()));
                }
            }
            None
        })
        .min_by_key(|(span_len, _)| *span_len)
        .map(|(_, c)| c)?;

    // Only subscript calls (`l.helper()`) provide a base whose type we can look up.
    // Bare calls (`helper()`) resolve via the in-class or inherited interface — handled
    // by the existing `render_hover` type-label path; skip them here.
    let callee_id = call_node.callee?;
    let NodeKind::Subscript(sub) = &tree.get(callee_id).kind else {
        return None;
    };
    let base_id = sub.base?;

    // Gate: the cursor must land on the callee member name itself (the `.helper` identifier), not
    // on the base receiver or an argument. Both of those live inside the enclosing Call span but
    // carry their own types that `render_hover` must report; without this gate, hovering `l` or an
    // argument in `l.helper(arg)` would wrongly surface `helper`'s signature.
    let Some(SubscriptAccess::Attribute(Some(attr_id))) = sub.access else {
        return None;
    };
    let attr_span = tree.get(attr_id).span;
    if !(attr_span.start <= cursor_byte && cursor_byte < attr_span.end) {
        return None;
    }

    let fn_name = &call_node.function_name;
    if fn_name.is_empty() {
        return None;
    }

    // The base expression's resolved type routes the lookup: a project Script kind reads the
    // declaring interface; a Native (or Builtin) kind reads the NativeDb (v1.0.4 #35 — these
    // used to fall through to the bare expression-type label, `stop()` → `Nil`).
    let base_dt = analyzed.types.get(base_id);
    match base_dt.kind {
        DtKind::Script => {
            let script_ref = base_dt.script_type.as_ref()?;
            let callee_file = script_ref.file;

            // Look up the method name in the callee file's interface.
            let iface = state.workspace.index.interface(callee_file)?;
            let decl = iface.members.iter().find(|m| m.name.as_str() == fn_name)?;

            let sig = format_func_signature(fn_name, decl);
            let mut md = String::from("```gdscript\n");
            md.push_str(&sig);
            md.push_str("\n```");
            // M7 (#62): the declaring file's `##` doc for this member, BBCode → GFM.
            if let Some(doc) = &decl.doc {
                crate::docs::append_doc(
                    &mut md,
                    crate::docs::ProseFormat::Markdown,
                    &doc.description,
                );
            }
            Some(md)
        }
        DtKind::Native if !base_dt.native_type.is_empty() => {
            let (decl, member) = state
                .workspace
                .native
                .lookup_member(&base_dt.native_type, fn_name)?;
            let declaring = state.workspace.native.name_of(decl.name).to_owned();
            Some(native_member_hover_md(
                &state.workspace.native,
                &declaring,
                &member,
            ))
        }
        DtKind::Builtin => {
            let bt_name = gd_analyze::data_type::variant_type_name(base_dt.builtin_type);
            let (bt, member) = state
                .workspace
                .native
                .lookup_builtin_member(bt_name, fn_name)?;
            let declaring = state.workspace.native.name_of(bt.name).to_owned();
            Some(native_member_hover_md(
                &state.workspace.native,
                &declaring,
                &member,
            ))
        }
        _ => None,
    }
}

/// The fenced native-member hover body: the declaration line in Godot's detail format, then the
/// member's docstring when the dump carries one (`append_class_docs`-style) — never the bare
/// expression type (#35).
fn native_member_hover_md(
    db: &gd_types::NativeDb,
    declaring: &str,
    member: &gd_types::NativeMember,
) -> String {
    let sig = crate::native_render::member_detail(db, declaring, member);
    let mut md = format!("```gdscript\n{sig}\n```");
    let desc = match member {
        gd_types::NativeMember::Method(m) => m.description.as_str(),
        gd_types::NativeMember::Property(p) => p.description.as_str(),
        gd_types::NativeMember::Signal(s) => s.description.as_str(),
        _ => "",
    };
    // M7 (#62): dump descriptions are BBCode — converted to GFM here at the hover boundary
    // (anti-catalog W8: raw BBCode never reaches the wire). The body is assembled as markdown
    // regardless of the client's format; `hover_contents` applies the plaintext downgrade once,
    // at the response boundary.
    crate::docs::append_doc(&mut md, crate::docs::ProseFormat::Markdown, desc);
    md
}

/// Bare-call hover (v1.0.4 #35): the callee identifier of `stop()` under
/// `extends AudioStreamPlayer` resolves through the file's chain native root; `print(...)`
/// resolves as a `@GlobalScope` utility. Project members shadow natives — a name declared
/// anywhere in the file's extends chain returns `None` so the project-script paths own it.
/// Interface MEMBERS only: a local `Callable` shadowing an inherited native name still hovers
/// as the native — the interface walk can't see function bodies (accepted gap, same as
/// [`native_definition`]'s bare-call arm).
fn hover_bare_native_signature(
    state: &ServerState,
    tree: &ParseTree,
    cursor_byte: usize,
    uri: &Uri,
) -> Option<String> {
    // A Call whose callee is a BARE identifier spanning the cursor. Callee identifier spans are
    // disjoint source tokens, so the first hit is the only hit.
    let callee_name = tree.iter_ids().find_map(|id| {
        let node = tree.get(id);
        if let NodeKind::Call(c) = &node.kind {
            let callee = c.callee?;
            if let NodeKind::Identifier(i) = &tree.get(callee).kind {
                let s = tree.get(callee).span;
                if s.start <= cursor_byte && cursor_byte < s.end {
                    return Some(i.name.clone());
                }
            }
        }
        None
    })?;

    let db = &state.workspace.native;
    if let Some(fid) = crate::uri::uri_to_path(uri).and_then(|p| state.workspace.index.file_id(&p))
    {
        let (chain, root) = state.workspace.index.extends_chain_files(fid, db);
        let declared_in_project = chain.iter().any(|f| {
            state
                .workspace
                .index
                .interface(*f)
                .is_some_and(|i| i.members.iter().any(|m| m.name == callee_name))
        });
        if declared_in_project {
            return None;
        }
        if let Some(root) = root {
            if let Some((decl, member)) = db.lookup_member(&root, &callee_name) {
                // Only callable shapes — a bare property/constant identifier isn't a call.
                if matches!(
                    member,
                    gd_types::NativeMember::Method(_) | gd_types::NativeMember::Signal(_)
                ) {
                    let declaring = db.name_of(decl.name).to_owned();
                    return Some(native_member_hover_md(db, &declaring, &member));
                }
            }
        }
    }
    // `@GlobalScope` utility — also reachable in buffers outside the project index.
    let u = db.utility(&callee_name)?;
    Some(format!(
        "```gdscript\n{}\n```",
        crate::native_render::utility_detail(db, u)
    ))
}

/// Render a `MemberDecl`'s params as `name: Type, …` — zip `param_names` and `params` in lockstep;
/// fall back to type-only when the name is empty.
fn format_member_params(decl: &gd_project::MemberDecl) -> String {
    decl.params
        .iter()
        .enumerate()
        .map(|(i, p)| {
            let type_str = match p {
                gd_project::TypeExpr::Named { path, .. } => path.join("."),
                gd_project::TypeExpr::None => "Variant".to_string(),
            };
            let name_str = decl.param_names.get(i).map(String::as_str).unwrap_or("");
            if name_str.is_empty() {
                type_str
            } else {
                format!("{name_str}: {type_str}")
            }
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// Format a `Func` member as `func name(param_name: ParamType, …) -> ReturnType`.
fn format_func_signature(fn_name: &str, decl: &gd_project::MemberDecl) -> String {
    let params_str = format_member_params(decl);
    let ret_str = match &decl.ty {
        gd_project::TypeExpr::Named { path, .. } => path.join("."),
        // `interface::type_expr` collapses BOTH an explicit `-> void` (empty type node) and an
        // absent return annotation to `TypeExpr::None`, so they're indistinguishable here. Render
        // `void`: explicit-void functions vastly outnumber truly-untyped ones in typed GDScript, so
        // this is correct far more often than `Variant` would be. (A precise split would need
        // interface extraction to carry "explicit void" vs "no annotation" — out of scope.)
        gd_project::TypeExpr::None => "void".to_string(),
    };
    format!("func {fn_name}({params_str}) -> {ret_str}")
}

/// Member-signature hover for a subscript ATTRIBUTE identifier outside a callee position — the
/// signal in `obj.sig.emit()` / `Singleton.sig.connect(…)`, an uncalled method reference like
/// `var f = obj.method`, or a `var`/`const` member — where the base expression resolves to a
/// project script. The Call-gated [`hover_member_signature`] can't reach these: for
/// `obj.sig.emit()` the enclosing Call's callee attribute is `emit`, never `sig`. Renders the
/// member's declaration shape from the base script's interface; named enums return `None` so the
/// expression-type label keeps reporting the analyzer's resolved enum meta type.
fn hover_attribute_member_signature(
    state: &ServerState,
    tree: &ParseTree,
    cursor_byte: usize,
    analyzed: &AnalysisResult,
) -> Option<String> {
    use gd_analyze::DtKind;

    // The subscript whose attribute identifier spans the cursor. Attribute identifier spans are
    // disjoint across nesting (each is a distinct source token), so the first hit is the only hit.
    let (sub_base, attr_id) = tree.iter_ids().find_map(|id| {
        let node = tree.get(id);
        if let NodeKind::Subscript(sub) = &node.kind {
            if let Some(SubscriptAccess::Attribute(Some(attr_id))) = sub.access {
                let s = tree.get(attr_id).span;
                if s.start <= cursor_byte && cursor_byte < s.end {
                    return Some((sub.base, attr_id));
                }
            }
        }
        None
    })?;
    let base_id = sub_base?;

    let name = ident_name(tree, attr_id);
    if name.is_empty() {
        return None;
    }
    let attr_span = tree.get(attr_id).span;

    // The declaring interface, two ways: the analyzer's `Binding::Use` at this attribute names
    // the PRECISE declaring file (covers Class-kind `self.<member>` bases and members inherited
    // deeper in the chain); a Script-kind base's head interface is the fallback for accesses the
    // member walk deliberately skipped (e.g. an instance method referenced through a meta base —
    // still worth a signature hover).
    let binding_iface = analyzed.bindings().iter().find_map(|b| match b {
        Binding::Use {
            site,
            target_file: Some(f),
            target_name,
            ..
        } if *site == attr_span && target_name.as_str() == name => {
            state.workspace.index.interface(*f)
        }
        _ => None,
    });
    let base_dt = analyzed.types.get(base_id);
    let direct_iface = if base_dt.kind == DtKind::Script {
        base_dt
            .script_type
            .as_ref()
            .and_then(|sr| state.workspace.index.interface(sr.file))
    } else {
        None
    };
    if let Some(decl) = [binding_iface, direct_iface]
        .into_iter()
        .flatten()
        .find_map(|i| i.members.iter().find(|m| m.name.as_str() == name))
    {
        let sig = format_member_signature(name, decl)?;
        return Some(format!("```gdscript\n{sig}\n```"));
    }

    // v1.0.4 (#35): no project declaration — a Native (or Builtin) base reads the NativeDb:
    // `player.volume_db`, `Input.MOUSE_MODE_CAPTURED`, an uncalled `player.stop` reference,
    // `Vector2.ZERO`. These used to fall through to the bare expression-type label.
    match base_dt.kind {
        DtKind::Native if !base_dt.native_type.is_empty() => {
            let (decl, member) = state
                .workspace
                .native
                .lookup_member(&base_dt.native_type, name)?;
            let declaring = state.workspace.native.name_of(decl.name).to_owned();
            Some(native_member_hover_md(
                &state.workspace.native,
                &declaring,
                &member,
            ))
        }
        DtKind::Builtin => {
            let bt_name = gd_analyze::data_type::variant_type_name(base_dt.builtin_type);
            let (bt, member) = state
                .workspace
                .native
                .lookup_builtin_member(bt_name, name)?;
            let declaring = state.workspace.native.name_of(bt.name).to_owned();
            Some(native_member_hover_md(
                &state.workspace.native,
                &declaring,
                &member,
            ))
        }
        _ => None,
    }
}

/// Render a [`gd_project::MemberDecl`]'s declaration shape — the one formatter behind both the
/// attribute-reference hover and the declaration-site hover (issue #26), so `obj.member` and the
/// member's own declaration line read byte-for-byte the same. `None` for named enums (the
/// analyzer's enum-meta type label is the better hover there).
fn format_member_signature(name: &str, decl: &gd_project::MemberDecl) -> Option<String> {
    let sig = match decl.kind {
        gd_project::MemberKind::Func => {
            let bare = format_func_signature(name, decl);
            if decl.flags.is_static {
                format!("static {bare}")
            } else {
                bare
            }
        }
        gd_project::MemberKind::Signal => {
            format!("signal {}({})", name, format_member_params(decl))
        }
        gd_project::MemberKind::Var | gd_project::MemberKind::Property => {
            let keyword = if decl.flags.is_static {
                "static var"
            } else {
                "var"
            };
            match &decl.ty {
                gd_project::TypeExpr::Named { path, .. } => {
                    format!("{keyword} {name}: {}", path.join("."))
                }
                gd_project::TypeExpr::None => format!("{keyword} {name}"),
            }
        }
        gd_project::MemberKind::Const => match &decl.ty {
            gd_project::TypeExpr::Named { path, .. } => {
                format!("const {name}: {}", path.join("."))
            }
            gd_project::TypeExpr::None => format!("const {name}"),
        },
        // Named enums keep the analyzer's enum-meta type label (no signature to render).
        gd_project::MemberKind::Enum => return None,
    };
    Some(sig)
}

/// Declaration-site hover (issue #26): when the cursor is on the NAME identifier of a class-level
/// declaration (`func`/`var`/`const`/`signal`/inner `class`), render that member's signature —
/// previously the typed-ancestor fallback walked up to the enclosing class node and surfaced its
/// `<Script #N>` meta placeholder. Routed through [`format_member_signature`], the same formatter
/// the call-site/attribute hovers use, so declaration and reference hovers agree byte-for-byte.
///
/// Locals and parameters deliberately fall through (`None`): membership is checked against the
/// owning `ClassNode.members` list, so a body-level `var x` — even one shadowing a class member —
/// keeps the analyzer's resolved-type hover.
fn hover_declaration_signature(
    state: &ServerState,
    tree: &ParseTree,
    leaf_id: NodeId,
    uri: &Uri,
    analyzed: Option<&AnalysisResult>,
) -> Option<String> {
    if !matches!(&tree.get(leaf_id).kind, NodeKind::Identifier(_)) {
        return None;
    }
    let name = ident_name(tree, leaf_id).to_owned();
    if name.is_empty() {
        return None;
    }

    // The declaration whose name slot is exactly this identifier node, plus its OWNING class:
    // scan every ClassNode's member list so locals (not class members) never match.
    let mut decl: Option<(NodeId, &Member, NodeId)> = None; // (decl node, member tag, owner class)
    for class_id in tree.iter_ids() {
        let NodeKind::Class(class) = &tree.get(class_id).kind else {
            continue;
        };
        for member in &class.members {
            let (mid, named_by_leaf) = match member {
                Member::Class(id) => match &tree.get(*id).kind {
                    NodeKind::Class(c) => (*id, c.identifier == Some(leaf_id)),
                    _ => continue,
                },
                Member::Constant(id) => match &tree.get(*id).kind {
                    NodeKind::Constant(c) => (*id, c.identifier == Some(leaf_id)),
                    _ => continue,
                },
                Member::Function(id) => match &tree.get(*id).kind {
                    NodeKind::Function(f) => (*id, f.identifier == Some(leaf_id)),
                    _ => continue,
                },
                Member::Signal(id) => match &tree.get(*id).kind {
                    NodeKind::Signal(s) => (*id, s.identifier == Some(leaf_id)),
                    _ => continue,
                },
                Member::Variable(id) => match &tree.get(*id).kind {
                    NodeKind::Variable(v) => (*id, v.identifier == Some(leaf_id)),
                    _ => continue,
                },
                Member::Enum(_) | Member::EnumValue(_) | Member::Group(_) => continue,
            };
            if named_by_leaf {
                decl = Some((mid, member, class_id));
                break;
            }
        }
        if decl.is_some() {
            break;
        }
    }
    let (decl_id, member, owner_class) = decl?;

    // An inner `class X:` declaration renders directly from the AST (inner classes aren't in
    // the `class_name` registry the leaf-type-label branch serves).
    if let Member::Class(_) = member {
        let mut sig = format!("class {name}");
        if let NodeKind::Class(c) = &tree.get(decl_id).kind {
            let extends: Vec<&str> = c.extends.iter().map(|&e| ident_name(tree, e)).collect();
            if !extends.is_empty() {
                sig = format!("{sig} extends {}", extends.join("."));
            } else if let Some(p) = &c.extends_path {
                sig = format!("{sig} extends \"{p}\"");
            }
        }
        let mut md = format!("```gdscript\n{sig}\n```");
        // M7 (#62): the inner class's `##` doc (brief, then long form when distinct).
        if let Some(doc) = tree.docs.class_docs.get(&decl_id) {
            crate::docs::append_doc(&mut md, crate::docs::ProseFormat::Markdown, &doc.brief);
            if doc.description != doc.brief {
                crate::docs::append_doc(
                    &mut md,
                    crate::docs::ProseFormat::Markdown,
                    &doc.description,
                );
            }
        }
        return Some(md);
    }

    // Locate this file's interface scope: the root Interface, descended through `inner` by the
    // owning class's chain of names (built via `ClassNode.outer`, innermost → outermost).
    let path = uri_to_path(uri)?;
    let fid = state.workspace.index.file_id(&path)?;
    let mut chain: Vec<String> = Vec::new();
    let mut cursor = Some(owner_class);
    while let Some(cid) = cursor {
        let NodeKind::Class(c) = &tree.get(cid).kind else {
            break;
        };
        if c.outer.is_some() {
            // Non-root classes contribute their name; the root scope is the interface itself.
            chain.push(c.identifier.map(|i| ident_name(tree, i).to_owned())?);
        }
        cursor = c.outer;
    }
    let mut iface = state.workspace.index.interface(fid)?;
    for class_name in chain.iter().rev() {
        iface = iface
            .inner
            .iter()
            .find(|i| i.class_name.as_deref() == Some(class_name))?;
    }
    let member_decl = iface.members.iter().find(|m| m.name == name)?;
    let member_doc = member_decl.doc.clone();
    let mut sig = format_member_signature(&name, member_decl)?;

    // An untyped `var`/`const` whose initializer the analyzer typed reads better with the
    // resolved type appended (`var made: ReproEntity` for `var made := ent.spawn(...)`).
    if matches!(member_decl.ty, gd_project::TypeExpr::None)
        && matches!(
            member_decl.kind,
            gd_project::MemberKind::Var
                | gd_project::MemberKind::Property
                | gd_project::MemberKind::Const
        )
    {
        if let Some(a) = analyzed {
            let dt = a.types.get(decl_id);
            if dt.is_set() && !dt.is_variant() {
                sig = format!("{sig}: {}", human_type_label(state, tree, dt));
            }
        }
    }
    let mut md = format!("```gdscript\n{sig}\n```");
    // M7 (#62): the member's own `##` doc prose, BBCode → GFM.
    if let Some(doc) = member_doc {
        crate::docs::append_doc(
            &mut md,
            crate::docs::ProseFormat::Markdown,
            &doc.description,
        );
    }
    Some(md)
}

/// A human-readable label for a resolved [`DataType`] — hover must never surface the
/// `Display` impl's `<Script #N>` / `<Class>` diagnostic placeholders (issue #26). Script types
/// render their global `class_name` (or file basename) plus any inner-class path; in-file Class
/// types render their declared identifier. Everything else keeps the faithful `Display` text.
fn human_type_label(state: &ServerState, tree: &ParseTree, dt: &gd_analyze::DataType) -> String {
    use gd_analyze::DtKind;
    match dt.kind {
        DtKind::Script => {
            let Some(sr) = &dt.script_type else {
                return dt.to_string();
            };
            let head = state
                .workspace
                .index
                .interface(sr.file)
                .and_then(|i| i.class_name.clone())
                .or_else(|| {
                    state
                        .workspace
                        .index
                        .path(sr.file)
                        .and_then(|p| p.file_name())
                        .map(str::to_owned)
                });
            match head {
                Some(h) if sr.inner.is_empty() => h,
                Some(h) => format!("{h}.{}", sr.inner.join(".")),
                None => dt.to_string(),
            }
        }
        DtKind::Class => dt
            .class_node
            .and_then(|cid| match &tree.get(cid).kind {
                NodeKind::Class(c) => c.identifier.map(|i| ident_name(tree, i).to_owned()),
                _ => None,
            })
            .filter(|n| !n.is_empty())
            .unwrap_or_else(|| dt.to_string()),
        _ => dt.to_string(),
    }
}

fn append_class_docs(md: &mut String, class: &NativeClass) {
    // M7 (#62): BBCode → GFM at the boundary (see `native_member_hover_md`).
    crate::docs::append_doc(
        md,
        crate::docs::ProseFormat::Markdown,
        &class.brief_description,
    );
    // Godot emits `brief_description` and `description` as two distinct strings even when the
    // class has only a short summary; in the with-docs dump they're often equal. Dedupe so the
    // hover doesn't show the same paragraph twice.
    if class.description != class.brief_description {
        crate::docs::append_doc(md, crate::docs::ProseFormat::Markdown, &class.description);
    }
}

/// M6-Fix2: when the cursor is on a `res://`-path string literal (e.g. inside
/// `preload("res://foo.gd")`), render a hover showing the resolved file's basename — a `.gd`
/// script or any other on-disk project resource (`.tscn`/`.tres`/asset). Returns `None` for
/// non-res strings, paths with no on-disk target, or non-`String` literal nodes.
fn hover_preload_string(state: &ServerState, tree: &ParseTree, node_id: NodeId) -> Option<String> {
    let NodeKind::Literal(LiteralNode {
        value: Literal::String(path),
    }) = &tree.get(node_id).kind
    else {
        return None;
    };
    if !path.starts_with("res://") {
        return None;
    }
    // Resolve to an on-disk project file with the same logic as `document_link`, so hover and
    // links agree on what a `res://` literal points to: a `.gd` script is in the index → use its
    // canonical interned path; any other resource isn't indexed (the index holds only `.gd`), so
    // join it against the project root and confirm it's a real file.
    let abs = match state.workspace.index.resolve_res_path(path) {
        Some(fid) => state.workspace.index.path(fid).map(|p| p.to_path_buf()),
        None => state
            .workspace
            .index
            .res_to_path(path)
            .filter(|p| p.is_file()),
    }?;
    let basename = abs.file_name().unwrap_or(abs.as_str());
    // A GDScript script gets a `gdscript`-fenced basename and a "GDScript:" label; any other
    // resource gets a plain-fenced basename and a "Resource:" label, so the hover never claims a
    // scene or asset is GDScript.
    let md = if abs.extension() == Some("gd") {
        format!("```gdscript\n{basename}\n```\n\nGDScript: `{path}`")
    } else {
        format!("```\n{basename}\n```\n\nResource: `{path}`")
    };
    Some(md)
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

/// Returns `true` when `ident_id` is in a method or signal role in `tree`:
/// - The `.identifier` child of a `Function` or `Signal` node (declaration-site click), OR
/// - The attribute of a `Subscript` that is itself the **callee of a `Call`** (`l.helper()` — a
///   method call site; the cursor lands on the `helper` attribute Identifier).
///
/// A bare attribute **property read** (`node.position`, `self.hp`) is *not* a call callee and
/// returns `false`, routing it to the non-method classification ([`NonMethodTarget`]): resolved
/// member reads ride the binding-backed precise path; only unresolvable reads keep the raw-scan
/// floor.
///
/// The declaration arm deliberately matches a `Function`/`Signal` identifier at *any* class
/// depth — inner-class methods (`class Foo:` … `func helper():`) included. The references
/// method path still filters call sites at FILE granularity, so a root-class and an inner-class
/// method sharing one name in one file may mix their call-site sets — the bounded residue of
/// the over-approximating stance. (`CalleeTarget::Script` now records the owning `class_path`;
/// threading it through the method path's target resolution — which would need the cursor
/// side's owning class too — is the remaining refinement.)
///
/// Used to decide whether `textDocument/references` takes the method path (call-site
/// projection + project-wide text scan) or the non-method classification. Purely structural
/// (O(#nodes), no analyzer involvement); works identically whether the cursor is on the
/// declaration or a call site.
fn is_member_or_attribute_ident(tree: &ParseTree, ident_id: NodeId) -> bool {
    // Single pass: short-circuit on a func/signal declaration; otherwise remember the subscript
    // that owns this attribute identifier and collect every `Call` callee node, then decide.
    let mut owning_subscript: Option<NodeId> = None;
    let mut call_callees: FxHashSet<NodeId> = FxHashSet::default();
    for nid in tree.iter_ids() {
        match &tree.get(nid).kind {
            NodeKind::Function(f) if f.identifier == Some(ident_id) => {
                return true;
            }
            NodeKind::Signal(s) if s.identifier == Some(ident_id) => {
                return true;
            }
            NodeKind::Subscript(s) => {
                if matches!(s.access, Some(SubscriptAccess::Attribute(Some(aid))) if aid == ident_id)
                {
                    owning_subscript = Some(nid);
                }
            }
            NodeKind::Call(c) => {
                if let Some(callee) = c.callee {
                    call_callees.insert(callee);
                }
            }
            _ => {}
        }
    }
    // The attribute identifier belongs to a subscript: a method target only if that subscript is a
    // `Call`'s callee (`l.helper()`), not a standalone property read (`node.position`).
    owning_subscript.is_some_and(|sub_id| call_callees.contains(&sub_id))
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
/// Location of `name`'s declaration in `fid`'s head interface (any member kind, incl. named
/// enums and unnamed-enum hoists — they all carry a `MemberDecl` with a span), anchored on the
/// declaration's NAME token (`MemberDecl::name_span`) so cross-file jumps share the in-file
/// arm's identifier-span shape — never the whole declaration node, which editors would select.
/// Built against the target file's current text (open buffer wins over disk), with the
/// [`find_global_class_definition`] validation discipline: the indexed span is accepted only
/// while its bytes still spell the member name; on drift (open-buffer edits / watcher lag) the
/// identifier is re-located in a live parse, degrading to the whole-declaration span only when
/// the member vanished from the live tree — never dropping a previously-working jump.
/// Inner-class members aren't head-interface visible and degrade to `None` (the documented
/// inner-class stance).
fn member_decl_location(
    state: &mut ServerState,
    fid: gd_project::FileId,
    name: &str,
) -> Option<Location> {
    let (name_span, decl_span) = state
        .workspace
        .index
        .interface(fid)?
        .members
        .iter()
        .find(|m| m.name == name)
        .map(|m| (m.name_span, m.span))?;
    let path = state.workspace.index.path(fid)?.to_path_buf();
    let uri = path_to_file_uri(&path)?;
    let uri_str = uri.as_str().to_owned();
    let text = if let Some(text) = state.vfs.get(&uri_str).map(|d| d.text()) {
        text
    } else {
        match std::fs::read_to_string(path.as_std_path()) {
            Ok(t) => t,
            Err(e) => {
                log::warn!(
                    "could not read {path} for member definition of `{name}`: {e}; \
                     jump degrades to no-result"
                );
                return None;
            }
        }
    };
    let rope = ropey::Rope::from_str(&text);
    let mapper = PositionMapper::new(&rope, state.encoding);
    if text.get(name_span.start..name_span.end) == Some(name) {
        return Some(Location {
            uri,
            range: mapper.span_to_range(name_span),
        });
    }
    let parsed = state.workspace.parse(&CanonicalKey::for_uri(&uri), &text);
    if let Some(loc) = find_in_file_definition(&parsed.tree, name, &uri, &mapper) {
        return Some(loc);
    }
    Some(Location {
        uri,
        range: mapper.span_to_range(decl_span),
    })
}

fn find_global_class_definition(state: &mut ServerState, name: &str) -> Option<Location> {
    let entry = state.workspace.index.registry().get(name)?;
    let path = entry.path.clone();
    let indexed_span = entry.name_span;
    let uri = path_to_file_uri(&path)?;
    let uri_str = uri.as_str().to_owned();

    // Open buffer: reuse the cached parse — an edited buffer is newer than the index, so the live
    // tree is the only correct span source. Closed file: the registry's recorded identifier span
    // (#33) replaces the old per-lookup re-parse, accepted only while its bytes still spell the
    // class name — a watcher-lagged index (the file shifted or shrank underneath the recorded
    // span) falls back to a fresh parse instead of anchoring at stale coordinates.
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
        if text.get(indexed_span.start..indexed_span.end) == Some(name) {
            (indexed_span, text)
        } else {
            let tree = gd_syntax::parse(&text).tree;
            (root_class_identifier_span(&tree)?, text)
        }
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
///
/// The existence gate uses `resolve_res_path` (which returns `Some` only for indexed, on-disk
/// files) rather than `res_to_path` (a pure path-join with no existence check). This prevents
/// emitting a dangling `file://` URI when the `project.godot` autoload entry points at a script
/// that hasn't been written to disk — the same class of bug already fixed in `find_res_path_definition`
/// and `document_link`.
fn find_autoload_definition(state: &ServerState, name: &str) -> Option<Location> {
    let res_path = state.workspace.project.autoload_script_path(name)?;
    // Gate on index membership — only emit a Location for paths that resolve to an actually
    // existing, indexed project file. `resolve_res_path` returns Some only for indexed (on-disk)
    // files; if save.gd is absent from disk, this returns None → no Location emitted.
    let fid = state.workspace.index.resolve_res_path(&res_path)?;
    let abs = state.workspace.index.path(fid).map(|p| p.to_path_buf())?;
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
/// Project-wide candidate collection for method/signal-shaped names — Godot's two-phase
/// workspace strategy (workspace.cpp:472, adopted in M6-E): enumerate every indexed file, keep
/// the ones whose TEXT contains `name` (VFS-first read, no analysis), and let the caller
/// lazy-analyze only those hits. The interface-level `name_referencers` set cannot see body-only
/// uses (a method called through a typed var never appears in the caller's *interface*), so
/// `references` and `incomingCalls` both fan out through this scan — riding `name_referencers`
/// alone left `callHierarchy/incomingCalls` structurally blind to cross-file callers (caught by
/// the v1.0.3 real-project walk).
fn method_scan_candidate_uris(
    state: &mut ServerState,
    name: &str,
    exclude_fid: Option<gd_project::FileId>,
    log_ctx: &str,
) -> Vec<(camino::Utf8PathBuf, Uri)> {
    // Collect (FileId → path) from the index first (index borrow), then read text separately
    // (VFS / disk borrow) so borrows don't overlap.
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
        if exclude_fid.is_some_and(|e| e == fid) {
            continue;
        }
        let Some(cand_uri) = path_to_file_uri(&p) else {
            log::warn!("{log_ctx}: dropping candidate {p} — path_to_file_uri rejected the path");
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
                "{log_ctx}: skipping candidate {p} (unreadable); \
                 cross-file results may be under-reported"
            );
            continue;
        };
        if text.contains(name) {
            out.push((p, cand_uri));
        }
    }
    out
}

/// Group a handler's filtered `Binding::Call` stream into `(key, fromRanges)` pairs, preserving
/// first-seen order (small N ⇒ linear find). `key_of` derives the grouping key from each binding
/// (callee identity for `outgoingCalls`, caller name for `incomingCalls`) and returns `None` to
/// skip a binding (e.g. a future non-`Call` variant). Shared by both callHierarchy handlers.
///
/// `callee_spans` (from [`callee_name_token_spans`] over the same file's tree) narrows each
/// `call_site` — the WHOLE call expression — to its callee name token before mapping, so a
/// multi-line call contributes a single-identifier-width range. A call with no identifier-shaped
/// callee (defensive: `super()`, malformed callees) falls back to the full call span rather
/// than dropping the range.
fn group_call_ranges<'a, K: PartialEq>(
    bindings: impl Iterator<Item = &'a Binding>,
    mapper: &PositionMapper,
    callee_spans: &FxHashMap<ByteSpan, ByteSpan>,
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
        let token_span = callee_spans.get(call_site).copied().unwrap_or(*call_site);
        let range = mapper.span_to_range(token_span);
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
/// Algorithm (per the M4 plan §6 + `docs/03 §7.1`, updated for M6-E and the precision rework):
///   1. Resolve cursor → identifier name, then classify the target:
///      - **Method/signal targets** (structural check) keep the M6-E shape: call-site
///        projection filtered by the callee's declaring file where resolved, raw scan where
///        not.
///      - **Non-method targets** classify via [`NonMethodTarget`]: a Use binding at the cursor
///        or a root-class member declaration ⇒ a resolved MEMBER target; an enclosing-function
///        local/parameter ⇒ a LOCAL target; everything else (class/enum/type names, autoloads,
///        unresolvable buffers) ⇒ the raw-scan residue.
///   2. Choose candidate files:
///      - Method/signal/autoload targets AND resolved member targets: project-wide textual
///        scan matching Godot's `gdscript_workspace.cpp:472` two-phase strategy — enumerate
///        ALL project files, read text (VFS/disk; no analysis), keep files whose text contains
///        `name`. This catches accesses through body-local typed vars
///        (`var l: Lib = Lib.new(); l.helper()` / `l.speed`) that never appear in
///        `name_referencers`.
///      - Local targets: none (locals cannot be referenced cross-file).
///      - Residue targets: `Index::name_referencers(name)` fast-path (interface-pass filter).
///   3. For each candidate (plus the current buffer): lazy-parse, lazy-analyze, then collect
///      and de-dupe per the classification: resolved member targets project ONLY
///      `Binding::Use` records filtered by `(declaring file, name)`
///      (`push_use_binding_locations_for`) — the raw scan's cross-class bleed is exactly what
///      this removes; local targets scan identifiers within the enclosing function; residue
///      targets keep both the loose binding scan and the raw identifier scan
///      (`push_identifier_locations` — `extends Foo`, `class_name`, annotations). The
///      never-under-report floor thus survives exactly where resolution can't decide.
///      `Binding::Call` is intentionally NOT projected on non-method paths — its span is the
///      whole call expression, and callee identifiers ride the scans above.
///   4. Resolve the declaration site unconditionally (`find_in_file_definition` /
///      `find_global_class_definition` from the M3 definition path):
///      `includeDeclaration: true` prepends it; `false` FILTERS any scan hit on it at final
///      assembly.
///
/// Returns `None` when the cursor doesn't land on an identifier (LSP wire = null). Returns
/// `Some(vec)` (possibly empty) otherwise.
pub fn references(state: &mut ServerState, params: ReferenceParams) -> Option<Vec<Location>> {
    // M7 (#58): honor a client-supplied workDoneToken — references is the genuinely long
    // request (project-wide candidate analysis). `begin` is deferred to the candidate loop: a
    // request that resolves trivially (no identifier under the cursor, empty candidate set)
    // sends no progress at all instead of a begin→end flash, and an unbegun reporter's drop
    // guard sends nothing on the `?` early returns below.
    let mut progress = params
        .work_done_progress_params
        .work_done_token
        .map(|token| {
            crate::progress::ProgressReporter::for_client_token(state.sender.clone(), token)
        });
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

    let current_path = crate::uri::uri_to_path(&uri);
    let current_fid = current_path
        .as_deref()
        .and_then(|p| state.workspace.index.file_id(p));

    // Detect whether the cursor identifier is in a method-or-signal role. We use a structural AST
    // check (`is_member_or_attribute_ident`) rather than an interface lookup because:
    //   1. The interface only contains members of the *declaring* file — a click on a call site in
    //      another file (e.g. `l.helper()` in `a.gd`) won't find `helper` in `a.gd`'s interface.
    //   2. Private (`_`-prefixed) methods appear in the AST regardless of class_name visibility.
    // The check is O(#nodes) on the current file's parse tree — already in cache — with no
    // analyzer call. It handles declaration-click (`func helper():`) and call-site attribute-click
    // (`l.helper()`) identically. A bare property read (`node.position`) is deliberately NOT matched
    // (it isn't a call callee) so it falls through to the raw-identifier scan — the method path emits
    // `Binding::Call` records only and would otherwise drop every property-read occurrence.
    let is_method_or_signal = is_member_or_attribute_ident(&parsed.tree, node_id);

    // M6-D: an autoload singleton name (e.g. `Global` in `Global.popup_error()`) is the base of a
    // subscript, not a call callee, so `is_member_or_attribute_ident` returns false and it would
    // otherwise take the `name_referencers` fast-path below. But autoload names never appear in
    // interface-level class-name annotations, so that referencer set is always empty — a cursor on
    // the singleton name itself would then scan only the current file and silently miss every other
    // file that uses `Global` in a function body.
    //
    // Match the definition handler's "never lie" gate: only treat this occurrence as an autoload
    // when the analyzer resolved THIS cursor span to the autoload script's FileId. A local variable,
    // parameter, or member named `Global` shadows the singleton and must stay on the cheap
    // `name_referencers` path instead of triggering a project-wide textual scan.
    let autoload_fid = state
        .workspace
        .project
        .autoload_script_path(&name)
        .and_then(|p| state.workspace.index.resolve_res_path(&p));
    let is_autoload = autoload_fid.is_some_and(|fid| {
        let node_span = parsed.tree.get(node_id).span;
        let Some(p) = current_path
            .as_ref()
            .filter(|p| p.extension() == Some("gd"))
        else {
            return false;
        };
        let result = analyze_with_request_token(state, &key, p, &parsed.tree, &text);
        result.bindings().iter().any(|b| {
            matches!(b,
                Binding::Use { site, target_file: Some(f), .. }
                    if *site == node_span && *f == fid
            )
        })
    });

    // For method/signal targets, compute `target_file` — the FileId of the file that DECLARES
    // the method. Used to filter `Binding::Call` records to genuine callers of this specific
    // method, excluding identically-named methods in unrelated files (Fix 2 Part B, M6-E).
    //
    // Priority:
    //   1. Call-site click: find a Binding::Call in the current file's analysis whose callee
    //      identifier (derived from the parse tree) spans the cursor byte position. The
    //      `callee_file` of that binding is the declaring file.
    //   2. Declaration-site click: `current_fid` (the file where `func name():` lives is the
    //      current file).
    //   3. `None` — if the file isn't indexed or the callee is native/unresolved. When None,
    //      fall back to raw identifier scan (no false-negative for unresolvable targets).
    // Shared lazy callee-identifier span map for the current file's parse tree, built at most once
    // and reused by both the call-site probe below and push_callee_ident_locations in the
    // current-file scan — eliminates a duplicate O(nodes) tree walk per references request.
    let mut callee_spans: Option<FxHashMap<ByteSpan, ByteSpan>> = None;
    let target_file: Option<gd_project::FileId> = if is_method_or_signal {
        // Analyze the current file to determine target_file.
        let target_file_from_binding = if let Some(p) = current_path
            .as_ref()
            .filter(|p| p.extension() == Some("gd"))
        {
            let cur_result = analyze_with_request_token(state, &key, p, &parsed.tree, &text);
            // Look for a Binding::Call whose callee identifier span (in the parse tree)
            // contains the cursor byte. If found (call-site click), target_file = the callee's
            // Script-declaring file. The shared callee-span map (`callee_spans`, hoisted above)
            // is built lazily on the first matching binding and reused by
            // push_callee_ident_locations below.
            cur_result.bindings().iter().find_map(|b| {
                if let Binding::Call {
                    callee,
                    callee_name,
                    call_site,
                    ..
                } = b
                {
                    if callee_name == name.as_str() {
                        let spans =
                            callee_spans.get_or_insert_with(|| callee_ident_spans(&parsed.tree));
                        if let Some(ident_span) = spans.get(call_site).copied() {
                            if ident_span.start <= byte && byte < ident_span.end {
                                return Some(callee.script_file());
                            }
                        }
                    }
                }
                None
            })
        } else {
            None
        };
        // Distinguish the two None origins that the old `.flatten().or(current_fid)` conflated —
        // collapsing them dropped every cross-file reference for native subscript calls
        // (e.g. `node.queue_free()`, whose Binding::Call classifies a non-Script callee):
        //   Some(Some(f)) — call-site click on a resolved callee: the declaring file is `f`.
        //   Some(None)    — call-site click on a NATIVE/unresolved callee: keep target_file None so
        //                   the scan falls back to push_identifier_locations (raw text scan) rather
        //                   than filtering on a Script file no Binding::Call carries.
        //   None          — no Binding::Call at the cursor (declaration-site click): the current
        //                   file declares the method, so target_file = current_fid.
        match target_file_from_binding {
            Some(cf) => cf,
            None => current_fid,
        }
    } else {
        None
    };

    // Classify NON-method targets BEFORE declaration resolution — a LOCAL target's declaration
    // lives inside its function, and resolving it through the class-member table would return a
    // same-named member's declaration instead. See [`NonMethodTarget`]: a resolved member scans
    // binding-backed, a local scans its enclosing function, and only the unresolved residue
    // keeps the raw-scan floor.
    let mut non_method_target = NonMethodTarget::Unresolved;
    if !is_method_or_signal && !is_autoload {
        if let Some(p) = current_path
            .as_ref()
            .filter(|p| p.extension() == Some("gd"))
        {
            let result = analyze_with_request_token(state, &key, p, &parsed.tree, &text);
            let node_span = parsed.tree.get(node_id).span;
            non_method_target = classify_non_method_target(
                &parsed.tree,
                &result,
                node_span,
                byte,
                &name,
                current_fid,
            );
        }
    }

    // Resolve the declaration site(s) UNCONDITIONALLY — `includeDeclaration` is a filter, not
    // just a prepend: when `true` the declaration joins the result up front, and when `false`
    // any scan hit on the declaration's own name token must be REMOVED at final assembly (the
    // raw identifier scan below emits every matching token, declaration included; reference
    // servers implement the flag as exactly this filter).
    //
    // For method/signal targets, the declaring file may be different from the current file
    // (cross-file call-site click). When target_file is known and differs from the current file,
    // read the declaring file and use find_in_file_definition on its tree to get the narrow
    // identifier span (not MemberDecl.span, which is the whole func node).
    let declaration_locations: Vec<Location> = {
        let mut decls = Vec::new();
        let decl_found = if is_method_or_signal {
            if let Some(tf) = target_file {
                if current_fid.is_some_and(|cf| cf == tf) {
                    // Declaration-site click: the current file IS the declaring file.
                    if let Some(loc) = find_in_file_definition(&parsed.tree, &name, &uri, &mapper) {
                        decls.push(loc);
                        true
                    } else {
                        false
                    }
                } else {
                    // Cross-file call-site click: read the declaring file and locate the identifier.
                    let decl_loc = state
                        .workspace
                        .index
                        .path(tf)
                        .map(|p| p.to_path_buf())
                        .and_then(|decl_path| path_to_file_uri(&decl_path).map(|u| (decl_path, u)))
                        .and_then(|(decl_path, decl_uri)| {
                            let text = match state.vfs.get(decl_uri.as_str()).map(|d| d.text()) {
                                Some(t) => t,
                                None => std::fs::read_to_string(decl_path.as_std_path()).ok()?,
                            };
                            let decl_parsed = state
                                .workspace
                                .parse(&CanonicalKey::for_uri(&decl_uri), &text);
                            let decl_rope = Rope::from_str(&text);
                            let decl_mapper = PositionMapper::new(&decl_rope, enc);
                            find_in_file_definition(
                                &decl_parsed.tree,
                                &name,
                                &decl_uri,
                                &decl_mapper,
                            )
                        });
                    if let Some(loc) = decl_loc {
                        decls.push(loc);
                        true
                    } else {
                        false
                    }
                }
            } else {
                false
            }
        } else {
            false
        };
        if !decl_found {
            // Non-method targets: use the existing class_name / in-file fallback. Also handle the
            // autoload case — when the cursor was confirmed to be an autoload name, include the
            // autoload script's start-of-file location (mirrors the M6-D definition handler).
            if is_autoload {
                if let Some(loc) = find_autoload_definition(state, &name) {
                    decls.push(loc);
                }
            } else if let NonMethodTarget::Local(fn_span) = &non_method_target {
                // A local's declaration is its own identifier inside the enclosing function —
                // find_in_file_definition would wrongly return a same-named class member's.
                if let Some(loc) =
                    local_declaration_location(&parsed.tree, *fn_span, &name, &uri, &mapper)
                {
                    decls.push(loc);
                }
            } else if let Some(loc) = find_in_file_definition(&parsed.tree, &name, &uri, &mapper) {
                decls.push(loc);
            } else if let Some(loc) = find_global_class_definition(state, &name) {
                decls.push(loc);
            }
        }
        decls
    };
    if params.context.include_declaration {
        locations.extend(declaration_locations.iter().cloned());
    }

    // Always scan the current file — name_referencers is the interface-level filter (cross-file
    // dependents), not the self-references set. The body of the current file may contain many
    // uses of `name` that name_referencers won't surface. The analysis is the cached result the
    // classification above already computed.
    if let Some(p) = current_path
        .as_ref()
        .filter(|p| p.extension() == Some("gd"))
    {
        let result = analyze_with_request_token(state, &key, p, &parsed.tree, &text);
        if is_method_or_signal {
            push_binding_locations(&mut locations, &result, &name, &uri, &mapper);
            // For method/signal targets: use callee-filtered call projection instead of raw
            // identifier scan to avoid false positives from identically-named declarations.
            // Only project when target_file is Some — if None (native/unresolved), fall back
            // to identifier scan so we never under-report for unresolvable targets.
            if let Some(tf) = target_file {
                push_callee_ident_locations(
                    &mut locations,
                    &result,
                    &parsed.tree,
                    tf,
                    &name,
                    &uri,
                    &mapper,
                    &mut callee_spans,
                );
            } else {
                push_identifier_locations(&mut locations, &parsed.tree, &name, &uri, &mapper);
            }
        } else {
            match &non_method_target {
                NonMethodTarget::Member(tf) => {
                    // Binding-backed: every recorded use resolving to (declaring file, name) —
                    // bare member uses, `self.x` writes/reads, typed attribute accesses. The
                    // raw scan is NOT run here; its cross-class bleed is the bug this closes.
                    push_use_binding_locations_for(
                        &mut locations,
                        &result,
                        *tf,
                        &name,
                        &uri,
                        &mapper,
                    );
                }
                NonMethodTarget::Local(fn_span) => {
                    push_identifier_locations_within(
                        &mut locations,
                        &parsed.tree,
                        &name,
                        *fn_span,
                        &uri,
                        &mapper,
                    );
                }
                NonMethodTarget::Unresolved => {
                    // Residue floor (incl. autoload + Class/Enum targets): the binding scan
                    // plus the raw identifier scan, which picks up `extends Foo`, type
                    // annotations, `class_name`, and other parser-level refs the reducer
                    // doesn't record. The dedup pass collapses overlap.
                    push_binding_locations(&mut locations, &result, &name, &uri, &mapper);
                    push_identifier_locations(&mut locations, &parsed.tree, &name, &uri, &mapper);
                }
            }
        }
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
    // hits to the existing per-candidate analyze + callee-filtered-binding loop. Autoload singleton
    // names need the same project-wide scan: an autoload name appears in other files' function
    // bodies (`Global.popup_error()`) but never in their interface-level annotations, so
    // `name_referencers` would return an empty set and miss every cross-file use. For the remaining
    // non-method targets (class names, variables) the `name_referencers` fast-path is sufficient:
    // they can only be reached via their interface-level type annotation, which the interface pass
    // records.
    //
    // Cost: one VFS-or-disk read per project file per references request for method/signal targets.
    // This matches Godot's behavior; a future identifier-occurrence index could optimize it. Do NOT
    // full-analyze every file — text-prefilter first, analyze only textual hits.
    let candidates: Vec<(camino::Utf8PathBuf, Uri)> = if is_method_or_signal || is_autoload {
        method_scan_candidate_uris(state, &name, current_fid, "references")
    } else {
        match &non_method_target {
            // Resolved member: the same project-wide textual fan-out method targets use —
            // also a RECALL fix: `a.speed` through a body-local typed var never names `speed`
            // in the accessor's interface, so the old `name_referencers` set missed those
            // files entirely.
            NonMethodTarget::Member(_) => {
                method_scan_candidate_uris(state, &name, current_fid, "references")
            }
            // Locals can never be referenced from another file — no fan-out at all.
            NonMethodTarget::Local(_) => Vec::new(),
            NonMethodTarget::Unresolved => {
                // Fast-path for class/type names: only files whose interface mentions `name`
                // can reference it; `name_referencers` already has that set. (Autoloads are
                // excluded — they take the project-wide textual scan above since they never
                // appear in interface sets.)
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
                            "references: dropping candidate {p} — path_to_file_uri rejected \
                             the path"
                        ),
                    }
                }
                out
            }
        }
    };

    let candidate_total = candidates.len();
    if let Some(reporter) = progress.as_mut() {
        if candidate_total > 0 {
            reporter.begin("References", None);
        }
    }
    for (done, (path, cand_uri)) in candidates.into_iter().enumerate() {
        if let Some(reporter) = progress.as_mut() {
            crate::progress::ProgressSink::progress(
                reporter,
                done + 1,
                Some(candidate_total),
                "analyzing candidates",
            );
        }
        let Some((text, parsed, cand_result)) =
            load_candidate_analysis(state, &path, &cand_uri, "references")
        else {
            continue;
        };
        let rope = Rope::from_str(&text);
        let cand_mapper = PositionMapper::new(&rope, enc);
        if is_method_or_signal {
            push_binding_locations(&mut locations, &cand_result, &name, &cand_uri, &cand_mapper);
            // For method/signal targets: use callee-filtered call projection (accurate) rather
            // than raw identifier scan (which would pick up unrelated same-named declarations
            // like `func helper():` in other.gd). When target_file is None (native/unresolved),
            // fall back to identifier scan to avoid under-reporting.
            if let Some(tf) = target_file {
                let mut cand_callee_spans: Option<FxHashMap<ByteSpan, ByteSpan>> = None;
                push_callee_ident_locations(
                    &mut locations,
                    &cand_result,
                    &parsed.tree,
                    tf,
                    &name,
                    &cand_uri,
                    &cand_mapper,
                    &mut cand_callee_spans,
                );
            } else {
                push_identifier_locations(
                    &mut locations,
                    &parsed.tree,
                    &name,
                    &cand_uri,
                    &cand_mapper,
                );
            }
        } else {
            match &non_method_target {
                NonMethodTarget::Member(tf) => {
                    // Binding-backed only — a candidate's same-named member of a DIFFERENT
                    // class records a different declaring file and is filtered out here.
                    push_use_binding_locations_for(
                        &mut locations,
                        &cand_result,
                        *tf,
                        &name,
                        &cand_uri,
                        &cand_mapper,
                    );
                }
                // Local targets fan out no candidates (unreachable; kept exhaustive).
                NonMethodTarget::Local(_) => {}
                NonMethodTarget::Unresolved => {
                    // Residue floor: identifier scan picks up `extends Foo` and other
                    // parser-level refs the reducer doesn't record. De-dupes happen below.
                    push_binding_locations(
                        &mut locations,
                        &cand_result,
                        &name,
                        &cand_uri,
                        &cand_mapper,
                    );
                    push_identifier_locations(
                        &mut locations,
                        &parsed.tree,
                        &name,
                        &cand_uri,
                        &cand_mapper,
                    );
                }
            }
        }
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

    // includeDeclaration:false — the FILTER half of the flag (final assembly, so it holds no
    // matter which scan produced the hit): drop any result whose (uri, range) exactly equals a
    // declaration site. Exact equality is sound — declaration and scan locations for the same
    // identifier both come from `mapper.span_to_range` over the same span source.
    if !params.context.include_declaration {
        locations.retain(|l| {
            !declaration_locations
                .iter()
                .any(|d| d.uri.as_str() == l.uri.as_str() && d.range == l.range)
        });
    }

    Some(locations)
}

/// `textDocument/documentHighlight` (#67): the **in-file subset** of [`references`], tagged with
/// [`DocumentHighlightKind`] per occurrence.
///
/// Reuses `references`'s cursor→symbol resolution **verbatim** ([`cursor_identifier`],
/// [`is_member_or_attribute_ident`], [`classify_non_method_target`]/[`NonMethodTarget`]) — this is
/// NOT a new token scan and NOT a text-grep. It then drops every cross-file path: highlight fires on
/// cursor-rest (a hot interactive request), so there is no project-wide candidate fan-out and no
/// `workDoneProgress` reporter — only the request file's own occurrences are collected, exactly the
/// set `references` gathers for the current buffer.
///
/// Read/Write derivation (the one bit `references` doesn't need): [`Binding::Use`] carries no
/// access-mode field, so a per-request **write-set** of identifier/attribute [`Range`]s is built by
/// walking the parse tree once ([`assignment_write_ranges`] — every `Assignment` LHS, any operator
/// incl. compound `+=`, plus each initializing `var` declaration). A collected site whose range is
/// in that set is [`DocumentHighlightKind::WRITE`]; every other site is
/// [`DocumentHighlightKind::READ`]. Range-equality across the two passes is sound because both
/// derive from per-request [`PositionMapper`]s over the same buffer text and encoding, so a span
/// maps to an identical [`Range`] either way (the same invariant `references`'s own `dedup_by` /
/// declaration filter rely on). `Text` is never emitted: with the
/// AST in hand every occurrence classifies as read or write.
///
/// Returns `None` (LSP wire `null`) when the cursor doesn't land on an identifier; `Some(vec)`
/// (possibly empty) otherwise — mirroring `references`'s degrade contract.
pub fn document_highlight(
    state: &mut ServerState,
    params: DocumentHighlightParams,
) -> Option<Vec<DocumentHighlight>> {
    let tdp = params.text_document_position_params;
    let uri = tdp.text_document.uri.clone();
    let text = state.vfs.get(uri.as_str()).map(|d| d.text())?;
    let key = CanonicalKey::for_uri(&uri);
    let parsed = state.workspace.parse(&key, &text);

    let enc = state.encoding;
    // Own the Rope so the mapper doesn't borrow from state.vfs — frees us to call mutating state
    // methods (lazy-analyze) below. Mirrors `references`.
    let rope = Rope::from_str(&text);
    let mapper = PositionMapper::new(&rope, enc);
    let byte = mapper.position_to_byte(tdp.position);
    let node_id = parsed.tree.innermost_node_at(byte)?;
    let name = cursor_identifier(&parsed.tree, node_id)?;

    // Collect this file's occurrences (no cross-file fan-out), then dedup by range BEFORE
    // classifying so a single occurrence never yields two highlights with conflicting kinds.
    let mut sites = collect_in_file_highlight_sites(state, &key, &uri, &text, &parsed, byte, &name);
    let range_key = |r: &Range| (r.start.line, r.start.character, r.end.line, r.end.character);
    sites.sort_by_key(|loc| range_key(&loc.range));
    sites.dedup_by(|a, b| a.range == b.range);

    // Write-set: the LHS identifier/attribute ranges of every assignment plus every initializing
    // `var` declaration. A collected site in this set is a Write; everything else is a Read.
    let write_ranges = assignment_write_ranges(&parsed.tree, &mapper);
    let highlights = sites
        .into_iter()
        .map(|loc| DocumentHighlight {
            range: loc.range,
            kind: Some(if write_ranges.contains(&loc.range) {
                DocumentHighlightKind::WRITE
            } else {
                DocumentHighlightKind::READ
            }),
        })
        .collect();
    Some(highlights)
}

/// The current-file occurrences of the cursor symbol — the in-file half of [`references`]'s scan,
/// run through the identical resolution + collection helpers but with the cross-file candidate loop
/// removed. Returns [`Location`]s (with `uri` == the request file); the caller converts them to
/// [`DocumentHighlight`]s after deduping and classifying.
///
/// The declaration site is **always** included (documentHighlight has no `includeDeclaration` flag —
/// the declaring identifier is itself an in-file occupant the editor highlights). For a local target
/// it is omitted here because [`push_identifier_locations_within`] already emits the `var`/param
/// identifier in scope (appending it again would double the decl; the dedup runs in the caller).
fn collect_in_file_highlight_sites(
    state: &mut ServerState,
    key: &CanonicalKey,
    uri: &Uri,
    text: &str,
    parsed: &gd_syntax::ParseResult,
    byte: usize,
    name: &str,
) -> Vec<Location> {
    let enc = state.encoding;
    let rope = Rope::from_str(text);
    let mapper = PositionMapper::new(&rope, enc);
    let node_id = match parsed.tree.innermost_node_at(byte) {
        Some(id) => id,
        None => return Vec::new(),
    };
    let node_span = parsed.tree.get(node_id).span;

    let current_path = crate::uri::uri_to_path(uri);
    let current_fid = current_path
        .as_deref()
        .and_then(|p| state.workspace.index.file_id(p));

    // Same role detection as `references`: a method/signal callee (declaration click or call-site
    // attribute click) takes the call-projection path; everything else takes the non-method
    // classification. Purely structural — no analyzer call.
    let is_method_or_signal = is_member_or_attribute_ident(&parsed.tree, node_id);

    // An autoload singleton name resolves to the autoload script's FileId only when the analyzer
    // pinned THIS span to it — a local/param/member named the same shadows the singleton. Matches
    // `references`'s `is_autoload` gate (without the cross-file consequence, since we never fan
    // out).
    let autoload_fid = state
        .workspace
        .project
        .autoload_script_path(name)
        .and_then(|p| state.workspace.index.resolve_res_path(&p));
    let is_autoload = autoload_fid.is_some_and(|fid| {
        let Some(p) = current_path
            .as_ref()
            .filter(|p| p.extension() == Some("gd"))
        else {
            return false;
        };
        let result = analyze_with_request_token(state, key, p, &parsed.tree, text);
        result.bindings().iter().any(|b| {
            matches!(b,
                Binding::Use { site, target_file: Some(f), .. }
                    if *site == node_span && *f == fid
            )
        })
    });

    // For a method/signal target, resolve the declaring file (call-site click → the callee's
    // Script file; declaration click → the current file) so the call projection filters to genuine
    // call sites of THIS method, not identically-named methods. `None` (native/unresolved) falls
    // back to the raw identifier scan, never under-reporting.
    let mut callee_spans: Option<FxHashMap<ByteSpan, ByteSpan>> = None;
    let target_file: Option<gd_project::FileId> = if is_method_or_signal {
        let target_file_from_binding = if let Some(p) = current_path
            .as_ref()
            .filter(|p| p.extension() == Some("gd"))
        {
            let cur_result = analyze_with_request_token(state, key, p, &parsed.tree, text);
            cur_result.bindings().iter().find_map(|b| {
                if let Binding::Call {
                    callee,
                    callee_name,
                    call_site,
                    ..
                } = b
                {
                    if callee_name == name {
                        let spans =
                            callee_spans.get_or_insert_with(|| callee_ident_spans(&parsed.tree));
                        if let Some(ident_span) = spans.get(call_site).copied() {
                            if ident_span.start <= byte && byte < ident_span.end {
                                return Some(callee.script_file());
                            }
                        }
                    }
                }
                None
            })
        } else {
            None
        };
        match target_file_from_binding {
            Some(cf) => cf,
            None => current_fid,
        }
    } else {
        None
    };

    // Non-method classification (resolved member / local / unresolved residue) — resolved BEFORE
    // any scan so precision rides the binding layer where resolution succeeded and the raw-scan
    // floor survives where it can't decide. Identical to `references`.
    let mut non_method_target = NonMethodTarget::Unresolved;
    if !is_method_or_signal && !is_autoload {
        if let Some(p) = current_path
            .as_ref()
            .filter(|p| p.extension() == Some("gd"))
        {
            let result = analyze_with_request_token(state, key, p, &parsed.tree, text);
            non_method_target = classify_non_method_target(
                &parsed.tree,
                &result,
                node_span,
                byte,
                name,
                current_fid,
            );
        }
    }

    let mut locations: Vec<Location> = Vec::new();

    // Declaration site — always part of the highlight set (no includeDeclaration flag). Locals are
    // handled by the within-scope identifier scan below (which already emits the decl token), so
    // they're excluded here to avoid a duplicate the caller would then have to dedup.
    if is_autoload {
        if let Some(loc) = find_autoload_definition(state, name) {
            locations.push(loc);
        }
    } else if let NonMethodTarget::Local(_) = &non_method_target {
        // decl emitted by push_identifier_locations_within below.
    } else if let Some(loc) = find_in_file_definition(&parsed.tree, name, uri, &mapper) {
        locations.push(loc);
    } else if let Some(loc) = find_global_class_definition(state, name) {
        // Only keep an in-file declaration — a global class declared in ANOTHER file is not an
        // in-file occurrence and must not leak across the file boundary.
        if loc.uri == *uri {
            locations.push(loc);
        }
    }

    // Current-file occurrence scan — exactly `references`'s current-file block, with no candidate
    // fan-out afterwards.
    if let Some(p) = current_path
        .as_ref()
        .filter(|p| p.extension() == Some("gd"))
    {
        let result = analyze_with_request_token(state, key, p, &parsed.tree, text);
        if is_method_or_signal {
            push_binding_locations(&mut locations, &result, name, uri, &mapper);
            if let Some(tf) = target_file {
                push_callee_ident_locations(
                    &mut locations,
                    &result,
                    &parsed.tree,
                    tf,
                    name,
                    uri,
                    &mapper,
                    &mut callee_spans,
                );
            } else {
                push_identifier_locations(&mut locations, &parsed.tree, name, uri, &mapper);
            }
        } else {
            match &non_method_target {
                NonMethodTarget::Member(tf) => {
                    push_use_binding_locations_for(
                        &mut locations,
                        &result,
                        *tf,
                        name,
                        uri,
                        &mapper,
                    );
                }
                NonMethodTarget::Local(fn_span) => {
                    push_identifier_locations_within(
                        &mut locations,
                        &parsed.tree,
                        name,
                        *fn_span,
                        uri,
                        &mapper,
                    );
                }
                NonMethodTarget::Unresolved => {
                    push_binding_locations(&mut locations, &result, name, uri, &mapper);
                    push_identifier_locations(&mut locations, &parsed.tree, name, uri, &mapper);
                }
            }
        }
    }

    locations
}

/// The set of LSP [`Range`]s that denote a **write** in `tree`: the LHS of every assignment (a
/// plain `Identifier` for a local/bare member, or the trailing `.attr` of a `self.x`/`obj.x`
/// subscript), plus the identifier of every initializing `var` declaration. Any assignment operator
/// counts — a compound `+=`/`-=`/… reads-then-writes, which LSP/Godot still render as a write.
///
/// Built once per `documentHighlight` request and consulted by membership: a collected occurrence
/// whose range is in this set is a write, every other occurrence a read. Ranges (not byte spans) so
/// the lookup composes directly with the collected [`Location`]s, both produced by the same
/// per-request [`PositionMapper`].
fn assignment_write_ranges(tree: &ParseTree, mapper: &PositionMapper) -> FxHashSet<Range> {
    let mut out: FxHashSet<Range> = FxHashSet::default();
    for id in tree.iter_ids() {
        match &tree.get(id).kind {
            NodeKind::Assignment(a) => {
                let Some(assignee) = a.assignee else { continue };
                // The write target's NAME token: a bare/local assignee is the Identifier itself; a
                // `self.hp = …` / `obj.x = …` assignee is a Subscript whose `.attribute` identifier
                // is the member being written.
                let ident = match &tree.get(assignee).kind {
                    NodeKind::Identifier(_) => Some(assignee),
                    NodeKind::Subscript(s) => match s.access {
                        Some(SubscriptAccess::Attribute(attr)) => attr,
                        _ => None,
                    },
                    _ => None,
                };
                if let Some(iid) = ident {
                    out.insert(mapper.span_to_range(tree.get(iid).span));
                }
            }
            // An initializing declaration (`var x = …`) writes `x` at its declaration token. A bare
            // `var x` with no initializer is a pure declaration (Godot zero-inits, but there is no
            // user-written value) — it falls through to the `_` arm so it stays a Read occurrence.
            NodeKind::Variable(v) if v.initializer.is_some() => {
                if let Some(iid) = v.identifier {
                    out.insert(mapper.span_to_range(tree.get(iid).span));
                }
            }
            _ => {}
        }
    }
    out
}

// ---------------------------------------------------------------------------------------------------
// M9 (#70): foldingRange + selectionRange — pure parse-priced projections of the AST + the lexer's
// comment side-channel. No analyzer, no cross-file fan-out; served even at Hard memory pressure.
// ---------------------------------------------------------------------------------------------------

/// `textDocument/foldingRange`: the foldable regions of one document. Three sources, all from the
/// shared cached parse (no analysis):
///   * **compound AST blocks** (kind `Region`) — `class`/`func`/`if`+`else`/`for`/`while` and each
///     `match` arm, anchored on the construct's header line so collapsing hides the body. The inner
///     `Suite` is deliberately *skipped*: every GDScript suite has a compound parent, so folding the
///     suite too would emit a second overlapping fold on the same block;
///   * **comment runs** (kind `Comment`) — ≥2 consecutive own-line `#` comment lines collapse to one
///     fold (inline trailing comments are excluded — they aren't a standalone block);
///   * **`#region` / `#endregion`** (kind `Region`) — a string-prefix scan over each comment pairs
///     markers with a stack (so nested regions fold independently; an unmatched marker is dropped).
///
/// Respects the client's `textDocument.foldingRange` hints: `rangeLimit` truncates the
/// deterministically-sorted result, and `lineFoldingOnly` drops the `startCharacter`/`endCharacter`
/// columns (whole-line folds). Out-of-range / degenerate (single-line) folds are never emitted, and
/// a malformed/partial parse still folds whatever did parse — it never panics.
pub fn folding_range(
    state: &mut ServerState,
    params: lsp_types::FoldingRangeParams,
) -> Option<Vec<lsp_types::FoldingRange>> {
    let uri = params.text_document.uri;
    let doc = state.vfs.get(uri.as_str())?;
    let text = doc.text();
    let parsed = state.workspace.parse(&CanonicalKey::for_uri(&uri), &text);
    let mapper = PositionMapper::new(&doc.rope, state.encoding);

    let line_folding_only = state.caps.folding_line_folding_only;
    let mut ranges: Vec<lsp_types::FoldingRange> = Vec::new();

    // (1) Compound AST blocks. Each carries `node.span`; fold header-line → last-content-line.
    // The implicit root `Class` (the whole-file module wrapper Godot always synthesizes) is
    // skipped: editors don't fold the top-level script as one region, and it would shadow the
    // file. An explicit inner `class Foo:` still folds (it isn't the root).
    let root = parsed.tree.root_id();
    for id in parsed.tree.iter_ids() {
        if Some(id) == root {
            continue;
        }
        if !is_foldable_block(&parsed.tree.get(id).kind) {
            continue;
        }
        if let Some(fr) = block_fold(
            parsed.tree.get(id).span,
            &mapper,
            lsp_types::FoldingRangeKind::Region,
        ) {
            ranges.push(fr);
        }
    }

    // (2) + (3) Comment runs and `#region`/`#endregion`, from the lexer side-channel.
    ranges.extend(comment_folds(&parsed.comments, &text, &mapper));

    // Deterministic order (line, then end-line, then a stable kind tag) so `rangeLimit` truncation
    // and the wire output are reproducible; dedup coincident folds (e.g. a region marker pair whose
    // span happens to match a comment-run, or two constructs sharing extents).
    ranges.sort_by_key(|r| (r.start_line, r.end_line, fold_kind_rank(&r.kind)));
    ranges.dedup_by(|a, b| {
        a.start_line == b.start_line && a.end_line == b.end_line && a.kind == b.kind
    });

    // `lineFoldingOnly`: the client ignores columns — drop them for whole-line folds.
    if line_folding_only {
        for r in &mut ranges {
            r.start_character = None;
            r.end_character = None;
        }
    }

    // `rangeLimit`: a hint — keep the first N of the sorted set.
    if let Some(limit) = state.caps.folding_range_limit {
        ranges.truncate(limit as usize);
    }

    Some(ranges)
}

/// Whether a node kind is a foldable compound block (`foldingRange` source (1)). Mirrors the spec's
/// "fold the func/class/loop/branch body": the header-bearing compound statements plus each `match`
/// arm. `Suite` is intentionally absent (its compound parent already covers the block).
fn is_foldable_block(kind: &NodeKind) -> bool {
    matches!(
        kind,
        NodeKind::Class(_)
            | NodeKind::Function(_)
            | NodeKind::If(_)
            | NodeKind::For(_)
            | NodeKind::While(_)
            | NodeKind::Match(_)
            | NodeKind::MatchBranch(_)
    )
}

/// Project a block's byte span to a [`lsp_types::FoldingRange`], or `None` if it would be a
/// single-line (degenerate) fold. `end_line` uses the column-0 rule: a span that ends exactly at a
/// line start (i.e. it absorbed a trailing newline) folds up to the *previous* line, so the
/// blank/dedent line after the block stays visible; the usual block-body span ends at the last
/// token (column ≠ 0) and folds to that line directly. This never indexes a non-codepoint-boundary
/// byte (unlike `span.end - 1`), keeping the "never crash" guarantee on the UTF-16/32 mapper paths.
fn block_fold(
    span: ByteSpan,
    mapper: &PositionMapper,
    kind: lsp_types::FoldingRangeKind,
) -> Option<lsp_types::FoldingRange> {
    let start = mapper.byte_to_position(span.start);
    let end = mapper.byte_to_position(span.end);
    let end_line = if end.character == 0 {
        end.line.saturating_sub(1)
    } else {
        end.line
    };
    if start.line >= end_line {
        return None;
    }
    Some(lsp_types::FoldingRange {
        start_line: start.line,
        start_character: Some(start.character),
        end_line,
        end_character: Some(end.character),
        kind: Some(kind),
        collapsed_text: None,
    })
}

/// Build the comment-run + `#region`/`#endregion` folds from the lexer's comment side-channel.
///
/// The map is keyed by 1-based line (unordered), each [`gd_syntax::CommentData`] carrying the
/// comment's byte span and a `new_line` flag (true ⇒ the comment owns its line; false ⇒ it trails
/// code). Two passes over the line-sorted comments:
///   * **runs** — maximal blocks of ≥2 *consecutive own-line* lines whose comment is NOT a region
///     marker collapse to one `Comment` fold (start line's `#` → end line's content);
///   * **regions** — a stack pairs each `#region` with the next `#endregion` (nested regions fold
///     independently); an unmatched `#region`/`#endregion` is dropped.
fn comment_folds(
    comments: &std::collections::HashMap<u32, gd_syntax::CommentData>,
    text: &str,
    mapper: &PositionMapper,
) -> Vec<lsp_types::FoldingRange> {
    let mut out: Vec<lsp_types::FoldingRange> = Vec::new();
    // Line-sorted view: (1-based line, span, new_line, region-marker).
    let mut lines: Vec<(u32, ByteSpan, bool, RegionMarker)> = comments
        .iter()
        .map(|(&line, c)| {
            (
                line,
                c.span,
                c.new_line,
                region_marker(comment_text(text, c.span)),
            )
        })
        .collect();
    lines.sort_by_key(|&(line, ..)| line);

    // (a) Comment runs: contiguous own-line, non-region-marker comment lines, length ≥ 2.
    let mut i = 0;
    while i < lines.len() {
        let runnable = |t: &(u32, ByteSpan, bool, RegionMarker)| t.2 && t.3 == RegionMarker::None;
        if !runnable(&lines[i]) {
            i += 1;
            continue;
        }
        let mut j = i;
        while j + 1 < lines.len() && runnable(&lines[j + 1]) && lines[j + 1].0 == lines[j].0 + 1 {
            j += 1;
        }
        if j > i {
            // span from the first comment's `#` to the last comment's end.
            let span = ByteSpan::new(lines[i].1.start, lines[j].1.end);
            if let Some(fr) = comment_run_fold(&mapper.span_to_range(span)) {
                out.push(fr);
            }
        }
        i = j + 1;
    }

    // (b) `#region` / `#endregion` pairs, stack-matched in line order. Only own-line comments
    // (`new_line`) are region markers — an inline trailing comment (`var x = 1  # region foo`) is
    // not a fold marker by VS Code / Godot convention.
    let mut stack: Vec<(u32, ByteSpan)> = Vec::new();
    for &(line, span, new_line, marker) in &lines {
        if !new_line {
            continue;
        }
        match marker {
            RegionMarker::Begin => stack.push((line, span)),
            RegionMarker::End => {
                if let Some((_open_line, open_span)) = stack.pop() {
                    // Fold from the `#region` line down to the `#endregion` line.
                    let region = ByteSpan::new(open_span.start, span.end);
                    if let Some(fr) =
                        block_fold(region, mapper, lsp_types::FoldingRangeKind::Region)
                    {
                        out.push(fr);
                    }
                }
            }
            RegionMarker::None => {}
        }
    }
    out
}

/// A `#region` / `#endregion` marker classification of a comment's text.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RegionMarker {
    Begin,
    End,
    None,
}

/// Classify a comment's source text as a `#region` / `#endregion` marker (Godot / VS Code folding
/// convention). The scan tolerates leading whitespace after `#` and an optional trailing label
/// (`#region Foo`); `#endregion` is checked before `#region` so the shared `#r…`/`#e…` prefixes
/// don't misfire. Anything else is [`RegionMarker::None`] — including `##` doc comments, since only
/// exactly one leading `#` is stripped (a `##region` line is doc prose, not a fold marker).
fn region_marker(comment: &str) -> RegionMarker {
    // Strip exactly one leading `#` and surrounding spaces, then match the keyword head. Stripping
    // only one `#` means a `##`-doc-comment line (`## region …`) keeps a leading `#` and falls to
    // `None` rather than minting a spurious region from documentation text.
    let body = comment.strip_prefix('#').unwrap_or(comment).trim_start();
    let head = body.split(|c: char| c.is_whitespace()).next().unwrap_or("");
    if head == "endregion" {
        RegionMarker::End
    } else if head == "region" {
        RegionMarker::Begin
    } else {
        RegionMarker::None
    }
}

/// Slice a comment's source text from its byte span (clamped — the span comes from the same source
/// the parse ran on, so it is in-bounds, but clamp defensively for the "never crash" guarantee).
fn comment_text(text: &str, span: ByteSpan) -> &str {
    let start = span.start.min(text.len());
    let end = span.end.min(text.len());
    text.get(start..end).unwrap_or("")
}

/// Project a comment-run (already mapped to `range`) to a `Comment` fold, dropping a single-line
/// (degenerate) run. The run's end span is the last comment's text end (column > 0), so — unlike a
/// block — no column-0 decrement applies; the end line IS the last comment line.
fn comment_run_fold(range: &Range) -> Option<lsp_types::FoldingRange> {
    if range.start.line >= range.end.line {
        return None;
    }
    Some(lsp_types::FoldingRange {
        start_line: range.start.line,
        start_character: Some(range.start.character),
        end_line: range.end.line,
        end_character: Some(range.end.character),
        kind: Some(lsp_types::FoldingRangeKind::Comment),
        collapsed_text: None,
    })
}

/// A stable tie-break rank for the fold sort so equal `(start_line, end_line)` folds order
/// deterministically by kind (and `dedup` sees identical-kind neighbors adjacently).
fn fold_kind_rank(kind: &Option<lsp_types::FoldingRangeKind>) -> u8 {
    match kind {
        Some(lsp_types::FoldingRangeKind::Comment) => 0,
        Some(lsp_types::FoldingRangeKind::Imports) => 1,
        Some(lsp_types::FoldingRangeKind::Region) => 2,
        None => 3,
    }
}

/// `textDocument/selectionRange`: for each requested cursor position, the "smart-select" ancestor
/// chain — the innermost AST node covering the position, then its nearest strictly-enclosing
/// ancestor, and so on to the root — as a `parent`-linked [`lsp_types::SelectionRange`] (innermost
/// first). Each parent range **strictly** contains its child (the helper excludes equal-span nodes),
/// so the chain is strictly increasing with no duplicate/looping range.
///
/// The result is index-aligned with `params.positions` (one entry per position, never dropped): a
/// position over no node (empty/partial parse, or past end-of-input) still yields a degenerate
/// single-point range at the clamped position, so the client can rely on `result[i]` ↔
/// `positions[i]`. Parse-priced; never panics.
pub fn selection_range(
    state: &mut ServerState,
    params: lsp_types::SelectionRangeParams,
) -> Option<Vec<lsp_types::SelectionRange>> {
    let uri = params.text_document.uri;
    let doc = state.vfs.get(uri.as_str())?;
    let text = doc.text();
    let parsed = state.workspace.parse(&CanonicalKey::for_uri(&uri), &text);
    let mapper = PositionMapper::new(&doc.rope, state.encoding);

    let out = params
        .positions
        .iter()
        .map(|&pos| {
            let byte = mapper.position_to_byte(pos);
            selection_chain_at(&parsed.tree, byte, &mapper).unwrap_or_else(|| {
                // Never drop a position: a degenerate point range keeps result/position alignment.
                // Round-trip the byte back to a position so an out-of-range request is answered at
                // the *clamped* spot (clamp-don't-lie) rather than echoing the bogus coordinates.
                let clamped = mapper.byte_to_position(byte);
                lsp_types::SelectionRange {
                    range: Range {
                        start: clamped,
                        end: clamped,
                    },
                    parent: None,
                }
            })
        })
        .collect();
    Some(out)
}

/// Build the `parent`-linked [`lsp_types::SelectionRange`] ancestor chain for `byte`: start at the
/// innermost node, walk up via [`crate::completion_context::smallest_node_strictly_containing`]
/// (the same nearest-strictly-enclosing-ancestor step completion uses) until nothing larger
/// contains it, then thread the spans into a child→parent linked list. `None` when no node covers
/// `byte`. A loop guard (bounded by the node count) makes a malformed tree's span relationships
/// unable to spin.
fn selection_chain_at(
    tree: &ParseTree,
    byte: usize,
    mapper: &PositionMapper,
) -> Option<lsp_types::SelectionRange> {
    let innermost = tree.innermost_node_at(byte)?;
    // Collect node ids innermost → outermost.
    let mut chain: Vec<NodeId> = vec![innermost];
    let mut cur = innermost;
    let mut guard = tree.len();
    while let Some(parent) = crate::completion_context::smallest_node_strictly_containing(tree, cur)
    {
        chain.push(parent);
        cur = parent;
        guard = guard.saturating_sub(1);
        if guard == 0 {
            break;
        }
    }
    // Thread from outermost back to innermost so each link points at its parent.
    let mut node: Option<Box<lsp_types::SelectionRange>> = None;
    for &id in chain.iter().rev() {
        node = Some(Box::new(lsp_types::SelectionRange {
            range: mapper.span_to_range(tree.get(id).span),
            parent: node,
        }));
    }
    node.map(|b| *b)
}

/// How `references` should scan for a NON-method cursor target — resolved before any scan runs,
/// so precision rides the binding layer where resolution succeeded and the raw-scan floor
/// survives exactly where it can't decide.
enum NonMethodTarget {
    /// The cursor's symbol is a member DECLARED in this file: scan `Binding::Use` records
    /// filtered by `(declaring file, name)` — two unrelated `var speed`s in different classes
    /// stop reporting each other's sites.
    Member(gd_project::FileId),
    /// A local/parameter of the enclosing function (its span): scan identifiers within that
    /// function only — locals can never be referenced cross-file, so the old project-wide scan
    /// was pure over-report.
    Local(ByteSpan),
    /// Couldn't resolve — the documented "over-approximate, never under-report" residue floor
    /// (raw identifier scan). Class/Enum/EnumValue targets classify here DELIBERATELY:
    /// `extends Foo`, `class_name`, and type annotations are resolver-level references with no
    /// bindings, so a binding-only scan would under-report them.
    Unresolved,
}

/// Classify a non-method cursor target (see [`NonMethodTarget`]). Resolution sources, in order:
/// a `Binding::Use` at the exact cursor span (attribute reads, bare member uses — inheriting
/// the analyzer's resolution by construction), the root class's own member declarations (a
/// declaration click), and the enclosing function's local declarations.
fn classify_non_method_target(
    tree: &ParseTree,
    result: &AnalysisResult,
    node_span: ByteSpan,
    byte: usize,
    name: &str,
    current_fid: Option<gd_project::FileId>,
) -> NonMethodTarget {
    for b in result.bindings() {
        if let Binding::Use {
            target_file: Some(f),
            target_kind,
            target_name,
            site,
        } = b
        {
            // Kind guard: only Class/Enum/EnumValue are excluded (their references live in
            // annotations/extends/match-patterns the reducer doesn't record — binding-only
            // would under-report them). Function/Signal/Variable/Constant/Member DELIBERATELY
            // pass: a function or signal reaching here is a NON-call-position reference
            // (`var f = obj.method`, `obj.sig` reads — call positions took the method path),
            // and record_member_use's precise kinds resolve exactly those. Parameter never
            // reaches here today (locals/params record no Use) — if it ever does, it belongs
            // with the passing set, not the exclusions.
            if *site == node_span
                && target_name == name
                && !matches!(
                    target_kind,
                    BindingTargetKind::Class
                        | BindingTargetKind::Enum
                        | BindingTargetKind::EnumValue
                )
            {
                return NonMethodTarget::Member(*f);
            }
        }
    }
    if let Some(fid) = current_fid {
        if let Some(root) = tree.root() {
            if let NodeKind::Class(class) = &root.kind {
                for m in &class.members {
                    // Class/Enum declaration clicks keep the union path (their references
                    // live in annotations/extends the reducer doesn't record).
                    if matches!(m, Member::Class(_) | Member::Enum(_)) {
                        continue;
                    }
                    if let Some(decl) = member_named(tree, m, name) {
                        if let Some(ident) = declaration_identifier(tree, decl) {
                            if tree.get(ident).span == node_span {
                                return NonMethodTarget::Member(fid);
                            }
                        }
                    }
                }
            }
        }
    }
    if let Some(fn_span) = enclosing_function_declaring(tree, byte, name) {
        return NonMethodTarget::Local(fn_span);
    }
    NonMethodTarget::Unresolved
}

/// The span of the smallest function containing `byte`, iff that function declares `name` as a
/// parameter or a body-local var/const. A class-level member can never pass the span filter
/// (declarations outside the function body), so a member target never mis-classifies as local.
///
/// Two bounded arena passes (find the enclosing function, then scan its contained nodes) — the
/// flat arena has no parent pointers or per-subtree iteration, so this is the same O(#nodes)
/// family as the sibling cursor walks (`innermost_node_at`, `is_member_or_attribute_ident`)
/// and runs at most once per references request.
fn enclosing_function_declaring(tree: &ParseTree, byte: usize, name: &str) -> Option<ByteSpan> {
    let mut best: Option<ByteSpan> = None;
    for id in tree.iter_ids() {
        if let NodeKind::Function(_) = &tree.get(id).kind {
            let span = tree.get(id).span;
            if span.start <= byte
                && byte < span.end
                && best.is_none_or(|s| span.end - span.start < s.end - s.start)
            {
                best = Some(span);
            }
        }
    }
    let fn_span = best?;
    for id in tree.iter_ids() {
        let node = tree.get(id);
        if node.span.start < fn_span.start || node.span.end > fn_span.end {
            continue;
        }
        let ident = match &node.kind {
            NodeKind::Parameter(p) => p.identifier,
            NodeKind::Variable(v) => v.identifier,
            NodeKind::Constant(c) => c.identifier,
            _ => None,
        };
        if ident.is_some_and(|iid| ident_name(tree, iid) == name) {
            return Some(fn_span);
        }
    }
    None
}

/// Append a [`Location`] for every [`Binding::Use`] that resolved to the member `name` DECLARED
/// in `target_file` — the precise, binding-backed references path for resolved member targets.
/// The raw identifier scan is deliberately NOT run alongside this: its project-wide cross-class
/// bleed is exactly what the binding filter removes.
fn push_use_binding_locations_for(
    out: &mut Vec<Location>,
    result: &AnalysisResult,
    target_file: gd_project::FileId,
    name: &str,
    uri: &Uri,
    mapper: &PositionMapper,
) {
    for binding in result.bindings() {
        if let Binding::Use {
            target_file: Some(tf),
            target_name,
            site,
            ..
        } = binding
        {
            if *tf == target_file && target_name == name {
                out.push(Location {
                    uri: uri.clone(),
                    range: mapper.span_to_range(*site),
                });
            }
        }
    }
}

/// The declaration [`Location`] of the local/parameter `name` inside `scope` (the enclosing
/// function's span): the first `Parameter`/`Variable`/`Constant` identifier of that name in
/// arena order — the local-target analog of [`find_in_file_definition`].
fn local_declaration_location(
    tree: &ParseTree,
    scope: ByteSpan,
    name: &str,
    uri: &Uri,
    mapper: &PositionMapper,
) -> Option<Location> {
    for id in tree.iter_ids() {
        let node = tree.get(id);
        if node.span.start < scope.start || node.span.end > scope.end {
            continue;
        }
        let ident = match &node.kind {
            NodeKind::Parameter(p) => p.identifier,
            NodeKind::Variable(v) => v.identifier,
            NodeKind::Constant(c) => c.identifier,
            _ => None,
        };
        if let Some(iid) = ident {
            if ident_name(tree, iid) == name {
                return Some(Location {
                    uri: uri.clone(),
                    range: mapper.span_to_range(tree.get(iid).span),
                });
            }
        }
    }
    None
}

/// [`push_identifier_locations`] restricted to identifiers inside `scope` — the function-scoped
/// references path for locals/parameters. A same-named member ACCESS inside the same function
/// still matches (bounded over-approximation); the cross-function and cross-file bleed is gone.
fn push_identifier_locations_within(
    out: &mut Vec<Location>,
    tree: &ParseTree,
    name: &str,
    scope: ByteSpan,
    uri: &Uri,
    mapper: &PositionMapper,
) {
    // This is the LOCAL/PARAMETER resolution path (the only callers are the `NonMethodTarget::Local`
    // arms). A local is a bare identifier in its function scope; it can NEVER be reached as the
    // attribute of a member access (`self.x` / `obj.x` — that `x` is a MEMBER, a different symbol).
    // The flat by-name scan would otherwise grab those attribute idents — harmless as a read (a
    // documentHighlight panel), but CORRUPTING for rename (its first mutating consumer): renaming a
    // local `x` would rewrite `self.x` into a dangling member reference (the BLOCKER-6 case). So
    // collect every attribute-position identifier (`SubscriptAccess::Attribute(Some(aid))` names it)
    // and exclude it — making the local resolution binding-correct w.r.t. member accesses.
    let mut attribute_idents: FxHashSet<NodeId> = FxHashSet::default();
    for id in tree.iter_ids() {
        if let NodeKind::Subscript(s) = &tree.get(id).kind {
            if let Some(SubscriptAccess::Attribute(Some(aid))) = s.access {
                attribute_idents.insert(aid);
            }
        }
    }
    for id in tree.iter_ids() {
        let node = tree.get(id);
        if node.span.start < scope.start || node.span.end > scope.end {
            continue;
        }
        if attribute_idents.contains(&id) {
            continue;
        }
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

/// Append a [`Location`] for every [`Binding::Use`] in `result.bindings` whose `target_name` is
/// `name`. [`Binding::Call`] is deliberately excluded (see the body comment): the callee-identifier
/// occurrence of every call is already covered by [`push_identifier_locations`] at the correct,
/// narrower range. The name-only filter belongs to the UNRESOLVED-target residue path (and the
/// method path's loose current-file scan) — resolved member targets take
/// [`push_use_binding_locations_for`]'s file-filtered projection instead.
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

/// Build, in one pass over `tree`, a map from each subscript-call's full [`ByteSpan`] to the span
/// of its callee attribute-identifier (e.g. `l.helper()` → span of the `helper` identifier, not
/// the whole `l.helper()` expression), following `CallNode.callee → SubscriptNode →
/// Attribute(ident_id)`. Callees that aren't a subscript-attribute (bare function calls,
/// `super()`, …) are absent from the map by design — their callee identifier is pre-reduced into a
/// `Binding::Use` (reducer Call arm) and reported via [`push_binding_locations`], so projecting them
/// here too would only double-report the same narrow span (which the de-dupe would then collapse).
///
/// [`push_callee_ident_locations`] consults this instead of re-scanning the whole arena once per
/// matched `Binding::Call` — that was O(nodes × matching_bindings) per file, slow on a large
/// project with a frequently-named method (`update`, `ready`). Call spans are unique (distinct
/// calls occupy distinct source extents), so there's no key collision and a lookup reproduces the
/// old first-match result. The caller builds this lazily, so a file whose textual pre-filter
/// matched but has no matching call pays nothing.
fn callee_ident_spans(tree: &ParseTree) -> FxHashMap<ByteSpan, ByteSpan> {
    let mut map = FxHashMap::default();
    for nid in tree.iter_ids() {
        let node = tree.get(nid);
        let NodeKind::Call(call) = &node.kind else {
            continue;
        };
        let Some(callee_id) = call.callee else {
            continue;
        };
        if let NodeKind::Subscript(sub) = &tree.get(callee_id).kind {
            if let Some(SubscriptAccess::Attribute(Some(attr_id))) = sub.access {
                map.insert(node.span, tree.get(attr_id).span);
            }
        }
    }
    map
}

/// Call-node span → callee NAME-token span, for BOTH callee shapes: subscript-attribute callees
/// (`l.helper()` → `helper`, via [`callee_ident_spans`]) and bare identifier callees
/// (`helper()` → `helper`). Used by `prepare_call_hierarchy` to resolve a cursor on a call-site
/// callee, and by both callHierarchy follow-up handlers to narrow `Binding::Call.call_site` —
/// the whole call expression — down to the callee token for `fromRanges` (the conventional
/// "ranges at which the calls appear": rust-analyzer/gopls/clangd all emit the name token).
fn callee_name_token_spans(tree: &ParseTree) -> FxHashMap<ByteSpan, ByteSpan> {
    let mut map = callee_ident_spans(tree);
    for nid in tree.iter_ids() {
        let node = tree.get(nid);
        let NodeKind::Call(call) = &node.kind else {
            continue;
        };
        let Some(callee_id) = call.callee else {
            continue;
        };
        if matches!(tree.get(callee_id).kind, NodeKind::Identifier(_)) {
            map.insert(node.span, tree.get(callee_id).span);
        }
    }
    map
}

/// Append a [`Location`] for every [`Binding::Call`] in `result.bindings` where
/// `callee_file == target_file && callee_name == name`, emitting the **narrow callee-identifier
/// span** derived from the parse tree (via [`callee_ident_span`]).
///
/// This replaces [`push_identifier_locations`] for method/signal targets in the M6-E references
/// fix: raw textual identifier matching would include unrelated same-named declarations (e.g.
/// `func helper():` in `other.gd`) whereas this filters to genuine callers of the specific method
/// declared in `target_file`. Only subscript-attribute call sites are emitted here; bare and
/// `super` call sites are intentionally absent — but NOT dropped from references. The dispatcher
/// pre-reduces a bare callee (and a subscript callee's base) as an identifier, recording a
/// `Binding::Use` at that narrow span which [`push_binding_locations`] reports. (Bare calls DO
/// classify their declaring script on `CalleeTarget::Script` — recall for them rides that `Use`
/// binding, not this call projection; see `references_finds_bare_same_file_call` and
/// `references_finds_signal_emit_and_connect_sites`.)
///
/// Caller must ensure `target_file` is `Some` before calling; the `None` guard lives in
/// `references()` (fall back to `push_identifier_locations` when `target_file` is `None`).
// The 8th parameter is a shared lazy span-memo cache (`callee_spans`): it lets the current-file
// scan reuse the map already built by the target_file probe in `references`, eliminating a
// duplicate O(nodes) `callee_ident_spans` walk, while staying lazy for cross-file candidates.
// Bundling args into a struct or inlining the projection loop would be worse than this localized allow.
#[allow(clippy::too_many_arguments)]
fn push_callee_ident_locations(
    out: &mut Vec<Location>,
    result: &AnalysisResult,
    tree: &ParseTree,
    target_file: gd_project::FileId,
    name: &str,
    uri: &Uri,
    mapper: &PositionMapper,
    // Lazy callee-identifier span map for `tree`, built at most once and shared with the
    // target_file probe in `references` to avoid a duplicate O(nodes) tree walk per request.
    callee_spans: &mut Option<FxHashMap<ByteSpan, ByteSpan>>,
) {
    for binding in result.bindings() {
        if let Binding::Call {
            callee,
            callee_name,
            call_site,
            ..
        } = binding
        {
            if callee.script_file() == Some(target_file) && callee_name == name {
                let spans = callee_spans.get_or_insert_with(|| callee_ident_spans(tree));
                if let Some(span) = spans.get(call_site).copied() {
                    out.push(Location {
                        uri: uri.clone(),
                        range: mapper.span_to_range(span),
                    });
                }
            }
        }
    }
}

fn normalize_eq(a: &camino::Utf8Path, b: &camino::Utf8Path) -> bool {
    gd_project::normalize_path(a) == gd_project::normalize_path(b)
}

fn function_node_span_for_identifier(
    tree: &ParseTree,
    ident_id: NodeId,
    name: &str,
) -> Option<gd_syntax::ByteSpan> {
    for id in tree.iter_ids() {
        let node = tree.get(id);
        if let NodeKind::Function(f) = &node.kind {
            if f.identifier == Some(ident_id) && ident_name(tree, ident_id) == name {
                return Some(node.span);
            }
        }
    }
    None
}

fn function_identifier_span_for_decl(
    tree: &ParseTree,
    name: &str,
    decl_span: gd_syntax::ByteSpan,
) -> Option<gd_syntax::ByteSpan> {
    for id in tree.iter_ids() {
        let node = tree.get(id);
        if node.span == decl_span {
            if let NodeKind::Function(f) = &node.kind {
                if let Some(ident) = f.identifier {
                    if ident_name(tree, ident) == name {
                        return Some(tree.get(ident).span);
                    }
                }
            }
        }
    }
    None
}

// =============================================================================================
// WP-N3: textDocument/implementation.
// =============================================================================================

/// M6-G: if the cursor is on a root `Func` member of the file identified by `uri`, BFS the
/// inverse-extends graph to find subclasses and return `Location`s for each subclass that also
/// declares a method named `fn_name`. Returns `None` to fall through to the existing class-identifier
/// BFS when the cursor is NOT on that method declaration (e.g. on a class name, a variable, a local
/// with the same name, or an inner-class method).
fn find_method_overrides(
    state: &mut ServerState,
    fn_name: &str,
    uri: &Uri,
    tree: &ParseTree,
    node_id: NodeId,
    enc: crate::position::PositionEncoding,
) -> Option<Vec<Location>> {
    // Resolve the current file's FileId and interface.
    let current_path = crate::uri::uri_to_path(uri)?;
    let current_fid = state.workspace.index.file_id(&current_path)?;
    let iface = state.workspace.index.interface(current_fid)?;

    // The cursor must be on this file's root method declaration, not merely an identifier whose
    // text matches a method name. This keeps locals/params/call sites named like a method on the
    // class-level implementation path, and excludes inner-class methods that are not interface
    // members of this script's root class.
    let cursor_fn_span = function_node_span_for_identifier(tree, node_id, fn_name)?;
    let is_root_func = iface.members.iter().any(|m| {
        m.name == fn_name && m.kind == gd_project::MemberKind::Func && m.span == cursor_fn_span
    });
    if !is_root_func {
        return None;
    }

    // Seed the BFS on the current file's own class_name. The cursor is confirmed to be on a
    // method of THIS file, so an unnamed script simply has no named subclasses to find — return
    // an empty result, not `None`. `None` would fall through to the class-identifier BFS in
    // `implementation`, which would then look up the function name in the class_name registry; a
    // class whose name happens to equal the function name would wrongly surface its subclasses.
    let Some(seed_name) = iface.class_name.clone() else {
        return Some(Vec::new());
    };

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
            // A name-extends parent matches the known class_name set; a path-extends parent
            // (`extends "res://base.gd"`) resolves through the index and matches the declaring
            // file or any already-known subclass file. `current_fid` is checked explicitly
            // because `known_files` only ever holds discovered subclasses, never the BFS seed.
            let parent_known = match &sub_iface.extends {
                gd_project::Extends::Names(parts) => {
                    parts.last().is_some_and(|p| known_names.contains(p))
                }
                gd_project::Extends::Path(res_path) => state
                    .workspace
                    .index
                    .resolve_res_path(res_path)
                    .is_some_and(|f| f == current_fid || known_files.contains(&f)),
                gd_project::Extends::None => false,
            };
            if parent_known {
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

        // Check the subclass interface for the override and capture its span in one pass.
        let Some(sub_iface) = state.workspace.index.interface(fid) else {
            continue;
        };
        let Some(override_decl) = sub_iface
            .members
            .iter()
            .find(|m| m.name == fn_name && m.kind == gd_project::MemberKind::Func)
        else {
            continue;
        };

        // Point to the override's identifier span, not the whole `func ...:` signature. The
        // interface stores FunctionNode spans for members, so parse the candidate text below and
        // locate the matching FunctionNode identifier just like class-level implementation narrows
        // subclass locations to `class_name`'s identifier.
        let fallback_span = override_decl.span;

        let range = {
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
            let cand_parsed = gd_syntax::parse(&cand_text);
            let span = function_identifier_span_for_decl(&cand_parsed.tree, fn_name, fallback_span)
                .unwrap_or(fallback_span);
            let rope = Rope::from_str(&cand_text);
            let cand_mapper = PositionMapper::new(&rope, enc);
            cand_mapper.span_to_range(span)
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
    // root `func` identifier returns override locations rather than null. The helper verifies the
    // cursor is on that root method declaration, then BFSes the subclass graph (seeded on the
    // current file's class_name) and emits a Location for each subclass override.
    if let Some(locs) = find_method_overrides(state, &name, &uri, &parsed.tree, node_id, enc) {
        return Some(GotoDefinitionResponse::Array(locs));
    }

    // Only project class_names participate; native classes have no project subclasses to list.
    // Resolve the seed class's file up front: the BFS below matches path-extends subclasses
    // against it (`known_files` only ever holds discovered subclasses, never the seed), and the
    // emission loop excludes it from the results.
    let cursor_fid = {
        let entry = state.workspace.index.registry().get(&name)?;
        state.workspace.index.file_id(&entry.path)
    };

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
            // `extends Outer.Inner` ⇒ parent name = "Inner"; `extends Hero` ⇒ "Hero"). A
            // path-extends parent (`extends "res://hero.gd"`) resolves through the index and
            // matches the seed class's file or any already-known subclass file.
            let parent_known = match &iface.extends {
                gd_project::Extends::Names(parts) => {
                    parts.last().is_some_and(|p| known_names.contains(p))
                }
                gd_project::Extends::Path(res_path) => state
                    .workspace
                    .index
                    .resolve_res_path(res_path)
                    .is_some_and(|f| Some(f) == cursor_fid || known_files.contains(&f)),
                gd_project::Extends::None => false,
            };
            if parent_known {
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
// M9 (#69): textDocument/prepareTypeHierarchy + typeHierarchy/{supertypes,subtypes}.
//
// A class-tree navigator over the same structures `implementation` already walks: the
// `class_name` registry, the per-file `Interface::extends`, and the native `inherits` chain.
// `prepare` resolves the class under the cursor to ONE `TypeHierarchyItem`; the two follow-ups
// re-resolve that item from its `data` blob (never from a cursor) and walk ONE level — up
// (`supertypes`) or down (`subtypes`). The blob is what makes expansion survive past depth 2
// (the #50 call-hierarchy lesson): every returned item carries its own blob so the client can
// expand it again with no document position.
// =============================================================================================

/// The identity of a type, round-tripped through a [`TypeHierarchyItem::data`] blob so
/// `supertypes`/`subtypes` re-resolve the type WITHOUT a cursor (the #50 lesson — expansion must
/// survive past depth 2). Serialized compactly (anti-catalog W9 — only the minimal identity in
/// `data`): a project script is `{"fid": <FileId as u32>}`, a native engine class is
/// `{"native": "<ClassName>"}`.
#[derive(Clone, Debug)]
enum TypeRef {
    /// A project `.gd` file, keyed on its (1-based) [`FileId`](gd_project::FileId).
    Script(gd_project::FileId),
    /// A native engine class, keyed on its name in the [`NativeDb`](gd_types::native_db::NativeDb).
    Native(String),
}

impl TypeRef {
    /// Encode into the compact `data` JSON blob.
    fn to_data(&self) -> serde_json::Value {
        match self {
            TypeRef::Script(fid) => serde_json::json!({ "fid": fid.get() }),
            TypeRef::Native(name) => serde_json::json!({ "native": name }),
        }
    }

    /// Decode a `data` blob produced by [`Self::to_data`]. A blob with neither key (or a
    /// malformed one) yields `None` — the handler then degrades to the LSP `null` response rather
    /// than guessing (never crash, never lie).
    fn from_data(data: Option<&serde_json::Value>) -> Option<Self> {
        let data = data?;
        if let Some(fid) = data.get("fid").and_then(serde_json::Value::as_u64) {
            // `FileId::new` panics on 0; the index never mints 0, but a hand-forged blob could
            // carry it, so guard rather than trust the wire.
            let raw = u32::try_from(fid).ok()?;
            if raw == 0 {
                return None;
            }
            return Some(TypeRef::Script(gd_project::FileId::new(raw)));
        }
        if let Some(name) = data.get("native").and_then(serde_json::Value::as_str) {
            return Some(TypeRef::Native(name.to_owned()));
        }
        None
    }
}

/// Build the [`TypeHierarchyItem`] for a project script `fid`: name from its `class_name` (or the
/// file stem for an unnamed script), `uri`/`range`/`selectionRange` anchored at the class-name
/// identifier (the #48 name-token lesson — `selectionRange` is the identifier; `range` is set to
/// the same span, which trivially satisfies the LSP `range ⊇ selectionRange` containment rule),
/// and a `data` blob re-encoding the `fid` so the item re-resolves with no cursor.
fn script_hierarchy_item(
    state: &mut ServerState,
    fid: gd_project::FileId,
) -> Option<TypeHierarchyItem> {
    let path = state.workspace.index.path(fid)?.to_path_buf();
    let uri = path_to_file_uri(&path)?;
    let name = state
        .workspace
        .index
        .interface(fid)
        .and_then(|i| i.class_name.clone())
        .unwrap_or_else(|| path.file_stem().unwrap_or("script").to_owned());
    // Anchor on the class-name identifier when the script has one; else the file start (the
    // file-target convention shared with `script_decl_location`).
    let range = match script_decl_location(state, fid) {
        Some(loc) => loc.range,
        None => file_start_range(),
    };
    Some(TypeHierarchyItem {
        name,
        kind: LspSymbolKind::CLASS,
        tags: None,
        detail: None,
        uri,
        range,
        selection_range: range,
        data: Some(TypeRef::Script(fid).to_data()),
    })
}

/// Build the [`TypeHierarchyItem`] for a native engine class: anchored at its stub `class_name`
/// header (the same stub machinery `definition`/`typeDefinition` use, via the phase-3
/// [`native_class_header_location`] helper — anti-catalog W4: natives anchored at a real stub
/// `Location`, never a synthetic one). `range`/`selectionRange` are both the header token span.
/// Returns `None` for a name the DB doesn't know — `native_class_header_location` →
/// `ensure_class_stub` re-checks `class_named` internally and yields `None` (no stub written) for
/// a non-class name, so this degrades to no item without a separate guard (never a guess).
fn native_hierarchy_item(state: &ServerState, class: &str) -> Option<TypeHierarchyItem> {
    let loc = native_class_header_location(state, class)?;
    Some(TypeHierarchyItem {
        name: class.to_owned(),
        kind: LspSymbolKind::CLASS,
        tags: None,
        detail: None,
        uri: loc.uri,
        range: loc.range,
        selection_range: loc.range,
        data: Some(TypeRef::Native(class.to_owned()).to_data()),
    })
}

/// Build the [`TypeHierarchyItem`] for an already-resolved [`TypeRef`] — the shared item-builder
/// the supertypes/subtypes walks emit through, so every produced item carries its own re-resolving
/// `data` blob.
fn hierarchy_item(state: &mut ServerState, ty: &TypeRef) -> Option<TypeHierarchyItem> {
    match ty {
        TypeRef::Script(fid) => script_hierarchy_item(state, *fid),
        TypeRef::Native(name) => native_hierarchy_item(state, name),
    }
}

/// Resolve the parent named in a project script's `extends` clause to a [`TypeRef`], ONE level up.
/// Mirrors the per-hop resolution `Index::extends_chain_files` performs internally, but yields the
/// parent's identity (script `FileId` / native name) rather than walking the whole chain:
///   - `extends Foo` / `extends A.B` → the LAST identifier (`Foo`/`B`) resolved against the
///     `class_name` registry (→ `Script`) then the native DB (→ `Native`);
///   - `extends "res://base.gd"` → the path resolved through the index (→ `Script`);
///   - no `extends` → Godot implies `RefCounted`, a native base.
///
/// `None` when the parent can't be resolved (an unknown name / an unindexed path) — the walk then
/// stops rather than inventing a parent.
fn project_extends_parent(state: &ServerState, fid: gd_project::FileId) -> Option<TypeRef> {
    let iface = state.workspace.index.interface(fid)?;
    match &iface.extends {
        gd_project::Extends::None => Some(TypeRef::Native("RefCounted".to_owned())),
        gd_project::Extends::Path(res_path) => state
            .workspace
            .index
            .resolve_res_path(res_path)
            .map(TypeRef::Script),
        gd_project::Extends::Names(parts) => {
            let head = parts.last()?;
            resolve_type_name(state, head)
        }
    }
}

/// Resolve a bare type NAME to a [`TypeRef`]: a project `class_name` (registry) shadows a native
/// class of the same name, matching the analyzer's precedence (`class_name` before native in
/// `resolve_name`). `None` for a name that is neither.
fn resolve_type_name(state: &ServerState, name: &str) -> Option<TypeRef> {
    if let Some(entry) = state.workspace.index.registry().get(name) {
        if let Some(fid) = state.workspace.index.file_id(&entry.path) {
            return Some(TypeRef::Script(fid));
        }
    }
    if state.workspace.native.class_named(name).is_some() {
        return Some(TypeRef::Native(name.to_owned()));
    }
    None
}

/// `true` when project file `iface` directly extends the type `ty` (ONE hop, no transitive walk) —
/// the per-candidate predicate [`type_hierarchy_subtypes`] applies to every interface for the
/// direct-children-only walk. It encodes the **same** parent-matching rule as [`implementation`]'s
/// BFS body (last `extends` identifier → registry/native; or a `res://` path → the index), but is
/// deliberately a separate predicate rather than a shared extraction: `implementation` matches each
/// candidate against a *growing set* of known names/files (the transitive fixpoint), whereas this
/// matches against a *single* resolved [`TypeRef`]. Sharing one helper would force `implementation`
/// to restructure its set-membership test, so — exactly as `implementation` declines to share its
/// cursor prologue with `references` — `implementation`'s loop is left untouched and stays
/// byte-identical (the `implementation_overrides` regression suite proves it).
///   - `extends Foo`/`extends A.Foo` matches a `Script` parent by the parent's `class_name`, or a
///     `Native` parent by the engine class name (the last `extends` identifier);
///   - `extends "res://x.gd"` matches a `Script` parent by resolving the path through the index.
fn extends_matches(state: &ServerState, iface: &gd_project::Interface, ty: &TypeRef) -> bool {
    match &iface.extends {
        gd_project::Extends::Names(parts) => {
            let Some(last) = parts.last() else {
                return false;
            };
            // The parent name resolves the same way the cursor's type does; comparing resolved
            // `TypeRef`s (rather than raw strings) means a project `class_name` and a native class
            // that happen to share a name never cross-match.
            resolve_type_name(state, last).is_some_and(|parent| match (&parent, ty) {
                (TypeRef::Script(a), TypeRef::Script(b)) => a == b,
                (TypeRef::Native(a), TypeRef::Native(b)) => a == b,
                _ => false,
            })
        }
        gd_project::Extends::Path(res_path) => match ty {
            TypeRef::Script(target) => {
                state.workspace.index.resolve_res_path(res_path) == Some(*target)
            }
            TypeRef::Native(_) => false,
        },
        // A script with no `extends` implicitly extends `RefCounted` (Godot's implied base — see
        // `project_extends_parent`), so it IS a direct subtype of the native `RefCounted`. Matching
        // it here makes the supertypes/subtypes round-trip symmetric: a bare `class_name` reached by
        // walking up to `RefCounted` reappears when `RefCounted`'s subtypes are expanded.
        gd_project::Extends::None => matches!(ty, TypeRef::Native(n) if n == "RefCounted"),
    }
}

/// `textDocument/prepareTypeHierarchy`: resolve the class under the cursor and return ONE
/// [`TypeHierarchyItem`] the client passes to `typeHierarchy/{supertypes,subtypes}`. Per LSP 3.17,
/// returns `TypeHierarchyItem[]` or `null`.
///
/// Resolution order (precise, never a guess):
///   1. the cursor identifier as a project `class_name` (registry) — the everyday `class Foo`
///      navigator entry, anchored at `Foo`'s declaration;
///   2. else as a native engine class name (`Node`) — anchored at its stub header;
///   3. else, if the cursor sits on the CURRENT file's own root class (a `class_name` site already
///      covered by 1, or an UNNAMED script clicked on its `class`/`extends` header), the current
///      file itself — so an unnamed script is still a hierarchy entry (the spec's unnamed-fid path).
///
/// Index/parse-priced only (registry + interface + native DB — no analyzer), like `implementation`.
pub fn prepare_type_hierarchy(
    state: &mut ServerState,
    params: TypeHierarchyPrepareParams,
) -> Option<Vec<TypeHierarchyItem>> {
    let tdp = params.text_document_position_params;
    let uri = tdp.text_document.uri.clone();
    let text = state.vfs.get(uri.as_str()).map(|d| d.text())?;
    let parsed = state.workspace.parse(&CanonicalKey::for_uri(&uri), &text);

    let rope = Rope::from_str(&text);
    let mapper = PositionMapper::new(&rope, state.encoding);
    let byte = mapper.position_to_byte(tdp.position);
    let node_id = parsed.tree.innermost_node_at(byte)?;
    let name = cursor_identifier(&parsed.tree, node_id)?;

    // (1)/(2): the cursor names a project class or a native class. `resolve_type_name` applies the
    // analyzer's class_name-before-native precedence.
    if let Some(ty) = resolve_type_name(state, &name) {
        return hierarchy_item(state, &ty).map(|item| vec![item]);
    }

    // (3): unnamed-script fallback — the cursor is on the current file's root class header (its
    // `class`/`extends`/identifier region) but the name resolved to no registered/native class.
    // Treat the current file as the subject so an unnamed script (or one whose header identifier
    // isn't itself a global class_name) is still navigable. Gated on the cursor actually sitting
    // inside the root class node's header span — a click on an arbitrary unresolved identifier
    // deeper in the file must NOT return the whole file (that would be a guess).
    if cursor_on_root_class_header(&parsed.tree, byte) {
        let fid = uri_to_path(&uri).and_then(|p| state.workspace.index.file_id(&p))?;
        return script_hierarchy_item(state, fid).map(|item| vec![item]);
    }

    None
}

/// `true` when `byte` lies on the root class node's own header: its `class_name` identifier span,
/// or (for an unnamed script with no identifier) the head of the root class node, before its first
/// member. Used to gate the unnamed-script `prepareTypeHierarchy` fallback so it fires only on a
/// genuine "navigate THIS class" click, never on an arbitrary identifier in the body.
fn cursor_on_root_class_header(tree: &ParseTree, byte: usize) -> bool {
    let Some(root_id) = tree.root_id() else {
        return false;
    };
    let NodeKind::Class(root) = &tree.get(root_id).kind else {
        return false;
    };
    // Named root: the cursor is on the `class_name` identifier.
    if let Some(ident) = root.identifier {
        let s = tree.get(ident).span;
        if s.start <= byte && byte < s.end {
            return true;
        }
    }
    // Unnamed (or cursor off the identifier): accept the region before the first member — the
    // `extends …` header line. The first member's span start bounds the header; with no members,
    // the whole (tiny) class node is header.
    let first_member_start = root
        .members
        .iter()
        .filter_map(member_node_id)
        .map(|id| tree.get(id).span.start)
        .min();
    let root_span = tree.get(root_id).span;
    match first_member_start {
        Some(end) => root_span.start <= byte && byte < end,
        None => root_span.start <= byte && byte < root_span.end,
    }
}

/// The [`NodeId`] backing a [`Member`], for span queries. Group/category markers carry no node.
fn member_node_id(member: &Member) -> Option<NodeId> {
    Some(match member {
        Member::Class(id)
        | Member::Variable(id)
        | Member::Constant(id)
        | Member::Function(id)
        | Member::Signal(id)
        | Member::Enum(id) => *id,
        Member::EnumValue(v) => v.identifier?,
        Member::Group(_) => return None,
    })
}

/// `typeHierarchy/supertypes`: from the item's `data` blob, walk the `extends` chain UP exactly
/// one level. Per LSP 3.17, returns `TypeHierarchyItem[]` or `null`. Each returned item carries
/// its OWN `data` blob, so the client expands again (`supertypes` of a supertype) with no cursor —
/// the depth>2 guarantee.
///
///   - a project script → its `extends` parent (another project script, or a native base, or the
///     implied `RefCounted` for a script with no `extends`), via [`project_extends_parent`];
///   - a native class → its `inherits` parent (one hop up `NativeDb`), anchored at that class's
///     stub header. `Object` (no `inherits`) yields an empty list — the top of the chain.
///
/// GDScript single-inheritance means at most one parent, so the returned vec is 0- or 1-long.
pub fn type_hierarchy_supertypes(
    state: &mut ServerState,
    params: TypeHierarchySupertypesParams,
) -> Option<Vec<TypeHierarchyItem>> {
    let ty = TypeRef::from_data(params.item.data.as_ref())?;
    let parent = match ty {
        TypeRef::Script(fid) => project_extends_parent(state, fid),
        TypeRef::Native(name) => state
            .workspace
            .native
            .class_named(&name)
            .and_then(|c| c.inherits)
            .map(|sym| TypeRef::Native(state.workspace.native.name_of(sym).to_owned())),
    };
    // Always return a (possibly empty) array, never `null`: the cursor RESOLVED to a type here
    // (the blob decoded); "this type has no parent" is an empty list, distinct from "no type".
    let mut items = Vec::new();
    if let Some(parent) = parent {
        if let Some(item) = hierarchy_item(state, &parent) {
            items.push(item);
        }
    }
    Some(items)
}

/// `typeHierarchy/subtypes`: from the item's `data` blob, list the project files that DIRECTLY
/// extend this type — ONE level down (NOT the transitive closure `implementation` returns). Per
/// LSP 3.17, returns `TypeHierarchyItem[]` or `null`. Each item carries its own `data` blob for
/// further expansion (the depth>2 guarantee).
///
/// Walks the same `index.iter_interfaces()` enumeration as [`implementation`] and applies the same
/// parent-matching rule (via [`extends_matches`]), but minus the transitive BFS AND the method-name
/// filter: every project file whose `extends` resolves to this type, direct children only.
/// (Symmetric with `supertypes`, which also walks one level. `implementation` keeps its own
/// transitive fixpoint loop untouched — `extends_matches` is a parallel predicate, not a rewrite of
/// it — so `implementation` stays byte-identical; the `implementation_overrides` suite proves it.)
pub fn type_hierarchy_subtypes(
    state: &mut ServerState,
    params: TypeHierarchySubtypesParams,
) -> Option<Vec<TypeHierarchyItem>> {
    let ty = TypeRef::from_data(params.item.data.as_ref())?;
    // Collect direct-child FileIds first (immutable borrow of the index), then build items (which
    // needs `&mut state`) — so the borrow of `iter_interfaces` is dropped before `hierarchy_item`.
    let children: Vec<gd_project::FileId> = state
        .workspace
        .index
        .iter_interfaces()
        .filter(|(_, iface)| extends_matches(state, iface, &ty))
        .map(|(fid, _)| fid)
        .collect();
    let mut items = Vec::with_capacity(children.len());
    for fid in children {
        if let Some(item) = script_hierarchy_item(state, fid) {
            items.push(item);
        }
    }
    Some(items)
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

    // A cursor on a CALL-SITE callee identifier prepares the CALLEE's item, not the function
    // the cursor happens to sit inside — exploring `boost` from `b.boost(1)` must target
    // boost's hierarchy (the v1.0.3 real-project walk caught the old enclosing-only behavior
    // returning the caller). Same Binding::Call projection as definition's dotted-call step;
    // a native/unresolved callee (callee_file None) falls through to the enclosing item.
    if let Some(node_id) = parsed.tree.innermost_node_at(byte) {
        if let Some(name) = cursor_identifier(&parsed.tree, node_id) {
            if let Some(analyzed) = analyze_if_gd(state, &uri, &parsed.tree, &text) {
                // Unlike references' subscript-only `callee_ident_spans` (bare callees ride
                // `Binding::Use` there), prepare needs BOTH callee shapes: a bare in-file call
                // (`_find_attached_meshes()`) records a `Binding::Call` too, and the cursor on
                // its identifier must prepare that callee rather than the enclosing function.
                let mut spans: Option<FxHashMap<ByteSpan, ByteSpan>> = None;
                let target = analyzed.bindings().iter().find_map(|b| match b {
                    Binding::Call {
                        callee,
                        callee_name,
                        call_site,
                        ..
                    } if callee_name == &name => {
                        let f = callee.script_file()?;
                        let spans =
                            spans.get_or_insert_with(|| callee_name_token_spans(&parsed.tree));
                        let ident = spans.get(call_site).copied()?;
                        (ident.start <= byte && byte < ident.end).then_some(f)
                    }
                    _ => None,
                });
                if let Some(fid) = target {
                    if let Some((path, callee_uri)) = state
                        .workspace
                        .index
                        .path(fid)
                        .map(|p| p.to_path_buf())
                        .and_then(|p| path_to_file_uri(&p).map(|u| (p, u)))
                    {
                        let (range, selection_range) =
                            resolve_fn_item_ranges(state, &path, &callee_uri, &name);
                        let data = serde_json::json!({
                            "uri": callee_uri.as_str(),
                            "name": name,
                        });
                        #[allow(deprecated)]
                        let item = CallHierarchyItem {
                            name,
                            kind: LspSymbolKind::FUNCTION,
                            tags: None,
                            detail: script_detail(state, &path),
                            uri: callee_uri,
                            range,
                            selection_range,
                            data: Some(data),
                        };
                        return Some(vec![item]);
                    }
                }
            }
        }
    }

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
    let detail = uri_to_path(&uri).and_then(|p| script_detail(state, &p));

    #[allow(deprecated)]
    let item = CallHierarchyItem {
        name: fn_name,
        kind: LspSymbolKind::FUNCTION,
        tags: None,
        detail,
        uri,
        range: fn_range,
        selection_range: ident_range,
        data: Some(data),
    };
    Some(vec![item])
}

/// gopls-style container disambiguator for call-hierarchy items' `detail`: the script's
/// `res://` path (same-named GDScript lifecycle callers — `_ready` in every script — are
/// otherwise indistinguishable in the tree), falling back to the file basename for
/// out-of-root paths.
fn script_detail(state: &ServerState, path: &camino::Utf8Path) -> Option<String> {
    state
        .workspace
        .project
        .path_to_res(path)
        .or_else(|| path.file_name().map(str::to_owned))
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
/// symbol declaration can't be located (the synthetic `<top>` caller, or an unreadable file):
/// LSP requires *a* location, and pointing at `(0,0)` is honest ("somewhere in this file")
/// rather than the wrong-but-specific call-site range the pre-fix code shipped. Native and
/// unresolved OUTGOING callees never reach this — they anchor into their API stub or are
/// omitted entirely.
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
    let (uri, fn_name) = resolve_call_hierarchy_item(state, &params.item)?;
    // Stub API pages have no project call graph: expanding a stub-anchored item (the
    // references-view hands native `to` items back verbatim) gets a clean empty list — never
    // an error, and never an attempt to analyze pseudo-GDScript. Mirrors publish_diagnostics'
    // suppression gate.
    if crate::stubs::is_stub_uri(&uri, state.options.stub_cache_dir.as_deref()) {
        return Some(Vec::new());
    }
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

    // Group calls by (callee target, callee_name), preserving first-seen order.
    // `find_outgoing_calls` already filtered to Call bindings whose caller matches `fn_name`.
    type CalleeKey = (CalleeTarget, String);
    let callee_spans = callee_name_token_spans(&parsed.tree);
    let groups: Vec<(CalleeKey, Vec<lsp_types::Range>)> = group_call_ranges(
        find_outgoing_calls(&result, fn_name.as_str()),
        &mapper,
        &callee_spans,
        |b| match b {
            Binding::Call {
                callee,
                callee_name,
                ..
            } => Some((callee.clone(), callee_name.clone())),
            _ => None,
        },
    );

    let stub_root = state.options.stub_cache_dir.clone();
    let mut out = Vec::with_capacity(groups.len());
    for ((callee, callee_name), ranges) in groups {
        let (to_uri, to_range, to_selection, to_detail) = match callee {
            CalleeTarget::Script { file: fid, .. } => {
                match state.workspace.index.path(fid).map(|p| p.to_path_buf()) {
                    Some(path) => match path_to_file_uri(&path) {
                        Some(u) => {
                            // The `to` item locates the callee's DECLARATION (LSP 3.17), not the
                            // call site — load the callee's file and resolve `func callee_name`'s
                            // spans.
                            let (range, selection) =
                                resolve_fn_item_ranges(state, &path, &u, &callee_name);
                            let detail = script_detail(state, &path);
                            (u, range, selection, detail)
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
                        // A CalleeTarget::Script carried a fid the Index has no path for. This
                        // is NOT an Index-internal invariant — Index::verify() validates the
                        // Index's own structures (interfaces / registry / depgraph /
                        // name_referencers), never the `Binding`s held in an AnalysisResult —
                        // so it can't catch this. It's a stale-analysis-cache artifact: the
                        // binding out-lived the file's removal / quarantine and hasn't been
                        // flushed from the analysis cache yet (a reconcile re-analyzes and
                        // re-stamps the bindings). The on-call's first question is
                        // "which fid?" — log loudly.
                        log::warn!(
                            "outgoingCalls: callee {callee_name} bindings reference \
                             FileId({fid:?}) but Index::path returned None — a binding \
                             out-lived its file's removal/quarantine. The analysis cache is \
                             stale; re-run `gdls diagnose --reconcile` to re-analyze and \
                             re-stamp the bindings.",
                            fid = fid
                        );
                        continue;
                    }
                }
            }
            // Native callee: anchor the `to` item into the DECLARING class's API stub at the
            // member's name token — the "real external declaration" rust-analyzer/gopls point
            // at for std-lib callees. `detail` names the declaring class (the stub the item
            // opens into). Stub materialization failure (no cache root, IO error) omits the
            // entry rather than fabricating a location.
            CalleeTarget::Native { class } => {
                let declaring = {
                    let db = &state.workspace.native;
                    db.lookup_member(&class, &callee_name)
                        .map(|(decl, _)| db.name_of(decl.name).to_owned())
                };
                match native_member_stub_location(state, &class, &callee_name, stub_root.as_deref())
                {
                    Some(loc) => (loc.uri, loc.range, loc.range, declaring),
                    None => {
                        log::debug!(
                            "outgoingCalls: omitting native callee {callee_name} — the {class} \
                             stub could not be materialized"
                        );
                        continue;
                    }
                }
            }
            // Unresolved callee: no project or native declaration to point at — OMIT the entry
            // (the rust-analyzer/gopls convention for nav-less callees). Never the pre-fix
            // fabrication of the caller's uri with a (0,0) anchor, which claimed the callee was
            // declared at the top of the calling script.
            CalleeTarget::Unresolved => continue,
        };
        // `to` items carry the same {uri, name} blob prepare/incoming items do — the client
        // hands them back verbatim on expansion ("show outgoing calls of this callee"), and a
        // data-less item used to dead-end the whole outgoing tree at depth 2.
        let data = serde_json::json!({
            "uri": to_uri.as_str(),
            "name": callee_name,
        });
        #[allow(deprecated)]
        let to = CallHierarchyItem {
            name: callee_name.clone(),
            kind: LspSymbolKind::FUNCTION,
            tags: None,
            detail: to_detail,
            uri: to_uri,
            range: to_range,
            selection_range: to_selection,
            data: Some(data),
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
    let (target_uri, target_name) = resolve_call_hierarchy_item(state, &params.item)?;
    // Same stub gate as outgoing_calls: a stub-anchored item resolves to an API page no
    // project code is indexed against — empty, not an error.
    if crate::stubs::is_stub_uri(&target_uri, state.options.stub_cache_dir.as_deref()) {
        return Some(Vec::new());
    }
    let target_path = crate::uri::uri_to_path(&target_uri)?;
    let target_fid = state.workspace.index.file_id(&target_path);
    let enc = state.encoding;

    // Candidate caller files: the target's own file (a function may call itself or other in-file
    // funcs) + every file whose TEXT mentions the name (the project-wide two-phase scan shared
    // with `references`). The previous interface-level `name_referencers` set was structurally
    // blind to body-only callers — `cel.get_image()` through a typed var never names `get_image`
    // in the caller's interface — so cross-file incoming calls always came back empty.
    let mut candidates: Vec<(camino::Utf8PathBuf, Uri)> = Vec::new();
    candidates.push((target_path.clone(), target_uri.clone()));
    candidates.extend(method_scan_candidate_uris(
        state,
        &target_name,
        target_fid,
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
        let callee_spans = callee_name_token_spans(&parsed.tree);
        let groups = group_call_ranges(
            find_incoming_calls(&result, target_fid, &target_name),
            &mapper,
            &callee_spans,
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
                detail: script_detail(state, &path),
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
/// The empty query is a real request, not a degenerate one — spec: "Clients may send an empty
/// string here to request all symbols." (Helix's picker opens with it.) It skips the matcher
/// and scores every candidate uniformly; the class-before-member tie-break and the 256 cap
/// shape the list.
///
/// Builds the flat candidate list on demand (no precomputed flat index per docs/03 §7.4): the
/// registry + per-file interface tables iterate in O(N) once per request. Re-running the query as
/// the user types is the same cost — adequate for v1; M5 can revisit if soak tests reveal it as
/// hot.
pub fn workspace_symbol(
    state: &mut ServerState,
    params: WorkspaceSymbolParams,
) -> Option<WorkspaceSymbolResponse> {
    // M7 (#58): honor a client-supplied workDoneToken — at 10k-file scale the candidate build +
    // fuzzy scan is the other request worth a spinner. The drop guard ends the arc on exit.
    let mut progress = params
        .work_done_progress_params
        .work_done_token
        .map(|token| {
            crate::progress::ProgressReporter::for_client_token(state.sender.clone(), token)
        });
    let query = params.query;

    // WP-RD7 micro-op — bench witness, no-op landed. The flat-candidate list below is rebuilt from
    // `iter_interfaces` on every `workspace/symbol` request; precomputing it on `Index` mutation
    // (and only re-deriving the changed files' rows) would trade per-request CPU for memory + an
    // invalidation hook. The Phase-C calibration on a large real-world project measured this within
    // budget, so the precompute is deferred per the plan's "lands OR documented bench witness" rule.
    // Build the flat candidate list.
    struct SymbolCandidate {
        name: String,
        kind: LspSymbolKind,
        container: Option<String>,
        path: camino::Utf8PathBuf,
        /// 1-based declaration line — the zero-width fallback anchor when `name_span` fails
        /// live-text validation.
        line: u32,
        is_class: bool,
        /// The recorded name-identifier span (`ClassEntry::name_span` / `MemberDecl::name_span`).
        name_span: ByteSpan,
    }
    let mut candidates: Vec<SymbolCandidate> = Vec::new();

    // Class-name registry entries — top-level class declarations across the project, anchored at
    // the `class_name` identifier's recorded line (#33; line 1 only as the registry's defensive
    // default).
    for (name, entry) in state.workspace.index.registry().entries() {
        candidates.push(SymbolCandidate {
            name: name.to_string(),
            kind: LspSymbolKind::CLASS,
            container: None,
            path: entry.path.clone(),
            line: entry.line,
            is_class: true,
            name_span: entry.name_span,
        });
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
            candidates.push(SymbolCandidate {
                name: member.name.clone(),
                kind,
                container: container.clone(),
                path: path.clone(),
                line: member.line,
                is_class: false,
                name_span: member.name_span,
            });
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
    let mut scored: Vec<(u16, SymbolCandidate)> = Vec::with_capacity(candidates.len().min(256));
    let mut hay_buf: Vec<char> = Vec::new();
    let candidate_total = candidates.len();
    if let Some(reporter) = progress.as_mut() {
        if candidate_total > 0 {
            reporter.begin("Workspace symbols", None);
        }
    }
    for (done, cand) in candidates.into_iter().enumerate() {
        if let Some(reporter) = progress.as_mut() {
            crate::progress::ProgressSink::progress(
                reporter,
                done + 1,
                Some(candidate_total),
                "matching symbols",
            );
        }
        // nucleo asserts non-empty input. An empty haystack here would be a registry /
        // interface bug — log loudly so the operator can investigate the bad entry.
        if cand.name.is_empty() {
            log::warn!(
                "workspace_symbol: empty-name candidate at {path} (line {line}); this should be \
                 impossible — Index registry / Interface members carry a name. Investigate the \
                 emit site.",
                path = cand.path,
                line = cand.line
            );
            continue;
        }
        // Empty query = request for all symbols (see the doc comment): every candidate gets a
        // uniform score, and the matcher (which asserts on its inputs) is never consulted.
        if query.is_empty() {
            scored.push((0, cand));
            continue;
        }
        hay_buf.clear();
        let hay = Utf32Str::new(&cand.name, &mut hay_buf);
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
    let cmp = |a: &(u16, SymbolCandidate), b: &(u16, SymbolCandidate)| {
        b.0.cmp(&a.0).then_with(|| b.1.is_class.cmp(&a.1.is_class))
    };
    if scored.len() > 256 {
        let _ = scored.select_nth_unstable_by(255, cmp);
        scored.truncate(256);
    }
    scored.sort_by(cmp);

    // M9 (#71): when the client advertised `workspace.symbol.resolveSupport`, return the 3.17
    // partial `WorkspaceSymbol[]` shape — a location WITHOUT the precise range plus a compact
    // self-sufficient `data` blob (the symbol's file path + recorded name span). This touches ZERO
    // files: each winner maps to a `WorkspaceLocation { uri }` and the per-file text load that the
    // flat path below pays for is deferred to `workspaceSymbol/resolve`, which reads one file. The
    // `data` blob is the resolve key (anti-catalog W18: extensions ride `data`, never the request
    // params); it carries the byte span so resolve never re-derives it from the (possibly edited)
    // text. A candidate whose path won't form a URI is dropped here exactly as in the flat path.
    if state.caps.symbol_resolve_support {
        let symbols: Vec<WorkspaceSymbol> = scored
            .into_iter()
            .filter_map(|(_score, cand)| {
                let uri = match path_to_file_uri(&cand.path) {
                    Some(u) => u,
                    None => {
                        log::debug!(
                            "workspace_symbol: dropping {name} at {path} — path_to_file_uri \
                             rejected the path; the symbol is invisible to the client",
                            name = cand.name,
                            path = cand.path,
                        );
                        return None;
                    }
                };
                Some(WorkspaceSymbol {
                    name: cand.name,
                    kind: cand.kind,
                    tags: None,
                    container_name: cand.container,
                    location: OneOf::Right(WorkspaceLocation { uri }),
                    data: Some(serde_json::json!({
                        "path": cand.path.as_str(),
                        "start": cand.name_span.start,
                        "end": cand.name_span.end,
                    })),
                })
            })
            .collect();
        return Some(WorkspaceSymbolResponse::Nested(symbols));
    }

    // Real name-token ranges need each winner file's text for the encoding-correct
    // byte→character mapping (the spec reads the range to reveal/select the symbol; a
    // zero-width point at column 0 lands the caret on leading syntax). Bounded by the 256
    // cap: each distinct winner file is loaded ONCE (open buffer wins over disk, the
    // member_decl_location pattern), one rope per file, and every recorded name span maps
    // through it — accepted only while its bytes still spell the symbol's name (the
    // find_global_class_definition validation discipline). Validation or read failure falls
    // back to the pre-#46 zero-width point at the declaration line's start: never drop the
    // symbol over a stale anchor.
    //
    // Worst-case latency (an all-cold empty-query picker open): ≤256 small sequential reads —
    // strictly inside the per-request envelope `references` already pays for its Godot-parity
    // project-wide text scan (which reads EVERY project file). If soak flags this, the named
    // mitigations are per-line slicing (only the span's line is needed per symbol) or an LRU
    // rope cache on ServerState; index-time columns are NOT one (characters are
    // encoding-negotiated per session).
    let enc = state.encoding;
    let mut texts: FxHashMap<camino::Utf8PathBuf, Option<(String, ropey::Rope)>> =
        FxHashMap::default();
    #[allow(deprecated)]
    let symbols: Vec<SymbolInformation> = scored
        .into_iter()
        .filter_map(|(_score, cand)| {
            let uri = match path_to_file_uri(&cand.path) {
                Some(u) => u,
                None => {
                    log::debug!(
                        "workspace_symbol: dropping {name} at {path} — path_to_file_uri \
                         rejected the path; the symbol is invisible to the client",
                        name = cand.name,
                        path = cand.path,
                    );
                    return None;
                }
            };
            let entry = texts.entry(cand.path.clone()).or_insert_with(|| {
                let text = match state.vfs.get(uri.as_str()).map(|d| d.text()) {
                    Some(t) => t,
                    None => std::fs::read_to_string(cand.path.as_std_path()).ok()?,
                };
                let rope = ropey::Rope::from_str(&text);
                Some((text, rope))
            });
            let range = match entry {
                Some((text, rope))
                    if text.get(cand.name_span.start..cand.name_span.end)
                        == Some(cand.name.as_str()) =>
                {
                    PositionMapper::new(rope, enc).span_to_range(cand.name_span)
                }
                _ => {
                    let pos = Position {
                        line: cand.line.saturating_sub(1),
                        character: 0,
                    };
                    Range {
                        start: pos,
                        end: pos,
                    }
                }
            };
            Some(SymbolInformation {
                name: cand.name,
                kind: cand.kind,
                tags: None,
                deprecated: None,
                location: Location { uri, range },
                container_name: cand.container,
            })
        })
        .collect();
    Some(WorkspaceSymbolResponse::Flat(symbols))
}

/// `workspaceSymbol/resolve`: fill the precise `location.range` deferred by the partial
/// [`WorkspaceSymbol`] that [`workspace_symbol`] returned under `workspace.symbol.resolveSupport`.
///
/// The item's `data` blob is the self-sufficient key (`{path, start, end}` — anti-catalog W18:
/// the extension rides `data`, never re-derived from request params). This reads that one file
/// (open buffer first, then disk — the `member_decl_location` precedence), builds a
/// [`PositionMapper`] over the session encoding, and maps the recorded name span to a `Range`,
/// returning the symbol with `location: OneOf::Left(Location { uri, range })`.
///
/// **Never crash, never lie.** Missing / malformed `data`, a path that won't form a URI, or a file
/// that can't be read all return the item *unchanged* (its location stays the uri-only form) — a
/// resolve failure must never drop the symbol or panic. The mapped range is accepted only while
/// the span's bytes still spell the symbol's name (the same validate-or-fallback the eager flat
/// path applies, so a resolved range EQUALS the eager range for an unchanged file and degrades to
/// the same zero-width point under a stale span rather than pointing at moved text).
pub fn workspace_symbol_resolve(
    state: &mut ServerState,
    mut params: WorkspaceSymbol,
) -> Option<WorkspaceSymbol> {
    // Extract the `{path, start, end}` resolve key. Any shape failure ⇒ return the item unchanged.
    let (path, start, end) = match &params.data {
        Some(data) => {
            let path = data.get("path").and_then(|v| v.as_str());
            let start = data.get("start").and_then(|v| v.as_u64());
            let end = data.get("end").and_then(|v| v.as_u64());
            match (path, start, end) {
                (Some(p), Some(s), Some(e)) => {
                    (camino::Utf8PathBuf::from(p), s as usize, e as usize)
                }
                _ => return Some(params),
            }
        }
        None => return Some(params),
    };

    let Some(uri) = path_to_file_uri(&path) else {
        return Some(params);
    };

    // Load the file text: open buffer wins over disk (same precedence as the eager path's range
    // resolution). A read failure leaves the item's uri-only location intact.
    let text = match state.vfs.get(uri.as_str()).map(|d| d.text()) {
        Some(t) => t,
        None => match std::fs::read_to_string(path.as_std_path()) {
            Ok(t) => t,
            Err(_) => return Some(params),
        },
    };

    let span = ByteSpan { start, end };
    let rope = Rope::from_str(&text);
    let mapper = PositionMapper::new(&rope, state.encoding);
    // Validate the recorded span still spells the symbol's name before trusting it — mirrors the
    // flat path's `text.get(..) == Some(name)` guard so the resolved range matches the eager range
    // exactly (and falls back identically when the span is stale). `PositionMapper` clamps an
    // out-of-range span regardless, so this is a fidelity guard, not a panic guard.
    let range = if text.get(span.start..span.end) == Some(params.name.as_str()) {
        mapper.span_to_range(span)
    } else {
        // Stale span (the file changed since the query and the recorded bytes no longer spell the
        // name): clamp the recorded START byte to a position as the closest stable anchor. This is
        // a degraded best-effort, NOT identical to the eager path's fallback (which anchors at the
        // declaration line at col 0 — `data` carries no line to reproduce that). It never points at
        // moved text — never crash, never lie.
        let pos = mapper.byte_to_position(span.start.min(text.len()));
        Range {
            start: pos,
            end: pos,
        }
    };

    params.location = OneOf::Left(Location { uri, range });
    Some(params)
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

/// Resolve a `CallHierarchyItem` back to `(uri, bare function name)` for the follow-up
/// handlers. Server-issued `data` wins ([`decode_call_hierarchy_data`], which keeps its
/// malformed-data logging). Items without data — clients that strip the field, or items
/// synthesized by another provider — re-resolve rust-analyzer/gopls-style from `item.uri` +
/// `item.selection_range.start` (the function whose declaration identifier contains that
/// position), with `item.name` as the lossless floor: every gdls-issued item satisfies
/// `data.name == item.name`, and GDScript has no overloads, so the bare name identifies the
/// function within its file.
fn resolve_call_hierarchy_item(
    state: &mut ServerState,
    item: &CallHierarchyItem,
) -> Option<(Uri, String)> {
    if let Some(decoded) = decode_call_hierarchy_data(item) {
        return Some(decoded);
    }
    let name =
        position_function_name(state, &item.uri, item.selection_range.start).unwrap_or_else(|| {
            log::debug!(
                "call hierarchy: item `{}` carries no data and its selectionRange resolves no \
                 function declaration; falling back to the item's own name",
                item.name
            );
            item.name.clone()
        });
    Some((item.uri.clone(), name))
}

/// The name of the function whose declaration IDENTIFIER contains `pos` in `uri`'s current
/// text (open buffer wins over disk). `None` when the file is unreadable or no declaration
/// identifier contains the position.
fn position_function_name(state: &mut ServerState, uri: &Uri, pos: Position) -> Option<String> {
    let path = crate::uri::uri_to_path(uri)?;
    let text = match state.vfs.get(uri.as_str()).map(|d| d.text()) {
        Some(t) => t,
        None => std::fs::read_to_string(path.as_std_path()).ok()?,
    };
    let parsed = state.workspace.parse(&CanonicalKey::for_uri(uri), &text);
    let rope = Rope::from_str(&text);
    let byte = PositionMapper::new(&rope, state.encoding).position_to_byte(pos);
    for id in parsed.tree.iter_ids() {
        let NodeKind::Function(f) = &parsed.tree.get(id).kind else {
            continue;
        };
        let Some(ident) = f.identifier else {
            continue;
        };
        let span = parsed.tree.get(ident).span;
        let name = ident_name(&parsed.tree, ident);
        if !name.is_empty() && span.start <= byte && byte < span.end {
            return Some(name.to_owned());
        }
    }
    None
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

// =================================================================================================
// M9 (#66): textDocument/prepareRename + textDocument/rename — workspace-wide semantic rename.
//
// The whole discipline is **refuse rather than corrupt**: a rename either edits the EXACT set
// `references` resolves (declaration + every reference, binding/index-backed — never a text grep,
// W16), or it refuses with a typed request error and ZERO edits. Every gate runs BEFORE any edit
// is assembled. Refusals carry a human-readable message (the corruption firewall is that the
// client SEES the refusal rather than receiving a silent null or a partial edit).
// =================================================================================================

/// A typed refusal of a syntactically-valid request (M9 #66 rename/prepareRename). The dispatch
/// `handle_fallible!` arm projects it into a `Response::new_err(code, message)`. `code` is a
/// JSON-RPC / LSP error code (`ERR_REQUEST_FAILED` for a non-editable native/stub target,
/// `ERR_INVALID_PARAMS` for an invalid new name); `message` is the human-readable reason.
pub(crate) struct RequestRefusal {
    pub(crate) code: i32,
    pub(crate) message: String,
}

impl RequestRefusal {
    /// A target that is not an editable project source — a native engine symbol or a generated API
    /// stub. LSP 3.17 `RequestFailed` (-32803): the request was well-formed, it just cannot succeed.
    fn not_editable(message: impl Into<String>) -> Self {
        RequestRefusal {
            code: crate::server::ERR_REQUEST_FAILED,
            message: message.into(),
        }
    }

    /// An invalid `new_name` (empty / not an identifier / a keyword / colliding). The rename spec
    /// says return a `ResponseError` with an appropriate message; `ERR_INVALID_PARAMS` (-32602) is
    /// the conventional code for a request the server understood but whose parameter is unusable.
    fn invalid_name(message: impl Into<String>) -> Self {
        RequestRefusal {
            code: crate::server::ERR_INVALID_PARAMS,
            message: message.into(),
        }
    }
}

/// `true` iff `name` is a valid GDScript identifier that is NOT a keyword — the rename validity
/// rule, derived MECHANICALLY from the lexer (faithful-port discipline) rather than a hand-rolled
/// `[A-Za-z_]\w*` regex: tokenize the candidate and require it to be exactly one
/// [`gd_syntax::TokenKind::Identifier`] with no lexer errors.
///
/// Why this is the correct, conservative rule (every case the criterion names falls out of it):
///   - empty string → tokenizes to just `Newline?`/`Eof` → no `Identifier` → rejected.
///   - `1bad` → `Literal` + `Identifier` (two content tokens) → rejected.
///   - `has space` → two `Identifier` tokens → rejected.
///   - `func` / `if` → the keyword kinds `Func`/`If` (not `Identifier`) → rejected — this is the
///     "not a keyword" half, inherited from the lexer's `keyword_kind` table for free.
///   - `true` / `false` / `null` → `Literal` tokens → rejected (reserved literals, not renameable).
///   - `match` / `when` / `PI` → the engine-API keyword kinds `Match`/`When`/`ConstPi`: gdls
///     requires the STRICT `Identifier` kind, so these are rejected too — the safe choice for a
///     rename target (they are keywords in declaration position).
///   - any non-ASCII confusable / leading whitespace → a lexer error or a stray `Indent`/`Dedent`
///     content token → rejected.
fn is_valid_rename_identifier(name: &str) -> bool {
    // A leading/trailing-whitespace candidate must never slip through on a trimmed match — reject
    // it outright (it would also emit an `Indent` token below, but this is the explicit guard).
    if name.is_empty() || name != name.trim() {
        return false;
    }
    let (tokens, errors) = gd_syntax::tokenize(name);
    if !errors.is_empty() {
        return false;
    }
    // Drop the synthetic line-structure tokens the lexer appends (`Newline`/`Indent`/`Dedent`/
    // `Eof`); a clean identifier leaves exactly one content token of kind `Identifier`.
    let content: Vec<gd_syntax::TokenKind> = tokens
        .iter()
        .map(|t| t.kind)
        .filter(|k| {
            !matches!(
                k,
                gd_syntax::TokenKind::Newline
                    | gd_syntax::TokenKind::Indent
                    | gd_syntax::TokenKind::Dedent
                    | gd_syntax::TokenKind::Eof
            )
        })
        .collect();
    matches!(content.as_slice(), [gd_syntax::TokenKind::Identifier])
}

/// The corruption firewall — **fail-CLOSED**: refuse a rename unless the cursor target positively
/// resolves to an editable PROJECT symbol. An earlier fail-OPEN version only enumerated specific
/// native categories (classes + stubbed members) and silently let through builtins (`Vector2`),
/// `@GlobalScope` enum values (`SIDE_LEFT`/`OK`/`KEY_ESCAPE`), global utilities (`print`), and
/// native constants — each of which `references` then mass-edited via a raw current-file scan
/// (every occurrence of the engine name), corrupting source with `error: None`. The inverted gate
/// closes that whole class of holes at once.
///
/// Refusal signals (any one refuses):
///   1. the request file itself is a materialized stub (`is_stub_uri`) — the user opened an API
///      page and tried to rename a symbol inside it.
///   2. the cursor name resolves to ANY engine symbol in the native DB —
///      `class_named` / `builtin_named` / `singleton_type` / `global_enum_value` / `utility` /
///      `global_constant`. This is the positive native catch for symbols [`definition`] has no arm
///      for (builtins, global enum values, utilities) and so would let pass.
///   3. [`definition`] resolves the cursor symbol into a stub page — catches a NATIVE MEMBER access
///      through dotted access (`node.queue_free()`), whose declaring site lands in the stub cache.
///   4. fail-closed residue: a NON-method target that classifies as [`NonMethodTarget::Unresolved`]
///      with NO project declaration (`find_in_file_definition` / `find_global_class_definition`
///      both miss) cannot be confirmed an editable project symbol → refuse rather than raw-scan it.
///      A project class_name / member / function-local all have a project declaration (or classify
///      as `Member`/`Local`) and pass; only the genuinely-unresolvable residue is refused.
///
/// Returns `Some(refusal)` to refuse, `None` when the target is an editable project symbol (project
/// class_name / member / local / signal — those flow on to validation + edit assembly).
fn rename_native_or_stub_refusal(
    state: &mut ServerState,
    uri: &Uri,
    name: &str,
    position: Position,
) -> Option<RequestRefusal> {
    // Own the stub root so the immutable `state.options` borrow doesn't straddle the mutable
    // `definition(state, ..)` / analyze calls below.
    let stub_root = state.options.stub_cache_dir.clone();
    // (1) The request file is a stub page.
    if crate::stubs::is_stub_uri(uri, stub_root.as_deref()) {
        return Some(RequestRefusal::not_editable(
            "Cannot rename inside a generated API stub",
        ));
    }
    // (2) POSITIVE project anchor → ALLOW. A project local / parameter / member / method / class
    // shadows any engine name (analyzer precedence: local→param→member→native), so the anchor check
    // runs BEFORE the engine-name refusal below — otherwise a project var named `max`/`min`/`abs`
    // (all native utilities) would be wrongly refused. `rename_target_has_project_anchor` only admits
    // a positively-resolved project target (a native method on an untyped/script-typed base does NOT
    // anchor — its `Binding::Call` callee has no project `script_file`).
    if rename_target_has_project_anchor(state, uri, name, position) {
        return None;
    }
    // Not project-anchored → REFUSE; pick the most specific message.
    // (3) The cursor name is an engine symbol the native DB knows (class / builtin / singleton /
    // global enum value / utility / constant). `definition` has no arm for most of these, so the
    // positive name lookup is what catches builtins / enum values / utilities.
    if rename_name_is_engine_symbol(state, name) {
        return Some(RequestRefusal::not_editable(format!(
            "Cannot rename the native symbol `{name}`"
        )));
    }
    // (4) The symbol resolves (via the definition pipeline) into a stub page — the native-member
    // (dotted access on a Native-typed base) case. Reuse `definition` verbatim.
    let def_params = GotoDefinitionParams {
        text_document_position_params: TextDocumentPositionParams {
            text_document: lsp_types::TextDocumentIdentifier { uri: uri.clone() },
            position,
        },
        work_done_progress_params: WorkDoneProgressParams::default(),
        partial_result_params: lsp_types::PartialResultParams::default(),
    };
    if let Some(GotoDefinitionResponse::Scalar(loc)) = definition(state, def_params.clone()) {
        if crate::stubs::is_stub_uri(&loc.uri, stub_root.as_deref()) {
            return Some(RequestRefusal::not_editable(format!(
                "Cannot rename the native symbol `{name}`"
            )));
        }
    }
    // (5) Genuinely unresolvable residue — an unknown identifier, `extends UnknownThing`, or a
    // native method on an untyped/script-typed base (which would otherwise raw-scan project-wide).
    Some(RequestRefusal::not_editable(format!(
        "Cannot rename `{name}`: it does not resolve to an editable project symbol"
    )))
}

/// `true` iff `name` is any engine symbol the native DB knows — a class, a builtin type, a
/// singleton, a `@GlobalScope` enum value, a `@GlobalScope` utility, or a `@GlobalScope` constant.
/// The positive-native half of the fail-closed gate (signal 2). Pure DB lookups, no analysis.
fn rename_name_is_engine_symbol(state: &ServerState, name: &str) -> bool {
    let db = &state.workspace.native;
    db.class_named(name).is_some()
        || db.builtin_named(name).is_some()
        || db.singleton_type(name).is_some()
        || db.global_enum_value(name).is_some()
        || db.utility(name).is_some()
        || db.global_constant(name).is_some()
}

/// `true` iff the cursor target positively resolves to an editable PROJECT declaration — the
/// fail-closed gate's signal 4. A method/signal ROLE (declaration click or dotted call) is treated
/// as anchored (native methods were already refused by signals 2/3; project methods have an in-file
/// declaration). Otherwise require one of: an in-file root-class member declaration, a project
/// `class_name`, an enclosing-function local/param, or a `Member`-classified analyzer use at the
/// cursor span. The genuinely-unresolvable residue (unknown identifiers, `extends UnknownThing`)
/// returns `false` → the caller refuses.
///
/// KNOWN LIMITATION (deliberate, fail-closed side effect — track as a follow-up issue): a *project*
/// `@GlobalScope`-style enum VALUE (`enum E { NORTH }`; cursor on `NORTH`) and an autoload singleton
/// NAME both lack a project anchor here — `classify_non_method_target` excludes `EnumValue`, and
/// `member_named` matches an enum's own name, not its values — so they now REFUSE where the prior
/// fail-open path raw-scanned and edited them. This is the refuse-rather-than-corrupt stance
/// (renaming an enum value by raw text scan is exactly the W16 grep-rename), not a regression to fix
/// by widening the gate (which would reopen the native-enum-value hole). Enum TYPE names and
/// members rename normally.
fn rename_target_has_project_anchor(
    state: &mut ServerState,
    uri: &Uri,
    name: &str,
    position: Position,
) -> bool {
    let Some(text) = state.vfs.get(uri.as_str()).map(|d| d.text()) else {
        return false;
    };
    let key = CanonicalKey::for_uri(uri);
    let parsed = state.workspace.parse(&key, &text);
    let rope = Rope::from_str(&text);
    let mapper = PositionMapper::new(&rope, state.encoding);
    let byte = mapper.position_to_byte(position);
    let Some(node_id) = parsed.tree.innermost_node_at(byte) else {
        return false;
    };

    // In-file root-class member declaration / use (covers methods + vars declared in THIS file —
    // a method-declaration click anchors here, not via the call probe below).
    if find_in_file_definition(&parsed.tree, name, uri, &mapper).is_some() {
        return true;
    }
    // Enclosing-function local or parameter.
    if enclosing_function_declaring(&parsed.tree, byte, name).is_some() {
        return true;
    }
    // A project `class_name` (declared in any project file).
    if find_global_class_definition(state, name).is_some() {
        return true;
    }
    // Analyze once for the call-callee + cross-file-member anchors below.
    let current_path = crate::uri::uri_to_path(uri);
    let Some(p) = current_path
        .as_deref()
        .filter(|p| p.extension() == Some("gd"))
    else {
        return false;
    };
    let current_fid = state.workspace.index.file_id(p);
    let result = analyze_with_request_token(state, &key, p, &parsed.tree, &text);

    // A call whose callee resolves to a PROJECT Script file (bare `m()` or dotted `x.m()`) — mirror
    // the `references` target_file resolution: `callee.script_file()` is `Some` ONLY for a project
    // Script callee; it is `None` for a native/unresolved callee (`node.queue_free()` on an untyped
    // OR script-typed base), which must NOT anchor — otherwise `references` raw-scans the engine
    // method name PROJECT-WIDE. (Replaces the old blanket `is_member_or_attribute_ident` exemption,
    // which let every dotted call through and mass-edited native methods on non-Native-typed bases.)
    let callee_spans = callee_ident_spans(&parsed.tree);
    let call_anchored = result.bindings().iter().any(|b| {
        if let Binding::Call {
            callee,
            callee_name,
            call_site,
            ..
        } = b
        {
            if callee_name == name {
                if let Some(ident_span) = callee_spans.get(call_site).copied() {
                    if ident_span.start <= byte && byte < ident_span.end {
                        return callee.script_file().is_some();
                    }
                }
            }
        }
        false
    });
    if call_anchored {
        return true;
    }

    // A `Member`/`Local`-classified analyzer use at the cursor span (cross-file member read/write
    // through a typed var — `other.speed`): the analyzer resolved it to a declaring project file.
    let node_span = parsed.tree.get(node_id).span;
    if matches!(
        classify_non_method_target(&parsed.tree, &result, node_span, byte, name, current_fid),
        NonMethodTarget::Member(_) | NonMethodTarget::Local(_)
    ) {
        return true;
    }
    false
}

/// `textDocument/prepareRename` (#66): pre-flight a rename at the cursor. Resolves the symbol under
/// the cursor via the SAME path `references`/`definition` use ([`cursor_identifier`]), so prepare
/// and the rename agree on what is renameable by construction.
///
/// Outcomes:
///   - cursor not on an identifier → `Ok(None)` (LSP `null`): there is nothing to rename, but this
///     is not an error.
///   - native engine symbol / generated stub target → `Err(refusal)`: a typed request error with a
///     human message (NOT a silent null — the client must SEE the refusal).
///   - otherwise → `Ok(Some(...))`: the identifier token's range, with a placeholder (the current
///     name) when the client advertised `rename.prepareSupport`, else a bare range (so the rename
///     keybinding still works for a client that did not opt into placeholder support — the
///     `PrepareRenameResponse::Range` variant the spec provides for exactly this).
pub fn prepare_rename(
    state: &mut ServerState,
    params: TextDocumentPositionParams,
) -> Result<Option<PrepareRenameResponse>, RequestRefusal> {
    let uri = params.text_document.uri.clone();
    let Some(text) = state.vfs.get(uri.as_str()).map(|d| d.text()) else {
        return Ok(None);
    };
    let parsed = state.workspace.parse(&CanonicalKey::for_uri(&uri), &text);
    let rope = Rope::from_str(&text);
    let mapper = PositionMapper::new(&rope, state.encoding);
    let byte = mapper.position_to_byte(params.position);
    let Some(node_id) = parsed.tree.innermost_node_at(byte) else {
        return Ok(None);
    };
    let Some(name) = cursor_identifier(&parsed.tree, node_id) else {
        return Ok(None);
    };

    // Corruption firewall — refuse a native/stub target with a typed error before answering.
    if let Some(refusal) = rename_native_or_stub_refusal(state, &uri, &name, params.position) {
        return Err(refusal);
    }

    let range = mapper.span_to_range(parsed.tree.get(node_id).span);
    if state.caps.rename_prepare_support {
        Ok(Some(PrepareRenameResponse::RangeWithPlaceholder {
            range,
            placeholder: name,
        }))
    } else {
        Ok(Some(PrepareRenameResponse::Range(range)))
    }
}

/// `textDocument/rename` (#66): workspace-wide semantic rename of the symbol under the cursor.
///
/// Order is the whole point — every check runs BEFORE any edit is assembled, so a refusal produces
/// ZERO edits (refuse rather than corrupt):
///   1. cursor must land on an identifier (else `Ok(None)` → LSP `null`).
///   2. native/stub gate ([`rename_native_or_stub_refusal`]) → `Err` for a non-editable target.
///   3. `new_name` is a valid GDScript identifier and not a keyword
///      ([`is_valid_rename_identifier`]) → `Err` otherwise.
///   4. `new_name` does not collide with an existing member/local in the affected scope
///      ([`rename_collision`]) → `Err` otherwise.
///   5. ONLY now: reuse [`references`] (`include_declaration: true`) to collect the declaration +
///      every reference site, and assemble the [`WorkspaceEdit`] — one [`TextEdit`] per site,
///      range = identifier token, `new_text = new_name`. The edited set EQUALS what `references`
///      returns for the same symbol (no independent resolution, no grep).
///
/// `WorkspaceEdit` shape is gated on `workspace.workspaceEdit.documentChanges`: when advertised,
/// versioned `documentChanges` (each [`TextDocumentEdit`] carries its file's current open-buffer
/// version, or `None` for an unopened file); otherwise the legacy `changes` URI→edits map. Exactly
/// one of the two fields is populated.
pub fn rename(
    state: &mut ServerState,
    params: RenameParams,
) -> Result<Option<WorkspaceEdit>, RequestRefusal> {
    let tdp = params.text_document_position;
    let new_name = params.new_name;
    let uri = tdp.text_document.uri.clone();
    let Some(text) = state.vfs.get(uri.as_str()).map(|d| d.text()) else {
        return Ok(None);
    };
    let parsed = state.workspace.parse(&CanonicalKey::for_uri(&uri), &text);
    let rope = Rope::from_str(&text);
    let mapper = PositionMapper::new(&rope, state.encoding);
    let byte = mapper.position_to_byte(tdp.position);
    let Some(node_id) = parsed.tree.innermost_node_at(byte) else {
        return Ok(None);
    };
    let Some(old_name) = cursor_identifier(&parsed.tree, node_id) else {
        return Ok(None);
    };

    // (2) Corruption firewall: refuse a native/stub target (same gate as prepare).
    if let Some(refusal) = rename_native_or_stub_refusal(state, &uri, &old_name, tdp.position) {
        return Err(refusal);
    }

    // (3) The new name must be a valid identifier and not a keyword.
    if !is_valid_rename_identifier(&new_name) {
        return Err(RequestRefusal::invalid_name(format!(
            "`{new_name}` is not a valid GDScript identifier for a rename"
        )));
    }

    // A no-op rename (same name) is vacuously valid — return an empty edit rather than running the
    // collision checks against the symbol's own declaration (which would always "collide").
    if new_name == old_name {
        return Ok(Some(empty_workspace_edit(state)));
    }

    // (3b) New-name GLOBAL collision (independent of the old-name fail-closed gate): when the
    // cursor target is a project `class_name` (a class-level rename), the new name must not clash
    // with a NATIVE class / builtin / singleton (renaming `class_name Hero` → `Node` would declare
    // `class_name Node`, shadowing the engine class) NOR with an already-registered project
    // `class_name` in ANOTHER file (two files declaring the same global class). Both produce a
    // global-registry collision the same-file [`rename_collision`] below cannot see.
    let renaming_project_class = state.workspace.index.registry().contains(&old_name);
    if renaming_project_class {
        if rename_name_is_engine_symbol(state, &new_name) {
            return Err(RequestRefusal::invalid_name(format!(
                "Cannot rename to `{new_name}`: it is a native engine type"
            )));
        }
        // Another file already declares this `class_name`. (The symbol being renamed is `old_name`,
        // so `new_name == old_name` was already returned above — any hit here is a genuine clash.)
        if state.workspace.index.registry().contains(&new_name) {
            return Err(RequestRefusal::invalid_name(format!(
                "Cannot rename to `{new_name}`: a project class named `{new_name}` already exists"
            )));
        }
    }

    // (4) The new name must not collide with an existing member/local in the affected scope.
    if let Some(refusal) = rename_collision(state, &parsed.tree, byte, node_id, &new_name) {
        return Err(refusal);
    }

    // (5) Canonicalize the cursor to the symbol's DECLARATION before collecting the edit set. The
    // `references` set is click-site-dependent for a method: a click on a BARE `helper()` call can
    // yield a NARROWER set than a click on the declaration (it may miss the `self.helper()`-qualified
    // siblings). For a read that is a cosmetic panel gap; for a MUTATING rename it silently drops
    // occurrences → a dangling call to the old name → broken code. The declaration is the canonical,
    // complete anchor, so resolve it via `definition` and collect from THERE — making the edit set
    // click-site-INDEPENDENT. Falls back to the cursor when the target is already its own declaration
    // (definition → None / non-scalar), where the cursor set is itself complete.
    // Skip canonicalization for a function-local / parameter: they are function-scoped and
    // click-site-SYMMETRIC (no method-style bare-vs-`self.`-qualified asymmetry), so the cursor set
    // is already complete — and `definition()` is member-FIRST, so canonicalizing a local that
    // SHADOWS a member (`func set_value(value): …` over `var value`) would jump to the member and
    // rename the WRONG symbol project-wide, leaving the local broken. Only methods / members (which
    // carry the bare-vs-qualified asymmetry) need the declaration anchor.
    let is_local_or_param = enclosing_function_declaring(&parsed.tree, byte, &old_name).is_some();
    let edit_tdp = if is_local_or_param {
        tdp.clone()
    } else {
        match definition(
            state,
            GotoDefinitionParams {
                text_document_position_params: tdp.clone(),
                work_done_progress_params: WorkDoneProgressParams::default(),
                partial_result_params: lsp_types::PartialResultParams::default(),
            },
        ) {
            Some(GotoDefinitionResponse::Scalar(loc)) => TextDocumentPositionParams {
                text_document: lsp_types::TextDocumentIdentifier { uri: loc.uri },
                position: loc.range.start,
            },
            _ => tdp.clone(),
        }
    };

    // (6) Reuse `references` (declaration + every reference) for the edit set — index/binding-backed
    // resolution, never a text grep. `include_declaration: true` so the declaration token is edited
    // too. The resulting (uri, range) set IS the edited set by construction.
    let ref_params = ReferenceParams {
        text_document_position: edit_tdp,
        context: ReferenceContext {
            include_declaration: true,
        },
        work_done_progress_params: WorkDoneProgressParams::default(),
        partial_result_params: lsp_types::PartialResultParams::default(),
    };
    let locations = references(state, ref_params).unwrap_or_default();

    Ok(Some(build_workspace_edit(state, locations, &new_name)))
}

/// An empty [`WorkspaceEdit`] in whichever shape the client negotiated (a no-op rename). Keeps the
/// shape consistent with a real edit so a client that switches on `documentChanges` vs `changes`
/// sees the field it expects (empty), not a bare `null`.
// See `build_workspace_edit` — `HashMap<Uri, _>` is the mandated `changes` shape; the empty map
// here has no keys at all, so `clippy::mutable_key_type` is doubly moot.
#[allow(clippy::mutable_key_type)]
fn empty_workspace_edit(state: &ServerState) -> WorkspaceEdit {
    if state.caps.workspace_edit_document_changes {
        WorkspaceEdit {
            document_changes: Some(DocumentChanges::Edits(Vec::new())),
            ..Default::default()
        }
    } else {
        WorkspaceEdit {
            changes: Some(std::collections::HashMap::new()),
            ..Default::default()
        }
    }
}

/// Group `locations` (the `references` set) by URI into one [`TextEdit`] per site
/// (`new_text = new_name`, range = the identifier token), then project into the negotiated
/// [`WorkspaceEdit`] shape:
///   - `workspace.workspaceEdit.documentChanges` advertised → versioned `documentChanges`: one
///     [`TextDocumentEdit`] per file, its [`OptionalVersionedTextDocumentIdentifier`] carrying the
///     file's CURRENT open-buffer version (pulled from the VFS) or `None` for an unopened file (the
///     optional-versioned id's documented "content on disk is master" case). Zero stale-version
///     edits: the version is read live here, at assembly time.
///   - otherwise → the legacy `changes` URI→edits map (no version).
///
/// Exactly one field is populated; the other is `None`.
// `lsp_types::Uri` carries interior mutability (it caches its parsed components in a `Cell`), which
// trips `clippy::mutable_key_type` when used as a `HashMap` key — but `WorkspaceEdit.changes` IS
// `HashMap<Uri, Vec<TextEdit>>` by the LSP wire shape, and we never mutate a key after insertion,
// so the lint's hazard (a key whose hash changes under us) cannot occur here.
#[allow(clippy::mutable_key_type)]
fn build_workspace_edit(
    state: &ServerState,
    locations: Vec<Location>,
    new_name: &str,
) -> WorkspaceEdit {
    // Group by URI string, preserving first-seen order for deterministic output.
    let mut order: Vec<Uri> = Vec::new();
    let mut by_uri: FxHashMap<String, Vec<TextEdit>> = FxHashMap::default();
    for loc in locations {
        let edit = TextEdit {
            range: loc.range,
            new_text: new_name.to_string(),
        };
        let key = loc.uri.as_str().to_string();
        if let Some(edits) = by_uri.get_mut(&key) {
            edits.push(edit);
        } else {
            order.push(loc.uri.clone());
            by_uri.insert(key, vec![edit]);
        }
    }

    if state.caps.workspace_edit_document_changes {
        let edits: Vec<TextDocumentEdit> = order
            .into_iter()
            .map(|uri| {
                // Pull the CURRENT version from the open buffer; `None` when the file isn't open
                // (the OptionalVersioned id allows it — "the content on disk is the master").
                let version = state.vfs.get(uri.as_str()).map(|d| d.version);
                let text_edits = by_uri
                    .remove(uri.as_str())
                    .unwrap_or_default()
                    .into_iter()
                    .map(OneOf::Left)
                    .collect();
                TextDocumentEdit {
                    text_document: OptionalVersionedTextDocumentIdentifier { uri, version },
                    edits: text_edits,
                }
            })
            .collect();
        WorkspaceEdit {
            document_changes: Some(DocumentChanges::Edits(edits)),
            ..Default::default()
        }
    } else {
        let mut changes: std::collections::HashMap<Uri, Vec<TextEdit>> =
            std::collections::HashMap::with_capacity(order.len());
        for uri in order {
            let text_edits = by_uri.remove(uri.as_str()).unwrap_or_default();
            changes.insert(uri, text_edits);
        }
        WorkspaceEdit {
            changes: Some(changes),
            ..Default::default()
        }
    }
}

/// Refuse a rename whose `new_name` would collide with an existing declaration in the SAME scope as
/// the symbol being renamed — index/binding-backed, scoped to exactly what the criterion names
/// ("a name colliding with an existing member in scope"), not a full scope-resolution engine.
///
/// Two scopes, mirroring the `references` classification:
///   - a class member (the cursor classifies as a member, OR a method/signal declaration) →
///     collision iff the current file's root class already declares `new_name` as a member.
///   - an enclosing-function local/parameter → collision iff that function already declares
///     `new_name` as a param/var/const.
///
/// A target that resolves to neither (an unresolved residue) is not collision-checked here — there
/// is no scope to check against; the edit assembly still uses the binding-backed `references` set.
fn rename_collision(
    _state: &mut ServerState,
    tree: &ParseTree,
    byte: usize,
    node_id: NodeId,
    new_name: &str,
) -> Option<RequestRefusal> {
    // Local/parameter scope FIRST: a cursor inside a function on its own param/var/const is a
    // local target (locals shadow members), so its collision scope is the function, not the class.
    // The enclosing-function walk only matches when `byte` is inside a function that declares the
    // OLD name as a local — exactly the `NonMethodTarget::Local` precondition.
    let old_name = cursor_identifier(tree, node_id);
    if let Some(old) = old_name.as_deref() {
        if enclosing_function_declaring(tree, byte, old).is_some() {
            if function_declares_local(tree, byte, new_name) {
                return Some(RequestRefusal::invalid_name(format!(
                    "Cannot rename to `{new_name}`: `{new_name}` is already declared in this scope"
                )));
            }
            // A local target: its scope is the function; the class-member check below would be the
            // wrong scope, so stop here.
            return None;
        }
    }
    // Member scope: a method/signal declaration, an attribute access, or a root-member declaration
    // click → collision iff the current file's root class already declares `new_name`.
    let is_member_role =
        is_member_or_attribute_ident(tree, node_id) || node_is_root_member(tree, node_id);
    if is_member_role && root_class_declares(tree, new_name) {
        return Some(RequestRefusal::invalid_name(format!(
            "Cannot rename to `{new_name}`: a member named `{new_name}` already exists in this class"
        )));
    }
    None
}

/// `true` iff `node_id` is the identifier of a root-class member declaration (a declaration-site
/// click on `var x` / `func f` / `signal s` / `const C` / `enum E` / `class Inner`).
fn node_is_root_member(tree: &ParseTree, node_id: NodeId) -> bool {
    let Some(root_id) = tree.root_id() else {
        return false;
    };
    let NodeKind::Class(root) = &tree.get(root_id).kind else {
        return false;
    };
    root.members.iter().any(|m| {
        member_decl_id(m)
            .and_then(|decl| declaration_identifier(tree, decl))
            .is_some_and(|iid| iid == node_id)
    })
}

/// The declaration `NodeId` backing a [`Member`], for the root-member identifier walk.
fn member_decl_id(member: &Member) -> Option<NodeId> {
    use gd_syntax::ast::Member::*;
    match member {
        Class(id) | Constant(id) | Function(id) | Signal(id) | Variable(id) | Enum(id) => Some(*id),
        EnumValue(_) | Group(_) => None,
    }
}

/// `true` iff the current file's ROOT class declares a member named `name` — the member-collision
/// predicate. Walks the root class's own members only (inner-class and inherited members are a
/// different scope; an inherited collision is a shadow, which GDScript permits).
fn root_class_declares(tree: &ParseTree, name: &str) -> bool {
    let Some(root_id) = tree.root_id() else {
        return false;
    };
    let NodeKind::Class(root) = &tree.get(root_id).kind else {
        return false;
    };
    root.members
        .iter()
        .any(|m| member_named(tree, m, name).is_some())
}

/// `true` iff the smallest function containing `byte` declares `name` as a parameter or a
/// body-local var/const — the local-collision predicate. Reuses the exact shape of
/// [`enclosing_function_declaring`] (the `references` local-classification walk).
fn function_declares_local(tree: &ParseTree, byte: usize, name: &str) -> bool {
    enclosing_function_declaring(tree, byte, name).is_some()
}

// ===================================================================================================
// M10 (#72): semanticTokens — full / full/delta / range. Standard-legend-only projection over the
// existing binding/resolution info (see `crate::semantic_tokens`). `full`/`full/delta` are
// analysis-priced (shed at Hard memory pressure); `range` is parse-priced and stays served (it
// classifies against the CACHED analysis only — `None` on a miss → structural-only tokens).
// ===================================================================================================

/// `textDocument/semanticTokens/full`: every semantic token for the document, LSP delta-encoded,
/// stamped with a fresh result id (cached so the next `full/delta` can diff against it).
///
/// Analysis-priced: classifies against a full `analyze` (always available when this runs — it sheds
/// at Hard pressure before reaching here). Returns `Some(Tokens)`; never `None` (an unparseable
/// buffer yields whatever the tokenizer/analyzer recovered, possibly an empty token set).
pub fn semantic_tokens_full(
    state: &mut ServerState,
    params: lsp_types::SemanticTokensParams,
) -> Option<lsp_types::SemanticTokensResult> {
    let uri = params.text_document.uri;
    let text = state.vfs.get(uri.as_str()).map(|d| d.text())?;
    let path = uri_to_path(&uri)?;
    let key = CanonicalKey::for_uri(&uri);
    let enc = state.encoding;
    let parsed = state.workspace.parse(&key, &text);
    let rope = Rope::from_str(&text);
    let mapper = PositionMapper::new(&rope, enc);
    let analysis = analyze_with_request_token(state, &key, &path, &parsed.tree, &text);

    let raw = crate::semantic_tokens::classify_document(
        &parsed.tree,
        Some(&analysis),
        &state.workspace.native,
    );
    let data = crate::semantic_tokens::encode(&raw, &mapper, &state.caps.semantic_tokens.legend);

    let result_id = next_semantic_tokens_id(state);
    state.semantic_tokens_cache.insert(
        uri,
        crate::server::SemanticTokensCacheEntry {
            result_id: result_id.clone(),
            tokens: data.clone(),
        },
    );
    Some(lsp_types::SemanticTokensResult::Tokens(
        crate::semantic_tokens::semantic_tokens(result_id, data),
    ))
}

/// `textDocument/semanticTokens/full/delta`: a `SemanticTokensDelta` (flat-array edits) versus the
/// `previous_result_id`'s token array, or a fresh full set when that id is unknown (the client's
/// record diverged from ours — e.g. a session restart, or the entry was evicted).
///
/// Re-classifies the current document (analysis-priced, like `full`), diffs against the cached array
/// when the previous id matches, and re-stamps the cache with a new id + the new array regardless.
pub fn semantic_tokens_full_delta(
    state: &mut ServerState,
    params: lsp_types::SemanticTokensDeltaParams,
) -> Option<lsp_types::SemanticTokensFullDeltaResult> {
    let uri = params.text_document.uri;
    let text = state.vfs.get(uri.as_str()).map(|d| d.text())?;
    let path = uri_to_path(&uri)?;
    let key = CanonicalKey::for_uri(&uri);
    let enc = state.encoding;
    let parsed = state.workspace.parse(&key, &text);
    let rope = Rope::from_str(&text);
    let mapper = PositionMapper::new(&rope, enc);
    let analysis = analyze_with_request_token(state, &key, &path, &parsed.tree, &text);

    let raw = crate::semantic_tokens::classify_document(
        &parsed.tree,
        Some(&analysis),
        &state.workspace.native,
    );
    let data = crate::semantic_tokens::encode(&raw, &mapper, &state.caps.semantic_tokens.legend);

    // Does the previous id match what we last handed this URI? If so, emit edits; else fall back to a
    // full set (the spec's documented behavior for an unknown previous id).
    let prev = state.semantic_tokens_cache.get(&uri);
    let matched = prev.is_some_and(|e| e.result_id == params.previous_result_id);
    let new_id = next_semantic_tokens_id(state);

    let response = if matched {
        let old = &state.semantic_tokens_cache[&uri].tokens;
        let edits = crate::semantic_tokens::diff(old, &data);
        lsp_types::SemanticTokensFullDeltaResult::TokensDelta(lsp_types::SemanticTokensDelta {
            result_id: Some(new_id.clone()),
            edits,
        })
    } else {
        lsp_types::SemanticTokensFullDeltaResult::Tokens(crate::semantic_tokens::semantic_tokens(
            new_id.clone(),
            data.clone(),
        ))
    };

    state.semantic_tokens_cache.insert(
        uri,
        crate::server::SemanticTokensCacheEntry {
            result_id: new_id,
            tokens: data,
        },
    );
    Some(response)
}

/// `textDocument/semanticTokens/range`: only the tokens intersecting `params.range`.
///
/// Parse-priced — it classifies against the CACHED analysis ([`Workspace::cached_analysis`], an
/// `Option`), never a fresh `analyze`, so it stays served at Hard memory pressure (NOT in the
/// `analyze_using` shed set). On a cache miss (e.g. while shedding) the classifier gets `None` and
/// emits only the structurally-derivable tokens (declarations + annotations). Mints no result id and
/// never touches the `full/delta` cache (a partial set must never seed a delta baseline).
pub fn semantic_tokens_range(
    state: &mut ServerState,
    params: lsp_types::SemanticTokensRangeParams,
) -> Option<lsp_types::SemanticTokensRangeResult> {
    let uri = params.text_document.uri;
    let text = state.vfs.get(uri.as_str()).map(|d| d.text())?;
    let path = uri_to_path(&uri)?;
    let key = CanonicalKey::for_uri(&uri);
    let enc = state.encoding;
    let parsed = state.workspace.parse(&key, &text);
    let rope = Rope::from_str(&text);
    let mapper = PositionMapper::new(&rope, enc);
    // Parse-priced: cached analysis only (None under Hard pressure → structural-only classification).
    let analysis = state.workspace.cached_analysis(&key, &path, &text);

    let raw = crate::semantic_tokens::classify_document(
        &parsed.tree,
        analysis.as_deref(),
        &state.workspace.native,
    );

    // Filter to tokens intersecting the requested range, on the byte spans (before encoding, so the
    // relative-delta baseline restarts cleanly from the first surviving token).
    let start_byte = mapper.position_to_byte(params.range.start);
    let end_byte = mapper.position_to_byte(params.range.end);
    let in_range: Vec<_> = raw
        .into_iter()
        .filter(|t| t.span.start < end_byte && t.span.end > start_byte)
        .collect();

    let data =
        crate::semantic_tokens::encode(&in_range, &mapper, &state.caps.semantic_tokens.legend);
    Some(lsp_types::SemanticTokensRangeResult::Tokens(
        crate::semantic_tokens::semantic_tokens_no_id(data),
    ))
}

/// Mint the next opaque `semanticTokens` result id (`"st-{n}"`). Monotonic per session; the id is
/// only used to correlate the next `full/delta` request.
fn next_semantic_tokens_id(state: &mut ServerState) -> String {
    state.semantic_tokens_result_seq += 1;
    format!("st-{}", state.semantic_tokens_result_seq)
}
