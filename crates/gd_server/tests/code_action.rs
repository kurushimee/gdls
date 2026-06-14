//! M10 (#75): the `textDocument/codeAction` pipeline — over a `Connection` (protocol-shape).
//!
//! Covers the phase-4 acceptance criteria:
//!   1. Diagnostics publish with an additive `data` payload (gated on `publishDiagnostics.dataSupport`);
//!      message + range stay byte-identical vs a no-dataSupport baseline (FIDELITY).
//!   2. `codeAction` → `codeAction/resolve` for the `@warning_ignore` action: a `quickfix` whose
//!      resolved edit inserts a correctly-formed `@warning_ignore("CODE")` at the right line/indent —
//!      and re-analyzes CLEAN (the warning is actually suppressed).
//!   3. `context.only`: filtered to `source.fixAll` → `[]`; filtered to `quickfix` → the action.
//!   4. `codeActionLiteralSupport` fallback: a client without it gets a `Command`;
//!      `workspace/executeCommand` triggers a correctly-correlated `workspace/applyEdit` server→client
//!      request (BOTH accept and reject handled, session stays live).
//!   5. `executeCommandProvider` advertises ONLY existing commands; an unknown command → a proper
//!      error (not a panic).
//!   6. Generic-client degraded path: no `resolveSupport` → the edit is computed EAGERLY in the
//!      `codeAction` response (no resolve round-trip needed).

mod common;

use common::{file_uri, notification, recv, request, shutdown, try_recv, TempProject};
use lsp_server::{Connection, Message, RequestId, Response};
use lsp_types::{
    ApplyWorkspaceEditResponse, ClientCapabilities, CodeAction, CodeActionClientCapabilities,
    CodeActionContext, CodeActionKind, CodeActionLiteralSupport, CodeActionOrCommand,
    CodeActionParams, CodeActionResponse, Diagnostic, DidOpenTextDocumentParams,
    ExecuteCommandParams, InitializeParams, InitializeResult, InitializedParams, NumberOrString,
    PartialResultParams, Position, PublishDiagnosticsClientCapabilities, PublishDiagnosticsParams,
    Range, TextDocumentClientCapabilities, TextDocumentIdentifier, TextDocumentItem, Uri,
    WorkDoneProgressParams,
};
use std::time::Duration;

/// A fixture with exactly one `UNUSED_VARIABLE` warning: `var dead = 1` (tab-indented) on 0-based
/// line 4 that is never read. (Mirrors `gd_analyze`'s `unused_variable_and_local_constant_warn`.) The
/// `const` is dropped so the file has a SINGLE fixable diagnostic — simpler assertions.
const UNUSED_VAR_SRC: &str = "extends Node\n\n\nfunc test() -> void:\n\tvar dead = 1\n\tprint(0)\n";

/// The 0-based line of the `var dead = 1` declaration (where the suppression annotation must land).
const UNUSED_VAR_LINE: u32 = 4;

/// A base project (project.godot + minimal api), no source files — tests write their own.
fn base_project() -> TempProject {
    let p = TempProject::new();
    p.write("project.godot", "config_version=5\n");
    p.write("extension_api.json", common::MINI_API);
    p
}

/// Client capabilities for the codeAction pipeline, each gate independently toggleable:
///   * `literal` — `codeAction.codeActionLiteralSupport` (else the `Command[]` fallback).
///   * `resolve` — `codeAction.resolveSupport` (else eager edits).
///   * `data` — `publishDiagnostics.dataSupport` (else no additive `Diagnostic.data`).
fn caps(literal: bool, resolve: bool, data: bool) -> ClientCapabilities {
    ClientCapabilities {
        text_document: Some(TextDocumentClientCapabilities {
            code_action: Some(CodeActionClientCapabilities {
                code_action_literal_support: literal.then(|| CodeActionLiteralSupport {
                    code_action_kind: lsp_types::CodeActionKindLiteralSupport {
                        value_set: vec!["quickfix".to_string(), "refactor".to_string()],
                    },
                }),
                resolve_support: resolve.then(|| lsp_types::CodeActionCapabilityResolveSupport {
                    properties: vec!["edit".to_string()],
                }),
                ..Default::default()
            }),
            publish_diagnostics: Some(PublishDiagnosticsClientCapabilities {
                data_support: Some(data),
                ..Default::default()
            }),
            ..Default::default()
        }),
        ..Default::default()
    }
}

/// Initialize against `project` with `client_caps`, send `initialized`, open `files`, and RETURN the
/// `InitializeResult` plus the published diagnostics for the FIRST opened file (drained so the wire is
/// clean for the test's own requests). Each opened file's didOpen pushes one `publishDiagnostics`.
fn init_open(
    project: &TempProject,
    client: &Connection,
    files: &[(&str, &str)],
    client_caps: ClientCapabilities,
) -> (InitializeResult, PublishDiagnosticsParams) {
    let init = InitializeParams {
        initialization_options: Some(serde_json::json!({
            "projectRoot": project.root.as_str(),
            "autoDumpExtensionApi": false,
            "extensionApiPath": project.root.join("extension_api.json").as_str(),
        })),
        capabilities: client_caps,
        ..Default::default()
    };
    client.sender.send(request(1, "initialize", init)).unwrap();
    let init_resp = common::recv_response(client);
    assert!(
        init_resp.error.is_none(),
        "initialize errored: {:?}",
        init_resp.error
    );
    let result: InitializeResult =
        serde_json::from_value(init_resp.result.expect("initialize result")).unwrap();
    client
        .sender
        .send(notification("initialized", InitializedParams {}))
        .unwrap();

    let mut first_diags: Option<PublishDiagnosticsParams> = None;
    for (i, (rel, text)) in files.iter().enumerate() {
        project.write(rel, text);
        let uri = file_uri(&project.root.join(rel));
        client
            .sender
            .send(notification(
                "textDocument/didOpen",
                DidOpenTextDocumentParams {
                    text_document: TextDocumentItem {
                        uri: uri.clone(),
                        language_id: "gdscript".to_string(),
                        version: (i + 2) as i32,
                        text: text.to_string(),
                    },
                },
            ))
            .unwrap();
        if i == 0 {
            first_diags = Some(recv_publish(client));
        }
    }
    // Drain any further notifications (later files' diagnostics) so the wire is clean.
    while try_recv(client, Duration::from_millis(300)).is_some() {}
    (result, first_diags.expect("at least one file opened"))
}

/// Receive messages until a `publishDiagnostics` notification arrives, returning its params.
fn recv_publish(client: &Connection) -> PublishDiagnosticsParams {
    loop {
        if let Message::Notification(n) = recv(client) {
            if n.method == "textDocument/publishDiagnostics" {
                return serde_json::from_value(n.params).unwrap();
            }
        }
    }
}

/// The single `UNUSED_VARIABLE` diagnostic in a publish set (panics if absent).
fn unused_var_diag(diags: &PublishDiagnosticsParams) -> Diagnostic {
    diags
        .diagnostics
        .iter()
        .find(|d| d.code == Some(NumberOrString::String("UNUSED_VARIABLE".to_string())))
        .cloned()
        .unwrap_or_else(|| panic!("UNUSED_VARIABLE must fire; got {:?}", diags.diagnostics))
}

/// Send a `textDocument/codeAction` over `range` carrying `context_diags` in the context, with an
/// optional `only` filter, and return the parsed response.
fn request_code_action(
    client: &Connection,
    id: i32,
    uri: &Uri,
    range: Range,
    context_diags: Vec<Diagnostic>,
    only: Option<Vec<CodeActionKind>>,
) -> CodeActionResponse {
    let params = CodeActionParams {
        text_document: TextDocumentIdentifier { uri: uri.clone() },
        range,
        context: CodeActionContext {
            diagnostics: context_diags,
            only,
            trigger_kind: None,
        },
        work_done_progress_params: WorkDoneProgressParams::default(),
        partial_result_params: PartialResultParams::default(),
    };
    client
        .sender
        .send(request(id, "textDocument/codeAction", params))
        .unwrap();
    let resp = common::recv_response(client);
    assert!(resp.error.is_none(), "codeAction errored: {:?}", resp.error);
    serde_json::from_value(resp.result.expect("codeAction result")).unwrap()
}

/// The whole diagnosed line range (start = the var's column, end same line). A real client passes the
/// diagnostic's own range; this mirrors that.
fn diag_range(diag: &Diagnostic) -> Range {
    diag.range
}

// ===================================================================================================
// 1. Diagnostic.data additive + fidelity
// ===================================================================================================

/// A client advertising `publishDiagnostics.dataSupport` gets the additive `data` tag on the
/// `UNUSED_VARIABLE` diagnostic; a client WITHOUT it gets byte-identical message + range + severity
/// and NO `data` — the fidelity guarantee (the analyzer stream is untouched).
#[test]
fn diagnostic_data_is_additive_and_fidelity_preserved() {
    // With dataSupport.
    let p1 = base_project();
    let (server1, client1) = Connection::memory();
    let t1 = std::thread::spawn(move || gd_server::serve(server1));
    let (_r1, diags_with) = init_open(
        &p1,
        &client1,
        &[("a.gd", UNUSED_VAR_SRC)],
        caps(true, true, true),
    );
    let with = unused_var_diag(&diags_with);
    assert!(
        with.data.is_some(),
        "with dataSupport, the diagnostic must carry an additive `data` payload; got {with:?}"
    );
    // The payload names the warning code under the `gdls` namespace.
    let data = with.data.clone().unwrap();
    assert_eq!(
        data.pointer("/gdls/warningCode").and_then(|v| v.as_str()),
        Some("UNUSED_VARIABLE"),
        "data must carry the warning PNAME under gdls.warningCode; got {data}"
    );
    shutdown(&client1, t1);

    // Without dataSupport — the fidelity baseline.
    let p2 = base_project();
    let (server2, client2) = Connection::memory();
    let t2 = std::thread::spawn(move || gd_server::serve(server2));
    let (_r2, diags_without) = init_open(
        &p2,
        &client2,
        &[("a.gd", UNUSED_VAR_SRC)],
        caps(true, true, false),
    );
    let without = unused_var_diag(&diags_without);
    assert!(
        without.data.is_none(),
        "without dataSupport, the diagnostic must carry NO `data`; got {without:?}"
    );
    // FIDELITY: every field EXCEPT `data` must be byte-identical across the two clients — the `data`
    // tag is the ONLY thing dataSupport adds, and an absent-dataSupport diagnostic must equal the
    // pre-feature diagnostic. (Asserting the whole projected surface, not just message/range, so a
    // future `..Default::default()` regression that shifted another field couldn't hide here.)
    assert_eq!(
        with.message, without.message,
        "message must be byte-identical"
    );
    assert_eq!(with.range, without.range, "range must be byte-identical");
    assert_eq!(
        with.severity, without.severity,
        "severity must be identical"
    );
    assert_eq!(with.code, without.code, "code must be identical");
    assert_eq!(with.source, without.source, "source must be identical");
    assert_eq!(with.tags, without.tags, "tags must be identical");
    assert_eq!(
        with.code_description, without.code_description,
        "codeDescription must be identical"
    );
    assert_eq!(
        with.related_information, without.related_information,
        "relatedInformation must be identical"
    );
    shutdown(&client2, t2);
}

// ===================================================================================================
// 2. codeAction -> resolve (the @warning_ignore action), and the re-analyze-clean proof
// ===================================================================================================

/// A full literal+resolve client: `codeAction` over the `UNUSED_VARIABLE` diagnostic returns ONE
/// `quickfix` CodeAction with NO `edit` (deferred) carrying `data`; `codeAction/resolve` fills the
/// edit, inserting `\t@warning_ignore("UNUSED_VARIABLE")\n` at (line 4, col 0). Applying that edit to
/// the source re-analyzes CLEAN — the real proof the action works end-to-end.
#[test]
fn code_action_then_resolve_inserts_warning_ignore_and_suppresses() {
    let p = base_project();
    let (server, client) = Connection::memory();
    let t = std::thread::spawn(move || gd_server::serve(server));
    let (_r, diags) = init_open(
        &p,
        &client,
        &[("a.gd", UNUSED_VAR_SRC)],
        caps(true, true, true),
    );
    let uri = file_uri(&p.root.join("a.gd"));
    let diag = unused_var_diag(&diags);

    let actions = request_code_action(
        &client,
        10,
        &uri,
        diag_range(&diag),
        vec![diag.clone()],
        None,
    );
    assert_eq!(actions.len(), 1, "exactly one quickfix; got {actions:?}");
    let CodeActionOrCommand::CodeAction(action) = actions.into_iter().next().unwrap() else {
        panic!("a literal-support client must get a CodeAction, not a Command");
    };
    assert_eq!(
        action.kind,
        Some(CodeActionKind::QUICKFIX),
        "kind must be quickfix"
    );
    assert!(
        action.edit.is_none(),
        "with resolveSupport the edit is DEFERRED to resolve; got {:?}",
        action.edit
    );
    assert!(
        action.data.is_some(),
        "a deferred action must carry resolve data"
    );

    // Resolve fills the edit.
    let resolved = resolve_action(&client, 11, action);
    let edit = resolved.edit.expect("resolve must fill the edit");
    let (edited_uri, new_text, range) = single_text_edit(&edit);
    assert_eq!(edited_uri, uri, "the edit targets the diagnosed file");
    assert_eq!(
        new_text, "\t@warning_ignore(\"UNUSED_VARIABLE\")\n",
        "the insertion must be the tab-indented annotation with a trailing newline"
    );
    assert_eq!(
        range,
        Range {
            start: Position {
                line: UNUSED_VAR_LINE,
                character: 0
            },
            end: Position {
                line: UNUSED_VAR_LINE,
                character: 0
            },
        },
        "the insertion is a zero-width splice at column 0 of the diagnosed line"
    );

    // RE-ANALYZE CLEAN: apply the edit to the source and re-open — the warning is gone.
    let patched = apply_insertion(UNUSED_VAR_SRC, UNUSED_VAR_LINE, &new_text);
    let uri2 = file_uri(&p.root.join("b.gd"));
    p.write("b.gd", &patched);
    client
        .sender
        .send(notification(
            "textDocument/didOpen",
            DidOpenTextDocumentParams {
                text_document: TextDocumentItem {
                    uri: uri2.clone(),
                    language_id: "gdscript".to_string(),
                    version: 50,
                    text: patched.clone(),
                },
            },
        ))
        .unwrap();
    let after = recv_publish_for(&client, &uri2);
    assert!(
        !after
            .diagnostics
            .iter()
            .any(|d| d.code == Some(NumberOrString::String("UNUSED_VARIABLE".to_string()))),
        "after inserting @warning_ignore the warning must be SUPPRESSED; got {:?}",
        after.diagnostics
    );
    // Distinguish a genuine suppression from a malformed insertion that broke parsing (which would
    // ALSO drop the warning): the patched file must produce NO error-severity diagnostics — the
    // annotation is valid GDScript spliced at the right place.
    assert!(
        !after
            .diagnostics
            .iter()
            .any(|d| d.severity == Some(lsp_types::DiagnosticSeverity::ERROR)),
        "the inserted annotation must keep the file clean (no parse/type errors); got {:?}",
        after.diagnostics
    );
    shutdown(&client, t);
}

/// Generic degraded path: a literal client WITHOUT `resolveSupport` gets the edit EAGERLY in the
/// `codeAction` response (no resolve round-trip), still correctly formed.
#[test]
fn code_action_computes_edit_eagerly_without_resolve_support() {
    let p = base_project();
    let (server, client) = Connection::memory();
    let t = std::thread::spawn(move || gd_server::serve(server));
    // literal = true, resolve = FALSE.
    let (_r, diags) = init_open(
        &p,
        &client,
        &[("a.gd", UNUSED_VAR_SRC)],
        caps(true, false, true),
    );
    let uri = file_uri(&p.root.join("a.gd"));
    let diag = unused_var_diag(&diags);

    let actions = request_code_action(&client, 10, &uri, diag_range(&diag), vec![diag], None);
    let CodeActionOrCommand::CodeAction(action) = actions.into_iter().next().unwrap() else {
        panic!("a literal-support client must get a CodeAction");
    };
    let edit = action
        .edit
        .expect("WITHOUT resolveSupport the edit must be EAGER in the codeAction response");
    let (_uri, new_text, _range) = single_text_edit(&edit);
    assert_eq!(new_text, "\t@warning_ignore(\"UNUSED_VARIABLE\")\n");
    shutdown(&client, t);
}

// ===================================================================================================
// 3. context.only filtering
// ===================================================================================================

/// `context.only` honoring: filtered to `source.fixAll` (which does NOT admit `quickfix`) → `[]`;
/// filtered to `quickfix` → the action. The first proves the suppression action is never swept into
/// `source.fixAll`.
#[test]
fn context_only_filters_to_offered_kinds() {
    let p = base_project();
    let (server, client) = Connection::memory();
    let t = std::thread::spawn(move || gd_server::serve(server));
    let (_r, diags) = init_open(
        &p,
        &client,
        &[("a.gd", UNUSED_VAR_SRC)],
        caps(true, true, true),
    );
    let uri = file_uri(&p.root.join("a.gd"));
    let diag = unused_var_diag(&diags);

    // only: ["source.fixAll"] → nothing offered.
    let filtered_out = request_code_action(
        &client,
        10,
        &uri,
        diag_range(&diag),
        vec![diag.clone()],
        Some(vec![CodeActionKind::from("source.fixAll".to_string())]),
    );
    assert!(
        filtered_out.is_empty(),
        "a source.fixAll filter must exclude the quickfix; got {filtered_out:?}"
    );

    // only: ["quickfix"] → the action.
    let filtered_in = request_code_action(
        &client,
        11,
        &uri,
        diag_range(&diag),
        vec![diag],
        Some(vec![CodeActionKind::QUICKFIX]),
    );
    assert_eq!(
        filtered_in.len(),
        1,
        "a quickfix filter must admit the action; got {filtered_in:?}"
    );
    shutdown(&client, t);
}

// ===================================================================================================
// 4. Command fallback + applyEdit correlation (accept AND reject)
// ===================================================================================================

/// A client WITHOUT `codeActionLiteralSupport` gets a `Command`; running it via
/// `workspace/executeCommand` triggers a `workspace/applyEdit` server→client request that the test
/// ACCEPTS (`applied: true`). The applyEdit request must arrive (correctly formed) BEFORE the
/// executeCommand response; the session stays live after the correlated reply.
#[test]
fn command_fallback_triggers_correlated_apply_edit_accept() {
    run_command_fallback(true);
}

/// The same path, but the test REJECTS the applyEdit (`applied: false`). gdls must handle the
/// rejection gracefully (no crash, no bounce) and the session must stay live.
#[test]
fn command_fallback_triggers_correlated_apply_edit_reject() {
    run_command_fallback(false);
}

/// Drive the Command-fallback path, replying to the server's `workspace/applyEdit` with `applied =
/// `accept``, then assert liveness with a follow-up request.
fn run_command_fallback(accept: bool) {
    let p = base_project();
    let (server, client) = Connection::memory();
    let t = std::thread::spawn(move || gd_server::serve(server));
    // literal = FALSE → the Command fallback.
    let (_r, diags) = init_open(
        &p,
        &client,
        &[("a.gd", UNUSED_VAR_SRC)],
        caps(false, false, true),
    );
    let uri = file_uri(&p.root.join("a.gd"));
    let diag = unused_var_diag(&diags);

    let actions = request_code_action(
        &client,
        10,
        &uri,
        diag_range(&diag),
        vec![diag.clone()],
        None,
    );
    assert_eq!(actions.len(), 1, "one action; got {actions:?}");
    let CodeActionOrCommand::Command(cmd) = actions.into_iter().next().unwrap() else {
        panic!("a client WITHOUT literal support must get a Command, not a CodeAction");
    };
    assert_eq!(
        cmd.command, "gdls.applyWarningIgnore",
        "the command must be the advertised one"
    );

    // Run the command.
    client
        .sender
        .send(request(
            20,
            "workspace/executeCommand",
            ExecuteCommandParams {
                command: cmd.command.clone(),
                arguments: cmd.arguments.clone().unwrap_or_default(),
                work_done_progress_params: WorkDoneProgressParams::default(),
            },
        ))
        .unwrap();

    // The server sends a `workspace/applyEdit` REQUEST (before its executeCommand response). Capture
    // it, validate its edit, and reply with `applied = accept`.
    let apply_req = recv_request_for(&client, "workspace/applyEdit");
    let params: lsp_types::ApplyWorkspaceEditParams =
        serde_json::from_value(apply_req.params.clone()).unwrap();
    let (edited_uri, new_text, _range) = single_text_edit(&params.edit);
    assert_eq!(edited_uri, uri, "the applyEdit targets the diagnosed file");
    assert_eq!(
        new_text, "\t@warning_ignore(\"UNUSED_VARIABLE\")\n",
        "the applyEdit carries the correctly-formed suppression"
    );
    client
        .sender
        .send(Message::Response(Response {
            id: apply_req.id.clone(),
            result: Some(
                serde_json::to_value(ApplyWorkspaceEditResponse {
                    applied: accept,
                    failure_reason: (!accept).then(|| "user declined".to_string()),
                    failed_change: None,
                })
                .unwrap(),
            ),
            error: None,
        }))
        .unwrap();

    // The executeCommand response is `null` (the command ran; the applyEdit reply is correlated
    // separately).
    let exec_resp = recv_response_for(&client, &RequestId::from(20));
    assert!(
        exec_resp.error.is_none(),
        "executeCommand must succeed (the command ran); got {:?}",
        exec_resp.error
    );

    // LIVENESS: a follow-up request still gets a valid response — the correlated applyEdit reply
    // (accept OR reject) did not wedge the worker. An empty-context codeAction returns [], which is a
    // well-formed response proving the session is still serving.
    let actions2 = request_code_action(&client, 30, &uri, diag_range(&diag), Vec::new(), None);
    assert!(
        actions2.is_empty(),
        "an empty-context codeAction returns [], proving the session is still live"
    );
    shutdown(&client, t);
}

// ===================================================================================================
// 5. executeCommandProvider lists only real commands; unknown command errors
// ===================================================================================================

/// `executeCommandProvider` advertises EXACTLY the commands gdls handles (anti-catalog W15), and an
/// unknown command returns a proper JSON-RPC error — never a panic / never a silent success.
#[test]
fn execute_command_provider_lists_only_real_commands_and_rejects_unknown() {
    let p = base_project();
    let (server, client) = Connection::memory();
    let t = std::thread::spawn(move || gd_server::serve(server));
    let (result, _diags) = init_open(
        &p,
        &client,
        &[("a.gd", "extends Node\n")],
        caps(true, true, true),
    );

    let provider = result
        .capabilities
        .execute_command_provider
        .expect("executeCommandProvider must be advertised");
    assert_eq!(
        provider.commands,
        vec!["gdls.applyWarningIgnore".to_string()],
        "the advertised command list must be EXACTLY the handled set"
    );

    // An unknown command → a proper error response (not a panic, not a success).
    client
        .sender
        .send(request(
            40,
            "workspace/executeCommand",
            ExecuteCommandParams {
                command: "gdls.doesNotExist".to_string(),
                arguments: Vec::new(),
                work_done_progress_params: WorkDoneProgressParams::default(),
            },
        ))
        .unwrap();
    let resp = recv_response_for(&client, &RequestId::from(40));
    let err = resp
        .error
        .expect("an unknown command must return a ResponseError");
    assert!(
        err.message.contains("unknown command"),
        "the error must name the unknown-command cause; got {}",
        err.message
    );

    // LIVENESS: the server kept serving after the error.
    let provider_still = request_code_action(
        &client,
        41,
        &file_uri(&p.root.join("a.gd")),
        Range::default(),
        Vec::new(),
        None,
    );
    assert!(
        provider_still.is_empty(),
        "session stays live after the error"
    );
    shutdown(&client, t);
}

// ===================================================================================================
// 6. codeActionProvider capability advertised
// ===================================================================================================

/// The server advertises `codeActionProvider` with `resolveProvider` + the `quickfix` kind.
#[test]
fn code_action_provider_advertised() {
    let p = base_project();
    let (server, client) = Connection::memory();
    let t = std::thread::spawn(move || gd_server::serve(server));
    let (result, _diags) = init_open(
        &p,
        &client,
        &[("a.gd", "extends Node\n")],
        ClientCapabilities::default(),
    );

    let provider = result
        .capabilities
        .code_action_provider
        .expect("codeActionProvider must be advertised");
    match provider {
        lsp_types::CodeActionProviderCapability::Options(opts) => {
            assert_eq!(
                opts.resolve_provider,
                Some(true),
                "resolveProvider must be advertised"
            );
            assert_eq!(
                opts.code_action_kinds,
                Some(vec![CodeActionKind::QUICKFIX]),
                "the offered kinds must be exactly [quickfix]"
            );
        }
        other => panic!("expected CodeActionProviderCapability::Options; got {other:?}"),
    }
    shutdown(&client, t);
}

// ===================================================================================================
// Helpers
// ===================================================================================================

/// Send `codeAction/resolve` for `action` and return the resolved action.
fn resolve_action(client: &Connection, id: i32, action: CodeAction) -> CodeAction {
    client
        .sender
        .send(request(id, "codeAction/resolve", action))
        .unwrap();
    let resp = common::recv_response(client);
    assert!(
        resp.error.is_none(),
        "codeAction/resolve errored: {:?}",
        resp.error
    );
    serde_json::from_value(resp.result.expect("resolve result")).unwrap()
}

/// The single `(uri, new_text, range)` of a `WorkspaceEdit` carrying exactly one `TextEdit` — in
/// EITHER the `documentChanges` or `changes` shape (the test clients here don't advertise
/// `documentChanges`, so it is the legacy `changes` map; the helper covers both for robustness).
// `WorkspaceEdit.changes` is keyed on `Uri` (interior mutability) — only read here, never mutated as
// a key, so the `mutable_key_type` hazard cannot occur.
#[allow(clippy::mutable_key_type)]
fn single_text_edit(edit: &lsp_types::WorkspaceEdit) -> (Uri, String, Range) {
    if let Some(lsp_types::DocumentChanges::Edits(edits)) = &edit.document_changes {
        let tde = edits.first().expect("one TextDocumentEdit");
        let lsp_types::OneOf::Left(te) = tde.edits.first().expect("one edit") else {
            panic!("expected a plain TextEdit");
        };
        return (tde.text_document.uri.clone(), te.new_text.clone(), te.range);
    }
    let changes = edit.changes.as_ref().expect("changes map present");
    let (uri, edits) = changes.iter().next().expect("one file");
    let te = edits.first().expect("one edit");
    (uri.clone(), te.new_text.clone(), te.range)
}

/// Splice `insert` (a full line WITH its trailing newline) in ABOVE 0-based `line` of `src` — the
/// exact effect of the server's zero-width insertion at (line, col 0).
fn apply_insertion(src: &str, line: u32, insert: &str) -> String {
    let mut lines: Vec<&str> = src.split_inclusive('\n').collect();
    // `insert` already ends in `\n`; trim it so we can re-join cleanly as one element.
    let insert_line = insert.strip_suffix('\n').unwrap_or(insert);
    let owned = format!("{insert_line}\n");
    lines.insert(line as usize, &owned);
    lines.concat()
}

/// Receive until a `publishDiagnostics` for the GIVEN `uri` arrives (skip others).
fn recv_publish_for(client: &Connection, uri: &Uri) -> PublishDiagnosticsParams {
    loop {
        let p = recv_publish(client);
        if &p.uri == uri {
            return p;
        }
    }
}

/// Receive until a server→client REQUEST with `method` arrives, returning it (skip notifications /
/// responses in between, e.g. a late publishDiagnostics).
fn recv_request_for(client: &Connection, method: &str) -> lsp_server::Request {
    loop {
        if let Message::Request(r) = recv(client) {
            if r.method == method {
                return r;
            }
        }
    }
}

/// Receive until the `Response` with `id` arrives (skip server-initiated requests / notifications).
fn recv_response_for(client: &Connection, id: &RequestId) -> Response {
    loop {
        if let Message::Response(r) = recv(client) {
            if &r.id == id {
                return r;
            }
        }
    }
}
