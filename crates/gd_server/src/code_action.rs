//! M10 (#75): the `textDocument/codeAction` pipeline — `codeAction`, `codeAction/resolve`,
//! `workspace/executeCommand`, and the (server→client) `workspace/applyEdit` it triggers.
//!
//! This is the MUTATING-feature plumbing. Phase 4 stands up the whole pipeline with exactly ONE
//! low-risk action (`@warning_ignore` suppression insertion); the mutating warning quickfixes come
//! later. Because the action is mechanical (it inserts a clearly-labelled suppression annotation, it
//! never rewrites the diagnosed code) the infrastructure is proven independently of edit-correctness.
//!
//! ## How a fixable diagnostic is identified
//!
//! NOT off a round-tripped `data` blob. With `publishDiagnostics.dataSupport` absent the client never
//! echoes our `data`, so the robust signal is the diagnostic's own `code` (Godot's warning `PNAME`,
//! e.g. `UNUSED_VARIABLE`) carried in `context.diagnostics`. A diagnostic whose `code` is a known
//! warning name is `@warning_ignore`-able. The publish-time `Diagnostic.data` tag
//! ([`crate::server::warning_diagnostic_data`]) is additive enrichment for a later phase, not a
//! dependency of this request.
//!
//! ## The three independent capability gates (generic-LSP-first, #30)
//!
//!   * **`codeAction.codeActionLiteralSupport`** — absent ⇒ the response is a [`Command`] (routed
//!     through `workspace/executeCommand`, which triggers the `workspace/applyEdit` fallback);
//!     present ⇒ a `CodeAction` literal.
//!   * **`codeAction.resolveSupport`** (only meaningful when literals are supported) — present ⇒ the
//!     `CodeAction` ships WITHOUT its `edit`, carrying a self-contained `data` blob that
//!     [`code_action_resolve`] turns into the `WorkspaceEdit`; absent ⇒ the `edit` is computed
//!     EAGERLY in the `codeAction` response.
//!   * **`publishDiagnostics.dataSupport`** — gates the additive `Diagnostic.data` tag at publish
//!     time (the `codeAction`/resolve/executeCommand paths do not depend on it).
//!
//! ## `context.only` honoring
//!
//! The only kind gdls offers is [`CodeActionKind::QUICKFIX`]. A request whose `only` filter does not
//! admit `quickfix` (hierarchical prefix match — `source.fixAll` does NOT admit `quickfix`) returns
//! `[]` WITHOUT computing anything, so a `source.fixAll` sweep never picks up the suppression action.
//!
//! ## The applyEdit fallback (gdls's FIRST server→client REQUEST that expects a RESPONSE)
//!
//! `workspace/executeCommand` does NOT block on the applyEdit response (the worker is the sole
//! consumer of the response channel — blocking would deadlock). It sends `workspace/applyEdit`
//! fire-and-forget, registers the outgoing id in [`crate::server::ServerState::outbound`] as
//! [`crate::server::OutboundKind::ApplyEdit`], and answers `executeCommand` with `null` immediately.
//! The applyEdit response is correlated later by `handle_outbound_response` (accept ⇒ debug log,
//! reject ⇒ warn log; neither crashes, neither bounces — anti-catalog W3). gdls owns no buffer, so a
//! rejected edit needs no rollback. This mirrors the refresh-request pattern (semanticTokens/inlayHint
//! refresh) rather than the create pattern (which the router fully consumes).

use lsp_types::{
    CodeAction, CodeActionKind, CodeActionOrCommand, CodeActionParams, CodeActionResponse, Command,
    Diagnostic, ExecuteCommandParams, NumberOrString, Position, Range, TextEdit, Uri,
    WorkspaceEdit,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use gd_analyze::warnings::code_from_name;
use gd_syntax::ast::NodeKind;
use gd_syntax::ByteSpan;

use crate::position::PositionMapper;
use crate::server::ServerState;
use crate::uri::CanonicalKey;

/// The one `workspace/executeCommand` command gdls advertises — applies a `@warning_ignore`
/// suppression for a fixable diagnostic. The ONLY entry in `executeCommandProvider.commands`
/// (anti-catalog W15: never advertise a command that does not exist). Namespaced under `gdls.` so it
/// can never collide with another server's command in a multi-server session.
pub(crate) const CMD_APPLY_WARNING_IGNORE: &str = "gdls.applyWarningIgnore";

/// Every command gdls actually handles — the source of truth for both `executeCommandProvider` and
/// [`execute_command`]'s unknown-command guard, so the advertised list and the handled set can never
/// drift (anti-catalog W15). The mutating warning quickfixes (#75) ship their edits via `CodeAction`
/// literals / `codeAction/resolve`; only the `@warning_ignore` suppression has a `Command` fallback
/// (it is the one action a no-`codeActionLiteralSupport` client can still apply), so this list stays
/// a single entry.
pub(crate) const COMMANDS: &[&str] = &[CMD_APPLY_WARNING_IGNORE];

/// The code-action kinds gdls offers — `quickfix` (every per-diagnostic fix, suppression included)
/// and `source.fixAll` (the aggregate of the SAFE auto-applicable fixes, so `editor.codeActionsOnSave`
/// with `source.fixAll` finds them). Advertised in `codeActionProvider.codeActionKinds`; the request
/// matcher ([`request_wants`]) uses `context.only` to decide which family to compute.
pub(crate) fn offered_kinds() -> Vec<CodeActionKind> {
    vec![CodeActionKind::QUICKFIX, CodeActionKind::SOURCE_FIX_ALL]
}

/// The self-contained payload identifying a `@warning_ignore` action across the
/// `codeAction`→`codeAction/resolve` round-trip AND the `executeCommand`→`applyEdit` fallback. It
/// carries the **URI** (resolve/executeCommand receive no `textDocument`), the warning `code`, and
/// the resolved 0-based `line` to insert above — everything [`build_warning_ignore_edit`] needs to
/// reconstruct the edit without re-running diagnostics. Opaque to the client (round-tripped verbatim).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct WarningIgnoreData {
    /// Discriminant for the action family, so a future action's `data` is distinguishable. Always
    /// [`Self::ACTION`] (`"warning_ignore"`) here; checked on read ([`WarningIgnoreData::parse`]) so a
    /// later action family's `data` can never be mis-decoded as this one.
    action: String,
    /// The document URI (the action needs it; resolve/executeCommand carry no `textDocument`).
    uri: String,
    /// Godot's warning `PNAME` (e.g. `UNUSED_VARIABLE`) — inserted verbatim into the annotation. The
    /// analyzer upper-cases an annotation arg before lookup, and this is already the upper-case
    /// `PNAME`, so the inserted `@warning_ignore("<code>")` round-trips and actually suppresses.
    code: String,
    /// The 0-based line the suppression annotation is inserted ABOVE — the first line of the
    /// enclosing statement/declaration (NOT the raw diagnostic-range start line, which can be a
    /// continuation line of a multi-line statement; see [`enclosing_statement_line`]). The annotation
    /// is inserted at column 0 of this line, copying its leading indentation.
    line: u32,
}

impl WarningIgnoreData {
    /// The `action` discriminant value for this family.
    const ACTION: &'static str = "warning_ignore";

    fn new(uri: &Uri, code: &str, line: u32) -> Self {
        WarningIgnoreData {
            action: Self::ACTION.to_string(),
            uri: uri.as_str().to_string(),
            code: code.to_string(),
            line,
        }
    }

    /// Deserialize a `data` / argument blob into a `WarningIgnoreData`, REJECTING a blob whose
    /// `action` discriminant isn't ours — so a future action family's `data` (round-tripped through
    /// the same `codeAction/resolve` / `executeCommand` channel) can never be silently mis-decoded as
    /// a `@warning_ignore` action. `None` on a parse failure or a foreign discriminant.
    fn parse(value: Value) -> Option<Self> {
        let parsed: WarningIgnoreData = serde_json::from_value(value).ok()?;
        (parsed.action == Self::ACTION).then_some(parsed)
    }
}

// =================================================================================================
// Mutating warning quickfixes (#75): the resolve-data recipes. Each is self-contained (resolve /
// executeCommand carry no `textDocument`) and discriminated by `action` so one family's blob can
// never be mis-decoded as another's. The EDIT is rebuilt at resolve from the recipe + the CURRENT
// buffer (never a frozen edit), so a buffer that changed between offer and resolve re-runs the same
// refuse-gate rather than applying a stale edit.
// =================================================================================================

/// Recipe for the `_`-prefix fix (UNUSED_VARIABLE / UNUSED_PARAMETER): rename the unused local/param
/// binding to `_`+name. Carries the URI + the 0-based [`Position`] of the binding's DECLARATION
/// identifier — the cursor [`crate::handlers::rename`] is driven from, so the edit is the binding-
/// correct rename set (declaration + every write occurrence, attribute-position idents excluded),
/// never a raw by-name scan. An internal builder input only (the edit is carried eagerly, never
/// round-tripped through `data`).
#[derive(Debug, Clone)]
struct UnderscorePrefixData {
    uri: String,
    /// 0-based line of the declaration identifier (the rename cursor anchor).
    line: u32,
    /// 0-based character of the declaration identifier, in the negotiated encoding.
    character: u32,
}

impl UnderscorePrefixData {
    fn new(uri: &Uri, anchor: Position) -> Self {
        UnderscorePrefixData {
            uri: uri.as_str().to_string(),
            line: anchor.line,
            character: anchor.character,
        }
    }

    fn anchor(&self) -> Position {
        Position {
            line: self.line,
            character: self.character,
        }
    }
}

/// Recipe for the `@onready` insertion (GET_NODE_DEFAULT_WITHOUT_ONREADY): splice `@onready` on its
/// own line above the `var` declaration. Carries the URI + the 0-based line of the declaration (the
/// col-0 insertion line, indentation copied from it) — the same proven-safe splice shape the
/// `@warning_ignore` action uses. An internal builder input only (the edit is carried eagerly).
#[derive(Debug, Clone)]
struct AddOnreadyData {
    uri: String,
    /// 0-based line of the `var` declaration the `@onready` is inserted above.
    line: u32,
}

impl AddOnreadyData {
    fn new(uri: &Uri, line: u32) -> Self {
        AddOnreadyData {
            uri: uri.as_str().to_string(),
            line,
        }
    }
}

/// Recipe for a drop-annotation fix (ONREADY_WITH_EXPORT, two directions): delete exactly the target
/// annotation's byte range plus its trailing separator. Carries the URI + the BYTE range to delete —
/// resolved at offer time over the declaration's annotation list so it spans only the dropped
/// annotation (never the sibling it must keep) and absorbs the inter-annotation whitespace/newline.
/// The byte range is converted to an LSP [`Range`] at edit-build time. An internal builder input only
/// (the edit is built + gated + carried eagerly at offer time, never round-tripped).
#[derive(Debug, Clone)]
struct DropAnnotationData {
    uri: String,
    /// Inclusive start byte of the deletion (the `@` of the dropped annotation).
    delete_start: usize,
    /// Exclusive end byte of the deletion (the start of the next annotation / the `var` keyword).
    delete_end: usize,
}

impl DropAnnotationData {
    fn new(uri: &Uri, span: ByteSpan) -> Self {
        DropAnnotationData {
            uri: uri.as_str().to_string(),
            delete_start: span.start,
            delete_end: span.end,
        }
    }
}

/// Append every MUTATING warning quickfix applicable to `diag` to `out`. Dispatches on the warning
/// `code`; each builder is FAIL-CLOSED (offers nothing when the fix can't be applied safely — the
/// rename lesson). The fixes:
///   * UNUSED_VARIABLE / UNUSED_PARAMETER → `_`-prefix rename ([`push_underscore_prefix_action`]).
///     UNUSED_PRIVATE_CLASS_VARIABLE is deliberately NOT handled: that warning fires *because* the
///     var is already `_`-prefixed and unused, so a second `_` (`__x`) still warns — the fix
///     wouldn't clear its own diagnostic (see issue tracker).
///   * GET_NODE_DEFAULT_WITHOUT_ONREADY → `@onready` insertion ([`push_add_onready_action`]).
///   * ONREADY_WITH_EXPORT → the two drop-annotation directions ([`push_drop_annotation_actions`]).
fn push_mutating_actions(
    state: &mut ServerState,
    uri: &Uri,
    diag: &Diagnostic,
    out: &mut CodeActionResponse,
) {
    // Only literal-support clients get the mutating fixes: they ship their (eagerly-built, gated)
    // edit via a `CodeAction`. A no-literal client gets only the `@warning_ignore` Command fallback
    // (the one action expressible as a bare command); offering a mutating fix it can't carry an edit
    // for would be a dead lightbulb.
    if !state.caps.code_action.literal_support {
        return;
    }
    let Some(code) = diag_warning_code(diag) else {
        return;
    };
    match code.as_str() {
        "UNUSED_VARIABLE" | "UNUSED_PARAMETER" => {
            push_underscore_prefix_action(state, uri, diag, out);
        }
        "GET_NODE_DEFAULT_WITHOUT_ONREADY" => {
            push_add_onready_action(state, uri, diag, out);
        }
        "ONREADY_WITH_EXPORT" => {
            push_drop_annotation_actions(state, uri, diag, out);
        }
        _ => {}
    }
}

/// `textDocument/codeAction`: offer the quickfixes for each diagnostic in the request range and/or
/// the `source.fixAll` aggregate, gated on `context.only`.
///
/// Two families, each honoring `context.only` (so a `source.fixAll`-on-save sweep gets ONLY the
/// aggregate, and a `quickfix`-triggered lightbulb gets the per-diagnostic fixes):
///
///   * **`quickfix`** (per diagnostic): the `@warning_ignore` suppression (always, for any
///     suppressible warning) PLUS the MUTATING warning fixes — `_`-prefix for an unused local/param,
///     `@onready` insertion, and the two drop-annotation directions. Every mutating fix is computed
///     through a refuse-gate that produces NO action when the fix would corrupt or re-induce a
///     warning (the rename lesson: a mutating consumer never offer-and-hope).
///   * **`source.fixAll`** (aggregate): one [`CodeActionKind::SOURCE_FIX_ALL`] action bundling only
///     the DETERMINISTIC, auto-applicable fixes (`_`-prefix + `@onready`) — never the suppression
///     (it's a suppression, not a fix) and never the drop-annotation directions (two valid choices,
///     no canonical one). See [`build_fix_all`].
///
/// Parse-/index-priced for the suppression + the annotation fixes (a cached shallow parse, never the
/// analyzer); the `_`-prefix fix reuses the rename pipeline, which IS analyzer-priced — but a mutating
/// quickfix is exactly the kind of action it's acceptable to shed under memory pressure. Returns
/// `Some(vec)` (possibly empty); the LSP `null` shape never applies here.
#[must_use]
pub fn code_action(state: &mut ServerState, params: CodeActionParams) -> CodeActionResponse {
    let only = params.context.only.as_deref();
    let want_quickfix = request_wants(only, &CodeActionKind::QUICKFIX);
    let want_fix_all = request_wants(only, &CodeActionKind::SOURCE_FIX_ALL);
    if !want_quickfix && !want_fix_all {
        // A filter that admits neither family (e.g. `refactor`, `source.organizeImports`) → compute
        // nothing. Keeps every offered action out of an unrelated sweep.
        return Vec::new();
    }

    let uri = params.text_document.uri.clone();
    let mut out: CodeActionResponse = Vec::new();

    if want_quickfix {
        for diag in &params.context.diagnostics {
            // Each builder is fail-closed: it returns no action when the fix can't be applied
            // safely. The suppression is always offered for a suppressible warning; the mutating
            // fixes are offered only when their refuse-gate passes.
            push_warning_ignore_action(state, &uri, diag, &mut out);
            push_mutating_actions(state, &uri, diag, &mut out);
        }
    }

    // `source.fixAll` is a `CodeAction` literal (it carries a multi-edit `WorkspaceEdit`), so it is
    // only meaningful to a client with `codeActionLiteralSupport` — a no-literal client can't render
    // it. (`editor.codeActionsOnSave` clients all advertise literal support.)
    if want_fix_all && state.caps.code_action.literal_support {
        if let Some(action) = build_fix_all(state, &params) {
            out.push(CodeActionOrCommand::CodeAction(action));
        }
    }

    out
}

/// Offer the `@warning_ignore` suppression for `diag` (when its `code` is a suppressible warning),
/// appending it to `out`. Factored out of [`code_action`] unchanged from the phase-4 pipeline: the
/// `Command` fallback for a client without `codeActionLiteralSupport`, else a `CodeAction` literal
/// whose `edit` is deferred to resolve (with `resolveSupport`) or eager. FAIL-CLOSED on the enclosing
/// statement line ([`enclosing_statement_line`]).
fn push_warning_ignore_action(
    state: &mut ServerState,
    uri: &Uri,
    diag: &Diagnostic,
    out: &mut CodeActionResponse,
) {
    let Some(code) = fixable_warning_code(diag) else {
        return;
    };
    // The line the annotation lands on: NOT `diag.range.start.line` (which can be a sub-expression on
    // a CONTINUATION line of a multi-line statement — inserting there would splice the annotation
    // *inside* the statement and produce invalid GDScript). Resolve the enclosing
    // statement/declaration's first line so the splice can never corrupt. FAIL-CLOSED: no positively
    // resolved line ⇒ no action.
    let Some(line) = enclosing_statement_line(state, uri, diag) else {
        return;
    };
    let data = WarningIgnoreData::new(uri, &code, line);
    let title = format!("Ignore \"{code}\" warning on this line");

    if !state.caps.code_action.literal_support {
        // No `codeActionLiteralSupport`: the client only understands a `Command`, routed through
        // `workspace/executeCommand` (→ the `workspace/applyEdit` fallback). The whole payload travels
        // in `arguments` (self-contained — executeCommand carries no textDocument).
        let arguments = serde_json::to_value(&data).ok().map(|v| vec![v]);
        out.push(CodeActionOrCommand::Command(Command {
            title,
            command: CMD_APPLY_WARNING_IGNORE.to_string(),
            arguments,
        }));
        return;
    }

    let (edit, action_data) = if state.caps.code_action.resolve_support {
        (None, serde_json::to_value(&data).ok())
    } else {
        (build_warning_ignore_edit(state, &data), None)
    };
    out.push(CodeActionOrCommand::CodeAction(CodeAction {
        title,
        kind: Some(CodeActionKind::QUICKFIX),
        diagnostics: Some(vec![diag.clone()]),
        edit,
        data: action_data,
        ..Default::default()
    }));
}

/// `codeAction/resolve`: fill the deferred `edit` of the `@warning_ignore` suppression action.
///
/// Only the suppression defers its edit (gated on `resolveSupport`); the MUTATING fixes carry their
/// gated edit EAGERLY in the `codeAction` response (so what the ERROR backstop proved safe is exactly
/// what the client applies — a re-derive-at-resolve against a buffer that changed in between could land
/// on a different valid target the offer-time gate never saw). So this only reconstructs the
/// suppression edit from its self-contained `data`; any other action (a mutating fix with an eager
/// edit and no `data`, or a foreign action) is returned unchanged.
#[must_use]
pub fn code_action_resolve(state: &mut ServerState, mut action: CodeAction) -> CodeAction {
    let Some(data) = action.data.clone() else {
        return action;
    };
    if let Some(parsed) = WarningIgnoreData::parse(data) {
        if let Some(edit) = build_warning_ignore_edit(state, &parsed) {
            action.edit = Some(edit);
        }
    }
    action
}

/// `workspace/executeCommand`: run a server command. The ONE command is
/// [`CMD_APPLY_WARNING_IGNORE`], which builds the `@warning_ignore` [`WorkspaceEdit`] and asks the
/// client to apply it via a `workspace/applyEdit` server→client request (the fallback path for a
/// client without `codeActionLiteralSupport`).
///
/// Returns `Ok(Value::Null)` (the LSP result for a command that produced no direct value) on every
/// command that ran — INCLUDING when the applyEdit could not be sent or the edit was empty (those are
/// logged, never surfaced as a request error). An UNKNOWN command returns `Err(RequestRefusal)` →
/// `Response::new_err`, never a panic (anti-catalog W15).
///
/// The applyEdit is FIRE-AND-FORGET: it is sent here, but its response arrives later on the worker's
/// own channel and is correlated by `handle_outbound_response` — blocking on it here would deadlock
/// the single-threaded worker. See the module doc.
pub fn execute_command(
    state: &mut ServerState,
    params: ExecuteCommandParams,
) -> Result<Value, crate::handlers::RequestRefusal> {
    match params.command.as_str() {
        CMD_APPLY_WARNING_IGNORE => {
            // The action payload travels in `arguments[0]`. A malformed / missing argument is a
            // client bug, but it must never panic — log + answer null (the command "ran", it just
            // had nothing valid to do).
            let Some(data) = params
                .arguments
                .first()
                .cloned()
                .and_then(WarningIgnoreData::parse)
            else {
                log::warn!(
                    "{CMD_APPLY_WARNING_IGNORE}: missing or malformed argument; ignoring (no edit \
                     applied)"
                );
                return Ok(Value::Null);
            };
            match build_warning_ignore_edit(state, &data) {
                Some(edit) => send_apply_edit(state, &data.code, edit),
                None => log::warn!(
                    "{CMD_APPLY_WARNING_IGNORE}: could not build the edit (buffer {} gone?); no \
                     applyEdit sent",
                    data.uri
                ),
            }
            Ok(Value::Null)
        }
        other => Err(crate::handlers::RequestRefusal::unknown_command(format!(
            "unknown command: {other:?} (gdls advertises only {COMMANDS:?})"
        ))),
    }
}

/// Send a `workspace/applyEdit` server→client request (fire-and-forget) and register its id so
/// [`crate::server::handle_outbound_response`] can correlate the client's accept/reject reply. The
/// label is the action title, which the client surfaces on its undo stack.
fn send_apply_edit(state: &mut ServerState, code: &str, edit: WorkspaceEdit) {
    use lsp_server::{Message, Request};
    use lsp_types::ApplyWorkspaceEditParams;

    let id = state.shared.next_outgoing_id();
    state
        .outbound
        .insert(id.clone(), crate::server::OutboundKind::ApplyEdit);
    let params = ApplyWorkspaceEditParams {
        label: Some(format!("Ignore \"{code}\" warning")),
        edit,
    };
    let req = Request {
        id,
        method: "workspace/applyEdit".to_string(),
        params: serde_json::to_value(params)
            .expect("invariant: ApplyWorkspaceEditParams always serializes"),
    };
    if state.sender.send(Message::Request(req)).is_err() {
        log::warn!("workspace/applyEdit send failed (client disconnected?)");
    }
}

/// Whether a diagnostic is `@warning_ignore`-able: its `code` is a known Godot warning `PNAME`. The
/// robust fixability signal — driven off `Diagnostic.code` (always present on a warning), NOT off a
/// round-tripped `data` blob (which a client without `publishDiagnostics.dataSupport` never echoes).
/// Returns the upper-case `PNAME` to embed verbatim in the annotation. A bare-error diagnostic
/// (`code == "error"`) or a non-warning code yields `None` (no suppression offered).
fn fixable_warning_code(diag: &Diagnostic) -> Option<String> {
    let code = match diag.code.as_ref()? {
        NumberOrString::String(s) => s.as_str(),
        NumberOrString::Number(_) => return None,
    };
    // `code_from_name` is case-sensitive on the upper-case PNAMEs; the diagnostic's code already IS
    // the PNAME, so a successful lookup means it is a real, suppressible warning.
    code_from_name(code).map(|_| code.to_string())
}

/// Resolve the 0-based line the `@warning_ignore` annotation must be inserted ABOVE: the first line
/// of the smallest enclosing statement/declaration node covering the diagnostic's anchor.
///
/// Why not `diag.range.start.line` directly: a warning's anchor can be a sub-expression on a
/// CONTINUATION line of a multi-line statement (a parameter on a wrapped `func` signature, a
/// sub-expression inside a parenthesized initializer). Inserting `@warning_ignore(...)` at column 0
/// of *that* line splices it INSIDE the statement — invalid GDScript. Walking up to the enclosing
/// statement (mirroring the analyzer's `@warning_ignore` *target* model in
/// `gd_analyze::context::build_warning_ignored_lines`) gives a line where a column-0 insertion is
/// always syntactically safe AND whose ignore-span covers the original anchor, so the warning is
/// actually suppressed.
///
/// `None` (fail-closed ⇒ no action offered) when the buffer is gone or no enclosing statement can be
/// resolved (the anchor is at class scope, in a header, or no node covers it) — never a guessed line,
/// because this is a mutating feature.
fn enclosing_statement_line(state: &mut ServerState, uri: &Uri, diag: &Diagnostic) -> Option<u32> {
    use crate::position::PositionMapper;
    use crate::uri::CanonicalKey;

    let doc = state.vfs.get(uri.as_str())?;
    let text = doc.text();
    let mapper = PositionMapper::new(&doc.rope, state.encoding);
    let byte = mapper.position_to_byte(diag.range.start);

    let parsed = state.workspace.parse(&CanonicalKey::for_uri(uri), &text);
    let tree = &parsed.tree;

    // Innermost node at the anchor, then walk up via strictly-containing ancestors (the same step
    // selectionRange/completion use). Stop at `cur` when it is either an `@warning_ignore`-target
    // node OR a direct statement of a `Suite` (its strict-container is the block) — the latter
    // catches a BARE EXPRESSION-STATEMENT (`a == 1`, a standalone ternary): there is no
    // expression-statement wrapper node, so the statement IS the expression node, which is not a
    // target kind; without this stop the walk would over-shoot to the enclosing function and the
    // annotation would land on the signature (whose ignore-span doesn't cover the body line — valid
    // GDScript but the warning wouldn't be suppressed). Godot's `_ => {}` arm covers such a target
    // through its own `loc.end.line`, so the expression line is suppressed.
    let mut cur = tree.innermost_node_at(byte)?;
    let mut guard = tree.len();
    loop {
        if is_ignore_target(&tree.get(cur).kind) {
            // The node's byte-span start → 0-based line (clamp-don't-lie via the mapper).
            return Some(mapper.byte_to_position(tree.get(cur).span.start).line);
        }
        let parent = crate::completion_context::smallest_node_strictly_containing(tree, cur)?;
        if matches!(tree.get(parent).kind, gd_syntax::ast::NodeKind::Suite(_)) {
            // `cur` is a direct block statement — a safe, span-covering insertion point.
            return Some(mapper.byte_to_position(tree.get(cur).span.start).line);
        }
        cur = parent;
        guard = guard.saturating_sub(1);
        if guard == 0 {
            return None; // Malformed-tree span cycle guard — refuse rather than spin.
        }
    }
}

/// Whether a node kind is an `@warning_ignore` *target* — a statement or declaration the parser
/// attaches an annotation to, and therefore a syntactically-safe place to insert the suppression
/// above. Mirrors the annotation-owner kinds in
/// `gd_analyze::context::build_warning_ignored_lines` (the declaration kinds it special-cases) plus
/// the statement kinds that fall through its `_ => {}` (`return`/`assert`/assignment/call). Anything
/// not in this set is a sub-expression / sub-component (an `Identifier`, `Type`, `Parameter`,
/// `Pattern`, a `BinaryOp` operand, …) the walk steps past.
fn is_ignore_target(kind: &gd_syntax::ast::NodeKind) -> bool {
    use gd_syntax::ast::NodeKind::{
        Assert, Assignment, Call, Class, Constant, Enum, For, Function, If, Match, MatchBranch,
        Return, Signal, Variable, While,
    };
    matches!(
        kind,
        Variable(_)
            | Constant(_)
            | Function(_)
            | Signal(_)
            | Enum(_)
            | Class(_)
            | For(_)
            | If(_)
            | While(_)
            | Match(_)
            | MatchBranch(_)
            | Return(_)
            | Assert(_)
            | Assignment(_)
            | Call(_)
    )
}

/// Hierarchical `context.only` matching: does the filter admit the `offered` kind? `None` (no filter)
/// ⇒ yes. A filter admits `offered` iff some requested kind equals it or is a dotted-segment ANCESTOR
/// of it (the empty kind admits everything). The dotted-segment rule (LSP §CodeActionKind) means a
/// `source` request admits `source.fixAll`, a bare `quickfix` request admits any `quickfix.*`, but
/// `quick` (not a segment boundary) admits neither. This is the load-bearing family separation:
/// `source.fixAll` does NOT admit `quickfix` (so a fix-all sweep never picks up the per-diagnostic
/// suppression / mutating fixes), and `quickfix` does NOT admit `source.fixAll`.
fn request_wants(only: Option<&[CodeActionKind]>, offered: &CodeActionKind) -> bool {
    let Some(filter) = only else {
        return true; // No filter ⇒ everything is admitted.
    };
    let offered = offered.as_str();
    filter.iter().any(|requested| {
        let requested = requested.as_str();
        requested.is_empty()
            || offered == requested
            || offered
                .strip_prefix(requested)
                .is_some_and(|rest| rest.starts_with('.'))
    })
}

// =================================================================================================
// Fix 1: `_`-prefix for UNUSED_VARIABLE / UNUSED_PARAMETER (a SCOPED RENAME).
// =================================================================================================

/// Offer the `_`-prefix fix for an unused local/parameter, appending it to `out` when (and only when)
/// it can be applied SAFELY.
///
/// CRITICAL — an "unused" (never-read) local may still be ASSIGNED (`var x = 1; x = 2`), so renaming
/// only the declaration would leave `x = 2` dangling. The complete binding occurrence set is rewritten
/// by REUSING [`crate::handlers::rename`] — the six-round-hardened, binding-correct local resolution
/// (declaration-anchored, attribute-position idents like the `.x` of `self.x` excluded, shadowing
/// boundaries respected, name collisions refused). We feed rename a synthetic cursor at the binding's
/// DECLARATION identifier and `new_name = "_" + old`:
///   * `Ok(Some(edit))` ⇒ a safe, complete rewrite ⇒ OFFER it.
///   * `Err(_)` (a `_name` collision with an existing in-scope symbol, an invalid name, a non-editable
///     target) ⇒ REFUSE (no action) — a colliding rename would corrupt.
///   * `Ok(None)` (cursor didn't land on a renameable identifier) ⇒ REFUSE.
///
/// The refuse decision runs HERE, at offer time. The gated edit is carried EAGERLY in the action (not
/// deferred to resolve) — so what the backstop proved safe is exactly what the client applies. A
/// deferred edit, re-derived at resolve against a buffer that changed in between, could land the rename
/// on a different (but valid) binding the offer-time gate never saw (the stale-resolve corruption the
/// adversarial review found); carrying the edit and relying on the `documentChanges` version stamp for
/// apply-time staleness closes that by construction.
fn push_underscore_prefix_action(
    state: &mut ServerState,
    uri: &Uri,
    diag: &Diagnostic,
    out: &mut CodeActionResponse,
) {
    let Some(anchor) = underscore_decl_anchor(state, uri, diag) else {
        return;
    };
    let data = UnderscorePrefixData::new(uri, anchor);
    // Run the rename now: rename's own collision/native gates refuse a colliding `_name` here, and a
    // refusal / empty result ⇒ no action.
    let Some(edit) = build_underscore_prefix_edit(state, &data) else {
        return;
    };
    // ERROR BACKSTOP: refuse if applying the rename would introduce a new error (e.g. the unused
    // local's name collides with a forward-referenced member/parameter/global, so renaming a use that
    // actually binds elsewhere would dangle). Catches the open-ended induction the structural gates
    // can't enumerate.
    if !edit_is_safe(state, uri, &edit) {
        return;
    }
    out.push(CodeActionOrCommand::CodeAction(CodeAction {
        title: "Prefix unused name with \"_\"".to_string(),
        kind: Some(CodeActionKind::QUICKFIX),
        diagnostics: Some(vec![diag.clone()]),
        edit: Some(edit),
        ..Default::default()
    }));
}

/// The 0-based [`Position`] of the unused binding's DECLARATION identifier — the cursor the rename is
/// driven from. For UNUSED_PARAMETER / UNUSED_PRIVATE_CLASS_VARIABLE the analyzer already anchors the
/// diagnostic on the identifier, so `diag.range.start` is it. For UNUSED_VARIABLE the diagnostic spans
/// the whole `var x = …` statement, so we walk the enclosing `Variable` node at the anchor and take
/// its `identifier` span start — NEVER `diag.range.start` (the `var` keyword), which would put the
/// rename cursor on a keyword and resolve nothing. `None` (fail-closed) when the buffer is gone or no
/// declaration identifier can be located.
fn underscore_decl_anchor(
    state: &mut ServerState,
    uri: &Uri,
    diag: &Diagnostic,
) -> Option<Position> {
    let doc = state.vfs.get(uri.as_str())?;
    let text = doc.text();
    let mapper = PositionMapper::new(&doc.rope, state.encoding);
    let byte = mapper.position_to_byte(diag.range.start);

    let parsed = state.workspace.parse(&CanonicalKey::for_uri(uri), &text);
    let tree = &parsed.tree;

    // UNUSED_PARAMETER anchors directly on the identifier; if the anchor node IS the identifier we use
    // it as-is. Otherwise (UNUSED_VARIABLE on the whole statement) walk up to the enclosing Variable
    // and take its identifier.
    let node_id = tree.innermost_node_at(byte)?;
    if matches!(tree.get(node_id).kind, NodeKind::Identifier(_)) {
        return Some(mapper.byte_to_position(tree.get(node_id).span.start));
    }
    // Walk up to the enclosing Variable/Parameter and read its identifier child.
    let mut cur = node_id;
    let mut guard = tree.len();
    loop {
        let ident = match &tree.get(cur).kind {
            NodeKind::Variable(v) => v.identifier,
            NodeKind::Parameter(p) => p.identifier,
            _ => None,
        };
        if let Some(iid) = ident {
            return Some(mapper.byte_to_position(tree.get(iid).span.start));
        }
        cur = crate::completion_context::smallest_node_strictly_containing(tree, cur)?;
        guard = guard.saturating_sub(1);
        if guard == 0 {
            return None; // Malformed-tree cycle guard.
        }
    }
}

/// Build the `_`-prefix rename edit by calling [`crate::handlers::rename`] against the CURRENT buffer
/// with a cursor at the declaration anchor and `new_name = "_" + old`. Returns the binding-correct
/// [`WorkspaceEdit`] on success, or `None` when rename refuses / returns nothing (collision, invalid
/// name, buffer gone) — the refuse-gate.
fn build_underscore_prefix_edit(
    state: &mut ServerState,
    data: &UnderscorePrefixData,
) -> Option<WorkspaceEdit> {
    use lsp_types::{
        RenameParams, TextDocumentIdentifier, TextDocumentPositionParams, WorkDoneProgressParams,
    };
    let uri: Uri = data.uri.parse().ok()?;
    // Read the declaration name at the anchor so `new_name` is exactly `_` + that name. If the anchor
    // doesn't land on an identifier, bail — fail-closed. (The edit is built + gated + carried eagerly
    // at offer time, so this always runs against the same buffer the action was offered for.)
    let old_name = identifier_name_at(state, &uri, data.anchor())?;
    let new_name = format!("_{old_name}");
    let params = RenameParams {
        text_document_position: TextDocumentPositionParams {
            text_document: TextDocumentIdentifier { uri },
            position: data.anchor(),
        },
        new_name,
        work_done_progress_params: WorkDoneProgressParams::default(),
    };
    // `rename` is the binding-correct, collision-refusing pipeline. `Err` (refusal) or `Ok(None)` ⇒
    // no edit. An `Ok(Some(empty))` can't happen here (new != old since we prepend `_`).
    match crate::handlers::rename(state, params) {
        Ok(Some(edit)) => {
            // Guard against a vacuous edit (no occurrences resolved): an empty edit is not a fix.
            if workspace_edit_is_empty(&edit) {
                None
            } else {
                Some(edit)
            }
        }
        _ => None,
    }
}

/// The identifier name at `pos` in `uri`'s current buffer, or `None` if `pos` doesn't land on an
/// identifier. Used to derive `new_name = "_" + old` for the `_`-prefix rename from the live buffer.
fn identifier_name_at(state: &mut ServerState, uri: &Uri, pos: Position) -> Option<String> {
    let doc = state.vfs.get(uri.as_str())?;
    let text = doc.text();
    let mapper = PositionMapper::new(&doc.rope, state.encoding);
    let byte = mapper.position_to_byte(pos);
    let parsed = state.workspace.parse(&CanonicalKey::for_uri(uri), &text);
    let id = parsed.tree.innermost_node_at(byte)?;
    match &parsed.tree.get(id).kind {
        NodeKind::Identifier(i) => Some(i.name.clone()),
        _ => None,
    }
}

// =================================================================================================
// Fix 2: add `@onready` for GET_NODE_DEFAULT_WITHOUT_ONREADY.
// =================================================================================================

/// Offer the `@onready` insertion for a `var x = $Node` / `get_node(...)` default, appending it to
/// `out` when safe.
///
/// Two gates:
///   * structural fast-path: adding `@onready` to a var that ALSO carries an `@export*` annotation
///     would induce ONREADY_WITH_EXPORT — refuse without paying an analyze (the common case).
///   * the ERROR BACKSTOP: `@onready` on a NON-Node-derived class is a hard error
///     (`"@onready" can only be used in classes that inherit "Node"`), and GET_NODE_DEFAULT fires for
///     a bare `get_node(...)` call regardless of the class's node-ness — so the non-Node induction IS
///     reachable (`extends Object; var n = get_node("Child")`). [`edit_is_safe`] catches it (and any
///     other induced error, e.g. a property var) by re-analyzing the patched buffer.
fn push_add_onready_action(
    state: &mut ServerState,
    uri: &Uri,
    diag: &Diagnostic,
    out: &mut CodeActionResponse,
) {
    let Some((var_line, has_export, _onready, _export_span)) =
        enclosing_variable_facts(state, uri, diag)
    else {
        return;
    };
    if has_export {
        // Would induce ONREADY_WITH_EXPORT — refuse (the user can drop @export or suppress instead).
        return;
    }
    let data = AddOnreadyData::new(uri, var_line);
    let Some(edit) = build_add_onready_edit(state, &data) else {
        return;
    };
    // ERROR BACKSTOP: refuse if adding @onready would introduce an error (non-Node class, etc.).
    if !edit_is_safe(state, uri, &edit) {
        return;
    }
    // Carry the gated edit EAGERLY (not deferred) so what was proven safe is what's applied.
    out.push(CodeActionOrCommand::CodeAction(CodeAction {
        title: "Add \"@onready\" annotation".to_string(),
        kind: Some(CodeActionKind::QUICKFIX),
        diagnostics: Some(vec![diag.clone()]),
        edit: Some(edit),
        ..Default::default()
    }));
}

/// Build the `@onready` insertion: a zero-width col-0 splice of `"<indent>@onready\n"` above the `var`
/// declaration line, copying that line's leading indentation — the SAME proven-safe shape as the
/// `@warning_ignore` action (an insertion can never corrupt surrounding code). `None` when the buffer
/// is gone or the line is out of range.
fn build_add_onready_edit(state: &ServerState, data: &AddOnreadyData) -> Option<WorkspaceEdit> {
    let uri: Uri = data.uri.parse().ok()?;
    let doc = state.vfs.get(uri.as_str())?;
    let line_slice = doc.rope.get_line(data.line as usize)?;
    let indent: String = line_slice
        .to_string()
        .chars()
        .take_while(|c| *c == ' ' || *c == '\t')
        .collect();
    let new_text = format!("{indent}@onready\n");
    let at = Position {
        line: data.line,
        character: 0,
    };
    let text_edit = TextEdit {
        range: Range { start: at, end: at },
        new_text,
    };
    Some(workspace_edit_for(state, uri, text_edit))
}

// =================================================================================================
// Fix 3: drop a conflicting annotation for ONREADY_WITH_EXPORT (two directions).
// =================================================================================================

/// Offer BOTH drop-annotation directions for an `@onready`+`@export` conflict, appending the safe ones
/// to `out`.
///
/// Each direction deletes EXACTLY one annotation (its byte span + the trailing separator up to the
/// next annotation / the `var` keyword), so the sibling it must keep is never touched. Two refuse-gates
/// guard corruption:
///   * **drop `@onready`** is REFUSED when the initializer is a get_node-default form (`$`/`%`/
///     `get_node`), because removing `@onready` would then induce GET_NODE_DEFAULT_WITHOUT_ONREADY.
///     The detection REUSES the analyzer's own [`gd_analyze::resolver::get_node_default_form`] (the
///     exact emission predicate — never re-derived).
///   * **either direction** is REFUSED when a COMMENT falls inside its deletion range (comments live
///     in a side-channel invisible to the AST, so the byte-range delete would silently eat user text;
///     refuse rather than corrupt). `drop @export` is otherwise always safe (at worst it induces a
///     benign UNUSED warning — a documented limitation, not corruption).
fn push_drop_annotation_actions(
    state: &mut ServerState,
    uri: &Uri,
    diag: &Diagnostic,
    out: &mut CodeActionResponse,
) {
    // Collect the candidate (title, recipe) pairs FIRST (over the parse tree), then drop the tree
    // borrow before building + gating each edit (the backstop needs `&mut state`).
    let candidates = drop_annotation_candidates(state, uri, diag);
    for (title, data) in candidates {
        let Some(edit) = build_drop_annotation_edit(state, &data) else {
            continue;
        };
        // ERROR BACKSTOP: refuse a direction whose deletion would introduce an error (defensive — the
        // structural gates already exclude the known inductions, but a deletion that broke parsing
        // would be caught here too).
        if !edit_is_safe(state, uri, &edit) {
            continue;
        }
        // Carry the gated edit EAGERLY (not deferred): the frozen byte range is resolved + gated NOW,
        // so a buffer change before resolve can't re-interpret it against different bytes.
        out.push(CodeActionOrCommand::CodeAction(CodeAction {
            title,
            kind: Some(CodeActionKind::QUICKFIX),
            diagnostics: Some(vec![diag.clone()]),
            edit: Some(edit),
            ..Default::default()
        }));
    }
}

/// The drop-annotation candidate (title, recipe) pairs for the `@onready`+`@export` conflict at
/// `diag`, computed over the parse tree (returns OWNED data so the tree borrow is released before the
/// caller builds + gates edits). Applies the two structural refuse-gates: drop-`@onready` is omitted
/// when the initializer is a get_node-default (would re-induce GET_NODE_DEFAULT_WITHOUT_ONREADY); a
/// direction whose deletion range overlaps a comment is omitted (would eat user text). `drop @export`
/// is otherwise always a candidate.
fn drop_annotation_candidates(
    state: &mut ServerState,
    uri: &Uri,
    diag: &Diagnostic,
) -> Vec<(String, DropAnnotationData)> {
    let mut out = Vec::new();
    let Some(doc) = state.vfs.get(uri.as_str()) else {
        return out;
    };
    let text = doc.text();
    let mapper = PositionMapper::new(&doc.rope, state.encoding);
    let byte = mapper.position_to_byte(diag.range.start);
    let parsed = state.workspace.parse(&CanonicalKey::for_uri(uri), &text);
    let tree = &parsed.tree;

    let Some(var_id) = enclosing_variable(tree, byte) else {
        return out;
    };
    let var_span = tree.get(var_id).span;
    let annotations: Vec<gd_syntax::ast::NodeId> = tree.get(var_id).annotations.clone();

    // Classify the var's annotations: the FIRST @onready and the FIRST @export* (mirrors the
    // analyzer's ONREADY_WITH_EXPORT detection, which uses the first of each).
    let mut onready: Option<gd_syntax::ast::NodeId> = None;
    let mut export: Option<gd_syntax::ast::NodeId> = None;
    for &ann_id in &annotations {
        if let NodeKind::Annotation(a) = &tree.get(ann_id).kind {
            if a.name == "@onready" && onready.is_none() {
                onready = Some(ann_id);
            } else if a.name.starts_with("@export") && export.is_none() {
                export = Some(ann_id);
            }
        }
    }
    let (Some(onready_id), Some(export_id)) = (onready, export) else {
        return out; // Not actually the @onready+@export shape — nothing to offer.
    };

    // drop @onready — refuse if removing it would re-induce GET_NODE_DEFAULT_WITHOUT_ONREADY.
    let init_is_get_node = match &tree.get(var_id).kind {
        NodeKind::Variable(v) => v
            .initializer
            .and_then(|init| gd_analyze::resolver::get_node_default_form(tree, init))
            .is_some(),
        _ => false,
    };
    if !init_is_get_node {
        if let Some(span) =
            annotation_delete_span(tree, &annotations, onready_id, var_span, &parsed.comments)
        {
            out.push((
                "Remove \"@onready\" annotation".to_string(),
                DropAnnotationData::new(uri, span),
            ));
        }
    }

    // drop @export — always a candidate (no induction path; comment-in-range still refuses).
    if let Some(span) =
        annotation_delete_span(tree, &annotations, export_id, var_span, &parsed.comments)
    {
        let export_name = match &tree.get(export_id).kind {
            NodeKind::Annotation(a) => a.name.clone(),
            _ => "@export".to_string(),
        };
        out.push((
            format!("Remove \"{export_name}\" annotation"),
            DropAnnotationData::new(uri, span),
        ));
    }
    out
}

/// The BYTE range to delete to drop `target` from a declaration's annotation list: `[target.start,
/// end)` where `end` is the span START of the NEXT annotation in `annotations` (source order) if
/// `target` isn't the last, else the Variable node's span start (the `var` keyword). This absorbs the
/// inter-annotation whitespace / newline separator while deleting EXACTLY the target — the sibling
/// annotation it must keep is never in range.
///
/// Returns `None` (refuse) when a COMMENT span overlaps the candidate range — comments are an
/// AST-invisible side-channel ([`ParseResult::comments`](gd_syntax::ParseResult::comments)), so the
/// byte-range delete would silently remove user text. Refuse rather than corrupt.
fn annotation_delete_span(
    tree: &gd_syntax::ast::ParseTree,
    annotations: &[gd_syntax::ast::NodeId],
    target: gd_syntax::ast::NodeId,
    var_span: ByteSpan,
    comments: &std::collections::HashMap<u32, gd_syntax::lexer::CommentData>,
) -> Option<ByteSpan> {
    let target_span = tree.get(target).span;
    // Annotations in SOURCE order (by span start), to find the one immediately after `target`.
    let mut ordered: Vec<ByteSpan> = annotations.iter().map(|&a| tree.get(a).span).collect();
    ordered.sort_by_key(|s| s.start);
    // The next annotation strictly after `target`'s start, else the `var` keyword.
    let end = ordered
        .iter()
        .find(|s| s.start > target_span.start)
        .map(|s| s.start)
        .unwrap_or(var_span.start);
    let delete = ByteSpan {
        start: target_span.start,
        end,
    };
    // Refuse if any comment overlaps the deletion range (the [start, end) half-open interval).
    let eats_comment = comments
        .values()
        .any(|c| c.span.start < delete.end && c.span.end > delete.start);
    if eats_comment {
        return None;
    }
    Some(delete)
}

/// Build the drop-annotation edit: a deletion of `data`'s byte range, converted to the negotiated
/// [`WorkspaceEdit`] shape against the current buffer. `None` when the buffer is gone or the byte
/// range is out of bounds for the current text (the buffer changed since offer — fail-closed).
fn build_drop_annotation_edit(
    state: &ServerState,
    data: &DropAnnotationData,
) -> Option<WorkspaceEdit> {
    let uri: Uri = data.uri.parse().ok()?;
    let doc = state.vfs.get(uri.as_str())?;
    let len = doc.rope.len_bytes();
    if data.delete_start >= data.delete_end || data.delete_end > len {
        return None; // Stale / out-of-range range — refuse.
    }
    let mapper = PositionMapper::new(&doc.rope, state.encoding);
    let start = mapper.byte_to_position(data.delete_start);
    let end = mapper.byte_to_position(data.delete_end);
    let text_edit = TextEdit {
        range: Range { start, end },
        new_text: String::new(),
    };
    Some(workspace_edit_for(state, uri, text_edit))
}

// =================================================================================================
// source.fixAll: aggregate the SAFE, auto-applicable fixes.
// =================================================================================================

// =================================================================================================
// The ERROR backstop — the authoritative mutation-correctness gate.
//
// Every structural refuse-gate above (has_export, get_node-default, collision) catches a SPECIFIC
// known induction. But the induction space is open-ended (a `@onready` on a non-Node class, a local
// `_x` that rebinds a forward-referenced member/parameter/global, a property-var…), and an EDIT that
// introduces a parse/type ERROR is exactly "broken code" — the BLOCKER tier of this phase's bar. So
// before any mutating fix is OFFERED, its candidate edit is applied to an in-memory copy of the
// buffer and RE-ANALYZED; the fix is withheld if the edit introduces a NEW error-severity diagnostic.
// This is the one mechanism that closes every blocker (known and unknown) uniformly. It is scoped to
// ERRORS — a fix is allowed to shift/alter WARNINGS (a benign over-edit is a documented limitation),
// and errors don't move under a line insert the way warning ranges do, so the comparison is robust.
// =================================================================================================

/// `true` iff applying `edit` to `uri`'s current buffer would introduce NO new error-severity
/// diagnostic — the authoritative blocker-tier gate for a mutating fix. Applies the edit in memory,
/// re-parses + re-analyzes EPHEMERALLY (no cache pollution; real path so the cross-file / native
/// environment is intact), and compares the post-edit error set against the pre-edit one by MESSAGE
/// (position-independent — a line insert shifts ranges). A buffer that can't be read, an edit that
/// can't be applied, or an analysis that BAILED (partial side tables ⇒ can't trust "no error") all
/// return `false` (fail-closed: don't offer a fix we can't prove safe).
fn edit_is_safe(state: &mut ServerState, uri: &Uri, edit: &WorkspaceEdit) -> bool {
    let Some(path) = crate::uri::uri_to_path(uri) else {
        return false;
    };
    if path.extension() != Some("gd") {
        return false;
    }
    let Some(doc) = state.vfs.get(uri.as_str()) else {
        return false;
    };
    let before_text = doc.text();

    // Baseline error messages (pre-edit), from an ephemeral analysis of the CURRENT text — so a file
    // that already has errors isn't blamed on the fix. Bailed ⇒ refuse.
    let before_tree = state
        .workspace
        .parse(&CanonicalKey::for_uri(uri), &before_text);
    let before_analysis = state.workspace.analyze_ephemeral(&path, &before_tree.tree);
    if before_analysis.bailed {
        return false;
    }
    let before_errors = error_message_counts(&before_analysis);
    let before_syntax = before_tree.diagnostics.len();
    let before_shadows = shadow_code_counts(&before_analysis);

    // Apply the edit to the buffer text. `None` ⇒ the edit didn't apply cleanly ⇒ refuse.
    let Some(after_text) = apply_workspace_edit_to_text(uri, &before_text, edit, state.encoding)
    else {
        return false;
    };
    let after_parsed = gd_syntax::parse(&after_text);
    let after_analysis = state.workspace.analyze_ephemeral(&path, &after_parsed.tree);
    if after_analysis.bailed {
        return false;
    }

    // Reject if the EDIT added a syntax (parse) error — a structurally broken splice. Compare COUNTS,
    // not emptiness: a file that already had a parse error must not mask a NEW one the edit introduces.
    if after_parsed.diagnostics.len() > before_syntax {
        return false;
    }
    // No NEW analyzer error may appear — compared by MESSAGE MULTIPLICITY (position-independent: a
    // line insert shifts ranges). Counts, not a set: if `M` occurs once before and the edit makes it
    // occur twice, that SECOND `M` is a new error (the duplicate-message bypass the review found).
    let after_errors = error_message_counts(&after_analysis);
    let no_new_error = after_errors
        .iter()
        .all(|(msg, n)| before_errors.get(msg).copied().unwrap_or(0) >= *n);
    if !no_new_error {
        return false;
    }

    // No NEW shadow/confusable WARNING may appear — the SILENT-CAPTURE backstop. The `_`-prefix rename
    // could rename a binding to `_name` in a scope where `_name` already exists, silently re-binding a
    // reference to a different symbol with NO error (`var _y` member + `var y` local + `print(_y)` →
    // after `y`→`_y`, `print(_y)` reads the local). The analyzer flags that capture as a SHADOWED_* /
    // CONFUSABLE_* warning, so refuse if any such code's count rose. Compared by CODE (not message):
    // shadow messages embed line numbers, which a line insert shifts — the code string is
    // position-independent. This is the principled close for the open-ended capture-vector space
    // (member / outer-local / parameter / global / autoload) the structural collision check can't
    // enumerate. (Over-refuses a benign NON-capturing new shadow — withholding the lightbulb there is
    // a safe under-offer; see the filed limitation.)
    let after_shadows = shadow_code_counts(&after_analysis);
    after_shadows
        .iter()
        .all(|(code, n)| before_shadows.get(code).copied().unwrap_or(0) >= *n)
}

/// A multiset (message → count) of error-severity analyzer diagnostics — the backstop's
/// position-independent identity. Multiplicity matters: an edit that turns one occurrence of message
/// `M` into two has introduced a new error even though the message set is unchanged.
fn error_message_counts(
    result: &gd_analyze::AnalysisResult,
) -> std::collections::BTreeMap<String, usize> {
    let mut counts: std::collections::BTreeMap<String, usize> = std::collections::BTreeMap::new();
    for d in &result.diagnostics {
        if matches!(d.severity(), gd_analyze::Severity::Error) {
            *counts.entry(d.message().to_owned()).or_insert(0) += 1;
        }
    }
    counts
}

/// The shadow / confusable warning family — the codes a SILENT identifier CAPTURE manifests as (an
/// edit that renames a binding into a scope where the new name already exists rebinds a reference with
/// no error). The full `SHADOWED_*` / `CONFUSABLE_*` set from `gd_analyze::warnings` (enumerated, not
/// hand-picked, so a future addition is caught by re-checking this list against the source).
const SHADOW_CONFUSABLE_CODES: &[&str] = &[
    "SHADOWED_VARIABLE",
    "SHADOWED_VARIABLE_BASE_CLASS",
    "SHADOWED_GLOBAL_IDENTIFIER",
    "CONFUSABLE_IDENTIFIER",
    "CONFUSABLE_LOCAL_DECLARATION",
    "CONFUSABLE_LOCAL_USAGE",
    "CONFUSABLE_CAPTURE_REASSIGNMENT",
];

/// A multiset (warning CODE → count) of the shadow/confusable family — the silent-capture backstop's
/// position-independent identity (message text embeds shifting line numbers, the code does not).
fn shadow_code_counts(
    result: &gd_analyze::AnalysisResult,
) -> std::collections::BTreeMap<&'static str, usize> {
    let mut counts: std::collections::BTreeMap<&'static str, usize> =
        std::collections::BTreeMap::new();
    for d in &result.diagnostics {
        if let Some(code) = d.warning_code() {
            let name = gd_analyze::warnings::name_from_code(code);
            if let Some(&known) = SHADOW_CONFUSABLE_CODES.iter().find(|&&c| c == name) {
                *counts.entry(known).or_insert(0) += 1;
            }
        }
    }
    counts
}

/// Apply a single-file `WorkspaceEdit`'s [`TextEdit`]s (those targeting `uri`) to `text`, returning
/// the resulting string — the in-memory effect of a client applying the edit. Edits are converted to
/// byte offsets via a [`PositionMapper`] over the current rope and applied last-first so earlier
/// ranges stay valid. `None` if any edit's range maps out of bounds or the edits overlap (an
/// undefined edit set — the backstop refuses rather than guess).
fn apply_workspace_edit_to_text(
    uri: &Uri,
    text: &str,
    edit: &WorkspaceEdit,
    encoding: crate::position::PositionEncoding,
) -> Option<String> {
    use lsp_types::{DocumentChanges, OneOf};
    let mut tes: Vec<TextEdit> = Vec::new();
    if let Some(DocumentChanges::Edits(tdes)) = &edit.document_changes {
        for tde in tdes {
            if &tde.text_document.uri != uri {
                continue;
            }
            for e in &tde.edits {
                if let OneOf::Left(te) = e {
                    tes.push(te.clone());
                }
            }
        }
    } else if let Some(changes) = &edit.changes {
        if let Some(v) = changes.get(uri) {
            tes.extend(v.iter().cloned());
        }
    }
    if tes.is_empty() {
        return None;
    }

    let rope = ropey::Rope::from_str(text);
    // The edit ranges gdls emits are in the NEGOTIATED encoding (`encoding`), and the same mapper
    // round-trips them — so the probe applies exactly the bytes the client will edit.
    let mapper = PositionMapper::new(&rope, encoding);
    // Convert to (start_byte, end_byte, new_text), validate in-bounds + non-overlapping.
    let mut spans: Vec<(usize, usize, String)> = Vec::with_capacity(tes.len());
    let len = text.len();
    for te in &tes {
        let s = mapper.position_to_byte(te.range.start);
        let e = mapper.position_to_byte(te.range.end);
        if s > e || e > len {
            return None;
        }
        spans.push((s, e, te.new_text.clone()));
    }
    spans.sort_by_key(|(s, _, _)| *s);
    // Overlap check on the sorted spans (a zero-width insert sharing an endpoint is allowed).
    for w in spans.windows(2) {
        let (_, e0, _) = &w[0];
        let (s1, _, _) = &w[1];
        if s1 < e0 {
            return None;
        }
    }
    // Apply last-first.
    let mut out = text.to_string();
    for (s, e, nt) in spans.into_iter().rev() {
        out.replace_range(s..e, &nt);
    }
    Some(out)
}

/// Build the `source.fixAll` aggregate action: ONE [`CodeActionKind::SOURCE_FIX_ALL`] action whose
/// edit bundles only the DETERMINISTIC, auto-applicable fixes — the `_`-prefix rename and the
/// `@onready` insertion — for every fixable diagnostic in `context.diagnostics`.
///
/// DELIBERATELY EXCLUDED:
///   * the `@warning_ignore` suppression — it suppresses, it doesn't fix; never appropriate on save.
///   * the two ONREADY_WITH_EXPORT drop directions — two equally-valid choices, no canonical one, so
///     not auto-applicable.
///
/// Overlap handling: the included fixes touch disjoint regions by construction (`_`-prefix edits an
/// identifier token + its writes; `@onready` is a col-0 line insert; distinct vars insert on distinct
/// lines; UNUSED_VARIABLE is function-local while GET_NODE_DEFAULT_WITHOUT_ONREADY is a class member,
/// so they never co-occur on one statement). We still DEDUPLICATE exactly-equal edit ranges and DROP
/// any edit whose range overlaps one already collected — never blindly concatenating overlapping
/// ranges (an overlap would be an undefined `WorkspaceEdit`). Returns `None` (no action) when nothing
/// safe applies.
fn build_fix_all(state: &mut ServerState, params: &CodeActionParams) -> Option<CodeAction> {
    let uri = params.text_document.uri.clone();
    // Collect each safe fix's WorkspaceEdit, then merge their per-file TextEdits.
    let mut edits: Vec<WorkspaceEdit> = Vec::new();
    for diag in &params.context.diagnostics {
        let Some(code) = diag_warning_code(diag) else {
            continue;
        };
        // Each candidate goes through the SAME structural gates AND the ERROR BACKSTOP as the
        // interactive offer — fixAll fires silently on save, so an ungated path here would apply a
        // corrupting edit with no user interaction (the blocker class this gate closes).
        match code.as_str() {
            "UNUSED_VARIABLE" | "UNUSED_PARAMETER" => {
                if let Some(anchor) = underscore_decl_anchor(state, &uri, diag) {
                    let data = UnderscorePrefixData::new(&uri, anchor);
                    if let Some(edit) = build_underscore_prefix_edit(state, &data) {
                        if edit_is_safe(state, &uri, &edit) {
                            edits.push(edit);
                        }
                    }
                }
            }
            "GET_NODE_DEFAULT_WITHOUT_ONREADY" => {
                if let Some((var_line, has_export, _, _)) =
                    enclosing_variable_facts(state, &uri, diag)
                {
                    if !has_export {
                        let data = AddOnreadyData::new(&uri, var_line);
                        if let Some(edit) = build_add_onready_edit(state, &data) {
                            if edit_is_safe(state, &uri, &edit) {
                                edits.push(edit);
                            }
                        }
                    }
                }
            }
            _ => {}
        }
    }
    let merged = merge_workspace_edits(state, &uri, edits)?;
    // Final guard: re-analyze the AGGREGATE once (each fix is individually safe, but a combination
    // could in principle interact). Refuse the whole aggregate rather than ship a corrupting save fix.
    if !edit_is_safe(state, &uri, &merged) {
        return None;
    }
    Some(CodeAction {
        title: "Fix all auto-fixable warnings".to_string(),
        kind: Some(CodeActionKind::SOURCE_FIX_ALL),
        edit: Some(merged),
        ..Default::default()
    })
}

/// Merge per-file [`TextEdit`]s from several single-file [`WorkspaceEdit`]s (all targeting `uri`, the
/// request file) into ONE negotiated edit, dropping any edit whose range overlaps an
/// already-collected one (and de-duplicating exactly-equal ranges). `None` when no non-overlapping
/// edit survives. The fixAll fixes are same-file by construction (the `_`-prefix rename of a LOCAL is
/// function-scoped, single-file; `@onready` is in the request file), so cross-file merging isn't
/// needed here.
#[allow(clippy::mutable_key_type)]
fn merge_workspace_edits(
    state: &ServerState,
    uri: &Uri,
    edits: Vec<WorkspaceEdit>,
) -> Option<WorkspaceEdit> {
    let mut collected: Vec<TextEdit> = Vec::new();
    for edit in edits {
        for te in text_edits_for_uri(&edit, uri) {
            // Drop an edit overlapping any already-accepted one (or exactly duplicating it).
            let overlaps = collected
                .iter()
                .any(|c| ranges_overlap(&c.range, &te.range));
            if !overlaps {
                collected.push(te);
            }
        }
    }
    if collected.is_empty() {
        return None;
    }
    // Deterministic order: by start position.
    collected.sort_by_key(|te| (te.range.start.line, te.range.start.character));
    Some(workspace_edit_for_many(state, uri.clone(), collected))
}

/// Extract the [`TextEdit`]s targeting `uri` from a [`WorkspaceEdit`] in EITHER negotiated shape
/// (`documentChanges` or `changes`). Non-`uri` entries are ignored (the fixAll inputs are single-file
/// anyway). Drops the version (the merged edit re-stamps it).
#[allow(clippy::mutable_key_type)]
fn text_edits_for_uri(edit: &WorkspaceEdit, uri: &Uri) -> Vec<TextEdit> {
    use lsp_types::{DocumentChanges, OneOf};
    if let Some(DocumentChanges::Edits(tdes)) = &edit.document_changes {
        return tdes
            .iter()
            .filter(|tde| &tde.text_document.uri == uri)
            .flat_map(|tde| {
                tde.edits.iter().filter_map(|e| match e {
                    OneOf::Left(te) => Some(te.clone()),
                    OneOf::Right(_) => None, // AnnotatedTextEdit — gdls never emits these.
                })
            })
            .collect();
    }
    if let Some(changes) = &edit.changes {
        if let Some(tes) = changes.get(uri) {
            return tes.clone();
        }
    }
    Vec::new()
}

/// Two LSP [`Range`]s overlap iff their half-open intervals intersect. A zero-width insertion at a
/// position does NOT overlap an adjacent edit (it shares only an endpoint) — `start < end && end >
/// start` on the position ordering, with a zero-width range treated as a point.
fn ranges_overlap(a: &Range, b: &Range) -> bool {
    // Position ordering helper.
    let lt = |p: &Position, q: &Position| (p.line, p.character) < (q.line, q.character);
    // a and b overlap iff a.start < b.end AND b.start < a.end. For zero-width (insertion) ranges
    // (start == end) this correctly reports NO overlap with a touching edit (shared endpoint only).
    lt(&a.start, &b.end) && lt(&b.start, &a.end)
}

/// `true` when a [`WorkspaceEdit`] carries no actual text edits in either negotiated shape.
fn workspace_edit_is_empty(edit: &WorkspaceEdit) -> bool {
    use lsp_types::DocumentChanges;
    if let Some(DocumentChanges::Edits(tdes)) = &edit.document_changes {
        return tdes.iter().all(|tde| tde.edits.is_empty());
    }
    if let Some(changes) = &edit.changes {
        return changes.values().all(|v| v.is_empty());
    }
    // Neither field populated (or an operations-style documentChanges gdls never emits) → empty.
    true
}

/// Facts about the `Variable` declaration enclosing `diag`'s anchor, for the annotation fixes:
/// `(0-based var-decl line, has any @export*, has @onready, the first @export* span)`. `None` when the
/// buffer is gone or no enclosing Variable covers the anchor.
fn enclosing_variable_facts(
    state: &mut ServerState,
    uri: &Uri,
    diag: &Diagnostic,
) -> Option<(u32, bool, bool, Option<ByteSpan>)> {
    let doc = state.vfs.get(uri.as_str())?;
    let text = doc.text();
    let mapper = PositionMapper::new(&doc.rope, state.encoding);
    let byte = mapper.position_to_byte(diag.range.start);
    let parsed = state.workspace.parse(&CanonicalKey::for_uri(uri), &text);
    let tree = &parsed.tree;
    let var_id = enclosing_variable(tree, byte)?;
    let var_line = mapper.byte_to_position(tree.get(var_id).span.start).line;
    let mut has_export = false;
    let mut has_onready = false;
    let mut export_span: Option<ByteSpan> = None;
    for &ann_id in &tree.get(var_id).annotations {
        if let NodeKind::Annotation(a) = &tree.get(ann_id).kind {
            if a.name == "@onready" {
                has_onready = true;
            } else if a.name.starts_with("@export") {
                has_export = true;
                if export_span.is_none() {
                    export_span = Some(tree.get(ann_id).span);
                }
            }
        }
    }
    Some((var_line, has_export, has_onready, export_span))
}

/// The `NodeId` of the smallest `Variable` node covering `byte`, or `None`. Walks up from the
/// innermost node via strict-container ancestors (the same step the suppression line resolution uses).
fn enclosing_variable(
    tree: &gd_syntax::ast::ParseTree,
    byte: usize,
) -> Option<gd_syntax::ast::NodeId> {
    let mut cur = tree.innermost_node_at(byte)?;
    let mut guard = tree.len();
    loop {
        if matches!(tree.get(cur).kind, NodeKind::Variable(_)) {
            return Some(cur);
        }
        cur = crate::completion_context::smallest_node_strictly_containing(tree, cur)?;
        guard = guard.saturating_sub(1);
        if guard == 0 {
            return None;
        }
    }
}

/// The warning `PNAME` carried on `diag`'s `code` (a string code), or `None`. Unlike
/// [`fixable_warning_code`] this does NOT require the code to be a *known* warning — the caller
/// matches on specific names, so an unknown code simply won't match. Used by the mutating-fix and
/// fixAll dispatchers.
fn diag_warning_code(diag: &Diagnostic) -> Option<String> {
    match diag.code.as_ref()? {
        NumberOrString::String(s) => Some(s.clone()),
        NumberOrString::Number(_) => None,
    }
}

/// Build the `@warning_ignore("<code>")` insertion as a negotiated [`WorkspaceEdit`], or `None` when
/// the target buffer is gone.
///
/// The annotation is inserted at **column 0 of `data.line`** — the enclosing statement/declaration
/// line already resolved by [`enclosing_statement_line`] (so it is never a continuation line of a
/// multi-line statement) — copying that line's leading whitespace, as
/// `"<indent>@warning_ignore(\"<code>\")\n"`. Placing it directly above the enclosing statement is
/// what makes Godot's parser attach it to that statement's node (the analyzer's `@warning_ignore`
/// filter records the annotation owner's header lines, which cover the original anchor), so the
/// warning is actually suppressed. The exact leading whitespace is copied from the source (never
/// assumed to be tabs), so the indentation always matches. This stays a dumb splice — the line
/// correctness lives entirely in [`enclosing_statement_line`].
///
/// The edit is emitted in the client's negotiated shape: versioned `documentChanges` (carrying the
/// open buffer's CURRENT version) when `workspace.workspaceEdit.documentChanges` is advertised, else
/// the legacy `changes` map. A pure insertion (zero-width range) can never corrupt surrounding code.
fn build_warning_ignore_edit(
    state: &ServerState,
    data: &WarningIgnoreData,
) -> Option<WorkspaceEdit> {
    let uri: Uri = data.uri.parse().ok()?;
    let doc = state.vfs.get(uri.as_str())?;
    // The enclosing-statement line's text, to copy its leading indentation. ropey's `get_line` is
    // None for an out-of-range index (defensive — the line was resolved over this buffer's tree).
    let line_slice = doc.rope.get_line(data.line as usize)?;
    let line_text = line_slice.to_string();
    let indent: String = line_text
        .chars()
        .take_while(|c| *c == ' ' || *c == '\t')
        .collect();
    let new_text = format!("{indent}@warning_ignore(\"{}\")\n", data.code);

    // A zero-width insertion at (line, col 0): the new annotation line is spliced in ABOVE the
    // enclosing-statement line, pushing it (and everything below) down by one line.
    let at = Position {
        line: data.line,
        character: 0,
    };
    let text_edit = TextEdit {
        range: Range { start: at, end: at },
        new_text,
    };
    Some(workspace_edit_for(state, uri, text_edit))
}

/// Project a single [`TextEdit`] into the client's negotiated [`WorkspaceEdit`] shape — versioned
/// `documentChanges` (current open-buffer version, or `None` for an unopened file) when the client
/// advertised `workspace.workspaceEdit.documentChanges`, else the legacy `changes` map. Mirrors the
/// rename handler's `build_workspace_edit` convention so both mutating features emit identical shapes.
// `lsp_types::Uri` carries interior mutability (cached parsed components in a `Cell`), tripping
// `clippy::mutable_key_type` as a `HashMap` key — but `WorkspaceEdit.changes` IS keyed on `Uri` by
// the wire shape, and the key is never mutated after insertion, so the lint's hazard cannot occur.
#[allow(clippy::mutable_key_type)]
fn workspace_edit_for(state: &ServerState, uri: Uri, text_edit: TextEdit) -> WorkspaceEdit {
    use lsp_types::{
        DocumentChanges, OneOf, OptionalVersionedTextDocumentIdentifier, TextDocumentEdit,
    };

    if state.caps.workspace_edit_document_changes {
        let version = state.vfs.get(uri.as_str()).map(|d| d.version);
        WorkspaceEdit {
            document_changes: Some(DocumentChanges::Edits(vec![TextDocumentEdit {
                text_document: OptionalVersionedTextDocumentIdentifier { uri, version },
                edits: vec![OneOf::Left(text_edit)],
            }])),
            ..Default::default()
        }
    } else {
        let mut changes: std::collections::HashMap<Uri, Vec<TextEdit>> =
            std::collections::HashMap::with_capacity(1);
        changes.insert(uri, vec![text_edit]);
        WorkspaceEdit {
            changes: Some(changes),
            ..Default::default()
        }
    }
}

/// [`workspace_edit_for`] for MULTIPLE same-file [`TextEdit`]s — the `source.fixAll` aggregate shape.
/// Projects all of `text_edits` (already overlap-resolved + sorted by [`merge_workspace_edits`]) into
/// the negotiated shape: a single versioned `TextDocumentEdit` (current open-buffer version) under
/// `documentChanges`, else the legacy `changes` map. The caller guarantees the edits don't overlap, so
/// the resulting `WorkspaceEdit` is well-defined.
#[allow(clippy::mutable_key_type)]
fn workspace_edit_for_many(
    state: &ServerState,
    uri: Uri,
    text_edits: Vec<TextEdit>,
) -> WorkspaceEdit {
    use lsp_types::{
        DocumentChanges, OneOf, OptionalVersionedTextDocumentIdentifier, TextDocumentEdit,
    };
    if state.caps.workspace_edit_document_changes {
        let version = state.vfs.get(uri.as_str()).map(|d| d.version);
        WorkspaceEdit {
            document_changes: Some(DocumentChanges::Edits(vec![TextDocumentEdit {
                text_document: OptionalVersionedTextDocumentIdentifier { uri, version },
                edits: text_edits.into_iter().map(OneOf::Left).collect(),
            }])),
            ..Default::default()
        }
    } else {
        let mut changes: std::collections::HashMap<Uri, Vec<TextEdit>> =
            std::collections::HashMap::with_capacity(1);
        changes.insert(uri, text_edits);
        WorkspaceEdit {
            changes: Some(changes),
            ..Default::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kind(s: &str) -> CodeActionKind {
        CodeActionKind::from(s.to_string())
    }

    fn wants_quickfix(only: Option<&[CodeActionKind]>) -> bool {
        request_wants(only, &CodeActionKind::QUICKFIX)
    }
    fn wants_fix_all(only: Option<&[CodeActionKind]>) -> bool {
        request_wants(only, &CodeActionKind::SOURCE_FIX_ALL)
    }

    #[test]
    fn only_none_admits_quickfix() {
        assert!(wants_quickfix(None));
        assert!(wants_fix_all(None)); // No filter admits BOTH families.
    }

    #[test]
    fn only_quickfix_admits() {
        assert!(wants_quickfix(Some(&[kind("quickfix")])));
    }

    #[test]
    fn only_empty_kind_admits_everything() {
        assert!(wants_quickfix(Some(&[CodeActionKind::EMPTY])));
        assert!(wants_fix_all(Some(&[CodeActionKind::EMPTY])));
    }

    #[test]
    fn only_source_fixall_does_not_admit_quickfix() {
        // The load-bearing exclusion (both directions): a fix-all sweep must NOT pick up the
        // per-diagnostic quickfixes, and a quickfix lightbulb must NOT pick up the fixAll aggregate.
        assert!(!wants_quickfix(Some(&[kind("source.fixAll")])));
        assert!(!wants_quickfix(Some(&[kind("refactor")])));
        assert!(!wants_fix_all(Some(&[kind("quickfix")])));
    }

    #[test]
    fn only_source_admits_fix_all_but_not_quickfix() {
        // `source` is a dotted-segment ANCESTOR of `source.fixAll` (admits it) but unrelated to
        // `quickfix` (doesn't). This is how `editor.codeActionsOnSave: { "source.fixAll": true }`
        // and a bare `source` save sweep both reach the aggregate without dragging in the lightbulbs.
        assert!(wants_fix_all(Some(&[kind("source")])));
        assert!(wants_fix_all(Some(&[kind("source.fixAll")])));
        assert!(!wants_quickfix(Some(&[kind("source")])));
    }

    #[test]
    fn only_prefix_must_be_a_segment_boundary() {
        // `quick` is a string prefix of `quickfix` but NOT a dotted-segment ancestor — must not admit.
        assert!(!wants_quickfix(Some(&[kind("quick")])));
        // Likewise `source.fix` is not a segment ancestor of `source.fixAll`.
        assert!(!wants_fix_all(Some(&[kind("source.fix")])));
    }

    #[test]
    fn only_admits_when_any_requested_kind_matches() {
        // A mixed filter that includes quickfix among others admits.
        assert!(wants_quickfix(Some(&[kind("refactor"), kind("quickfix")])));
    }

    #[test]
    fn ranges_overlap_detects_intersection_not_touch() {
        let r = |sl: u32, sc: u32, el: u32, ec: u32| Range {
            start: Position {
                line: sl,
                character: sc,
            },
            end: Position {
                line: el,
                character: ec,
            },
        };
        // Overlapping spans.
        assert!(ranges_overlap(&r(0, 0, 0, 5), &r(0, 3, 0, 8)));
        // Touching at a shared endpoint — NOT an overlap.
        assert!(!ranges_overlap(&r(0, 0, 0, 5), &r(0, 5, 0, 9)));
        // A zero-width insertion at a point does NOT overlap a range starting there.
        assert!(!ranges_overlap(&r(1, 0, 1, 0), &r(1, 0, 1, 4)));
        // Disjoint.
        assert!(!ranges_overlap(&r(0, 0, 0, 2), &r(3, 0, 3, 2)));
    }

    /// A `Diagnostic` carrying just `code` — the only field `fixable_warning_code` reads.
    fn diag_with_code(code: Option<NumberOrString>) -> Diagnostic {
        Diagnostic {
            code,
            ..Default::default()
        }
    }

    #[test]
    fn ignore_target_accepts_statements_and_decls_rejects_subexpressions() {
        use gd_syntax::ast::NodeKind;
        // A representative declaration and statement are targets...
        assert!(is_ignore_target(&NodeKind::Variable(Default::default())));
        assert!(is_ignore_target(&NodeKind::Function(Default::default())));
        assert!(is_ignore_target(&NodeKind::If(Default::default())));
        assert!(is_ignore_target(&NodeKind::Return(Default::default())));
        assert!(is_ignore_target(&NodeKind::Call(Default::default())));
        // ...while a sub-expression / sub-component is NOT (the walk steps past it so the annotation
        // never lands mid-statement).
        assert!(!is_ignore_target(&NodeKind::Identifier(Default::default())));
        assert!(!is_ignore_target(&NodeKind::Subscript(Default::default())));
        assert!(!is_ignore_target(&NodeKind::Array(Default::default())));
        assert!(!is_ignore_target(&NodeKind::Type(Default::default())));
        assert!(!is_ignore_target(&NodeKind::Parameter(Default::default())));
    }

    #[test]
    fn fixable_code_recognizes_a_warning_pname() {
        let diag = diag_with_code(Some(NumberOrString::String("UNUSED_VARIABLE".to_string())));
        assert_eq!(
            fixable_warning_code(&diag).as_deref(),
            Some("UNUSED_VARIABLE")
        );
    }

    #[test]
    fn fixable_code_rejects_bare_error_and_unknown() {
        assert_eq!(
            fixable_warning_code(&diag_with_code(Some(NumberOrString::String(
                "error".to_string()
            )))),
            None,
            "bare error is not suppressible"
        );
        assert_eq!(
            fixable_warning_code(&diag_with_code(Some(NumberOrString::String(
                "NOT_A_WARNING".to_string()
            )))),
            None
        );
        assert_eq!(fixable_warning_code(&diag_with_code(None)), None);
        assert_eq!(
            fixable_warning_code(&diag_with_code(Some(NumberOrString::Number(7)))),
            None,
            "a numeric code is not a PNAME"
        );
    }
}
