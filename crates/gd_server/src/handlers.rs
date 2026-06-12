//! LSP request handlers.

use gd_analyze::{
    find_incoming_calls, find_outgoing_calls, AnalysisResult, Binding, BindingTargetKind, DtKind,
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
    DocumentLink, DocumentLinkParams, DocumentSymbolParams, DocumentSymbolResponse,
    GotoDefinitionParams, GotoDefinitionResponse, Hover, HoverContents, HoverParams, Location,
    MarkupContent, MarkupKind, Position, Range, ReferenceParams, SymbolInformation,
    SymbolKind as LspSymbolKind, Uri, WorkspaceSymbolParams, WorkspaceSymbolResponse,
};
use nucleo_matcher::{Config, Matcher, Utf32Str};
use ropey::Rope;
use rustc_hash::{FxHashMap, FxHashSet};

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
    // Single VFS lookup: hold `doc` across the parse and reuse its already-built rope for the
    // mapper (disjoint `&state.vfs` / `&mut state.workspace` borrows compose). Avoids both the
    // redundant hash lookup and re-allocating a rope we already hold.
    let Some(doc) = state.vfs.get(uri.as_str()) else {
        return DocumentSymbolResponse::Nested(Vec::new());
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
            contents: HoverContents::Markup(MarkupContent {
                kind: MarkupKind::Markdown,
                value: preload_md,
            }),
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
            contents: HoverContents::Markup(MarkupContent {
                kind: MarkupKind::Markdown,
                value: decl_md,
            }),
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
    // recorded a `Binding::Call` whose `callee_file` names the declaring script. Project the
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
                    callee_file: Some(f),
                    callee_name,
                    call_site,
                    ..
                } if callee_name == &name => {
                    let spans = spans.get_or_insert_with(|| callee_ident_spans(&parsed.tree));
                    let ident = spans.get(call_site).copied()?;
                    (ident.start <= node_byte && node_byte < ident.end).then_some(*f)
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

    // 1. Native class name.
    if state.workspace.native.class_named(name).is_some() {
        let (path, stub) = crate::stubs::ensure_class_stub(
            &state.stub_cache,
            &state.workspace.native,
            name,
            stub_root.as_deref(),
        )?;
        // `name.len()` is a byte count, but ensure_class_stub only materializes
        // identifier-shaped (ASCII) class names — see stub_token_location's encoding note.
        return stub_token_location(
            &path,
            stub.class_line,
            stub.class_name_col,
            name.len() as u32,
        );
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
    if !desc.is_empty() {
        md.push_str("\n\n");
        md.push_str(desc);
    }
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
        return Some(format!("```gdscript\n{sig}\n```"));
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
    Some(format!("```gdscript\n{sig}\n```"))
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
/// returns `false` so it falls through to the raw-identifier scan in `references`. This matters for
/// recall: the method/signal code path filters to `Binding::Call` records (via
/// [`push_callee_ident_locations`]), and the analyzer records no binding for a property attribute
/// read — so routing property reads through it would silently drop every read occurrence. Letting
/// them use the raw scan restores correct (over-approximating, never under-reporting) recall, which
/// is the v1 stance for property/field references.
///
/// The declaration arm deliberately matches a `Function`/`Signal` identifier at *any* class depth —
/// inner-class methods (`class Foo:` … `func helper():`) included. `Binding::Call` records carry a
/// `callee_file` but no owning-class path, so a root-class and an inner-class method sharing one
/// name in one file are indistinguishable at call-site granularity: both declaration clicks take
/// the project-wide scan and their result sets may mix the two methods' call sites. That is the
/// same over-approximating, never under-reporting stance as above — routing inner declarations to
/// the raw-identifier scan instead would *drop* their cross-file call sites (the `name_referencers`
/// index only sees interface-level names) while still mixing in-file textual matches. Splitting
/// them cleanly needs the owning class recorded on `Binding::Call` — a post-v1 refinement.
///
/// Used to decide whether `textDocument/references` uses the project-wide text scan (correct for
/// method/signal targets reached through body-local typed vars) or the faster `name_referencers`
/// index (correct for class/type/variable/property targets). Purely structural (O(#nodes), no
/// analyzer involvement); works identically whether the cursor is on the declaration or a call site.
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
///      - **Method/signal targets** (M6-E) and **autoload singleton names** (M6-D): project-wide
///        textual scan matching Godot's `gdscript_workspace.cpp:472` two-phase strategy — enumerate
///        ALL project files from the index, read text (VFS/disk; no analysis), keep only files whose
///        text contains `name` as a substring. This catches callers that reach the method through a
///        body-local typed var (`var l: Lib = Lib.new(); l.helper()`) that wouldn't appear in
///        `name_referencers`, and autoload names (`Global`) which appear only in function bodies,
///        never in interface-level annotations.
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
            // contains the cursor byte. If found (call-site click), target_file = callee_file.
            // The shared callee-span map (`callee_spans`, hoisted above) is built lazily on the
            // first matching binding and reused by push_callee_ident_locations below.
            cur_result.bindings().iter().find_map(|b| {
                if let Binding::Call {
                    callee_file,
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
                                return Some(*callee_file);
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
        // (e.g. `node.queue_free()`, whose Binding::Call carries callee_file: None):
        //   Some(Some(f)) — call-site click on a resolved callee: the declaring file is `f`.
        //   Some(None)    — call-site click on a NATIVE/unresolved callee: keep target_file None so
        //                   the scan falls back to push_identifier_locations (raw text scan) rather
        //                   than filtering on a callee_file that no Binding::Call carries.
        //   None          — no Binding::Call at the cursor (declaration-site click): the current
        //                   file declares the method, so target_file = current_fid.
        match target_file_from_binding {
            Some(cf) => cf,
            None => current_fid,
        }
    } else {
        None
    };

    // include_declaration: prepend the declaration site when requested.
    // For method/signal targets, the declaring file may be different from the current file
    // (cross-file call-site click). When target_file is known and differs from the current file,
    // read the declaring file and use find_in_file_definition on its tree to get the narrow
    // identifier span (not MemberDecl.span, which is the whole func node).
    if params.context.include_declaration {
        let decl_found = if is_method_or_signal {
            if let Some(tf) = target_file {
                if current_fid.is_some_and(|cf| cf == tf) {
                    // Declaration-site click: the current file IS the declaring file.
                    if let Some(loc) = find_in_file_definition(&parsed.tree, &name, &uri, &mapper) {
                        locations.push(loc);
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
                        locations.push(loc);
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
                    locations.push(loc);
                }
            } else if let Some(loc) = find_in_file_definition(&parsed.tree, &name, &uri, &mapper) {
                locations.push(loc);
            } else if let Some(loc) = find_global_class_definition(state, &name) {
                locations.push(loc);
            }
        }
    }

    // Always scan the current file's bindings — name_referencers is the interface-level filter
    // (cross-file dependents), not the self-references set. The body of the current file may
    // contain many uses of `name` that name_referencers won't surface.
    if let Some(p) = current_path
        .as_ref()
        .filter(|p| p.extension() == Some("gd"))
    {
        let result = analyze_with_request_token(state, &key, p, &parsed.tree, &text);
        push_binding_locations(&mut locations, &result, &name, &uri, &mapper);
        if is_method_or_signal {
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
            // Non-method targets: raw identifier scan picks up `extends Foo`, type annotations,
            // `class_name`, and other parser-level refs the reducer doesn't record. Cross-file
            // candidates below already get this scan; without it here, in-file extends/type/
            // class_name references to `name` would be silently under-reported. The dedup pass
            // at the end collapses any overlap with the binding scan.
            push_identifier_locations(&mut locations, &parsed.tree, &name, &uri, &mapper);
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
        // Fast-path for class/type/variable names: only files whose interface mentions `name` can
        // reference it; `name_referencers` already has that set. (Autoloads are excluded — they
        // take the project-wide textual scan above since they never appear in interface sets.)
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
        if is_method_or_signal {
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
            // Non-method targets: identifier scan picks up `extends Foo` and other parser-level
            // refs the reducer doesn't record. De-dupes happen below.
            push_identifier_locations(&mut locations, &parsed.tree, &name, &cand_uri, &cand_mapper);
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
/// `Binding::Use` at that narrow span which [`push_binding_locations`] reports. (Bare calls DO carry
/// `callee_file == Some(declaring_file)` via `resolve_callee_file`/WP-RD6 — recall for them rides
/// that `Use` binding, not this call projection; see `references_finds_bare_same_file_call` and
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
            callee_file: Some(cf),
            callee_name,
            call_site,
            ..
        } = binding
        {
            if *cf == target_file && callee_name == name {
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
                let build_spans = || {
                    let mut map = callee_ident_spans(&parsed.tree);
                    for nid in parsed.tree.iter_ids() {
                        let node = parsed.tree.get(nid);
                        let NodeKind::Call(call) = &node.kind else {
                            continue;
                        };
                        let Some(callee_id) = call.callee else {
                            continue;
                        };
                        if matches!(parsed.tree.get(callee_id).kind, NodeKind::Identifier(_)) {
                            map.insert(node.span, parsed.tree.get(callee_id).span);
                        }
                    }
                    map
                };
                let mut spans: Option<FxHashMap<ByteSpan, ByteSpan>> = None;
                let target = analyzed.bindings().iter().find_map(|b| match b {
                    Binding::Call {
                        callee_file: Some(f),
                        callee_name,
                        call_site,
                        ..
                    } if callee_name == &name => {
                        let spans = spans.get_or_insert_with(build_spans);
                        let ident = spans.get(call_site).copied()?;
                        (ident.start <= byte && byte < ident.end).then_some(*f)
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
                            detail: None,
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

    // Class-name registry entries — top-level class declarations across the project, anchored at
    // the `class_name` identifier's recorded line (#33; line 1 only as the registry's defensive
    // default).
    for (name, entry) in state.workspace.index.registry().entries() {
        candidates.push((
            name.to_string(),
            LspSymbolKind::CLASS,
            None,
            entry.path.clone(),
            entry.line,
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
