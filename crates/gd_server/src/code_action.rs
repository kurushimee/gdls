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

use crate::server::ServerState;

/// The one `workspace/executeCommand` command gdls advertises — applies a `@warning_ignore`
/// suppression for a fixable diagnostic. The ONLY entry in `executeCommandProvider.commands`
/// (anti-catalog W15: never advertise a command that does not exist). Namespaced under `gdls.` so it
/// can never collide with another server's command in a multi-server session.
pub(crate) const CMD_APPLY_WARNING_IGNORE: &str = "gdls.applyWarningIgnore";

/// Every command gdls actually handles — the source of truth for both `executeCommandProvider` and
/// [`execute_command`]'s unknown-command guard, so the advertised list and the handled set can never
/// drift (anti-catalog W15).
pub(crate) const COMMANDS: &[&str] = &[CMD_APPLY_WARNING_IGNORE];

/// The code-action kinds gdls offers — exactly [`CodeActionKind::QUICKFIX`] this phase. Advertised in
/// `codeActionProvider.codeActionKinds` and used by [`only_admits_quickfix`].
pub(crate) fn offered_kinds() -> Vec<CodeActionKind> {
    vec![CodeActionKind::QUICKFIX]
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

/// `textDocument/codeAction`: for every diagnostic in the request range carrying a fixable warning
/// `code`, offer a `@warning_ignore` suppression quickfix.
///
/// Index-/parse-priced — the offer is driven by `context.diagnostics`, plus a cached shallow PARSE of
/// the buffer (never the analyzer) to resolve each annotation's enclosing-statement line
/// ([`enclosing_statement_line`]), so it is NOT in the Hard-pressure shed set. Returns `Some(vec)`
/// (possibly empty); the LSP `null` shape never applies here (an empty array is the right "no actions"
/// answer).
#[must_use]
pub fn code_action(state: &mut ServerState, params: CodeActionParams) -> CodeActionResponse {
    // `context.only` honoring: if the filter excludes `quickfix`, compute NOTHING. This is what keeps
    // the suppression action out of a `source.fixAll` sweep.
    if !only_admits_quickfix(params.context.only.as_deref()) {
        return Vec::new();
    }

    let uri = params.text_document.uri;
    let literal_support = state.caps.code_action.literal_support;
    let resolve_support = state.caps.code_action.resolve_support;

    let mut out: CodeActionResponse = Vec::new();
    for diag in &params.context.diagnostics {
        let Some(code) = fixable_warning_code(diag) else {
            continue;
        };
        // The line the annotation lands on: NOT `diag.range.start.line` (which can be a sub-
        // expression on a CONTINUATION line of a multi-line statement — inserting there would splice
        // the annotation *inside* the statement and produce invalid GDScript). Resolve the enclosing
        // statement/declaration's first line so the annotation attaches to the right node and the
        // splice can never corrupt. FAIL-CLOSED: if the line can't be positively resolved (the buffer
        // is gone, or no enclosing target node covers the diagnostic), the action is NOT offered for
        // this diagnostic — a missing quickfix is a feature gap; a corrupting edit is not (the rename
        // lesson: a mutating consumer must never offer-and-hope).
        let Some(line) = enclosing_statement_line(state, &uri, diag) else {
            continue;
        };
        let data = WarningIgnoreData::new(&uri, &code, line);
        let title = format!("Ignore \"{code}\" warning on this line");

        if !literal_support {
            // No `codeActionLiteralSupport`: the client only understands a `Command`. Route it
            // through `workspace/executeCommand` (which triggers the `workspace/applyEdit` fallback).
            // The whole payload travels in `arguments` (self-contained — executeCommand carries no
            // textDocument).
            let arguments = serde_json::to_value(&data).ok().map(|v| vec![v]);
            out.push(CodeActionOrCommand::Command(Command {
                title,
                command: CMD_APPLY_WARNING_IGNORE.to_string(),
                arguments,
            }));
            continue;
        }

        // A `CodeAction` literal. The diagnostic it resolves is echoed back so the client can group
        // the action under it. The `edit` is deferred to resolve only when the client advertised
        // `resolveSupport`; otherwise it is computed EAGERLY here.
        let (edit, action_data) = if resolve_support {
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
    out
}

/// `codeAction/resolve`: fill the deferred `edit` of a `@warning_ignore` action from its
/// self-contained `data` blob.
///
/// Index-/parse-priced (reads only the round-tripped `data` — which already carries the resolved
/// enclosing-statement line — plus that line's text for indentation; never a fresh analyze), so it is
/// NOT in the Hard-pressure shed set — mirroring `inlayHint/resolve`. An action with no `data` (an
/// eager action, or a non-gdls action) or one whose edit can't be reconstructed (the target buffer is
/// gone) is returned unchanged.
#[must_use]
pub fn code_action_resolve(state: &mut ServerState, mut action: CodeAction) -> CodeAction {
    let Some(data) = action.data.clone() else {
        return action;
    };
    let Some(parsed) = WarningIgnoreData::parse(data) else {
        return action;
    };
    if let Some(edit) = build_warning_ignore_edit(state, &parsed) {
        action.edit = Some(edit);
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

/// Hierarchical `context.only` matching: does the filter admit `quickfix`? `None` (no filter) ⇒ yes.
/// A filter admits `quickfix` iff some requested kind is `quickfix` or a prefix of it
/// (`""`/`quickfix`). `source.fixAll` does NOT admit it — which is exactly what keeps the suppression
/// action out of a fix-all sweep. The match is dotted-segment-prefix (LSP §CodeActionKind), so a bare
/// `quickfix` request admits any `quickfix.*` gdls might add later, but `quick` (not a segment
/// boundary) does not.
fn only_admits_quickfix(only: Option<&[CodeActionKind]>) -> bool {
    let Some(filter) = only else {
        return true; // No filter ⇒ everything is admitted.
    };
    let quickfix = CodeActionKind::QUICKFIX;
    let offered = quickfix.as_str();
    filter.iter().any(|requested| {
        let requested = requested.as_str();
        // `requested` admits `offered` when `offered` equals `requested` or is a dotted-segment
        // descendant of it (the empty kind admits everything).
        requested.is_empty()
            || offered == requested
            || offered
                .strip_prefix(requested)
                .is_some_and(|rest| rest.starts_with('.'))
    })
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

#[cfg(test)]
mod tests {
    use super::*;

    fn kind(s: &str) -> CodeActionKind {
        CodeActionKind::from(s.to_string())
    }

    #[test]
    fn only_none_admits_quickfix() {
        assert!(only_admits_quickfix(None));
    }

    #[test]
    fn only_quickfix_admits() {
        assert!(only_admits_quickfix(Some(&[kind("quickfix")])));
    }

    #[test]
    fn only_empty_kind_admits_everything() {
        assert!(only_admits_quickfix(Some(&[CodeActionKind::EMPTY])));
    }

    #[test]
    fn only_source_fixall_does_not_admit_quickfix() {
        // The load-bearing exclusion: a fix-all sweep must NOT pick up the suppression action.
        assert!(!only_admits_quickfix(Some(&[kind("source.fixAll")])));
        assert!(!only_admits_quickfix(Some(&[kind("source")])));
        assert!(!only_admits_quickfix(Some(&[kind("refactor")])));
    }

    #[test]
    fn only_prefix_must_be_a_segment_boundary() {
        // `quick` is a string prefix of `quickfix` but NOT a dotted-segment ancestor — must not admit.
        assert!(!only_admits_quickfix(Some(&[kind("quick")])));
    }

    #[test]
    fn only_admits_when_any_requested_kind_matches() {
        // A mixed filter that includes quickfix among others admits.
        assert!(only_admits_quickfix(Some(&[
            kind("refactor"),
            kind("quickfix")
        ])));
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
