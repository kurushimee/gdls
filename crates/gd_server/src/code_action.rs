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
/// the 0-based `line` of the diagnosed code — everything [`build_warning_ignore_edit`] needs to
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
    /// The 0-based line of the diagnosed code (the diagnostic range's start line). The suppression
    /// annotation is inserted at column 0 of this line, copying its leading indentation.
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
/// Index-/parse-priced — the offer is driven entirely by `context.diagnostics` (the edit, when eager,
/// reads only the current buffer's diagnosed line for indentation), so it is NOT in the Hard-pressure
/// shed set. Returns `Some(vec)` (possibly empty); the LSP `null` shape never applies here (an empty
/// array is the right "no actions" answer).
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
        // The diagnosed line — the suppression goes above it at the same indent.
        let line = diag.range.start.line;
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
/// Index-/parse-priced (reads only the round-tripped `data` + the target buffer's diagnosed line for
/// indentation; never a fresh analyze), so it is NOT in the Hard-pressure shed set — mirroring
/// `inlayHint/resolve`. An action with no `data` (an eager action, or a non-gdls action) or one whose
/// edit can't be reconstructed (the target buffer is gone) is returned unchanged.
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
/// The annotation is inserted at **column 0 of the diagnosed line**, copying that line's leading
/// whitespace, as `"<indent>@warning_ignore(\"<code>\")\n"`. Placing it directly above the diagnosed
/// statement is what makes Godot's parser attach it to that statement's node (the analyzer's
/// `@warning_ignore` filter records the annotation owner's header lines), so the warning is actually
/// suppressed. The exact leading whitespace is copied from the source (never assumed to be tabs), so
/// the indentation always matches.
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
    // The diagnosed line's text, to copy its leading indentation. ropey's `get_line` is None for an
    // out-of-range index (defensive — the line came from a diagnostic range over this buffer).
    let line_slice = doc.rope.get_line(data.line as usize)?;
    let line_text = line_slice.to_string();
    let indent: String = line_text
        .chars()
        .take_while(|c| *c == ' ' || *c == '\t')
        .collect();
    let new_text = format!("{indent}@warning_ignore(\"{}\")\n", data.code);

    // A zero-width insertion at (line, col 0): the new annotation line is spliced in ABOVE the
    // diagnosed line, pushing it (and everything below) down by one line.
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
