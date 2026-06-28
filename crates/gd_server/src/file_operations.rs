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
//! Rewriting a `.tscn`'s own `ext_resource path="…"` entries (the *second* mutating surface) is
//! ALSO done (#131), under the same fail-closed bar: a scene that attaches a renamed `.gd` (or
//! instances a renamed `.tscn`) has its `ext_resource path="…"` rewritten, identity-matched and
//! exact-span and apply→reparse-verified (see `rewrite_tscn_ext_resources`). A scene gdls cannot
//! safely rewrite is left untouched and a `window/showMessage(Warning)` names it (the never-lie
//! backstop) — so a scene either gets its reference fixed or the user is told it will dangle.
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
use rustc_hash::{FxHashMap, FxHashSet};

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
/// rewritten), the OLD `res://` form (the key the `.tscn` `SceneIndex` reverse maps are keyed by —
/// #131), and the `res://` text of its NEW location (what references are rewritten to).
struct RenameTarget {
    old_identity: ResIdentity,
    old_res: String,
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
/// Also rewrites `.tscn` `ext_resource path="…"` entries that reference a renamed file (#131 — the
/// second mutating surface, see `rewrite_tscn_ext_resources`), and emits a `window/showMessage`
/// (Warning) naming only the scenes it could NOT safely rewrite (those left dangling).
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
    // #229: the `.tscn` analog of `renamed_gd_old_res` — old `res://` of every renamed SCENE, so a
    // scene that INSTANCES it as a sub-scene and is left unrewritten still gets a dangling warning.
    let mut renamed_tscn_old_res: Vec<String> = Vec::new();
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
            renamed_gd_old_res.push(old_res.clone());
        } else if old_abs.extension() == Some("tscn") {
            // #229: collected BEFORE the safety gate (like the `.gd` case), so a refused-rewrite
            // scene move still warns about the parent scenes that instance it.
            renamed_tscn_old_res.push(old_res.clone());
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
            old_res,
            new_res,
        });
    }

    if targets.is_empty() {
        // No safe rewrite target survived: still warn about every scene a renamed `.gd` (script
        // attachment) or `.tscn` (sub-scene instance) would leave dangling (the rename happens
        // client-side regardless), with an empty "already rewritten" set.
        warn_dangling_scene_references(
            state,
            &renamed_gd_old_res,
            &renamed_tscn_old_res,
            &FxHashSet::default(),
        );
        return None;
    }

    // First-seen URI order for deterministic output, mirroring `build_workspace_edit`. Each URI's
    // entry carries the version captured ALONGSIDE the text its spans were computed against, so the
    // assembled edit can never stamp a span from text A with the version of text B. Shared by the
    // `.gd` literal scan and the `.tscn` ext_resource scan (a file is each kind, never both).
    let mut order: Vec<Uri> = Vec::new();
    let mut by_uri: FxHashMap<String, (Option<i32>, Vec<TextEdit>)> = FxHashMap::default();

    // (2a) The SECOND mutating surface (#131): rewrite `ext_resource path="res://old"` entries inside
    // `.tscn` scenes that positively reference a renamed file (a script they attach, or a sub-scene
    // they instance) — driven by the `SceneIndex` reverse maps (NOT a raw `res://` text scan), each
    // edit anchored to the parser's exact `path_span` and verified by reparse. Returns, per scene
    // `res://`, whether it was rewritten, so the dangling warning fires ONLY for scenes left untouched.
    let rewritten_scenes = rewrite_tscn_ext_resources(state, &targets, &mut order, &mut by_uri);

    // Side effect: warn about scenes that will STILL dangle after the rewrite — a `.gd` move whose
    // attaching scene, or a `.tscn` move whose instancing parent scene (#229), we could not safely
    // rewrite (refused span / unsafe text). Scenes we DID rewrite are no longer dangling, so they
    // are excluded. Done before returning so the user always sees it.
    warn_dangling_scene_references(
        state,
        &renamed_gd_old_res,
        &renamed_tscn_old_res,
        &rewritten_scenes,
    );

    // (2b) Scan every indexed `.gd` ONCE (loops inverted: O(files), not O(renames × files)). For each
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

/// The SECOND mutating surface (#131): rewrite `ext_resource path="res://old"` entries inside the
/// project's `.tscn` scenes that POSITIVELY reference a renamed file, so a scene's script / sub-scene
/// reference does not dangle after the move. Returns the set of normalized scene `res://` paths that
/// were successfully rewritten (so the dangling warning fires only for scenes left untouched).
///
/// ## Mutating-consumer discipline (the rename-saga bar, carried to scene text)
///
/// This is a second client-applied edit surface with the same fail-closed bar as the `.gd` path:
///
///   1. **Candidate set by IDENTITY, never a `res://` text scan.** The scenes to consider are driven
///      by the [`SceneIndex`](gd_project::SceneIndex) reverse maps keyed by each renamed file's OLD
///      `res://`: `scenes_attaching_script` for a renamed `.gd`, `scenes_instancing` for a renamed
///      `.tscn`. A scene never referencing a renamed file is never opened. Within a candidate scene,
///      the `ext_resource` to rewrite is matched by RESOLVED IDENTITY ([`ResIdentity`]) against the
///      rename targets — exactly as the `.gd` literal scan matches — so a prefix/basename neighbour
///      (`res://a.gd` vs `res://ab.gd`) can never be hit.
///   2. **Exact span from the parser, verified by reparse.** The edit targets ONLY the bytes of the
///      `path="…"` value between its quotes ([`gd_project::scene::ExtResource::path_span`], the
///      single-source-of-truth the scene parser surfaces). After applying every edit to the scene
///      text in memory we REPARSE and assert NO ext_resource still resolves to a renamed OLD identity
///      — a scene whose verify fails is dropped wholesale (no partial edit ships). We deliberately do
///      NOT assert the new path re-resolves: at willRename time the renamed file is not yet on disk
///      (the client applies this edit and moves the file afterward), so the new path resolves to
///      `None` by design; "the old identity is gone" is the correct, sufficient invariant (the new
///      text was already proven well-formed at offer time by `is_safe_rewrite_target`). This is the
///      "verify by apply→reparse-by-identity, not string match" rule.
///   3. **Quote-safe new text + same text/version/mapper as the `.gd` path.** Each target already
///      passed [`is_safe_rewrite_target`] (in-root, no `..`, no quote/backslash/control char), so the
///      injected `res://` is safe inside the `.tscn` `path="…"` quotes too. The scene text + version
///      come from the SAME open-buffer-or-disk source the `.gd` scan uses (a `.tscn` may be open in
///      the client — it is in the `**/*.tscn` file-operations filter), so a span computed against one
///      text is never stamped with another's version.
///
/// Edits are merged into the shared `order`/`by_uri` accumulator (each file is a `.gd` OR a `.tscn`,
/// never both, so there is no per-URI collision with the `.gd` scan).
fn rewrite_tscn_ext_resources(
    state: &ServerState,
    targets: &[RenameTarget],
    order: &mut Vec<Uri>,
    by_uri: &mut FxHashMap<String, (Option<i32>, Vec<TextEdit>)>,
) -> FxHashSet<String> {
    let root = state.workspace.project.root.clone();
    let mut rewritten: FxHashSet<String> = FxHashSet::default();

    // (1) Candidate scenes BY IDENTITY: for each renamed target, ask the SceneIndex reverse maps which
    // scenes attach the moved `.gd` (a script) or instance the moved `.tscn` (a sub-scene), keyed by
    // the target's OLD `res://`. Collect the de-duplicated set of candidate scene `res://` paths under
    // the index borrow, then read/parse each once below (borrows don't overlap).
    let candidate_scene_res: Vec<String> = {
        let scenes = state.workspace.scenes();
        let mut seen: FxHashSet<String> = FxHashSet::default();
        let mut out: Vec<String> = Vec::new();
        for t in targets {
            // The SceneIndex reverse maps are keyed by the renamed file's OLD `res://`. A `.gd` is a
            // script some scenes attach; a `.tscn` is a sub-scene some scenes instance. We query BOTH
            // maps with the same key (a path is one kind, so the other map yields nothing) rather than
            // branch on extension — cheaper and robust to an odd spelling.
            let attaching = scenes.scenes_attaching_script(&t.old_res);
            let instancing = scenes.scenes_instancing(&t.old_res);
            for scene_res in attaching.chain(instancing) {
                let key = gd_project::scene::normalize_res(scene_res);
                if seen.insert(key.clone()) {
                    out.push(key);
                }
            }
        }
        out
    };
    if candidate_scene_res.is_empty() {
        return rewritten;
    }

    let open_overlay = open_buffer_overlay(state);

    for scene_res in candidate_scene_res {
        // Map the scene's `res://` to its absolute path (in-root, existence not required here — the
        // open-buffer/disk read below is the existence gate).
        let Some(abs) = gd_project::res_to_path(&root, &scene_res) else {
            continue;
        };
        let Some(uri) = path_to_file_uri(&abs) else {
            continue;
        };
        // ONE text/version pair, identical discipline to the `.gd` scan: open buffer (buffer text +
        // live version) when the `.tscn` is open in the client, else disk text + `None`.
        let (text, version) = match open_overlay.get(&gd_project::normalize_path(&abs)) {
            Some((text, version)) => (text.clone(), Some(*version)),
            None => match std::fs::read_to_string(abs.as_std_path()) {
                Ok(text) => (text, None),
                Err(_) => continue, // unreadable: can't compute spans safely → skip (never guess)
            },
        };

        let edits = match tscn_ext_resource_edits(state, &text, targets) {
            Some(edits) if !edits.is_empty() => edits,
            _ => continue, // nothing matched, or the apply→reparse verify failed → ship no edit
        };

        order.push(uri.clone());
        // A file is a `.gd` XOR a `.tscn`, so a scene URI can never already hold a `.gd` literal
        // edit; `insert` (overwrite) is therefore safe. Trip a debug assertion if that cross-surface
        // invariant is ever violated by a future change, so an overwrite that would silently drop
        // the other surface's edits surfaces in tests/CI instead of corrupting the WorkspaceEdit.
        debug_assert!(
            !by_uri.contains_key(uri.as_str()),
            "willRenameFiles: a scene URI already held an edit before the .tscn scan — the \
             .gd/.tscn per-URI exclusivity invariant is broken"
        );
        by_uri.insert(uri.as_str().to_string(), (version, edits));
        rewritten.insert(scene_res);
    }

    rewritten
}

/// Compute the `.tscn` `ext_resource path="…"` rewrite edits for one scene's `text`, or `None` when
/// nothing matched OR the apply→reparse-by-identity verify failed (fail-closed: a scene that does not
/// re-resolve cleanly ships NO edit). Each emitted [`TextEdit`] replaces only the bytes between the
/// `path="…"` quotes (the parser's [`path_span`](gd_project::scene::ExtResource::path_span)) with a
/// target's new `res://`, matched by RESOLVED IDENTITY (never the `res://` string).
fn tscn_ext_resource_edits(
    state: &ServerState,
    text: &str,
    targets: &[RenameTarget],
) -> Option<Vec<TextEdit>> {
    let scene = gd_project::scene::parse_scene(text);
    let rope = ropey::Rope::from_str(text);
    let mapper = PositionMapper::new(&rope, state.encoding);
    let bytes = text.as_bytes();

    // Collect (byte-span, new_res) for every ext_resource whose path resolves to a renamed target.
    let mut raw_edits: Vec<(usize, usize, String)> = Vec::new();
    for ext in scene.ext_resources.values() {
        let Some(path) = ext.path.as_deref() else {
            continue;
        };
        // POSITIVE RESOLUTION by identity, never by string (the prefix trap). An `ext_resource` path
        // that doesn't resolve, or resolves to some OTHER file, is left untouched.
        let Some(identity) = ResIdentity::resolve(&state.workspace.index, path) else {
            continue;
        };
        let Some(target) = targets.iter().find(|t| t.old_identity == identity) else {
            continue;
        };
        // EXACT SPAN: the parser surfaces the byte span of the value between the quotes. A `None`
        // span (not a plain double-quoted string) is refused — never guess the span.
        let Some((start, end)) = ext.path_span else {
            continue;
        };
        // Defensive: the span must be in range and its raw bytes must still equal the parsed path
        // (it always does for a fresh parse, but verify before trusting it as an edit target).
        if end > bytes.len() || start > end || std::str::from_utf8(&bytes[start..end]) != Ok(path) {
            continue;
        }
        raw_edits.push((start, end, target.new_res.clone()));
    }
    if raw_edits.is_empty() {
        return None;
    }

    // VERIFY by apply→reparse-by-identity (NOT string match): apply every edit to a copy of the text
    // (descending offset order so earlier spans stay valid), reparse the result, and confirm every
    // rewritten ext_resource now resolves to its target's NEW identity and the OLD identity is gone.
    // A scene that fails this is dropped wholesale — no partial / corrupting edit ships.
    let mut applied = text.to_string();
    let mut descending = raw_edits.clone();
    descending.sort_by_key(|e| std::cmp::Reverse(e.0));
    for (start, end, new_res) in &descending {
        applied.replace_range(start..end, new_res);
    }
    if !tscn_rewrite_verified(state, &applied, targets) {
        log::warn!(
            "willRenameFiles: refusing a .tscn ext_resource rewrite — the reparsed scene did not \
             resolve to the new identity; leaving the scene untouched"
        );
        return None;
    }

    // Passed verification: turn each raw byte span into an LSP TextEdit over the inner-quote bytes.
    let edits = raw_edits
        .into_iter()
        .map(|(start, end, new_res)| TextEdit {
            range: mapper.span_to_range(gd_syntax::ByteSpan::new(start, end)),
            new_text: new_res,
        })
        .collect();
    Some(edits)
}

/// Verify a post-rewrite `.tscn` text: NO `ext_resource` path may still resolve to a renamed target's
/// OLD identity. Identity-based — never a `res://` string compare — so a prefix neighbour can't
/// masquerade as a survivor. We do NOT assert the NEW path re-resolves: the renamed file is not yet on
/// disk at willRename time, so the new path resolves to `None` by design — "the old identity is gone"
/// is the correct, sufficient invariant (the new text's well-formedness was proven at offer time by
/// `is_safe_rewrite_target`). A future change MUST NOT add a NEW-resolves assertion here — it would
/// fail every legitimate rewrite (the file the new path names does not exist until the client moves it).
fn tscn_rewrite_verified(state: &ServerState, text: &str, targets: &[RenameTarget]) -> bool {
    let scene = gd_project::scene::parse_scene(text);
    let index = &state.workspace.index;
    // No ext_resource may still resolve to a renamed OLD identity.
    for ext in scene.ext_resources.values() {
        if let Some(path) = ext.path.as_deref() {
            if let Some(identity) = ResIdentity::resolve(index, path) {
                if targets.iter().any(|t| t.old_identity == identity) {
                    return false; // an old reference survived → the rewrite was incomplete
                }
            }
        }
    }
    true
}

/// Emit `window/showMessage(Warning)`s naming the scenes whose `ext_resource path="res://old"` is
/// left DANGLING by a refused move — the never-lie backstop for the residual fail-closed cases. Two
/// dangle kinds, each with its own message (a sub-scene→sub-scene dangle cannot be described by the
/// script-attachment text):
///   - SCRIPT ATTACHMENT — a renamed `.gd` (`renamed_gd_old_res`) attached by scenes via
///     `scenes_attaching_script`;
///   - SUB-SCENE INSTANCE (#229) — a renamed `.tscn` (`renamed_tscn_old_res`) instanced by parent
///     scenes via `scenes_instancing`.
///
/// A scene present in `rewritten_scenes` is no longer dangling (its `ext_resource` is in the returned
/// WorkspaceEdit), so it is excluded from both groups; only scenes we refused to rewrite (unsafe
/// span/text/verify) warn. No-op when every referencing scene was rewritten (or none exists).
fn warn_dangling_scene_references(
    state: &ServerState,
    renamed_gd_old_res: &[String],
    renamed_tscn_old_res: &[String],
    rewritten_scenes: &FxHashSet<String>,
) {
    // Collect the unrewritten referencing scenes for one dangle kind. Excludes scenes we rewrote,
    // compared on the normalized res spelling the rewrite recorded (so a `\`/`/` or `res://./`
    // variant can't slip past). `referencing` yields the scenes pointing at each renamed old-res.
    let collect = |old_set: &[String], referencing: &dyn Fn(&str) -> Vec<String>| -> Vec<String> {
        let mut affected: Vec<String> = Vec::new();
        for old_res in old_set {
            for scene_res in referencing(old_res) {
                if rewritten_scenes.contains(&gd_project::scene::normalize_res(&scene_res)) {
                    continue;
                }
                if !affected.iter().any(|s| s == &scene_res) {
                    affected.push(scene_res);
                }
            }
        }
        affected.sort(); // deterministic message
        affected
    };

    let scenes = state.workspace.scenes();
    let attach = collect(renamed_gd_old_res, &|old| {
        scenes
            .scenes_attaching_script(old)
            .map(str::to_owned)
            .collect()
    });
    if !attach.is_empty() {
        let list = attach.join(", ");
        show_message(
            state,
            MessageType::WARNING,
            &format!(
                "gdls: moving a script attached to {} will leave the scene's `ext_resource` path \
                 dangling — gdls could not safely rewrite the scene file(s); update them manually: {list}",
                if attach.len() == 1 { "a scene" } else { "scenes" },
            ),
        );
    }

    let instance = collect(renamed_tscn_old_res, &|old| {
        scenes.scenes_instancing(old).map(str::to_owned).collect()
    });
    if !instance.is_empty() {
        let list = instance.join(", ");
        show_message(
            state,
            MessageType::WARNING,
            &format!(
                "gdls: moving a sub-scene instanced by {} will leave the parent scene's \
                 `ext_resource` path dangling — gdls could not safely rewrite the scene file(s); \
                 update them manually: {list}",
                if instance.len() == 1 {
                    "another scene"
                } else {
                    "other scenes"
                },
            ),
        );
    }
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
