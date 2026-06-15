//! M11 (#79) — `workspace/willRenameFiles` + the `did*` file-operation funnel.
//!
//! ## `willRenameFiles` is a MUTATING surface
//!
//! `workspace/willRenameFiles` returns a [`WorkspaceEdit`] the client APPLIES to source before it
//! moves the files on disk. Its job: when a `.gd`/`.tscn` is renamed/moved, rewrite every `.gd`
//! `preload`/`load` `res://`-path string ARGUMENT that pointed at the *old* path so it points at the
//! *new* one — otherwise the move silently breaks those load references. The write-set is scoped to
//! `preload(…)` / `load(…)` / `ResourceLoader.load(…)` argument literals POSITIVELY (the spec's
//! `preload`/`load` scope — docs/09 §141,§252) — NOT every `res://` literal that resolves: a
//! `res://` string used as a *value* (`if p == "res://moved.gd"`, `const PATH := "res://moved.gd"`,
//! a dict key/value) is left untouched, because rewriting a non-load literal would CHANGE program
//! behavior (flip a guard, alter a display value) — a corrupting edit, not a benign over-edit. This
//! is the one place willRename's WRITE-set is *narrower* than `documentLink`'s READ-set (which still
//! links any `res://` literal): a mutating surface narrows, a read surface stays broad.
//!
//! Because the client applies the edit verbatim, the bar is the rename-saga bar (the
//! `rename`/#66 lesson, carried to every mutating consumer): **"broken code / wrong target?" →
//! refuse rather than corrupt.** Concretely this module is *fail-closed* in three independent ways:
//!
//!   1. **Write-set = positively-resolved `preload`/`load` argument literals only.** A literal is
//!      rewritten **iff** it is in a `preload`/`load` argument position AND resolves (through the
//!      same index/path resolution `documentLink` uses) to *exactly* the file being renamed —
//!      matched on resolved IDENTITY (an interned [`FileId`] for an indexed `.gd`, a normalized
//!      absolute path for a `.tscn`/other resource), **never** by string-comparing the `res://`
//!      text. A dynamic path (`load("res://" + x)`), a non-literal, a literal that doesn't resolve,
//!      OR a resolving literal that is NOT a load argument (a value/guard/display string) is left
//!      untouched. The prefix trap (`res://a.gd` vs `res://ab.gd`) cannot bite an identity match.
//!   2. **The edit span never swallows a quote.** The literal's parse span covers the *whole* token
//!      including its surrounding quotes; the rewrite targets only the bytes *between* the quotes,
//!      and only after confirming the raw inner slice round-trips to the decoded value (so an
//!      escaped / multi-line / raw string is refused, never half-rewritten).
//!   3. **Text / version / mapper are the same per file.** Spans are computed against the very text
//!      whose version stamps the edit — the open buffer's CURRENT text + its live version (correlated
//!      by normalized path, not by URI string, so the buffer is found regardless of the client's URI
//!      spelling / drive case), or, for an unopened file, disk text + `None` (the "content on disk is
//!      master" case). A span computed against one text but stamped with another's version is exactly
//!      the silent-corruption path #66 closed.
//!
//! Rewriting a `.tscn`'s own `ext_resource path="…"` entries (the *second* mutating surface) is out
//! of scope this phase; instead, when a scene-attached `.gd` moves, a `window/showMessage(Warning)`
//! names the scenes whose `ext_resource` will dangle so the user is never silently misled.
//!
//! ## `did*` are index nudges
//!
//! `didRenameFiles`/`didCreateFiles`/`didDeleteFiles` are notifications routed through the SAME
//! [`crate::watcher`] classification + reaction funnel the native watcher and the M7 (#60)
//! `didChangeWatchedFiles` path use — so the index stays fresh even on a client whose OS watcher is
//! dead, and the content-fingerprint gate dedupes a change the native watcher also observed (no
//! double-processing).

use camino::{Utf8Path, Utf8PathBuf};
use gd_syntax::ast::{LiteralNode, NodeKind};
use gd_syntax::Literal;
use lsp_types::{
    DocumentChanges, FileChangeType, FileEvent, MessageType, OneOf,
    OptionalVersionedTextDocumentIdentifier, RenameFilesParams, TextDocumentEdit, TextEdit, Uri,
    WorkspaceEdit,
};
use rustc_hash::FxHashMap;

use crate::position::PositionMapper;
use crate::server::{show_message, FileOperationsCaps, ServerState};
use crate::uri::{path_to_file_uri, uri_to_path, CanonicalKey};

/// Build the `workspace.fileOperations` server-capability block for whatever the client opted into,
/// or `None` when the client opted into nothing (so gdls never advertises a file operation it can't
/// receive — anti-catalog W15). gdls offers `willRename` (the mutating reference-rewrite) and the
/// three `did*` index nudges; `willCreate`/`willDelete` are intentionally omitted (gdls has no edit
/// to contribute on a create/delete). Every filter scopes to the only file kinds gdls tracks:
/// `**/*.gd` scripts and `**/*.tscn` scenes (scene TEXT only — W16).
#[must_use]
pub(crate) fn workspace_server_capabilities(
    caps: &FileOperationsCaps,
) -> Option<lsp_types::WorkspaceServerCapabilities> {
    use lsp_types::{
        FileOperationFilter, FileOperationPattern, FileOperationRegistrationOptions,
        WorkspaceFileOperationsServerCapabilities, WorkspaceServerCapabilities,
    };

    if !(caps.will_rename || caps.did_rename || caps.did_create || caps.did_delete) {
        return None;
    }

    // The shared `.gd` + `.tscn` file filter set every gdls file operation registers.
    let registration = || FileOperationRegistrationOptions {
        filters: ["**/*.gd", "**/*.tscn"]
            .into_iter()
            .map(|glob| FileOperationFilter {
                scheme: Some("file".to_string()),
                pattern: FileOperationPattern {
                    glob: glob.to_string(),
                    matches: Some(lsp_types::FileOperationPatternKind::File),
                    options: None,
                },
            })
            .collect(),
    };

    Some(WorkspaceServerCapabilities {
        workspace_folders: None,
        file_operations: Some(WorkspaceFileOperationsServerCapabilities {
            will_rename: caps.will_rename.then(registration),
            did_rename: caps.did_rename.then(registration),
            did_create: caps.did_create.then(registration),
            did_delete: caps.did_delete.then(registration),
            will_create: None,
            will_delete: None,
        }),
    })
}

/// The resolved IDENTITY of a `res://` literal's target — the key the write-set matches on. Matching
/// on identity (not on the `res://` string) is what makes the rewrite immune to the prefix trap
/// (`res://a.gd` vs `res://ab.gd`) and to `res://./a.gd`-style spellings: an indexed `.gd` resolves
/// to its interned [`FileId`] through `resolve_res_path` (which normalizes the join), and a
/// non-indexed resource (`.tscn`/`.tres`/asset) resolves to its normalized absolute path.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum ResIdentity {
    /// An indexed `.gd` file, by its interned id.
    Gd(gd_project::FileId),
    /// Any other resource (`.tscn` etc.) by its normalized absolute path (the index holds only
    /// `.gd`, so these never have a `FileId`).
    Resource(Utf8PathBuf),
}

impl ResIdentity {
    /// Resolve a `res://…` literal to its target identity, mirroring `documentLink`'s two-case
    /// resolution EXACTLY: an indexed `.gd` → its [`FileId`]; else a path-join confirmed to be a
    /// real on-disk file. Anything that resolves to neither (a dynamic/unresolvable path, a missing
    /// target) yields `None` and is therefore never in the write-set.
    fn resolve(index: &gd_project::Index, res: &str) -> Option<ResIdentity> {
        if let Some(fid) = index.resolve_res_path(res) {
            return Some(ResIdentity::Gd(fid));
        }
        index
            .res_to_path(res)
            .filter(|p| p.is_file())
            .map(|p| ResIdentity::Resource(gd_project::normalize_path(&p)))
    }

    /// The identity of an absolute on-disk path (a renamed file's OLD path). An indexed `.gd` is
    /// keyed by its [`FileId`]; any other path by its normalized absolute form. This is the *same*
    /// identity space [`Self::resolve`] produces, so a literal's resolved identity compares equal to
    /// a renamed file's identity iff they are the same file.
    fn of_path(index: &gd_project::Index, abs: &Utf8Path) -> ResIdentity {
        match index.file_id(abs) {
            Some(fid) => ResIdentity::Gd(fid),
            None => ResIdentity::Resource(gd_project::normalize_path(abs)),
        }
    }
}

/// One renamed file: the identity of its OLD location (what literals must resolve to, to be
/// rewritten) and the `res://` text of its NEW location (what they are rewritten to).
struct RenameTarget {
    old_identity: ResIdentity,
    new_res: String,
}

/// Whether `new_res` is safe to inject verbatim as a `res://` `preload`/`load` literal's path. The
/// rewrite replaces the bytes between a literal's quotes with this text, so the text must be:
///   * a VALID in-root `res://` path — re-resolving it through [`gd_project::res_to_path`] (the
///     project's canonical guard) rejects `..` traversal, an absolute relative part, and a drive
///     prefix. `path_to_res` does NOT collapse `..`, so without this an un-normalized `newUri`
///     containing `..` would emit a malformed `res://../…` edit (one `res_to_path` would itself
///     reject), and
///   * free of any character that would break the literal — or make the rewritten literal something
///     the GDScript tokenizer itself DIAGNOSES — see [`char_breaks_string_literal`].
///
/// Fail-closed: a `false` here drops the whole rename target (its references stay untouched). A
/// missed rewrite is acceptable; a corrupting one is not.
fn is_safe_rewrite_target(root: &Utf8Path, new_res: &str) -> bool {
    if gd_project::res_to_path(root, new_res).is_none() {
        return false;
    }
    !new_res.chars().any(char_breaks_string_literal)
}

/// A character that cannot be injected verbatim into a `res://` string literal without breaking it
/// (or making the GDScript tokenizer flag it). Mirrors the tokenizer's own string-scan rules
/// (`gd_syntax::lexer`):
///   * `"` / `'` — closes the string early (we refuse BOTH quote styles, not just the literal's own,
///     so the check is independent of the call site and errs toward safety);
///   * `\\` — opens an escape sequence inside the string;
///   * a control character (newline/CR/tab/C0/DEL) — breaks or mangles the single-line literal;
///   * the invisible text-direction (bidi) control characters the tokenizer explicitly diagnoses
///     inside a string with "Invisible text direction control character present in the string …"
///     (`U+200E`, `U+200F`, `U+202A..=U+202E`, `U+2066..=U+2069`). `char::is_control` does NOT cover
///     these, so they are listed verbatim from the tokenizer source — injecting one would turn a
///     clean literal into one Godot/gdls flags as invalid. (Such characters in a real filename are
///     pathological; refusing the rewrite — a missed rewrite — is the correct fail-closed outcome.)
fn char_breaks_string_literal(c: char) -> bool {
    c == '"'
        || c == '\''
        || c == '\\'
        || c.is_control()
        || c == '\u{200E}'
        || c == '\u{200F}'
        || ('\u{202A}'..='\u{202E}').contains(&c)
        || ('\u{2066}'..='\u{2069}').contains(&c)
}

/// `workspace/willRenameFiles`: for each renamed/moved `.gd`/`.tscn`, rewrite every indexed `.gd`'s
/// `preload`/`load` `res://`-path string ARGUMENT that POSITIVELY resolves to the renamed file (a
/// `preload(…)` / `load(…)` / `ResourceLoader.load(…)` argument — NOT a `res://` value/guard/display
/// string, see the module docs) so it points at the new `res://` path. Returns a versioned
/// [`WorkspaceEdit`] (one [`TextDocumentEdit`] per affected file), or `None` (LSP `null`) when
/// nothing needs rewriting.
///
/// Also (side effect, before returning): when a renamed `.gd` is attached to one or more scenes,
/// emits a `window/showMessage(Warning)` naming them — their `ext_resource path="res://old.gd"` will
/// dangle (full `.tscn` rewriting is out of scope this phase).
///
/// The write-set is the fail-closed firewall (see the module docs): only literals that resolve to a
/// renamed file's identity are touched; the edit targets only the bytes between the quotes; and each
/// edit's span/version come from the same per-file text.
#[must_use]
pub(crate) fn will_rename_files(
    state: &mut ServerState,
    params: RenameFilesParams,
) -> Option<WorkspaceEdit> {
    let root = state.workspace.project.root.clone();

    // (1) Build the rename map: old-identity → new-res. Short-circuit the no-ops HERE so they never
    // reach the scan: a same-path rename (old == new), a target whose old/new path isn't a project
    // `res://` path (outside the root, or a non-UTF-8/file:// URI). A move whose NEW path leaves the
    // project root has no `res://` form to rewrite to, so it is skipped (we never emit a non-`res://`
    // target). Each surviving entry's old identity is taken in the index's identity space so a
    // literal's resolved identity can be compared for equality.
    let mut targets: Vec<RenameTarget> = Vec::new();
    let mut renamed_gd_old_res: Vec<String> = Vec::new();
    for f in &params.files {
        let (Some(old_abs), Some(new_abs)) =
            (uri_str_to_path(&f.old_uri), uri_str_to_path(&f.new_uri))
        else {
            continue;
        };
        // old == new (a no-op the client shouldn't send, but be defensive): nothing to rewrite.
        if gd_project::normalize_path(&old_abs) == gd_project::normalize_path(&new_abs) {
            continue;
        }
        // Both ends must be project-rooted `res://` paths. The OLD path's `res://` form lets us
        // collect the scene-attach warning for a `.gd`; the NEW path's is the rewrite target.
        let (Some(old_res), Some(new_res)) = (
            gd_project::path_to_res(&root, &old_abs),
            gd_project::path_to_res(&root, &new_abs),
        ) else {
            continue;
        };
        if old_abs.extension() == Some("gd") {
            renamed_gd_old_res.push(old_res);
        }
        // FAIL-CLOSED on the WRITE side: the new `res://` text is injected verbatim BETWEEN a
        // literal's quotes, so a target whose `res://` text isn't a plain, in-root, quote-safe path
        // is REFUSED (the rename still happens client-side; we just don't rewrite refs to it). This
        // closes two corruption paths `path_to_res` does NOT itself prevent: a `..`-under-root
        // spelling that would emit a malformed `res://../…` (it does not collapse `..`), and a
        // filename containing a quote / backslash / control char that would break the literal it is
        // injected into. A missed rewrite here is a minor limitation; emitting either would be a
        // release-blocking corrupting edit. See `is_safe_rewrite_target`.
        if !is_safe_rewrite_target(&root, &new_res) {
            log::warn!(
                "willRenameFiles: refusing to rewrite references to {new_res:?} — the new path is \
                 not a plain quote-safe in-root res:// path; leaving every reference untouched"
            );
            continue;
        }
        targets.push(RenameTarget {
            old_identity: ResIdentity::of_path(&state.workspace.index, &old_abs),
            new_res,
        });
    }

    // Side effect: warn about scenes that will dangle when an attached `.gd` moves (full `.tscn`
    // ext_resource rewriting is out of scope this phase). Done before the edit so the user sees it
    // regardless of whether any `.gd` literal needed rewriting.
    warn_dangling_scene_attachments(state, &renamed_gd_old_res);

    if targets.is_empty() {
        return None;
    }

    // (2) Scan every indexed `.gd` ONCE (loops inverted: O(files), not O(renames × files)). For each
    // `preload`/`load` `res://` argument literal (positive identification — `collect_load_path_literals`),
    // resolve its identity and look it up in the rename map; a hit yields a `TextEdit` over the
    // path-inside-the-quotes. Edits are grouped per URI into ONE `TextDocumentEdit` (two for one URI
    // would be a malformed `WorkspaceEdit`).
    //
    // Collect (FileId → path) under the index borrow first, then read text per file (VFS / disk
    // borrow) so the borrows don't overlap — the same discipline the `references` candidate walk uses.
    let candidates: Vec<(gd_project::FileId, Utf8PathBuf)> = state
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

    // Open-buffer overlay, keyed by NORMALIZED PATH — NOT by the raw VFS URI string. The VFS is
    // keyed by the client's exact `didOpen` URI bytes, whose spelling (raw sub-delimiters, Windows
    // drive case) need not match the disk-walked candidate path we'd rebuild via `path_to_file_uri`.
    // Probing the VFS with that rebuilt URI can MISS an open buffer and silently fall through to disk
    // text stamped `version: None` — which, applied to a DIRTY buffer, corrupts it (spans from disk
    // text, "any version" stamp). So correlate disk paths to open buffers the way the watcher does
    // (`open_buffer_paths`): normalize both sides to a path and compare. This keeps invariant #3
    // (open buffer ⇒ buffer text + its live version) holding regardless of URI spelling.
    let open_overlay = open_buffer_overlay(state);

    // First-seen URI order for deterministic output, mirroring `build_workspace_edit`. Each URI's
    // entry carries the version captured ALONGSIDE the text its spans were computed against, so the
    // assembled edit can never stamp a span from text A with the version of text B.
    let mut order: Vec<Uri> = Vec::new();
    let mut by_uri: FxHashMap<String, (Option<i32>, Vec<TextEdit>)> = FxHashMap::default();

    for (_fid, path) in candidates {
        let Some(uri) = path_to_file_uri(&path) else {
            log::warn!("willRenameFiles: dropping candidate {path} — path_to_file_uri rejected it");
            continue;
        };

        // The ONE text/version pair for this file: open buffer (buffer text + live version) or, for
        // an unopened file, disk text + `None`. The mapper below is built from this exact text, so
        // the edit's span and the version it is stamped with describe the same bytes.
        let (text, version) = match open_overlay.get(&gd_project::normalize_path(&path)) {
            Some((text, version)) => (text.clone(), Some(*version)),
            None => match std::fs::read_to_string(&path) {
                Ok(text) => (text, None),
                Err(_) => continue, // unreadable: can't compute spans safely → skip (never guess)
            },
        };

        let edits = rewrite_literals_in(state, &uri, &text, &targets);
        if edits.is_empty() {
            continue;
        }
        // Group by URI (a file is scanned once, so this is the first and only batch for it).
        order.push(uri.clone());
        by_uri.insert(uri.as_str().to_string(), (version, edits));
    }

    if order.is_empty() {
        return None;
    }

    Some(assemble_workspace_edit(state, order, by_uri))
}

/// Build the open-buffer overlay keyed by NORMALIZED PATH → (current text, current version). This is
/// the spelling-independent correlation the watcher's `open_buffer_paths` uses: every open URI is
/// routed back to a path and normalized, so a candidate's disk-walked path looks up the buffer
/// regardless of the client's URI encoding or drive casing (where a raw VFS-URL-string probe would
/// miss and wrongly fall through to disk text). An open URI that no longer parses / isn't a path is
/// skipped (it just won't shadow disk — the safe direction).
fn open_buffer_overlay(state: &ServerState) -> FxHashMap<Utf8PathBuf, (String, i32)> {
    let mut overlay = FxHashMap::default();
    let open_uris: Vec<String> = state.vfs.open_uris().map(str::to_string).collect();
    for raw in open_uris {
        let Ok(uri) = raw.parse::<Uri>() else {
            continue;
        };
        let Some(path) = uri_to_path(&uri) else {
            continue;
        };
        if let Some(doc) = state.vfs.get(&raw) {
            overlay.insert(gd_project::normalize_path(&path), (doc.text(), doc.version));
        }
    }
    overlay
}

/// Scan one file's parse tree for the `res://` path STRING ARGUMENT of a `preload(…)` / `load(…)`
/// call whose resolved identity matches a renamed target, returning a [`TextEdit`] per match that
/// replaces the path BETWEEN the quotes with the new `res://` path. Fail-closed at every step (see
/// the module docs): a literal not in a `preload`/`load` argument position, one that doesn't resolve
/// to a target, or one whose inner span can't be safely isolated, yields no edit.
///
/// ## Write-set narrowing: POSITIVE preload/load identification, not scan-and-exclude
///
/// Unlike `documentLink` (a READ feature that links *any* `res://` literal — handlers.rs), the
/// WRITE-set here is scoped to literals that are positively a load reference: a `preload(…)` /
/// `load(…)` / `ResourceLoader.load(…)` argument. A `res://` string used as a *value* —
/// `if p == "res://a.gd"`, `const DISPLAY := "res://a.gd"`, a dict key/value, a bare expression
/// statement — is NOT a load and is left untouched, because rewriting it would change program
/// behavior (a guard comparison would silently flip; a display value would change), which is a
/// corrupting edit, not a benign over-edit. The identity / inner-span / quote-safe guards below are
/// unchanged — narrowing only restricts WHICH literals reach them. [`collect_load_path_literals`]
/// does the positive identification; documentLink keeps its own broad locator (nothing shared).
fn rewrite_literals_in(
    state: &mut ServerState,
    uri: &Uri,
    text: &str,
    targets: &[RenameTarget],
) -> Vec<TextEdit> {
    let parsed = state.workspace.parse(&CanonicalKey::for_uri(uri), text);
    let rope = ropey::Rope::from_str(text);
    let mapper = PositionMapper::new(&rope, state.encoding);
    let bytes = text.as_bytes();

    let mut edits = Vec::new();
    // Only the `res://` path argument of a `preload`/`load` call is a candidate (positive
    // identification — see the doc comment). Everything else is never a write-set member.
    for (node_id, path) in collect_load_path_literals(&parsed.tree) {
        // POSITIVE RESOLUTION: rewrite only when this literal resolves to a renamed file's identity.
        // Match on identity, never on the `res://` string (the prefix trap), and never speculatively.
        let Some(identity) = ResIdentity::resolve(&state.workspace.index, &path) else {
            continue; // dynamic / unresolvable / missing target — leave untouched
        };
        let Some(target) = targets.iter().find(|t| t.old_identity == identity) else {
            continue; // resolves, but to some OTHER file — leave untouched
        };

        // INNER SPAN: the literal's parse span covers the whole token INCLUDING its surrounding
        // quotes; the rewrite must touch only the bytes between them. Isolate the inner span
        // fail-closed, refusing anything we can't prove is a plain single-line quoted path.
        let span = parsed.tree.get(node_id).span;
        let Some(inner) = inner_string_span(bytes, span, &path) else {
            log::debug!(
                "willRenameFiles: refusing a `res://` literal whose inner span can't be safely \
                 isolated (escaped/raw/multiline?) — leaving it untouched"
            );
            continue;
        };
        edits.push(TextEdit {
            range: mapper.span_to_range(inner),
            new_text: target.new_res.clone(),
        });
    }
    edits
}

/// Walk the parse tree and collect the `(NodeId, "res://…")` of every STRING literal that is
/// positively a `preload`/`load` PATH ARGUMENT — the only literals willRename is allowed to rewrite.
/// Returns the literal node's id (for its span) and its decoded `res://` value. Three positive forms,
/// each identified by the AST node that *introduces* the load (never by walking up from a literal):
///
///   * `preload(<path>)` — the dedicated [`NodeKind::Preload`]; `path` points straight at the arg.
///   * `load(<path>)` — a [`NodeKind::Call`] whose callee is a bare `Identifier{name:"load"}` and is
///     not a `super` call. The `@GlobalScope` utility (reducer.rs `global_utility`), a free function,
///     never a method — so the callee being an `Identifier` (not a `Subscript`) is what distinguishes
///     it from an arbitrary `obj.load("res://x")` user method (which we must NOT rewrite).
///   * `ResourceLoader.load(<path>)` — a [`NodeKind::Call`] whose callee is a `Subscript` of the form
///     `ResourceLoader.load` (base `Identifier{name:"ResourceLoader"}`, attribute `load`). Matched on
///     the base name precisely, so `other_obj.load("res://x")` is excluded. (A `ResourceLoader`
///     shadowed by a local of that name is pathological and a missed rewrite — the safe direction.)
///
/// Parentheses are transparent in the AST (`parse_grouping` returns the inner expression), so
/// `preload(("res://x"))` still points `path`/`arguments[0]` straight at the literal. Only the FIRST
/// argument of a `load` call is the path (`load(path, type_hint, …)`); a `load()` with no argument
/// yields nothing. A non-`res://` string in any of these positions is skipped (the caller only
/// rewrites `res://` paths anyway).
fn collect_load_path_literals(
    tree: &gd_syntax::ast::ParseTree,
) -> Vec<(gd_syntax::ast::NodeId, String)> {
    use gd_syntax::ast::{CallNode, IdentifierNode, PreloadNode, SubscriptAccess, SubscriptNode};

    // The literal `res://` string at `id`, if `id` is a `Literal::String` starting with `res://`.
    let res_literal =
        |id: Option<gd_syntax::ast::NodeId>| -> Option<(gd_syntax::ast::NodeId, String)> {
            let id = id?;
            let NodeKind::Literal(LiteralNode {
                value: Literal::String(path),
            }) = &tree.get(id).kind
            else {
                return None;
            };
            path.starts_with("res://").then(|| (id, path.clone()))
        };

    // Whether `callee` is a bare `load` (an `Identifier{name:"load"}`) or `ResourceLoader.load`
    // (a `Subscript` `ResourceLoader.load`). Anything else (another method's `.load`, a complex
    // callee) is NOT a load reference.
    let is_load_callee = |callee: Option<gd_syntax::ast::NodeId>| -> bool {
        let Some(callee) = callee else {
            return false;
        };
        match &tree.get(callee).kind {
            NodeKind::Identifier(IdentifierNode { name }) => name == "load",
            NodeKind::Subscript(SubscriptNode {
                base: Some(base),
                access: Some(SubscriptAccess::Attribute(Some(attr))),
            }) => {
                let base_is_resource_loader = matches!(
                    &tree.get(*base).kind,
                    NodeKind::Identifier(IdentifierNode { name }) if name == "ResourceLoader"
                );
                let attr_is_load = matches!(
                    &tree.get(*attr).kind,
                    NodeKind::Identifier(IdentifierNode { name }) if name == "load"
                );
                base_is_resource_loader && attr_is_load
            }
            _ => false,
        }
    };

    let mut out = Vec::new();
    for node_id in tree.iter_ids() {
        match &tree.get(node_id).kind {
            NodeKind::Preload(PreloadNode { path }) => {
                out.extend(res_literal(*path));
            }
            NodeKind::Call(CallNode {
                callee,
                arguments,
                is_super,
                ..
            }) if !is_super && is_load_callee(*callee) => {
                // The path is the FIRST argument; a `load()` with none contributes nothing.
                out.extend(res_literal(arguments.first().copied()));
            }
            _ => {}
        }
    }
    out
}

/// Isolate the byte span of the path BETWEEN a string literal's quotes, or `None` (refuse) when the
/// token isn't a plain single-line quoted string whose content equals `decoded` verbatim.
///
/// The whole-token span `[start, end)` includes the surrounding quotes. We require: the first and
/// last bytes are the SAME ordinary quote (`"` or `'`); the token is at least two bytes; and the raw
/// slice between the quotes equals the parser's `decoded` value EXACTLY. That last check rejects any
/// string with escapes (`"\x2f"`), a multi-line `"""…"""` (whose span carries six quote bytes, not
/// two), or a raw string (`r"…"`, where `start` is `r`, not a quote) — in every such case the naive
/// "strip one quote each end" would corrupt, so we refuse and leave the literal untouched. (A
/// `res://` path is a plain forward-slash literal in practice, so the common case always passes.)
fn inner_string_span(
    bytes: &[u8],
    span: gd_syntax::ByteSpan,
    decoded: &str,
) -> Option<gd_syntax::ByteSpan> {
    let start = span.start;
    let end = span.end;
    // Need at least an opening and a closing quote byte, and the span must be in range.
    if end <= start + 1 || end > bytes.len() {
        return None;
    }
    let open = bytes[start];
    let close = bytes[end - 1];
    if open != close || (open != b'"' && open != b'\'') {
        return None; // raw-string prefix, multiline triple-quote tail, or not a quoted string
    }
    let inner = gd_syntax::ByteSpan::new(start + 1, end - 1);
    // The raw bytes between the quotes must round-trip to the decoded value — no escapes, no
    // embedded newlines, nothing the rewrite span wouldn't faithfully replace.
    let raw_inner = std::str::from_utf8(&bytes[inner.start..inner.end]).ok()?;
    if raw_inner != decoded {
        return None;
    }
    Some(inner)
}

/// Assemble the per-URI edits into the client's negotiated [`WorkspaceEdit`] shape — versioned
/// `documentChanges` (one [`TextDocumentEdit`] per file carrying the version captured alongside its
/// text) when the client advertised `workspace.workspaceEdit.documentChanges`, else the legacy
/// `changes` map. Mirrors the rename handler's `build_workspace_edit` so both mutating features emit
/// identical shapes.
// `lsp_types::Uri` has interior mutability (cached parsed components in a `Cell`), tripping
// `clippy::mutable_key_type` as a `HashMap` key — but `WorkspaceEdit.changes` IS keyed on `Uri` by
// the wire shape and the key is never mutated after insertion, so the lint's hazard cannot occur.
#[allow(clippy::mutable_key_type)]
fn assemble_workspace_edit(
    state: &ServerState,
    order: Vec<Uri>,
    mut by_uri: FxHashMap<String, (Option<i32>, Vec<TextEdit>)>,
) -> WorkspaceEdit {
    if state.caps.workspace_edit_document_changes {
        let edits: Vec<TextDocumentEdit> = order
            .into_iter()
            .filter_map(|uri| {
                let (version, text_edits) = by_uri.remove(uri.as_str())?;
                Some(TextDocumentEdit {
                    text_document: OptionalVersionedTextDocumentIdentifier { uri, version },
                    edits: text_edits.into_iter().map(OneOf::Left).collect(),
                })
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
            if let Some((_version, text_edits)) = by_uri.remove(uri.as_str()) {
                changes.insert(uri, text_edits);
            }
        }
        WorkspaceEdit {
            changes: Some(changes),
            ..Default::default()
        }
    }
}

/// Emit a `window/showMessage(Warning)` naming the scenes whose `ext_resource path="res://old.gd"`
/// will dangle when a script they attach is moved. Full `.tscn` `ext_resource` rewriting is the
/// second mutating surface and out of scope this phase; surfacing the warning keeps the user from
/// being silently misled. No-op when no renamed `.gd` is attached to any scene.
fn warn_dangling_scene_attachments(state: &ServerState, renamed_gd_old_res: &[String]) {
    let mut affected: Vec<String> = Vec::new();
    for old_res in renamed_gd_old_res {
        for scene_res in state.workspace.scenes().scenes_attaching_script(old_res) {
            if !affected.iter().any(|s| s == scene_res) {
                affected.push(scene_res.to_string());
            }
        }
    }
    if affected.is_empty() {
        return;
    }
    affected.sort(); // deterministic message
    let scenes = affected.join(", ");
    show_message(
        state,
        MessageType::WARNING,
        &format!(
            "gdls: moving a script attached to {} will leave the scene's `ext_resource` path \
             dangling — gdls does not yet rewrite scene files; update the scene(s) manually: {scenes}",
            if affected.len() == 1 { "a scene" } else { "scenes" },
        ),
    );
}

/// `workspace/didRenameFiles`: route a client-observed batch of renames into the index-reconcile
/// funnel as a delete (old path) + create (new path) per rename, deduped against the native watcher
/// by the content-fingerprint gate (no double-processing). A pure index nudge — the disk is already
/// in its post-rename state by the time this fires.
pub(crate) fn did_rename_files(state: &mut ServerState, params: RenameFilesParams) {
    let mut events = Vec::with_capacity(params.files.len() * 2);
    for f in &params.files {
        if let Some(uri) = parse_uri(&f.old_uri) {
            events.push(FileEvent {
                uri,
                typ: FileChangeType::DELETED,
            });
        }
        if let Some(uri) = parse_uri(&f.new_uri) {
            events.push(FileEvent {
                uri,
                typ: FileChangeType::CREATED,
            });
        }
    }
    crate::server::handle_client_file_events(state, events);
}

/// `workspace/didCreateFiles`: route a client-observed batch of creations into the index-reconcile
/// funnel as create events, deduped against the native watcher.
pub(crate) fn did_create_files(state: &mut ServerState, params: lsp_types::CreateFilesParams) {
    let events: Vec<FileEvent> = params
        .files
        .iter()
        .filter_map(|f| parse_uri(&f.uri))
        .map(|uri| FileEvent {
            uri,
            typ: FileChangeType::CREATED,
        })
        .collect();
    crate::server::handle_client_file_events(state, events);
}

/// `workspace/didDeleteFiles`: route a client-observed batch of deletions into the index-reconcile
/// funnel as delete events, deduped against the native watcher.
pub(crate) fn did_delete_files(state: &mut ServerState, params: lsp_types::DeleteFilesParams) {
    let events: Vec<FileEvent> = params
        .files
        .iter()
        .filter_map(|f| parse_uri(&f.uri))
        .map(|uri| FileEvent {
            uri,
            typ: FileChangeType::DELETED,
        })
        .collect();
    crate::server::handle_client_file_events(state, events);
}

/// Parse a wire `file://` URI string into an [`Uri`], or `None` (logged) when it is malformed —
/// never crash on a bad client URI ("never crash, never lie").
fn parse_uri(s: &str) -> Option<Uri> {
    match s.parse::<Uri>() {
        Ok(uri) => Some(uri),
        Err(e) => {
            log::warn!("file operation: dropping un-parseable URI {s:?}: {e}");
            None
        }
    }
}

/// Parse a wire `file://` URI string and map it to an absolute filesystem path, or `None` for a
/// malformed / non-`file:` URI.
fn uri_str_to_path(s: &str) -> Option<Utf8PathBuf> {
    uri_to_path(&parse_uri(s)?)
}
