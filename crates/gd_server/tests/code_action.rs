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

/// A fixture whose warning anchors on a CONTINUATION line of a multi-line statement: an
/// `INTEGER_DIVISION` inside a parenthesized initializer split across lines. The division `5 / 2` is
/// on 0-based line 5, but the enclosing statement (`var x = (`) starts on line 4. Inserting the
/// annotation at line 5 (col 0) would splice it INSIDE the parens → invalid GDScript. The fix must
/// resolve the enclosing statement (line 4). (Regression guard for the multi-line-corruption bug.)
const MULTILINE_INTDIV_SRC: &str =
    "extends Node\n\n\nfunc f() -> int:\n\tvar x = (\n\t\t5 / 2\n\t)\n\treturn x\n";

/// The 0-based line of `var x = (` — the enclosing statement the annotation must attach above.
const MULTILINE_INTDIV_STMT_LINE: u32 = 4;

/// A fixture whose warning IS a bare expression-statement (no enclosing target node between the
/// expression and the function): `a == 1` fires STANDALONE_EXPRESSION, anchored on the BinaryOp on
/// 0-based line 4 — which is itself the statement. The annotation must attach to that line (NOT
/// over-walk to the `func` signature, which would be valid GDScript but leave the warning unsuppressed
/// because the function's ignore-span is signature-only). (Regression guard.)
const STANDALONE_EXPR_SRC: &str = "extends Node\n\n\nfunc f(a: int) -> void:\n\ta == 1\n";

/// The 0-based line of the `a == 1` standalone expression statement.
const STANDALONE_EXPR_LINE: u32 = 4;

/// A base project (project.godot + minimal api), no source files — tests write their own.
fn base_project() -> TempProject {
    let p = TempProject::new();
    p.write(
        "project.godot",
        "config_version=5\n\n[application]\nconfig/features=PackedStringArray(\"4.6\")\n",
    );
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
    // The suppression action specifically (the unused var also yields a `_`-prefix quickfix and a
    // source.fixAll aggregate now — this test exercises the suppression's deferred-resolve flow).
    let action = find_action(&actions, "Ignore")
        .unwrap_or_else(|| panic!("the suppression action must be offered; got {actions:?}"));
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

/// REGRESSION (multi-line corruption): when a warning anchors on a CONTINUATION line of a multi-line
/// statement (here `INTEGER_DIVISION` inside a parenthesized initializer), the `@warning_ignore` must
/// be inserted above the ENCLOSING STATEMENT (`var x = (` on line 4), NOT at the raw diagnostic line
/// (the `5 / 2` continuation on line 5 — splicing there produced invalid GDScript). The patched
/// source must re-analyze with ZERO error-severity diagnostics AND the warning suppressed.
#[test]
fn warning_ignore_attaches_to_enclosing_statement_for_multiline_anchor() {
    let p = base_project();
    let (server, client) = Connection::memory();
    let t = std::thread::spawn(move || gd_server::serve(server));
    let (_r, diags) = init_open(
        &p,
        &client,
        &[("a.gd", MULTILINE_INTDIV_SRC)],
        caps(true, true, true),
    );
    let uri = file_uri(&p.root.join("a.gd"));
    let diag = diags
        .diagnostics
        .iter()
        .find(|d| d.code == Some(NumberOrString::String("INTEGER_DIVISION".to_string())))
        .cloned()
        .unwrap_or_else(|| panic!("INTEGER_DIVISION must fire; got {:?}", diags.diagnostics));

    // Sanity: the diagnostic itself anchors BELOW the enclosing statement (the bug's precondition).
    assert!(
        diag.range.start.line > MULTILINE_INTDIV_STMT_LINE,
        "fixture precondition: the diagnostic must anchor on a continuation line (got line {}, \
         enclosing statement line {MULTILINE_INTDIV_STMT_LINE})",
        diag.range.start.line
    );

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
        panic!("a literal-support client must get a CodeAction");
    };
    let resolved = resolve_action(&client, 11, action);
    let edit = resolved.edit.expect("resolve must fill the edit");
    let (_uri, new_text, range) = single_text_edit(&edit);
    assert_eq!(
        new_text, "\t@warning_ignore(\"INTEGER_DIVISION\")\n",
        "the insertion copies the ENCLOSING statement's tab indent"
    );
    assert_eq!(
        range.start,
        Position {
            line: MULTILINE_INTDIV_STMT_LINE,
            character: 0
        },
        "the insertion must land at col 0 of the ENCLOSING statement line (not the continuation line)"
    );

    // RE-ANALYZE CLEAN: applying the edit must NOT break parsing (the old behavior produced syntax
    // errors) and must suppress the warning.
    let patched = apply_insertion(MULTILINE_INTDIV_SRC, range.start.line, &new_text);
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
                    version: 60,
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
            .any(|d| d.severity == Some(lsp_types::DiagnosticSeverity::ERROR)),
        "the inserted annotation must keep the file parseable (NO errors); got {:?}\npatched:\n{patched}",
        after.diagnostics
    );
    assert!(
        !after
            .diagnostics
            .iter()
            .any(|d| d.code == Some(NumberOrString::String("INTEGER_DIVISION".to_string()))),
        "the warning must be SUPPRESSED after the edit; got {:?}",
        after.diagnostics
    );
    shutdown(&client, t);
}

/// REGRESSION (standalone-expression over-walk): a warning that IS a bare expression-statement has no
/// `@warning_ignore`-target node between it and the function. The annotation must attach to the
/// statement's OWN line (resolved via the direct-suite-statement stop), not over-walk to the `func`
/// signature — over-walking is valid GDScript but leaves the warning unsuppressed.
#[test]
fn warning_ignore_attaches_to_bare_expression_statement() {
    let p = base_project();
    let (server, client) = Connection::memory();
    let t = std::thread::spawn(move || gd_server::serve(server));
    let (_r, diags) = init_open(
        &p,
        &client,
        &[("a.gd", STANDALONE_EXPR_SRC)],
        caps(true, true, true),
    );
    let uri = file_uri(&p.root.join("a.gd"));
    let diag = diags
        .diagnostics
        .iter()
        .find(|d| d.code == Some(NumberOrString::String("STANDALONE_EXPRESSION".to_string())))
        .cloned()
        .unwrap_or_else(|| {
            panic!(
                "STANDALONE_EXPRESSION must fire; got {:?}",
                diags.diagnostics
            )
        });

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
        panic!("a literal-support client must get a CodeAction");
    };
    let resolved = resolve_action(&client, 11, action);
    let (_uri, new_text, range) = single_text_edit(&resolved.edit.expect("resolve fills the edit"));
    assert_eq!(
        range.start,
        Position {
            line: STANDALONE_EXPR_LINE,
            character: 0
        },
        "the annotation lands on the statement's own line, not the func signature"
    );

    // The edit must re-analyze clean AND actually suppress STANDALONE_EXPRESSION.
    let patched = apply_insertion(STANDALONE_EXPR_SRC, range.start.line, &new_text);
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
                    version: 70,
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
            .any(|d| d.severity == Some(lsp_types::DiagnosticSeverity::ERROR)),
        "the inserted annotation must keep the file parseable; got {:?}",
        after.diagnostics
    );
    assert!(
        !after
            .diagnostics
            .iter()
            .any(|d| d.code == Some(NumberOrString::String("STANDALONE_EXPRESSION".to_string()))),
        "STANDALONE_EXPRESSION must be SUPPRESSED after the edit; got {:?}",
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

/// `context.only` honoring (FAMILY SEPARATION): a `source.fixAll` filter yields ONLY the fixAll
/// aggregate — never the per-diagnostic suppression / quickfixes; a `quickfix` filter yields ONLY the
/// per-diagnostic fixes — never the fixAll aggregate. This is the load-bearing exclusion that keeps a
/// fix-all-on-save sweep from applying a suppression, and a lightbulb from offering the aggregate.
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

    // only: ["source.fixAll"] → ONLY the source.fixAll action (the unused var yields a `_`-prefix in
    // the aggregate). The suppression / per-diagnostic quickfixes must NOT appear.
    let fixall_only = request_code_action(
        &client,
        10,
        &uri,
        diag_range(&diag),
        vec![diag.clone()],
        Some(vec![CodeActionKind::from("source.fixAll".to_string())]),
    );
    assert!(
        fixall_only.iter().all(|a| matches!(
            a,
            CodeActionOrCommand::CodeAction(ca) if ca.kind == Some(CodeActionKind::SOURCE_FIX_ALL)
        )),
        "a source.fixAll filter must yield ONLY source.fixAll actions (no quickfix/suppression); \
         got {fixall_only:?}"
    );
    assert_eq!(
        fixall_only.len(),
        1,
        "exactly one source.fixAll aggregate; got {fixall_only:?}"
    );

    // only: ["quickfix"] → the per-diagnostic fixes (suppression + `_`-prefix), and NO fixAll.
    let quickfix_only = request_code_action(
        &client,
        11,
        &uri,
        diag_range(&diag),
        vec![diag],
        Some(vec![CodeActionKind::QUICKFIX]),
    );
    assert!(
        quickfix_only.iter().all(|a| matches!(
            a,
            CodeActionOrCommand::CodeAction(ca) if ca.kind == Some(CodeActionKind::QUICKFIX)
        )),
        "a quickfix filter must yield ONLY quickfix actions (no source.fixAll); got {quickfix_only:?}"
    );
    assert!(
        quickfix_only.iter().any(
            |a| matches!(a, CodeActionOrCommand::CodeAction(ca) if ca.title.contains("Ignore"))
        ),
        "the suppression quickfix must be present; got {quickfix_only:?}"
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
                Some(vec![
                    CodeActionKind::QUICKFIX,
                    CodeActionKind::SOURCE_FIX_ALL
                ]),
                "the offered kinds must be exactly [quickfix, source.fixAll]"
            );
        }
        other => panic!("expected CodeActionProviderCapability::Options; got {other:?}"),
    }
    shutdown(&client, t);
}

// ===================================================================================================
// #339: staleness — a recipe whose buffer moved between offer and accept
// ===================================================================================================

/// A file whose fixable warning sits ABOVE a multi-line call, so a stale-line splice lands inside
/// the argument list and breaks the parse. Line 4 is `var unused_thing := 1`.
const STALE_SRC: &str = "extends Node\nfunc g(a: int, b: int, c: int) -> int:\n\treturn a + b + c\nfunc f() -> void:\n\tvar unused_thing := 1\n\tprint(g(\n\t\t1,\n\t\t2,\n\t\t3))\n";

/// The same program with `func g` moved BELOW `func f` — every line of `f` shifts up by two, so the
/// offer-time line 4 now points at `\t\t1,` inside `print(g(`.
const STALE_SRC_SHIFTED: &str = "extends Node\nfunc f() -> void:\n\tvar unused_thing := 1\n\tprint(g(\n\t\t1,\n\t\t2,\n\t\t3))\nfunc g(a: int, b: int, c: int) -> int:\n\treturn a + b + c\n";

/// Push a whole-document `didChange` at `version`.
fn change_doc(client: &Connection, uri: &Uri, text: &str, version: i32) {
    client
        .sender
        .send(notification(
            "textDocument/didChange",
            lsp_types::DidChangeTextDocumentParams {
                text_document: lsp_types::VersionedTextDocumentIdentifier {
                    uri: uri.clone(),
                    version,
                },
                content_changes: vec![lsp_types::TextDocumentContentChangeEvent {
                    range: None,
                    range_length: None,
                    text: text.to_string(),
                }],
            },
        ))
        .unwrap();
}

/// The suppression action offered for the file's one fixable warning, with the diagnostic it came
/// from. Drains the wire afterwards so the next receive is the caller's own.
fn offer_suppression(client: &Connection, uri: &Uri, id: i32) -> (Diagnostic, CodeAction) {
    let diags = recv_publish_for(client, uri);
    let diag = unused_var_diag(&diags);
    while try_recv(client, Duration::from_millis(200)).is_some() {}
    let actions = request_code_action(client, id, uri, diag_range(&diag), vec![diag.clone()], None);
    let action = find_action(&actions, "Ignore")
        .unwrap_or_else(|| panic!("the suppression action must be offered; got {actions:?}"));
    (diag, action)
}

/// CORRUPTION GUARD (#339): the suppression recipe carries the line it resolved, so a `didChange`
/// between the offer and the resolve makes that line name something else. Resolving must REFUSE
/// with `ContentModified` rather than splice at the drifted line — which here would land the
/// annotation inside a multi-line argument list and stop the file parsing.
#[test]
fn resolve_refuses_a_suppression_whose_buffer_moved() {
    let p = base_project();
    let (server, client) = Connection::memory();
    let t = std::thread::spawn(move || gd_server::serve(server));
    init_open(&p, &client, &[("a.gd", STALE_SRC)], caps(true, true, true));
    let uri = file_uri(&p.root.join("a.gd"));
    let actions = request_code_action(
        &client,
        10,
        &uri,
        Range {
            start: Position {
                line: 4,
                character: 0,
            },
            end: Position {
                line: 4,
                character: 0,
            },
        },
        Vec::new(),
        None,
    );
    // No diagnostics in context ⇒ nothing offered; re-request with the real diagnostic.
    assert!(
        actions.is_empty(),
        "sanity: an empty context offers nothing"
    );
    let (_diag, action) = {
        // Re-publish by touching the file so a fresh diagnostic set arrives.
        change_doc(&client, &uri, STALE_SRC, 2);
        offer_suppression(&client, &uri, 11)
    };

    // The user edits: `func g` moves below `func f`.
    change_doc(&client, &uri, STALE_SRC_SHIFTED, 3);
    while try_recv(&client, Duration::from_millis(400)).is_some() {}

    client
        .sender
        .send(request(12, "codeAction/resolve", action))
        .unwrap();
    let resp = recv_response_for(&client, &RequestId::from(12));
    let err = resp
        .error
        .expect("a stale suppression must be REFUSED, never resolved to an edit at the old line");
    assert_eq!(
        err.code, -32801,
        "the refusal must be ContentModified (the request's basis changed); got {err:?}"
    );

    // RECOVERY: re-requesting code actions on the current buffer yields a working action whose edit
    // lands on the warning's CURRENT line (2), not the stale 4.
    let (_diag2, action2) = {
        change_doc(&client, &uri, STALE_SRC_SHIFTED, 4);
        offer_suppression(&client, &uri, 13)
    };
    let resolved = resolve_action(&client, 14, action2);
    let (_u, new_text, range) = single_text_edit(&resolved.edit.expect("resolve fills the edit"));
    assert_eq!(new_text, "\t@warning_ignore(\"UNUSED_VARIABLE\")\n");
    assert_eq!(
        range.start.line, 2,
        "the re-offered action is gated against the CURRENT buffer, so it targets the warning's \
         current line"
    );
    shutdown(&client, t);
}

/// The mutating channel: a `Command` can sit in a client menu indefinitely, so its staleness window
/// is the longest there is. `workspace/executeCommand` on a moved buffer must refuse AND send no
/// `workspace/applyEdit` at all — an applyEdit is a write the user never gets to inspect first.
#[test]
fn execute_command_refuses_a_stale_suppression_and_sends_no_apply_edit() {
    let p = base_project();
    let (server, client) = Connection::memory();
    let t = std::thread::spawn(move || gd_server::serve(server));
    // literal = FALSE → the Command fallback.
    let (_r, diags) = init_open(
        &p,
        &client,
        &[("a.gd", STALE_SRC)],
        caps(false, false, true),
    );
    let uri = file_uri(&p.root.join("a.gd"));
    let diag = unused_var_diag(&diags);
    let actions = request_code_action(&client, 10, &uri, diag_range(&diag), vec![diag], None);
    let CodeActionOrCommand::Command(cmd) = actions
        .into_iter()
        .find(|a| matches!(a, CodeActionOrCommand::Command(c) if c.title.starts_with("Ignore")))
        .expect("the suppression Command must be offered")
    else {
        unreachable!()
    };

    change_doc(&client, &uri, STALE_SRC_SHIFTED, 3);
    while try_recv(&client, Duration::from_millis(400)).is_some() {}

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
    // Drain to the response, failing loudly on any interleaved applyEdit — the whole point is that
    // no write is attempted.
    let resp = loop {
        match recv(&client) {
            Message::Request(r) if r.method == "workspace/applyEdit" => {
                panic!("a stale command must send NO workspace/applyEdit; got {r:?}")
            }
            Message::Response(r) if r.id == RequestId::from(20) => break r,
            _ => {}
        }
    };
    let err = resp.error.expect("a stale command must be REFUSED");
    assert_eq!(err.code, -32801, "ContentModified; got {err:?}");

    // LIVENESS: the refusal did not wedge the worker.
    let after = request_code_action(
        &client,
        30,
        &uri,
        Range {
            start: Position {
                line: 0,
                character: 0,
            },
            end: Position {
                line: 0,
                character: 0,
            },
        },
        Vec::new(),
        None,
    );
    assert!(after.is_empty(), "an empty context still answers []");
    shutdown(&client, t);
}

/// The gate is the LSP document VERSION, not a content hash: two edits that end at the original
/// text still refuse. Deliberate — the version is the protocol's own edit-validity coordinate, the
/// same one the outgoing `documentChanges` stamp carries, and a second notion of staleness would be
/// one more thing to keep in agreement with it.
#[test]
fn resolve_refuses_after_an_edit_round_trip_back_to_identical_text() {
    let p = base_project();
    let (server, client) = Connection::memory();
    let t = std::thread::spawn(move || gd_server::serve(server));
    init_open(&p, &client, &[("a.gd", STALE_SRC)], caps(true, true, true));
    let uri = file_uri(&p.root.join("a.gd"));
    change_doc(&client, &uri, STALE_SRC, 2);
    let (_diag, action) = offer_suppression(&client, &uri, 10);

    change_doc(&client, &uri, STALE_SRC_SHIFTED, 3);
    change_doc(&client, &uri, STALE_SRC, 4);
    while try_recv(&client, Duration::from_millis(400)).is_some() {}

    client
        .sender
        .send(request(11, "codeAction/resolve", action))
        .unwrap();
    let resp = recv_response_for(&client, &RequestId::from(11));
    let err = resp
        .error
        .expect("the version moved, so the recipe is stale even though the text matches again");
    assert_eq!(err.code, -32801, "ContentModified; got {err:?}");
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

// ===================================================================================================
// Mutating warning quickfixes (#75 part 2): per-fix round-trip + adversarial mutation-correctness.
//
// The mutation-correctness bar for EVERY fix: apply the resolved edit to the buffer, re-open, re-run
// diagnostics, and assert (a) the original warning is GONE and (b) NO NEW diagnostic appears by
// IDENTITY (the (code, message) set — ranges shift under a line insert, so identity is compared on
// (code, message), and fixtures are built so the post-fix set is otherwise empty). A broken/wrong
// edit is a BLOCKER; this harness is the proof it doesn't happen.
// ===================================================================================================

/// Every `TextEdit` (any file) in a `WorkspaceEdit`, in EITHER negotiated shape. The mutating fixes
/// here are single-file, but this returns all so a multi-edit (fixAll) or empty edit is visible.
#[allow(clippy::mutable_key_type)]
fn all_text_edits(edit: &lsp_types::WorkspaceEdit) -> Vec<lsp_types::TextEdit> {
    if let Some(lsp_types::DocumentChanges::Edits(tdes)) = &edit.document_changes {
        return tdes
            .iter()
            .flat_map(|tde| {
                tde.edits.iter().filter_map(|e| match e {
                    lsp_types::OneOf::Left(te) => Some(te.clone()),
                    lsp_types::OneOf::Right(_) => None,
                })
            })
            .collect();
    }
    if let Some(changes) = &edit.changes {
        return changes.values().flatten().cloned().collect();
    }
    Vec::new()
}

/// Apply a set of NON-OVERLAPPING LSP `TextEdit`s to ASCII `src` (fixtures here are ASCII, so an LSP
/// character offset == a byte offset == a char index). Edits are applied last-first so earlier ranges
/// stay valid. This is the exact effect of a client applying the `WorkspaceEdit`.
fn apply_text_edits(src: &str, mut edits: Vec<lsp_types::TextEdit>) -> String {
    // Convert each (line, character) to a flat byte offset over `src`.
    let line_starts: Vec<usize> = std::iter::once(0)
        .chain(src.match_indices('\n').map(|(i, _)| i + 1))
        .collect();
    let to_offset = |p: lsp_types::Position| -> usize {
        let line_start = line_starts
            .get(p.line as usize)
            .copied()
            .unwrap_or(src.len());
        (line_start + p.character as usize).min(src.len())
    };
    // Sort by start descending so applying one edit doesn't shift the offsets of the others.
    edits.sort_by_key(|e| std::cmp::Reverse((e.range.start.line, e.range.start.character)));
    let mut out = src.to_string();
    for e in edits {
        let start = to_offset(e.range.start);
        let end = to_offset(e.range.end);
        out.replace_range(start..end, &e.new_text);
    }
    out
}

/// Re-open `patched` under a fresh `rel` file and return its published diagnostics. (Each fix's
/// re-analysis: a NEW file so the server analyzes the patched text from scratch.)
fn reopen_and_diags(
    p: &TempProject,
    client: &Connection,
    rel: &str,
    patched: &str,
    version: i32,
) -> PublishDiagnosticsParams {
    let uri = file_uri(&p.root.join(rel));
    p.write(rel, patched);
    client
        .sender
        .send(notification(
            "textDocument/didOpen",
            DidOpenTextDocumentParams {
                text_document: TextDocumentItem {
                    uri: uri.clone(),
                    language_id: "gdscript".to_string(),
                    version,
                    text: patched.to_string(),
                },
            },
        ))
        .unwrap();
    recv_publish_for(client, &uri)
}

/// The set of `(code, message)` pairs in a publish set — the IDENTITY used to detect a NEW diagnostic
/// across an edit (range omitted: a line insert shifts ranges, but no NEW (code, message) may appear).
fn diag_identities(
    diags: &PublishDiagnosticsParams,
) -> std::collections::BTreeSet<(String, String)> {
    diags
        .diagnostics
        .iter()
        .map(|d| {
            let code = match &d.code {
                Some(NumberOrString::String(s)) => s.clone(),
                Some(NumberOrString::Number(n)) => n.to_string(),
                None => String::new(),
            };
            (code, d.message.clone())
        })
        .collect()
}

/// The diagnostic with the given warning `code` (panics if absent).
fn diag_with_warning(diags: &PublishDiagnosticsParams, code: &str) -> Diagnostic {
    diags
        .diagnostics
        .iter()
        .find(|d| d.code == Some(NumberOrString::String(code.to_string())))
        .cloned()
        .unwrap_or_else(|| panic!("{code} must fire; got {:?}", diags.diagnostics))
}

/// Whether any diagnostic carries `code`.
fn has_warning(diags: &PublishDiagnosticsParams, code: &str) -> bool {
    diags
        .diagnostics
        .iter()
        .any(|d| d.code == Some(NumberOrString::String(code.to_string())))
}

/// Request codeAction for `diag`, resolve the action whose title contains `title_needle`, and return
/// its resolved `WorkspaceEdit`. Panics if no such action is offered (used where the fix MUST appear).
fn resolve_fix_edit(
    client: &Connection,
    base_id: i32,
    uri: &Uri,
    diag: &Diagnostic,
    title_needle: &str,
) -> lsp_types::WorkspaceEdit {
    let actions = request_code_action(client, base_id, uri, diag.range, vec![diag.clone()], None);
    let action = find_action(&actions, title_needle).unwrap_or_else(|| {
        panic!(
            "expected an action whose title contains {title_needle:?}; got titles {:?}",
            action_titles(&actions)
        )
    });
    let resolved = resolve_action(client, base_id + 1, action);
    resolved.edit.expect("resolve must fill the edit")
}

/// The `CodeAction` whose title contains `needle`, if offered.
fn find_action(actions: &CodeActionResponse, needle: &str) -> Option<CodeAction> {
    actions.iter().find_map(|a| match a {
        CodeActionOrCommand::CodeAction(ca) if ca.title.contains(needle) => Some(ca.clone()),
        _ => None,
    })
}

/// Every offered action's title (CodeAction or Command) — for assertion messages.
fn action_titles(actions: &CodeActionResponse) -> Vec<String> {
    actions
        .iter()
        .map(|a| match a {
            CodeActionOrCommand::CodeAction(ca) => ca.title.clone(),
            CodeActionOrCommand::Command(c) => c.title.clone(),
        })
        .collect()
}

// ---------------------------------------------------------------------------------------------------
// Fix 1: `_`-prefix for UNUSED_VARIABLE / UNUSED_PARAMETER
// ---------------------------------------------------------------------------------------------------

/// UNUSED_VARIABLE round-trip: `var dead = 1` (unused) → renamed to `_dead`; re-analyze CLEAN (the
/// warning is gone, no new diagnostic by identity).
#[test]
fn underscore_prefix_clears_unused_variable() {
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
    let before = diag_identities(&diags);
    let diag = diag_with_warning(&diags, "UNUSED_VARIABLE");

    let edit = resolve_fix_edit(&client, 10, &uri, &diag, "Prefix unused name");
    // The edit must touch the `dead` declaration identifier (range, not the whole statement).
    let tes = all_text_edits(&edit);
    assert!(!tes.is_empty(), "the rename must produce at least one edit");
    assert!(
        tes.iter().all(|te| te.new_text == "_dead"),
        "every edit replaces with the `_`-prefixed name; got {tes:?}"
    );

    let patched = apply_text_edits(UNUSED_VAR_SRC, tes);
    let after = reopen_and_diags(&p, &client, "b.gd", &patched, 100);
    assert!(
        !has_warning(&after, "UNUSED_VARIABLE"),
        "UNUSED_VARIABLE must be cleared; got {:?}",
        after.diagnostics
    );
    let after_ids = diag_identities(&after);
    let induced: Vec<_> = after_ids.difference(&before).collect();
    assert!(
        induced.is_empty(),
        "no NEW diagnostic may appear after the fix; induced {induced:?}\npatched:\n{patched}"
    );
    shutdown(&client, t);
}

/// ADVERSARIAL (assigned-unused → the dangling write): `var x = 1; x = 2` (written, never read).
/// Since #464 a write is not a use, so this DOES warn and the `_`-prefix fix IS offered — which puts
/// the spec's worst case squarely in reach: renaming ONLY the declaration would leave `x = 2`
/// dangling and turn a warning into a broken script. The binding-correct collection behind the fix
/// (`push_identifier_locations_within`, handlers.rs) is what prevents it, and this test pins the
/// property end to end: both occurrences are rewritten, and the patched source re-analyzes with no
/// induced diagnostic — an undeclared-identifier error being exactly what a dangling write produces.
#[test]
fn underscore_prefix_rewrites_the_write_too() {
    const SRC: &str = "extends Node\n\n\nfunc f() -> void:\n\tvar x = 1\n\tx = 2\n\tprint(0)\n";
    let p = base_project();
    let (server, client) = Connection::memory();
    let t = std::thread::spawn(move || gd_server::serve(server));
    let (_r, diags) = init_open(&p, &client, &[("a.gd", SRC)], caps(true, true, true));
    let uri = file_uri(&p.root.join("a.gd"));
    let before = diag_identities(&diags);
    let diag = diag_with_warning(&diags, "UNUSED_VARIABLE");

    let edit = resolve_fix_edit(&client, 10, &uri, &diag, "Prefix unused name");
    let tes = all_text_edits(&edit);
    assert_eq!(
        tes.len(),
        2,
        "the declaration AND the write must both be rewritten; got {tes:?}"
    );
    assert!(
        tes.iter().all(|te| te.new_text == "_x"),
        "every edit replaces with the `_`-prefixed name; got {tes:?}"
    );

    let patched = apply_text_edits(SRC, tes);
    assert!(
        patched.contains("var _x = 1") && patched.contains("_x = 2"),
        "no occurrence may be left dangling; got\n{patched}"
    );
    let after = reopen_and_diags(&p, &client, "b.gd", &patched, 100);
    assert!(
        !has_warning(&after, "UNUSED_VARIABLE"),
        "UNUSED_VARIABLE must be cleared; got {:?}",
        after.diagnostics
    );
    let after_ids = diag_identities(&after);
    let induced: Vec<_> = after_ids.difference(&before).collect();
    assert!(
        induced.is_empty(),
        "no NEW diagnostic may appear after the fix; induced {induced:?}\npatched:\n{patched}"
    );
    shutdown(&client, t);
}

/// ADVERSARIAL (write-only local with a same-named write in ANOTHER function): renaming `f`'s
/// unused `x` must carry `f`'s write and leave `g`'s distinct `x` alone. The gate admits write
/// sites by NAME (an outer bound), so what keeps `g` out is `rename`'s binding-correct resolution —
/// this pins that the two layers compose instead of the looser one winning.
#[test]
fn underscore_prefix_write_does_not_reach_another_function() {
    const SRC: &str = "extends Node\n\n\nfunc f() -> void:\n\tvar x = 1\n\tx = 2\n\n\nfunc g() -> void:\n\tvar x = 3\n\tx = 4\n\tprint(x)\n";
    let p = base_project();
    let (server, client) = Connection::memory();
    let t = std::thread::spawn(move || gd_server::serve(server));
    let (_r, diags) = init_open(&p, &client, &[("a.gd", SRC)], caps(true, true, true));
    let uri = file_uri(&p.root.join("a.gd"));
    let diag = diag_with_warning(&diags, "UNUSED_VARIABLE");
    assert_eq!(diag.range.start.line, 4, "the warning is on f's `x`");

    let edit = resolve_fix_edit(&client, 10, &uri, &diag, "Prefix unused name");
    let tes = all_text_edits(&edit);
    assert_eq!(tes.len(), 2, "only f's two occurrences; got {tes:?}");
    assert!(
        tes.iter().all(|te| te.range.start.line < 6),
        "no edit may reach g; got {tes:?}"
    );

    let patched = apply_text_edits(SRC, tes);
    assert!(
        patched.contains("var _x = 1")
            && patched.contains("\t_x = 2")
            && patched.contains("var x = 3"),
        "g's binding must be untouched; got\n{patched}"
    );
    shutdown(&client, t);
}

/// ADVERSARIAL (attribute access + cross-scope NOT rewritten): an unused PARAMETER `pos` in `f`, with
/// member accesses `self.pos` / `n.pos` of the SAME name in a DIFFERENT function `g`. Renaming the
/// param `pos`→`_pos` must rewrite ONLY the param (scoped to `f`), NEVER the `.pos` attribute accesses
/// in `g` (those are members — a different symbol; rewriting them would dangle). This proves both the
/// scope boundary AND the attribute-position exclusion of the reused binding-correct local resolution
/// (`push_identifier_locations_within`, handlers.rs).
///
/// (UNUSED_VARIABLE can't co-occur with a same-name attribute in ONE function: gdls's unused sweep
/// over-approximates "used" by ANY in-scope identifier incl. an attribute ident, so the attribute
/// would suppress the warning. The cross-function parameter case is the faithful adversarial shape.)
#[test]
fn underscore_prefix_excludes_attribute_access_and_other_scope() {
    // `pos` is an unused PARAM of `f`; `self.pos` / `n.pos` in `g` are MEMBER accesses (other scope).
    const SRC: &str = "extends Node\n\nvar pos: Node\n\nfunc f(pos: Node) -> void:\n\tprint(0)\n\nfunc g(n: Node) -> void:\n\tself.pos = n\n\tn.pos = self\n";
    let p = base_project();
    let (server, client) = Connection::memory();
    let t = std::thread::spawn(move || gd_server::serve(server));
    let (_r, diags) = init_open(&p, &client, &[("a.gd", SRC)], caps(true, true, true));
    let uri = file_uri(&p.root.join("a.gd"));

    // The UNUSED_PARAMETER must be the param `pos` of `f` (line 4).
    let diag = diags
        .diagnostics
        .iter()
        .find(|d| {
            d.code == Some(NumberOrString::String("UNUSED_PARAMETER".to_string()))
                && d.range.start.line == 4
        })
        .cloned()
        .unwrap_or_else(|| {
            panic!(
                "the unused PARAM `pos` of `f` (line 4) must warn; got {:?}",
                diags.diagnostics
            )
        });

    let edit = resolve_fix_edit(&client, 10, &uri, &diag, "Prefix unused name");
    let patched = apply_text_edits(SRC, all_text_edits(&edit));
    // The attribute accesses in `g` MUST be untouched; only the param in `f` renamed.
    assert!(
        patched.contains("self.pos = n"),
        "`self.pos` (member access, other scope) must NOT be rewritten; patched:\n{patched}"
    );
    assert!(
        patched.contains("n.pos = self"),
        "`n.pos` (member access, other scope) must NOT be rewritten; patched:\n{patched}"
    );
    assert!(
        patched.contains("func f(_pos: Node)"),
        "the param must be renamed to `_pos`; patched:\n{patched}"
    );
    // The class member `var pos` must also be untouched (the param shadows it within `f` only).
    assert!(
        patched.contains("var pos: Node"),
        "the class member `pos` must NOT be renamed; patched:\n{patched}"
    );
    // Re-analyze: no errors (no dangling member reference).
    let after = reopen_and_diags(&p, &client, "b.gd", &patched, 100);
    assert!(
        !after
            .diagnostics
            .iter()
            .any(|d| d.severity == Some(lsp_types::DiagnosticSeverity::ERROR)),
        "no errors after the fix (member accesses intact); got {:?}\npatched:\n{patched}",
        after.diagnostics
    );
    assert!(
        !has_warning(&after, "UNUSED_PARAMETER"),
        "UNUSED_PARAMETER cleared; got {:?}",
        after.diagnostics
    );
    shutdown(&client, t);
}

/// ADVERSARIAL (shadowing): a local `value` shadows a class member `value`. Renaming the unused local
/// to `_value` must rewrite ONLY the local's occurrences, never the member declaration or its other
/// uses. The reused rename skips member-first canonicalization for locals exactly to avoid jumping to
/// the member.
#[test]
fn underscore_prefix_shadowing_renames_only_local() {
    // Member `value` is used elsewhere (in `g`); the unused LOCAL `value` in `f` shadows it.
    const SRC: &str = "extends Node\n\nvar value := 10\n\nfunc f() -> void:\n\tvar value = 1\n\tprint(0)\n\nfunc g() -> int:\n\treturn value\n";
    let p = base_project();
    let (server, client) = Connection::memory();
    let t = std::thread::spawn(move || gd_server::serve(server));
    let (_r, diags) = init_open(&p, &client, &[("a.gd", SRC)], caps(true, true, true));
    let uri = file_uri(&p.root.join("a.gd"));

    let diag = diags
        .diagnostics
        .iter()
        .find(|d| {
            d.code == Some(NumberOrString::String("UNUSED_VARIABLE".to_string()))
                && d.range.start.line == 5
        })
        .cloned()
        .unwrap_or_else(|| {
            panic!(
                "the unused LOCAL `value` (line 5) must warn; got {:?}",
                diags.diagnostics
            )
        });

    let edit = resolve_fix_edit(&client, 10, &uri, &diag, "Prefix unused name");
    let patched = apply_text_edits(SRC, all_text_edits(&edit));
    // The member declaration and the `return value` in `g` MUST be untouched; only the local renamed.
    assert!(
        patched.contains("var value := 10"),
        "the class member `value` declaration must NOT be renamed; patched:\n{patched}"
    );
    assert!(
        patched.contains("return value\n"),
        "the member use in `g` must NOT be renamed; patched:\n{patched}"
    );
    assert!(
        patched.contains("var _value = 1"),
        "the shadowing local must be renamed to `_value`; patched:\n{patched}"
    );
    let after = reopen_and_diags(&p, &client, "b.gd", &patched, 100);
    assert!(
        !after
            .diagnostics
            .iter()
            .any(|d| d.severity == Some(lsp_types::DiagnosticSeverity::ERROR)),
        "no errors after the fix; got {:?}\npatched:\n{patched}",
        after.diagnostics
    );
    shutdown(&client, t);
}

/// ADVERSARIAL (collision refusal): if `_name` would collide with an existing in-scope symbol, the
/// fix must be REFUSED (not offered) — applying a colliding rename would corrupt. Here a function has
/// both an unused param `x` and a local `_x`; renaming `x`→`_x` collides, so no `_`-prefix action is
/// offered for the param (only the suppression).
#[test]
fn underscore_prefix_refuses_on_collision() {
    // Param `x` is unused; a local `_x` already exists in the same function → `x`→`_x` collides.
    const SRC: &str = "extends Node\n\n\nfunc f(x: int) -> void:\n\tvar _x = 5\n\tprint(_x)\n";
    let p = base_project();
    let (server, client) = Connection::memory();
    let t = std::thread::spawn(move || gd_server::serve(server));
    let (_r, diags) = init_open(&p, &client, &[("a.gd", SRC)], caps(true, true, true));
    let uri = file_uri(&p.root.join("a.gd"));
    let diag = diag_with_warning(&diags, "UNUSED_PARAMETER");

    let actions = request_code_action(&client, 10, &uri, diag.range, vec![diag], None);
    // The `_`-prefix fix must NOT be offered (it would collide); the suppression still may be.
    assert!(
        find_action(&actions, "Prefix unused name").is_none(),
        "a `_`-prefix that would COLLIDE with `_x` must be refused; got titles {:?}",
        action_titles(&actions)
    );
    shutdown(&client, t);
}

/// UNUSED_PARAMETER round-trip: an unused parameter → renamed to `_`+name; re-analyze CLEAN.
#[test]
fn underscore_prefix_clears_unused_parameter() {
    const SRC: &str = "extends Node\n\n\nfunc f(unused_arg: int) -> void:\n\tprint(0)\n";
    let p = base_project();
    let (server, client) = Connection::memory();
    let t = std::thread::spawn(move || gd_server::serve(server));
    let (_r, diags) = init_open(&p, &client, &[("a.gd", SRC)], caps(true, true, true));
    let uri = file_uri(&p.root.join("a.gd"));
    let before = diag_identities(&diags);
    let diag = diag_with_warning(&diags, "UNUSED_PARAMETER");

    let edit = resolve_fix_edit(&client, 10, &uri, &diag, "Prefix unused name");
    let patched = apply_text_edits(SRC, all_text_edits(&edit));
    assert!(
        patched.contains("_unused_arg"),
        "the parameter must be renamed to `_unused_arg`; patched:\n{patched}"
    );
    let after = reopen_and_diags(&p, &client, "b.gd", &patched, 100);
    assert!(
        !has_warning(&after, "UNUSED_PARAMETER"),
        "UNUSED_PARAMETER must be cleared; got {:?}",
        after.diagnostics
    );
    let induced: Vec<_> = diag_identities(&after)
        .difference(&before)
        .cloned()
        .collect();
    assert!(
        induced.is_empty(),
        "no NEW diagnostic after the fix; induced {induced:?}\npatched:\n{patched}"
    );
    shutdown(&client, t);
}

/// UNUSED_PRIVATE_CLASS_VARIABLE must NOT get a `_`-prefix fix: it fires *because* the var is already
/// `_`-prefixed and unused, so `__x` would still warn (the fix wouldn't clear its own diagnostic). The
/// suppression is still offered; the `_`-prefix is deliberately withheld.
#[test]
fn underscore_prefix_not_offered_for_private_class_variable() {
    const SRC: &str = "extends Node\n\nvar _unused_private = 1\n\nfunc f() -> void:\n\tprint(0)\n";
    let p = base_project();
    let (server, client) = Connection::memory();
    let t = std::thread::spawn(move || gd_server::serve(server));
    let (_r, diags) = init_open(&p, &client, &[("a.gd", SRC)], caps(true, true, true));
    let uri = file_uri(&p.root.join("a.gd"));
    let diag = diag_with_warning(&diags, "UNUSED_PRIVATE_CLASS_VARIABLE");

    let actions = request_code_action(&client, 10, &uri, diag.range, vec![diag], None);
    assert!(
        find_action(&actions, "Prefix unused name").is_none(),
        "the `_`-prefix fix must NOT be offered for UNUSED_PRIVATE_CLASS_VARIABLE (it wouldn't \
         clear the warning); got titles {:?}",
        action_titles(&actions)
    );
    // But the suppression IS offered (it's a different, always-valid action).
    assert!(
        find_action(&actions, "Ignore").is_some(),
        "the suppression must still be offered; got titles {:?}",
        action_titles(&actions)
    );
    shutdown(&client, t);
}

/// UNUSED_PRIVATE_CLASS_VARIABLE gets a DELETE-fix ("Remove unused private variable") that removes the
/// whole declaration. Round-trip: `var _unused_private = 1` (unused, `_`-prefixed) → the declaration is
/// deleted; re-analyze CLEAN (the warning is gone, no new diagnostic by identity).
#[test]
fn delete_fix_offered_for_private_class_variable() {
    const SRC: &str = "extends Node\n\nvar _unused_private = 1\n\nfunc f() -> void:\n\tprint(0)\n";
    let p = base_project();
    let (server, client) = Connection::memory();
    let t = std::thread::spawn(move || gd_server::serve(server));
    let (_r, diags) = init_open(&p, &client, &[("a.gd", SRC)], caps(true, true, true));
    let uri = file_uri(&p.root.join("a.gd"));
    let before = diag_identities(&diags);
    let diag = diag_with_warning(&diags, "UNUSED_PRIVATE_CLASS_VARIABLE");

    let edit = resolve_fix_edit(&client, 10, &uri, &diag, "Remove unused private variable");
    let tes = all_text_edits(&edit);
    assert_eq!(tes.len(), 1, "one deletion; got {tes:?}");

    let patched = apply_text_edits(SRC, tes);
    let after = reopen_and_diags(&p, &client, "b.gd", &patched, 100);
    assert!(
        !has_warning(&after, "UNUSED_PRIVATE_CLASS_VARIABLE"),
        "UNUSED_PRIVATE_CLASS_VARIABLE must be cleared; got {:?}",
        after.diagnostics
    );
    let induced: Vec<_> = diag_identities(&after)
        .difference(&before)
        .cloned()
        .collect();
    assert!(
        induced.is_empty(),
        "no NEW diagnostic after the delete-fix; induced {induced:?}\npatched:\n{patched}"
    );
    shutdown(&client, t);
}

/// The delete-fix deletes the WHOLE declaration including a leading annotation: `@export var _x = 1`
/// (unused, `_`-prefixed) → both the `@export` line and the `var` line removed. The patched file
/// re-analyzes CLEAN (no dangling `@export`, no leftover blank line, no new diagnostic).
#[test]
fn delete_fix_removes_annotated_private_var_whole() {
    const SRC: &str = "extends Node\n\n@export var _x = 1\n\nfunc f() -> void:\n\tprint(0)\n";
    let p = base_project();
    let (server, client) = Connection::memory();
    let t = std::thread::spawn(move || gd_server::serve(server));
    let (_r, diags) = init_open(&p, &client, &[("a.gd", SRC)], caps(true, true, true));
    let uri = file_uri(&p.root.join("a.gd"));
    let before = diag_identities(&diags);
    let diag = diag_with_warning(&diags, "UNUSED_PRIVATE_CLASS_VARIABLE");

    let edit = resolve_fix_edit(&client, 10, &uri, &diag, "Remove unused private variable");
    let tes = all_text_edits(&edit);
    let patched = apply_text_edits(SRC, tes);
    // Neither the annotation nor the declaration may survive.
    assert!(
        !patched.contains("@export") && !patched.contains("_x"),
        "the whole annotated declaration must be removed; got:\n{patched}"
    );
    let after = reopen_and_diags(&p, &client, "b.gd", &patched, 100);
    assert!(
        !has_warning(&after, "UNUSED_PRIVATE_CLASS_VARIABLE"),
        "the warning must be cleared; got {:?}",
        after.diagnostics
    );
    let induced: Vec<_> = diag_identities(&after)
        .difference(&before)
        .cloned()
        .collect();
    assert!(
        induced.is_empty(),
        "no NEW diagnostic after deleting the annotated declaration; induced {induced:?}\n\
         patched:\n{patched}"
    );
    shutdown(&client, t);
}

/// ADVERSARIAL (#204, cross-file read): the warning and the error backstop are both SINGLE-FILE, so a
/// private member read from ANOTHER file through this script's `class_name` — `var a: A` then `a._x`,
/// both in function BODIES, invisible to the interface index and the dep graph — is reported unused
/// here. Deleting it does not error (a missing member on a script-class base degrades to `Variant`,
/// faithfully) but silently de-types the consumer, so the delete-fix must be REFUSED.
#[test]
fn delete_fix_refused_when_a_private_member_is_read_cross_file() {
    const SRC: &str = "class_name A\nextends Node\n\nvar _x = 1\n\nfunc f() -> void:\n\tprint(0)\n";
    const CONSUMER: &str =
        "extends Node\n\nfunc g() -> void:\n\tvar a: A = A.new()\n\tprint(a._x)\n";
    let p = base_project();
    p.write("consumer.gd", CONSUMER);
    let (server, client) = Connection::memory();
    let t = std::thread::spawn(move || gd_server::serve(server));
    let (_r, diags) = init_open(&p, &client, &[("a.gd", SRC)], caps(true, true, true));
    let uri = file_uri(&p.root.join("a.gd"));
    let diag = diag_with_warning(&diags, "UNUSED_PRIVATE_CLASS_VARIABLE");

    let actions = request_code_action(&client, 10, &uri, diag.range, vec![diag], None);
    assert!(
        find_action(&actions, "Remove unused private variable").is_none(),
        "the delete-fix must be REFUSED while another file reads `a._x` (deleting it silently \
         de-types that consumer); got titles {:?}",
        action_titles(&actions)
    );
    // The suppression is still offered — it changes nothing outside this file.
    assert!(
        find_action(&actions, "Ignore").is_some(),
        "the suppression must still be offered; got titles {:?}",
        action_titles(&actions)
    );
    shutdown(&client, t);
}

/// The #204 gate is fail-CLOSED and text-only, so it also refuses on a DYNAMIC read payload
/// (`a.get("_x")`) — the shape Godot's own unused-warning sweep credits in-file — and on an
/// attribute mention it cannot prove belongs to this script. Deliberate over-refusal: a false hit
/// only withholds a quickfix, it never applies a wrong edit.
#[test]
fn delete_fix_refused_for_a_quoted_cross_file_payload() {
    const SRC: &str = "class_name A\nextends Node\n\nvar _x = 1\n\nfunc f() -> void:\n\tprint(0)\n";
    const CONSUMER: &str = "extends Node\n\nfunc g(a: A) -> void:\n\tprint(a.get(\"_x\"))\n";
    let p = base_project();
    p.write("consumer.gd", CONSUMER);
    let (server, client) = Connection::memory();
    let t = std::thread::spawn(move || gd_server::serve(server));
    let (_r, diags) = init_open(&p, &client, &[("a.gd", SRC)], caps(true, true, true));
    let uri = file_uri(&p.root.join("a.gd"));
    let diag = diag_with_warning(&diags, "UNUSED_PRIVATE_CLASS_VARIABLE");

    let actions = request_code_action(&client, 10, &uri, diag.range, vec![diag], None);
    assert!(
        find_action(&actions, "Remove unused private variable").is_none(),
        "the delete-fix must be REFUSED while another file names `\"_x\"` as a dynamic payload; \
         got titles {:?}",
        action_titles(&actions)
    );
    shutdown(&client, t);
}

/// The gate must not swallow the fix wholesale: another file that merely mentions a LONGER name
/// sharing the prefix (`a._x_ray`) or the bare word (`_x` as its own local) is not a read of THIS
/// member, so the delete-fix is still offered and still deletes the declaration.
#[test]
fn delete_fix_still_offered_when_other_files_only_look_similar() {
    const SRC: &str = "class_name A\nextends Node\n\nvar _x = 1\n\nfunc f() -> void:\n\tprint(0)\n";
    const NEIGHBOR: &str =
        "extends Node\n\nfunc g(a: A) -> void:\n\tprint(a._x_ray)\n\nfunc h() -> void:\n\tvar _x = 2\n\tprint(_x)\n";
    let p = base_project();
    p.write("neighbor.gd", NEIGHBOR);
    let (server, client) = Connection::memory();
    let t = std::thread::spawn(move || gd_server::serve(server));
    let (_r, diags) = init_open(&p, &client, &[("a.gd", SRC)], caps(true, true, true));
    let uri = file_uri(&p.root.join("a.gd"));
    let diag = diag_with_warning(&diags, "UNUSED_PRIVATE_CLASS_VARIABLE");

    let edit = resolve_fix_edit(&client, 10, &uri, &diag, "Remove unused private variable");
    let patched = apply_text_edits(SRC, all_text_edits(&edit));
    assert!(
        !patched.contains("var _x"),
        "the declaration must still be deleted when no other file READS it; got:\n{patched}"
    );
    shutdown(&client, t);
}

/// ADVERSARIAL (comment-overlap refusal): a trailing comment on the declaration line lives in an
/// AST-invisible side-channel, so a line-range deletion would silently eat it. The delete-fix must be
/// REFUSED when a comment overlaps the deletion range (the suppression is still offered).
#[test]
fn delete_fix_refused_when_comment_in_range() {
    const SRC: &str =
        "extends Node\n\nvar _unused = 1  # keep this note\n\nfunc f() -> void:\n\tprint(0)\n";
    let p = base_project();
    let (server, client) = Connection::memory();
    let t = std::thread::spawn(move || gd_server::serve(server));
    let (_r, diags) = init_open(&p, &client, &[("a.gd", SRC)], caps(true, true, true));
    let uri = file_uri(&p.root.join("a.gd"));
    let diag = diag_with_warning(&diags, "UNUSED_PRIVATE_CLASS_VARIABLE");

    let actions = request_code_action(&client, 10, &uri, diag.range, vec![diag], None);
    assert!(
        find_action(&actions, "Remove unused private variable").is_none(),
        "the delete-fix must be REFUSED when a comment overlaps the deletion range (it would eat \
         the comment); got titles {:?}",
        action_titles(&actions)
    );
    // The suppression is still offered (it's a pure insertion, never eats anything).
    assert!(
        find_action(&actions, "Ignore").is_some(),
        "the suppression must still be offered; got titles {:?}",
        action_titles(&actions)
    );
    shutdown(&client, t);
}

/// ADVERSARIAL (same-line `;`-separated sibling): GDScript allows `;` as a member-statement separator,
/// so `var _a = 1 ; var _b = 2` is TWO declarations on one physical line. Whole-line deletion of the
/// targeted unused var would also eat the sibling; when the sibling is itself unused the error backstop
/// sees no new diagnostic and the collateral deletion would go through silently. The fix must be
/// REFUSED (fail-closed) for both — the suppression stays available.
#[test]
fn delete_fix_refused_for_same_line_sibling_declaration() {
    const SRC: &str = "extends Node\n\nvar _a = 1 ; var _b = 2\n\nfunc f() -> void:\n\tprint(0)\n";
    let p = base_project();
    let (server, client) = Connection::memory();
    let t = std::thread::spawn(move || gd_server::serve(server));
    let (_r, diags) = init_open(&p, &client, &[("a.gd", SRC)], caps(true, true, true));
    let uri = file_uri(&p.root.join("a.gd"));
    // Both `_a` and `_b` are unused, `_`-prefixed private members → both warn.
    let warns: Vec<_> = diags
        .diagnostics
        .iter()
        .filter(|d| {
            d.code
                == Some(NumberOrString::String(
                    "UNUSED_PRIVATE_CLASS_VARIABLE".into(),
                ))
        })
        .cloned()
        .collect();
    assert_eq!(
        warns.len(),
        2,
        "both same-line private vars must warn; got {:?}",
        diags.diagnostics
    );
    for diag in warns {
        let actions = request_code_action(&client, 10, &uri, diag.range, vec![diag.clone()], None);
        assert!(
            find_action(&actions, "Remove unused private variable").is_none(),
            "the delete-fix must be REFUSED when another declaration shares the physical line (it \
             would eat the sibling); got titles {:?}",
            action_titles(&actions)
        );
        assert!(
            find_action(&actions, "Ignore").is_some(),
            "the suppression must still be offered; got titles {:?}",
            action_titles(&actions)
        );
    }
    shutdown(&client, t);
}

/// PROPERTY var: an unused `_`-prefixed private var with an inline getter/setter block. The Variable
/// node span covers the whole accessor block, so the delete-fix removes ALL of its lines (no dangling
/// `get:`/`set:` residue). Re-analyze CLEAN.
#[test]
fn delete_fix_removes_property_var_with_accessor_block() {
    const SRC: &str = "extends Node\n\nvar _p: int:\n\tget:\n\t\treturn 1\n\tset(v):\n\t\tpass\n\nfunc f() -> void:\n\tprint(0)\n";
    let p = base_project();
    let (server, client) = Connection::memory();
    let t = std::thread::spawn(move || gd_server::serve(server));
    let (_r, diags) = init_open(&p, &client, &[("a.gd", SRC)], caps(true, true, true));
    let uri = file_uri(&p.root.join("a.gd"));
    let before = diag_identities(&diags);
    let diag = diag_with_warning(&diags, "UNUSED_PRIVATE_CLASS_VARIABLE");

    let edit = resolve_fix_edit(&client, 10, &uri, &diag, "Remove unused private variable");
    let tes = all_text_edits(&edit);
    let patched = apply_text_edits(SRC, tes);
    // No fragment of the property declaration (name, get, set body) may survive.
    assert!(
        !patched.contains("_p") && !patched.contains("get:") && !patched.contains("set(v)"),
        "the whole property declaration incl. its accessor block must be removed; got:\n{patched}"
    );
    let after = reopen_and_diags(&p, &client, "b.gd", &patched, 100);
    assert!(
        !has_warning(&after, "UNUSED_PRIVATE_CLASS_VARIABLE"),
        "the warning must be cleared; got {:?}",
        after.diagnostics
    );
    let induced: Vec<_> = diag_identities(&after)
        .difference(&before)
        .cloned()
        .collect();
    assert!(
        induced.is_empty(),
        "no NEW diagnostic after deleting the property declaration; induced {induced:?}\n\
         patched:\n{patched}"
    );
    shutdown(&client, t);
}

/// ADVERSARIAL (error backstop refuses an inducing deletion): a `_`-prefixed private var that the
/// warning's name-set sweep believes is unused, but whose declaration is load-bearing — removing it
/// makes the file fail to analyze. Here `_t` is a type used by a typed member (`var keep: _t`); the
/// name-set sweep does NOT credit a TYPE-annotation mention as a use, so `_t` warns UNUSED — but
/// deleting its declaration leaves `var keep: _t` referencing an undefined type (a hard ERROR). The
/// [`edit_is_safe`] re-analysis must catch the induced error and REFUSE the delete-fix; the suppression
/// stays available. (Pins the backstop actually firing on a deletion, not just by inspection.)
#[test]
fn delete_fix_refused_when_deletion_induces_error() {
    // `_t` is an inner class used as a type; the unused sweep doesn't see the type-position mention.
    const SRC: &str =
        "extends Node\n\nclass _t:\n\tpass\n\nvar keep: _t\n\nfunc f() -> void:\n\tprint(keep)\n";
    let p = base_project();
    let (server, client) = Connection::memory();
    let t = std::thread::spawn(move || gd_server::serve(server));
    let (_r, diags) = init_open(&p, &client, &[("a.gd", SRC)], caps(true, true, true));
    let uri = file_uri(&p.root.join("a.gd"));
    // Only proceed if `_t` actually warns unused (the sweep doesn't credit the type mention); if a
    // future sweep change credits it, there's no diagnostic to fix and the case is moot.
    let Some(diag) = diags
        .diagnostics
        .iter()
        .find(|d| {
            d.code
                == Some(NumberOrString::String(
                    "UNUSED_PRIVATE_CLASS_VARIABLE".into(),
                ))
                && d.message.contains("_t")
        })
        .cloned()
    else {
        shutdown(&client, t);
        return;
    };
    let actions = request_code_action(&client, 10, &uri, diag.range, vec![diag], None);
    assert!(
        find_action(&actions, "Remove unused private variable").is_none(),
        "the delete-fix must be REFUSED when removing the declaration induces an error (the type \
         `_t` is still referenced by `var keep: _t`); got titles {:?}",
        action_titles(&actions)
    );
    assert!(
        find_action(&actions, "Ignore").is_some(),
        "the suppression must still be offered; got titles {:?}",
        action_titles(&actions)
    );
    shutdown(&client, t);
}

// ---------------------------------------------------------------------------------------------------
// Fix 2: add `@onready` for GET_NODE_DEFAULT_WITHOUT_ONREADY
// ---------------------------------------------------------------------------------------------------

/// GET_NODE_DEFAULT_WITHOUT_ONREADY round-trip: `var n = $Node` (in a Node class) → `@onready`
/// inserted above; re-analyze CLEAN (the warning is gone, no new diagnostic — crucially NOT an induced
/// ONREADY_WITH_EXPORT or @onready-Node error).
#[test]
fn add_onready_clears_get_node_default() {
    const SRC: &str = "extends Node\n\nvar n = $Node\n\nfunc f() -> void:\n\tprint(0)\n";
    let p = base_project();
    let (server, client) = Connection::memory();
    let t = std::thread::spawn(move || gd_server::serve(server));
    let (_r, diags) = init_open(&p, &client, &[("a.gd", SRC)], caps(true, true, true));
    let uri = file_uri(&p.root.join("a.gd"));
    let before = diag_identities(&diags);
    let diag = diag_with_warning(&diags, "GET_NODE_DEFAULT_WITHOUT_ONREADY");

    let edit = resolve_fix_edit(&client, 10, &uri, &diag, "@onready");
    let tes = all_text_edits(&edit);
    assert_eq!(tes.len(), 1, "one insertion; got {tes:?}");
    assert_eq!(
        tes[0].new_text, "@onready\n",
        "the insertion is `@onready` on its own line (no indent at class scope)"
    );
    assert_eq!(
        tes[0].range.start,
        Position {
            line: 2,
            character: 0
        },
        "inserted at col 0 of the `var n = $Node` line"
    );

    let patched = apply_text_edits(SRC, tes);
    let after = reopen_and_diags(&p, &client, "b.gd", &patched, 100);
    assert!(
        !has_warning(&after, "GET_NODE_DEFAULT_WITHOUT_ONREADY"),
        "the warning must be cleared; got {:?}",
        after.diagnostics
    );
    // CRUCIAL: adding @onready to this (Node-derived, no @export) var induces NOTHING.
    let induced: Vec<_> = diag_identities(&after)
        .difference(&before)
        .cloned()
        .collect();
    assert!(
        induced.is_empty(),
        "no NEW diagnostic (no induced ONREADY_WITH_EXPORT / @onready-Node error); induced \
         {induced:?}\npatched:\n{patched}"
    );
    assert!(
        !after
            .diagnostics
            .iter()
            .any(|d| d.severity == Some(lsp_types::DiagnosticSeverity::ERROR)),
        "no errors after the fix; got {:?}",
        after.diagnostics
    );
    shutdown(&client, t);
}

/// ADVERSARIAL (add-@onready induction refusal): a `var n = $Node` that ALSO has `@export` — adding
/// `@onready` would induce ONREADY_WITH_EXPORT. The fix must be REFUSED (the user can drop @export or
/// suppress instead). The diagnostic here is GET_NODE_DEFAULT_WITHOUT_ONREADY (which still fires; the
/// @export var has a get_node default and no @onready).
#[test]
fn add_onready_refused_when_export_present() {
    const SRC: &str = "extends Node\n\n@export var n = $Node\n\nfunc f() -> void:\n\tprint(0)\n";
    let p = base_project();
    let (server, client) = Connection::memory();
    let t = std::thread::spawn(move || gd_server::serve(server));
    let (_r, diags) = init_open(&p, &client, &[("a.gd", SRC)], caps(true, true, true));
    let uri = file_uri(&p.root.join("a.gd"));
    let diag = diag_with_warning(&diags, "GET_NODE_DEFAULT_WITHOUT_ONREADY");

    let actions = request_code_action(&client, 10, &uri, diag.range, vec![diag], None);
    assert!(
        find_action(&actions, "@onready").is_none(),
        "adding @onready when @export is present would induce ONREADY_WITH_EXPORT — must be \
         refused; got titles {:?}",
        action_titles(&actions)
    );
    shutdown(&client, t);
}

// ---------------------------------------------------------------------------------------------------
// Fix 3: drop a conflicting annotation for ONREADY_WITH_EXPORT (two directions)
// ---------------------------------------------------------------------------------------------------

/// ONREADY_WITH_EXPORT round-trip, BOTH directions: `@onready @export var conflict = ""` offers two
/// fixes; each clears the warning and re-analyzes CLEAN, keeping the OTHER annotation intact.
#[test]
fn drop_annotation_both_directions_clear_onready_with_export() {
    const SRC: &str =
        "extends Node\n\n@onready @export var conflict = \"\"\n\nfunc f() -> void:\n\tprint(0)\n";
    let p = base_project();
    let (server, client) = Connection::memory();
    let t = std::thread::spawn(move || gd_server::serve(server));
    let (_r, diags) = init_open(&p, &client, &[("a.gd", SRC)], caps(true, true, true));
    let uri = file_uri(&p.root.join("a.gd"));
    let diag = diag_with_warning(&diags, "ONREADY_WITH_EXPORT");

    let actions = request_code_action(&client, 10, &uri, diag.range, vec![diag.clone()], None);
    // BOTH directions must be offered.
    assert!(
        find_action(&actions, "Remove \"@onready\"").is_some(),
        "drop-@onready must be offered; got titles {:?}",
        action_titles(&actions)
    );
    assert!(
        find_action(&actions, "Remove \"@export\"").is_some(),
        "drop-@export must be offered; got titles {:?}",
        action_titles(&actions)
    );

    // Direction A: drop @onready → `@export var conflict = ""`.
    let edit_a = resolve_fix_edit(&client, 20, &uri, &diag, "Remove \"@onready\"");
    let patched_a = apply_text_edits(SRC, all_text_edits(&edit_a));
    assert!(
        patched_a.contains("@export var conflict"),
        "dropping @onready must KEEP @export; patched:\n{patched_a}"
    );
    assert!(
        !patched_a.contains("@onready"),
        "@onready must be gone; patched:\n{patched_a}"
    );
    let after_a = reopen_and_diags(&p, &client, "drop_onready.gd", &patched_a, 100);
    assert!(
        !has_warning(&after_a, "ONREADY_WITH_EXPORT"),
        "ONREADY_WITH_EXPORT cleared by dropping @onready; got {:?}",
        after_a.diagnostics
    );
    assert!(
        !after_a
            .diagnostics
            .iter()
            .any(|d| d.severity == Some(lsp_types::DiagnosticSeverity::ERROR)),
        "no errors after dropping @onready; got {:?}\npatched:\n{patched_a}",
        after_a.diagnostics
    );

    // Direction B: drop @export → `@onready var conflict = ""`.
    let edit_b = resolve_fix_edit(&client, 30, &uri, &diag, "Remove \"@export\"");
    let patched_b = apply_text_edits(SRC, all_text_edits(&edit_b));
    assert!(
        patched_b.contains("@onready var conflict"),
        "dropping @export must KEEP @onready; patched:\n{patched_b}"
    );
    assert!(
        !patched_b.contains("@export"),
        "@export must be gone; patched:\n{patched_b}"
    );
    let after_b = reopen_and_diags(&p, &client, "drop_export.gd", &patched_b, 200);
    assert!(
        !has_warning(&after_b, "ONREADY_WITH_EXPORT"),
        "ONREADY_WITH_EXPORT cleared by dropping @export; got {:?}",
        after_b.diagnostics
    );
    assert!(
        !after_b
            .diagnostics
            .iter()
            .any(|d| d.severity == Some(lsp_types::DiagnosticSeverity::ERROR)),
        "no errors after dropping @export; got {:?}\npatched:\n{patched_b}",
        after_b.diagnostics
    );
    shutdown(&client, t);
}

/// ADVERSARIAL (over-delete the OTHER annotation): dropping @export from `@export @onready var x`
/// must delete ONLY `@export ` and KEEP `@onready` — a buggy "delete to var" would silently remove
/// @onready (which the identity round-trip can't see, since removing @onready induces no diagnostic).
/// This asserts the surviving annotation directly.
#[test]
fn drop_annotation_keeps_the_other_annotation_export_first() {
    // @export FIRST, @onready second — so dropping @export must stop at @onready, not run to `var`.
    const SRC: &str =
        "extends Node\n\n@export @onready var x = \"\"\n\nfunc f() -> void:\n\tprint(0)\n";
    let p = base_project();
    let (server, client) = Connection::memory();
    let t = std::thread::spawn(move || gd_server::serve(server));
    let (_r, diags) = init_open(&p, &client, &[("a.gd", SRC)], caps(true, true, true));
    let uri = file_uri(&p.root.join("a.gd"));
    let diag = diag_with_warning(&diags, "ONREADY_WITH_EXPORT");

    let edit = resolve_fix_edit(&client, 10, &uri, &diag, "Remove \"@export\"");
    let patched = apply_text_edits(SRC, all_text_edits(&edit));
    // @onready MUST survive; @export gone.
    assert!(
        patched.contains("@onready var x"),
        "dropping @export must KEEP @onready (not over-delete to `var`); patched:\n{patched}"
    );
    assert!(
        !patched.contains("@export"),
        "@export must be gone; patched:\n{patched}"
    );
    // The exact deletion was `@export ` (8 chars) — line 2 starts with `@onready` now.
    assert_eq!(
        patched.lines().nth(2),
        Some("@onready var x = \"\""),
        "exactly `@export ` was removed, leaving `@onready var x = \"\"`; patched:\n{patched}"
    );
    shutdown(&client, t);
}

/// ADVERSARIAL (drop-@onready induction refusal): `@onready @export var n = $Node` — dropping
/// @onready would induce GET_NODE_DEFAULT_WITHOUT_ONREADY (the initializer is a get_node default). The
/// drop-@onready direction must be REFUSED; drop-@export is still offered.
#[test]
fn drop_onready_refused_when_initializer_is_get_node() {
    const SRC: &str =
        "extends Node\n\n@onready @export var n = $Node\n\nfunc f() -> void:\n\tprint(0)\n";
    let p = base_project();
    let (server, client) = Connection::memory();
    let t = std::thread::spawn(move || gd_server::serve(server));
    let (_r, diags) = init_open(&p, &client, &[("a.gd", SRC)], caps(true, true, true));
    let uri = file_uri(&p.root.join("a.gd"));
    let diag = diag_with_warning(&diags, "ONREADY_WITH_EXPORT");

    let actions = request_code_action(&client, 10, &uri, diag.range, vec![diag.clone()], None);
    assert!(
        find_action(&actions, "Remove \"@onready\"").is_none(),
        "dropping @onready when the initializer is a get_node default would induce \
         GET_NODE_DEFAULT_WITHOUT_ONREADY — must be refused; got titles {:?}",
        action_titles(&actions)
    );
    // drop @export is still safe and offered.
    assert!(
        find_action(&actions, "Remove \"@export\"").is_some(),
        "drop-@export must still be offered; got titles {:?}",
        action_titles(&actions)
    );
    // And applying drop-@export clears the warning cleanly (it becomes `@onready var n = $Node`).
    let edit = resolve_fix_edit(&client, 20, &uri, &diag, "Remove \"@export\"");
    let patched = apply_text_edits(SRC, all_text_edits(&edit));
    assert!(
        patched.contains("@onready var n = $Node"),
        "patched:\n{patched}"
    );
    let after = reopen_and_diags(&p, &client, "b.gd", &patched, 100);
    assert!(
        !has_warning(&after, "ONREADY_WITH_EXPORT")
            && !has_warning(&after, "GET_NODE_DEFAULT_WITHOUT_ONREADY"),
        "dropping @export clears the conflict without inducing the get_node warning; got {:?}",
        after.diagnostics
    );
    shutdown(&client, t);
}

/// ADVERSARIAL (comment in deletion range): a comment between the two annotations
/// (`@onready # keep\n@export var x`). Dropping @onready would span the comment → the byte-range
/// delete would silently eat `# keep`. The drop-@onready direction must be REFUSED (refuse rather
/// than corrupt). (drop-@export is unaffected — no comment in ITS range.)
#[test]
fn drop_annotation_refused_when_comment_in_range() {
    // @onready on its own line with a trailing comment, @export on the next line.
    const SRC: &str = "extends Node\n\n@onready # keep me\n@export var x = \"\"\n\nfunc f() -> void:\n\tprint(0)\n";
    let p = base_project();
    let (server, client) = Connection::memory();
    let t = std::thread::spawn(move || gd_server::serve(server));
    let (_r, diags) = init_open(&p, &client, &[("a.gd", SRC)], caps(true, true, true));
    let uri = file_uri(&p.root.join("a.gd"));
    let diag = diag_with_warning(&diags, "ONREADY_WITH_EXPORT");

    let actions = request_code_action(&client, 10, &uri, diag.range, vec![diag], None);
    // Dropping @onready would delete from `@onready` to `@export`'s start — that range contains the
    // `# keep me` comment → REFUSE.
    assert!(
        find_action(&actions, "Remove \"@onready\"").is_none(),
        "dropping @onready would eat the `# keep me` comment — must be refused; got titles {:?}",
        action_titles(&actions)
    );
    shutdown(&client, t);
}

// ---------------------------------------------------------------------------------------------------
// Fix 4: source.fixAll
// ---------------------------------------------------------------------------------------------------

/// source.fixAll aggregates the SAFE fixes (a `_`-prefix unused var AND an `@onready` get_node
/// default) into ONE WorkspaceEdit; applying it clears BOTH warnings; the @warning_ignore suppression
/// is NEVER included. The request uses `only: ["source.fixAll"]`.
#[test]
fn fix_all_aggregates_safe_fixes_only() {
    // Two independent fixable warnings: an unused local (in `f`) and a get_node default (class member).
    const SRC: &str =
        "extends Node\n\nvar n = $Node\n\nfunc f() -> void:\n\tvar dead = 1\n\tprint(0)\n";
    let p = base_project();
    let (server, client) = Connection::memory();
    let t = std::thread::spawn(move || gd_server::serve(server));
    let (_r, diags) = init_open(&p, &client, &[("a.gd", SRC)], caps(true, true, true));
    let uri = file_uri(&p.root.join("a.gd"));
    let unused = diag_with_warning(&diags, "UNUSED_VARIABLE");
    let getnode = diag_with_warning(&diags, "GET_NODE_DEFAULT_WITHOUT_ONREADY");

    // Request source.fixAll over both diagnostics.
    let actions = request_code_action(
        &client,
        10,
        &uri,
        Range {
            start: Position {
                line: 0,
                character: 0,
            },
            end: Position {
                line: 6,
                character: 0,
            },
        },
        vec![unused.clone(), getnode.clone()],
        Some(vec![CodeActionKind::from("source.fixAll".to_string())]),
    );
    // Exactly one source.fixAll action; NO quickfix / suppression in a fixAll sweep.
    assert_eq!(
        actions.len(),
        1,
        "exactly one fixAll action; got {actions:?}"
    );
    let CodeActionOrCommand::CodeAction(action) = actions.into_iter().next().unwrap() else {
        panic!("fixAll must be a CodeAction");
    };
    assert_eq!(
        action.kind,
        Some(CodeActionKind::SOURCE_FIX_ALL),
        "kind must be source.fixAll"
    );
    let edit = action.edit.expect("fixAll edit is eager");
    let tes = all_text_edits(&edit);
    // Two edits: the rename of `dead`→`_dead` and the `@onready` insertion. Neither is a suppression.
    assert!(
        tes.iter().any(|te| te.new_text == "_dead"),
        "fixAll must include the `_`-prefix rename; got {tes:?}"
    );
    assert!(
        tes.iter().any(|te| te.new_text == "@onready\n"),
        "fixAll must include the @onready insertion; got {tes:?}"
    );
    assert!(
        !tes.iter().any(|te| te.new_text.contains("@warning_ignore")),
        "fixAll must NEVER include the @warning_ignore suppression; got {tes:?}"
    );

    let patched = apply_text_edits(SRC, tes);
    let after = reopen_and_diags(&p, &client, "b.gd", &patched, 100);
    assert!(
        !has_warning(&after, "UNUSED_VARIABLE")
            && !has_warning(&after, "GET_NODE_DEFAULT_WITHOUT_ONREADY"),
        "both warnings cleared by the aggregate; got {:?}\npatched:\n{patched}",
        after.diagnostics
    );
    assert!(
        !after
            .diagnostics
            .iter()
            .any(|d| d.severity == Some(lsp_types::DiagnosticSeverity::ERROR)),
        "no errors after the aggregate fix; got {:?}\npatched:\n{patched}",
        after.diagnostics
    );
    shutdown(&client, t);
}

/// source.fixAll EXCLUDES ONREADY_WITH_EXPORT (two valid directions, no canonical choice). A buffer
/// whose only fixable warning is the conflict yields NO fixAll action (nothing safe to auto-apply).
#[test]
fn fix_all_excludes_onready_with_export() {
    const SRC: &str =
        "extends Node\n\n@onready @export var conflict = \"\"\n\nfunc f() -> void:\n\tprint(0)\n";
    let p = base_project();
    let (server, client) = Connection::memory();
    let t = std::thread::spawn(move || gd_server::serve(server));
    let (_r, diags) = init_open(&p, &client, &[("a.gd", SRC)], caps(true, true, true));
    let uri = file_uri(&p.root.join("a.gd"));
    let diag = diag_with_warning(&diags, "ONREADY_WITH_EXPORT");

    let actions = request_code_action(
        &client,
        10,
        &uri,
        Range {
            start: Position {
                line: 0,
                character: 0,
            },
            end: Position {
                line: 5,
                character: 0,
            },
        },
        vec![diag],
        Some(vec![CodeActionKind::from("source.fixAll".to_string())]),
    );
    assert!(
        actions.is_empty(),
        "ONREADY_WITH_EXPORT has two valid directions — fixAll must offer NOTHING; got {actions:?}"
    );
    shutdown(&client, t);
}

// ---------------------------------------------------------------------------------------------------
// ERROR-backstop regression tests (corruption blockers caught by the adversarial review)
// ---------------------------------------------------------------------------------------------------

/// REGRESSION (blocker — @onready on a non-Node class): `extends Object` (NOT Node-derived) with a
/// bare `get_node(...)` default fires GET_NODE_DEFAULT_WITHOUT_ONREADY (which keys on initializer
/// shape, no node-ness gate), but adding `@onready` would induce the hard error
/// `"@onready" can only be used in classes that inherit "Node"`. The ERROR BACKSTOP must WITHHOLD the
/// `@onready` fix (only the suppression remains).
#[test]
fn add_onready_refused_on_non_node_class() {
    const SRC: &str =
        "extends Object\n\nvar n = get_node(\"Child\")\n\nfunc f() -> void:\n\tprint(0)\n";
    let p = base_project();
    let (server, client) = Connection::memory();
    let t = std::thread::spawn(move || gd_server::serve(server));
    let (_r, diags) = init_open(&p, &client, &[("a.gd", SRC)], caps(true, true, true));
    let uri = file_uri(&p.root.join("a.gd"));
    let diag = diag_with_warning(&diags, "GET_NODE_DEFAULT_WITHOUT_ONREADY");
    let actions = request_code_action(&client, 10, &uri, diag.range, vec![diag], None);
    assert!(
        find_action(&actions, "@onready").is_none(),
        "adding @onready to a non-Node class would induce a hard error — the ERROR backstop must \
         REFUSE it; got titles {:?}",
        action_titles(&actions)
    );
    // The suppression is still available (it never induces an error).
    assert!(
        find_action(&actions, "Ignore").is_some(),
        "the suppression must remain; got titles {:?}",
        action_titles(&actions)
    );
    shutdown(&client, t);
}

/// SCOPE-AWARE (#107): a forward-referenced member is NOT hijacked. `print(y)` (before the local
/// declaration) binds to the class MEMBER `var y = 0`; `var y = 1` is the unused local. The old
/// name-based function-wide scan over-captured `print(y)` into the local's rename set, so the count
/// gate / ERROR backstop had to REFUSE. With scope-aware resolution the forward-ref binds outward
/// (the local is declared AFTER it), so the fix renames ONLY `var y = 1`→`var _y = 1` — a precise,
/// safe one-edit rename that IS offered. Verified by apply→reanalyze: the member stays bound (no
/// dangling `_y`), no new error, and the warning clears.
#[test]
fn underscore_prefix_forward_ref_member_not_hijacked() {
    const SRC: &str = "extends Node\n\nvar y = 0\n\nfunc f() -> void:\n\tprint(y)\n\tvar y = 1\n";
    let p = base_project();
    let (server, client) = Connection::memory();
    let t = std::thread::spawn(move || gd_server::serve(server));
    let (_r, diags) = init_open(&p, &client, &[("a.gd", SRC)], caps(true, true, true));
    let uri = file_uri(&p.root.join("a.gd"));
    // Precondition: the input is valid (warnings only — any corruption would be NET-NEW).
    assert!(
        !diags
            .diagnostics
            .iter()
            .any(|d| d.severity == Some(lsp_types::DiagnosticSeverity::ERROR)),
        "precondition: input must be error-free; got {:?}",
        diags.diagnostics
    );
    let diag = diags
        .diagnostics
        .iter()
        .find(|d| {
            d.code == Some(NumberOrString::String("UNUSED_VARIABLE".to_string()))
                && d.range.start.line == 6
        })
        .cloned()
        .unwrap_or_else(|| {
            panic!(
                "the unused local `y` (line 6) must warn; got {:?}",
                diags.diagnostics
            )
        });
    let edit = resolve_fix_edit(&client, 10, &uri, &diag, "Prefix unused name");
    let patched = apply_text_edits(SRC, all_text_edits(&edit));
    // ONLY the unused local is renamed; the member decl and the forward-ref read stay verbatim.
    assert!(
        patched.contains("var _y = 1"),
        "the unused local must be renamed to `_y`; patched:\n{patched}"
    );
    assert!(
        patched.contains("var y = 0"),
        "the class member declaration must NOT be renamed; patched:\n{patched}"
    );
    assert!(
        patched.contains("print(y)"),
        "the forward-ref read (binds to the member) must NOT be rewritten; patched:\n{patched}"
    );
    // Apply→reanalyze: binding identity preserved — the member `y` is still declared/bound (no
    // dangling reference), no new error, and the unused-variable warning is gone.
    let after = reopen_and_diags(&p, &client, "b.gd", &patched, 100);
    assert!(
        !after
            .diagnostics
            .iter()
            .any(|d| d.severity == Some(lsp_types::DiagnosticSeverity::ERROR)),
        "no errors after the fix (forward-ref still bound to the member); got {:?}\npatched:\n{patched}",
        after.diagnostics
    );
    shutdown(&client, t);
}

/// REGRESSION (blocker — silent-on-save via source.fixAll): the non-Node `@onready` induction must be
/// excluded from the `source.fixAll` aggregate too (fixAll applies with zero user interaction). With
/// only that unsafe candidate present, fixAll offers NOTHING.
#[test]
fn fix_all_excludes_unsafe_add_onready_on_non_node_class() {
    const SRC: &str =
        "extends Object\n\nvar n = get_node(\"Child\")\n\nfunc f() -> void:\n\tprint(0)\n";
    let p = base_project();
    let (server, client) = Connection::memory();
    let t = std::thread::spawn(move || gd_server::serve(server));
    let (_r, diags) = init_open(&p, &client, &[("a.gd", SRC)], caps(true, true, true));
    let uri = file_uri(&p.root.join("a.gd"));
    let diag = diag_with_warning(&diags, "GET_NODE_DEFAULT_WITHOUT_ONREADY");
    let actions = request_code_action(
        &client,
        10,
        &uri,
        Range {
            start: Position {
                line: 0,
                character: 0,
            },
            end: Position {
                line: 5,
                character: 0,
            },
        },
        vec![diag],
        Some(vec![CodeActionKind::from("source.fixAll".to_string())]),
    );
    assert!(
        actions.is_empty(),
        "the unsafe @onready (non-Node) must be excluded from fixAll → nothing offered; got \
         {actions:?}"
    );
    shutdown(&client, t);
}

/// NON-ASCII encoding correctness: a multi-byte character (emoji in a string) precedes the fix
/// location, so byte offsets and UTF-16 columns diverge. The `_`-prefix fix (and the ERROR backstop's
/// in-memory edit application) must still produce a correct edit — proving `apply_workspace_edit_to_text`
/// and the rename emit ranges in the same (negotiated) encoding the client applies.
#[test]
fn underscore_prefix_handles_non_ascii_before_edit() {
    // Line 4 has an emoji (4 UTF-8 bytes, 2 UTF-16 units, 1 char) in a string; the unused param is
    // on a later line so its column resolution crosses the multibyte content.
    const SRC: &str =
        "extends Node\n\n\nfunc f(unused: int) -> void:\n\tprint(\"hi \u{1F600} there\")\n";
    let p = base_project();
    let (server, client) = Connection::memory();
    let t = std::thread::spawn(move || gd_server::serve(server));
    let (_r, diags) = init_open(&p, &client, &[("a.gd", SRC)], caps(true, true, true));
    let uri = file_uri(&p.root.join("a.gd"));
    let diag = diag_with_warning(&diags, "UNUSED_PARAMETER");
    let edit = resolve_fix_edit(&client, 10, &uri, &diag, "Prefix unused name");
    let tes = all_text_edits(&edit);
    assert!(
        tes.iter().all(|te| te.new_text == "_unused"),
        "the param renames to `_unused`; got {tes:?}"
    );
    // Apply via the byte-accurate helper (it converts LSP positions over the rope, like the server).
    let patched = apply_text_edits_utf16(SRC, tes);
    assert!(
        patched.contains("func f(_unused: int)"),
        "the rename must land correctly despite the multibyte line; patched:\n{patched}"
    );
    assert!(
        patched.contains("hi \u{1F600} there"),
        "the emoji string must be untouched; patched:\n{patched}"
    );
    let after = reopen_and_diags(&p, &client, "b.gd", &patched, 100);
    assert!(
        !has_warning(&after, "UNUSED_PARAMETER"),
        "UNUSED_PARAMETER cleared; got {:?}",
        after.diagnostics
    );
    assert!(
        !after
            .diagnostics
            .iter()
            .any(|d| d.severity == Some(lsp_types::DiagnosticSeverity::ERROR)),
        "no errors; got {:?}\npatched:\n{patched}",
        after.diagnostics
    );
    shutdown(&client, t);
}

/// Apply UTF-16-positioned `TextEdit`s to `src` (the LSP default encoding the test client negotiates).
/// Mirrors `apply_text_edits` but maps `character` as a UTF-16 offset, so a multibyte line resolves the
/// same way the server's PositionMapper does.
fn apply_text_edits_utf16(src: &str, mut edits: Vec<lsp_types::TextEdit>) -> String {
    let line_starts: Vec<usize> = std::iter::once(0)
        .chain(src.match_indices('\n').map(|(i, _)| i + 1))
        .collect();
    let to_offset = |p: lsp_types::Position| -> usize {
        let line_start = line_starts
            .get(p.line as usize)
            .copied()
            .unwrap_or(src.len());
        // Walk UTF-16 units from the line start to find the byte offset.
        let mut utf16 = 0u32;
        let mut byte = line_start;
        for ch in src[line_start..].chars() {
            if utf16 >= p.character {
                break;
            }
            utf16 += ch.len_utf16() as u32;
            byte += ch.len_utf8();
        }
        byte
    };
    edits.sort_by_key(|e| std::cmp::Reverse((e.range.start.line, e.range.start.character)));
    let mut out = src.to_string();
    for e in edits {
        let start = to_offset(e.range.start);
        let end = to_offset(e.range.end);
        out.replace_range(start..end, &e.new_text);
    }
    out
}

/// REGRESSION (eager edits for mutating fixes): even WITH resolveSupport, a mutating fix carries its
/// (gated) edit eagerly in the codeAction response and NO `data` — so what the ERROR backstop proved
/// safe is exactly what the client applies (a deferred re-derive against a changed buffer is the
/// stale-resolve corruption class). The suppression still defers (it carries `data`, no eager edit).
#[test]
fn mutating_fixes_carry_eager_edit_not_deferred() {
    let p = base_project();
    let (server, client) = Connection::memory();
    let t = std::thread::spawn(move || gd_server::serve(server));
    let (_r, diags) = init_open(
        &p,
        &client,
        &[("a.gd", UNUSED_VAR_SRC)],
        caps(true, true, true), // resolveSupport = TRUE
    );
    let uri = file_uri(&p.root.join("a.gd"));
    let diag = unused_var_diag(&diags);
    let actions = request_code_action(&client, 10, &uri, diag.range, vec![diag], None);

    let prefix = find_action(&actions, "Prefix unused name").unwrap_or_else(|| {
        panic!(
            "the `_`-prefix fix must be offered; got {:?}",
            action_titles(&actions)
        )
    });
    assert!(
        prefix.edit.is_some() && prefix.data.is_none(),
        "the mutating fix must carry an EAGER edit and NO data (even with resolveSupport); got \
         edit={:?} data={:?}",
        prefix.edit.is_some(),
        prefix.data.is_some()
    );
    // The suppression, by contrast, defers (data present, edit absent) under resolveSupport.
    let suppress = find_action(&actions, "Ignore").expect("suppression offered");
    assert!(
        suppress.edit.is_none() && suppress.data.is_some(),
        "the suppression must DEFER under resolveSupport (data present, edit absent)"
    );
    shutdown(&client, t);
}

/// REGRESSION (duplicate-message backstop bypass): the ERROR backstop compares error MULTIPLICITY,
/// not a set. A file with a PRE-EXISTING `@onready`-on-non-Node error must still have its UNSAFE
/// second @onready fix refused — even though the induced error has the SAME message as the existing
/// one (a set comparison would see {M} unchanged and wrongly accept). Here `already` has a standalone
/// @onready (pre-existing error M), and `child` is a get_node default whose @onready fix would add a
/// SECOND error M.
#[test]
fn add_onready_refused_despite_duplicate_existing_error_message() {
    // `extends Object` (non-Node). `already` already errors (@onready on non-Node). `child` is a
    // get_node default — its @onready fix would induce the SAME-message error a SECOND time.
    const SRC: &str = "extends Object\n\n@onready var already = 1\n\nvar child = get_node(\"C\")\n\nfunc get_node(p):\n\treturn null\n";
    let p = base_project();
    let (server, client) = Connection::memory();
    let t = std::thread::spawn(move || gd_server::serve(server));
    let (_r, diags) = init_open(&p, &client, &[("a.gd", SRC)], caps(true, true, true));
    let uri = file_uri(&p.root.join("a.gd"));
    // `child` must warn GET_NODE_DEFAULT (it's a get_node default with no @onready).
    let diag = diags
        .diagnostics
        .iter()
        .find(|d| {
            d.code
                == Some(NumberOrString::String(
                    "GET_NODE_DEFAULT_WITHOUT_ONREADY".to_string(),
                ))
        })
        .cloned();
    let Some(diag) = diag else {
        // If the fixture doesn't produce the warning shape, the test is vacuous — skip cleanly.
        shutdown(&client, t);
        return;
    };
    let actions = request_code_action(&client, 10, &uri, diag.range, vec![diag], None);
    assert!(
        find_action(&actions, "@onready").is_none(),
        "the @onready fix must be REFUSED even though the induced error shares a message with a \
         pre-existing one (multiplicity, not set); got titles {:?}",
        action_titles(&actions)
    );
    shutdown(&client, t);
}

/// REGRESSION (blocker — silent member capture): a class member `_y` is READ via `print(_y)`, and an
/// unused local `y` is renamed to `_y`. After the rename, `var _y` (local) shadows the member, so
/// `print(_y)` SILENTLY reads the local (=1) instead of the member (=0) — compiles, different behavior
/// (no error, so the error backstop alone wouldn't catch it). The SHADOW backstop must WITHHOLD the
/// fix (the capture manifests as a new SHADOWED_VARIABLE).
#[test]
fn underscore_prefix_refused_on_silent_member_capture() {
    const SRC: &str = "extends Node\n\nvar _y = 0\n\nfunc f() -> void:\n\tvar y = 1\n\tprint(_y)\n";
    let p = base_project();
    let (server, client) = Connection::memory();
    let t = std::thread::spawn(move || gd_server::serve(server));
    let (_r, diags) = init_open(&p, &client, &[("a.gd", SRC)], caps(true, true, true));
    let uri = file_uri(&p.root.join("a.gd"));
    // Precondition: valid input (warnings only — the rebind would be a NET-NEW behavior change).
    assert!(
        !diags
            .diagnostics
            .iter()
            .any(|d| d.severity == Some(lsp_types::DiagnosticSeverity::ERROR)),
        "precondition: input must be error-free; got {:?}",
        diags.diagnostics
    );
    let diag = diags
        .diagnostics
        .iter()
        .find(|d| {
            d.code == Some(NumberOrString::String("UNUSED_VARIABLE".to_string()))
                && d.range.start.line == 5
        })
        .cloned()
        .unwrap_or_else(|| {
            panic!(
                "the unused local `y` must warn; got {:?}",
                diags.diagnostics
            )
        });
    let actions = request_code_action(&client, 10, &uri, diag.range, vec![diag], None);
    assert!(
        find_action(&actions, "Prefix unused name").is_none(),
        "renaming `y`→`_y` would silently capture the `print(_y)` member read — the silent-capture \
         firewall (`_y` already exists) must REFUSE it; got titles {:?}",
        action_titles(&actions)
    );
    shutdown(&client, t);
}

/// REGRESSION (blocker — the adversarial reviewer's exact moved-shadow case): `var y` AND `var _y`
/// are both members; `print(y)` (forward-ref) reads member `y`; the unused local `var y = 1` shadows
/// member `y`. The input already carries a SHADOWED_VARIABLE (local `y` over member `y`), so renaming
/// `y`→`_y` MOVES the shadow to member `_y` (count unchanged — a count-based shadow check is blind),
/// and `print(...)`'s over-captured rewrite would read member `_y` instead of member `y` — a silent
/// behavior change with NO error. The silent-capture firewall (`_y` already exists) must REFUSE it.
#[test]
fn underscore_prefix_refused_on_moved_shadow_capture() {
    const SRC: &str =
        "extends Node\nvar y = 0\nvar _y = 99\nfunc f() -> void:\n\tprint(y)\n\tvar y = 1\n";
    let p = base_project();
    let (server, client) = Connection::memory();
    let t = std::thread::spawn(move || gd_server::serve(server));
    let (_r, diags) = init_open(&p, &client, &[("a.gd", SRC)], caps(true, true, true));
    let uri = file_uri(&p.root.join("a.gd"));
    let diag = diags
        .diagnostics
        .iter()
        .find(|d| {
            d.code == Some(NumberOrString::String("UNUSED_VARIABLE".to_string()))
                && d.range.start.line == 5
        })
        .cloned()
        .unwrap_or_else(|| {
            panic!(
                "the unused local `y` must warn; got {:?}",
                diags.diagnostics
            )
        });
    let actions = request_code_action(&client, 10, &uri, diag.range, vec![diag], None);
    assert!(
        find_action(&actions, "Prefix unused name").is_none(),
        "renaming `y`→`_y` would silently rebind the `print(y)` read to member `_y` (a MOVED shadow, \
         count unchanged) — the firewall must REFUSE it; got titles {:?}",
        action_titles(&actions)
    );
    shutdown(&client, t);
}

/// SCOPE-AWARE (#107): two distinct locals `x` in disjoint `if`/`else` sub-blocks are kept apart.
/// The then-block `var x` is UNUSED (the diagnostic); the else-block `var x` is a DIFFERENT binding
/// read by `print(x)`. The old name-based function-wide resolver over-reached (renaming the then `x`
/// returned 3 edits — then-decl + else-decl + `print(x)`), corrupting the else binding; only the
/// count gate caught it. With scope-aware resolution the then-block `x` resolves to EXACTLY its own
/// declaration (it has no uses — it is unused), so the fix is a precise one-edit rename that IS
/// offered, AND is safe in the `source.fixAll` aggregate (which applies on save). Verified by
/// apply→reanalyze on the fixAll result: the else binding stays intact, no new error.
#[test]
fn underscore_prefix_sibling_block_not_over_reached() {
    // Then-block `x` is unused; else-block `x` (distinct binding) is used by `print(x)`.
    const SRC: &str =
        "extends Node\n\nfunc f(cond):\n\tif cond:\n\t\tvar x = 1\n\telse:\n\t\tvar x = 2\n\t\tprint(x)\n";
    let p = base_project();
    let (server, client) = Connection::memory();
    let t = std::thread::spawn(move || gd_server::serve(server));
    let (_r, diags) = init_open(&p, &client, &[("a.gd", SRC)], caps(true, true, true));
    let uri = file_uri(&p.root.join("a.gd"));
    // Precondition: error-free input — any corruption would be NET-NEW.
    assert!(
        !diags
            .diagnostics
            .iter()
            .any(|d| d.severity == Some(lsp_types::DiagnosticSeverity::ERROR)),
        "precondition: input must be error-free; got {:?}",
        diags.diagnostics
    );
    // The UNUSED_VARIABLE must be the THEN-block `var x` on line 4 (the else-block `x` is used).
    let diag = diags
        .diagnostics
        .iter()
        .find(|d| {
            d.code == Some(NumberOrString::String("UNUSED_VARIABLE".to_string()))
                && d.range.start.line == 4
        })
        .cloned()
        .unwrap_or_else(|| {
            panic!(
                "the unused THEN-block `x` (line 4) must warn; got {:?}",
                diags.diagnostics
            )
        });
    // The offered `_`-prefix edit renames ONLY the then-block declaration.
    let edit = resolve_fix_edit(&client, 10, &uri, &diag.clone(), "Prefix unused name");
    let patched = apply_text_edits(SRC, all_text_edits(&edit));
    assert!(
        patched.contains("if cond:\n\t\tvar _x = 1"),
        "the unused then-block `x` must be renamed to `_x`; patched:\n{patched}"
    );
    assert!(
        patched.contains("else:\n\t\tvar x = 2\n\t\tprint(x)"),
        "the DISTINCT else-block binding + its use must be left untouched; patched:\n{patched}"
    );
    // ON-SAVE GUARD (task point 2): the `_`-prefix flows through `build_fix_all` too — the
    // `source.fixAll` aggregate (what `only: None` returns) applies with zero user interaction. With
    // precise resolution it now offers the safe one-edit rename. Apply the fixAll result and reanalyze
    // by identity: no new error, the else binding still resolves (its `print(x)` is not dangling), and
    // the then-block warning clears.
    let after = reopen_and_diags(&p, &client, "b.gd", &patched, 100);
    assert!(
        !after
            .diagnostics
            .iter()
            .any(|d| d.severity == Some(lsp_types::DiagnosticSeverity::ERROR)),
        "no errors after the fix (else binding intact); got {:?}\npatched:\n{patched}",
        after.diagnostics
    );
    shutdown(&client, t);
}

/// SCOPE-AWARE (#107): NO cross-file silent rebind. An autoload named `_y` makes `_y` a valid
/// PROJECT-WIDE global. In `a.gd`, `print(y)` forward-refs the class MEMBER `y`, and the unused LOCAL
/// `var y = 1` shadows it. The old name-based scan over-captured `print(y)` and rewrote it to
/// `print(_y)`, a SILENT error-free rebind to the autoload (the count gate was the only catch).
/// With scope-aware resolution the forward-ref binds outward to the member (the local is declared
/// AFTER it), so the fix renames ONLY `var y = 1`→`var _y = 1` and leaves `print(y)` verbatim — no
/// rebind. Verified by apply→reanalyze: `print(y)` text is unchanged (still bound to the member),
/// no new error, the warning clears.
#[test]
fn underscore_prefix_no_cross_file_autoload_rebind() {
    let p = TempProject::new();
    // Autoload `_y` → `_y` is a project-wide global identifier (lives in project.godot, NOT in a.gd).
    p.write(
        "project.godot",
        "[application]\nconfig/name=\"T\"\nconfig_version=5\n\n[autoload]\n_y=\"*res://auto.gd\"\n",
    );
    p.write("extension_api.json", common::MINI_API);
    p.write("auto.gd", "extends Node\n\nfunc ping() -> void:\n\tpass\n");
    // Member `y`; forward-ref `print(y)` binds to the member; unused local `y` shadows it.
    const SRC: &str = "extends Node\n\nvar y = 10\n\nfunc f() -> void:\n\tprint(y)\n\tvar y = 1\n";
    let (server, client) = Connection::memory();
    let t = std::thread::spawn(move || gd_server::serve(server));
    let (_r, diags) = init_open(&p, &client, &[("a.gd", SRC)], caps(true, true, true));
    let uri = file_uri(&p.root.join("a.gd"));
    // The unused LOCAL `y` is on line 6 (the member `y` on line 2 and forward-ref on line 5 don't warn).
    let diag = diags
        .diagnostics
        .iter()
        .find(|d| {
            d.code == Some(NumberOrString::String("UNUSED_VARIABLE".to_string()))
                && d.range.start.line == 6
        })
        .cloned()
        .unwrap_or_else(|| {
            panic!(
                "the unused local `y` (line 6) must warn; got {:?}",
                diags.diagnostics
            )
        });
    let edit = resolve_fix_edit(&client, 10, &uri, &diag, "Prefix unused name");
    let patched = apply_text_edits(SRC, all_text_edits(&edit));
    // ONLY the unused local is renamed; the forward-ref read stays verbatim → no rebind to `_y`.
    assert!(
        patched.contains("var _y = 1"),
        "the unused local must be renamed to `_y`; patched:\n{patched}"
    );
    assert!(
        patched.contains("print(y)"),
        "the forward-ref `print(y)` must stay VERBATIM (no silent rebind to the `_y` autoload); \
         patched:\n{patched}"
    );
    // Apply→reanalyze: binding identity preserved (the member `y` still declared, `print(y)` bound
    // to it), no new error.
    let after = reopen_and_diags(&p, &client, "b.gd", &patched, 100);
    assert!(
        !after
            .diagnostics
            .iter()
            .any(|d| d.severity == Some(lsp_types::DiagnosticSeverity::ERROR)),
        "no errors after the fix (no cross-file rebind); got {:?}\npatched:\n{patched}",
        after.diagnostics
    );
    shutdown(&client, t);
}

/// SCOPE-AWARE FIREWALL (#119): the `_`-prefix fix must be OFFERED when `_name` exists ONLY in a
/// genuinely UNRELATED function scope, where no capture is possible. Here `count` is an unused local
/// of `f`; `_count` is a distinct local of an unrelated `g`. The whole-file firewall over-refused
/// (any `_count` identifier anywhere blocked the fix); the scope-aware firewall sees that `_count` is
/// not visible in `f`'s scope (member / enclosing / global / `f`-local), so the rename `count`→`_count`
/// is fresh in `f` and capture-free — OFFER it. Verified by apply→reanalyze: the unrelated `g._count`
/// is untouched, `f`'s local renamed, no new error, the warning clears.
#[test]
fn underscore_prefix_offered_when_name_only_in_unrelated_scope() {
    // `count` unused in `f`; `_count` is a SEPARATE local of the unrelated `g`.
    const SRC: &str = "extends Node\n\nfunc f() -> void:\n\tvar count = 1\n\nfunc g() -> void:\n\tvar _count = 1\n\tprint(_count)\n";
    let p = base_project();
    let (server, client) = Connection::memory();
    let t = std::thread::spawn(move || gd_server::serve(server));
    let (_r, diags) = init_open(&p, &client, &[("a.gd", SRC)], caps(true, true, true));
    let uri = file_uri(&p.root.join("a.gd"));
    // Precondition: error-free input.
    assert!(
        !diags
            .diagnostics
            .iter()
            .any(|d| d.severity == Some(lsp_types::DiagnosticSeverity::ERROR)),
        "precondition: input must be error-free; got {:?}",
        diags.diagnostics
    );
    // The unused LOCAL `count` is on line 3 (the `_count` of `g` is used by `print`, doesn't warn).
    let diag = diags
        .diagnostics
        .iter()
        .find(|d| {
            d.code == Some(NumberOrString::String("UNUSED_VARIABLE".to_string()))
                && d.range.start.line == 3
        })
        .cloned()
        .unwrap_or_else(|| {
            panic!(
                "the unused local `count` (line 3) must warn; got {:?}",
                diags.diagnostics
            )
        });
    let edit = resolve_fix_edit(&client, 10, &uri, &diag, "Prefix unused name");
    let patched = apply_text_edits(SRC, all_text_edits(&edit));
    // Only `f`'s local is renamed; `g`'s unrelated `_count` is untouched.
    assert!(
        patched.contains("func f() -> void:\n\tvar _count = 1"),
        "the unused local `count` in `f` must be renamed to `_count`; patched:\n{patched}"
    );
    assert!(
        patched.contains("func g() -> void:\n\tvar _count = 1\n\tprint(_count)"),
        "the UNRELATED `g._count` + its use must be left untouched; patched:\n{patched}"
    );
    let after = reopen_and_diags(&p, &client, "b.gd", &patched, 100);
    assert!(
        !after
            .diagnostics
            .iter()
            .any(|d| d.severity == Some(lsp_types::DiagnosticSeverity::ERROR)),
        "no errors after the fix (unrelated scope untouched); got {:?}\npatched:\n{patched}",
        after.diagnostics
    );
    assert!(
        !has_warning(&after, "UNUSED_VARIABLE"),
        "UNUSED_VARIABLE cleared; got {:?}",
        after.diagnostics
    );
    shutdown(&client, t);
}

/// SOUNDNESS (#119): the narrowed firewall must STILL refuse a SAME-FUNCTION collision. `count` is an
/// unused local of `f`; `_count` is ALSO a (used) local of the SAME function `f`. Renaming `count`→
/// `_count` would collide with the live `_count` in the same scope — a capture. The fix must NOT be
/// offered (the scope-aware firewall sees `_count` visible in the renamed binding's own function).
#[test]
fn underscore_prefix_refused_when_name_in_same_function() {
    // BOTH `count` (unused) and `_count` (used) are locals of the SAME function `f`.
    const SRC: &str =
        "extends Node\n\nfunc f() -> void:\n\tvar count = 1\n\tvar _count = 2\n\tprint(_count)\n";
    let p = base_project();
    let (server, client) = Connection::memory();
    let t = std::thread::spawn(move || gd_server::serve(server));
    let (_r, diags) = init_open(&p, &client, &[("a.gd", SRC)], caps(true, true, true));
    let uri = file_uri(&p.root.join("a.gd"));
    let diag = diags
        .diagnostics
        .iter()
        .find(|d| {
            d.code == Some(NumberOrString::String("UNUSED_VARIABLE".to_string()))
                && d.range.start.line == 3
        })
        .cloned()
        .unwrap_or_else(|| {
            panic!(
                "the unused local `count` (line 3) must warn; got {:?}",
                diags.diagnostics
            )
        });
    let actions = request_code_action(&client, 10, &uri, diag.range, vec![diag], None);
    assert!(
        find_action(&actions, "Prefix unused name").is_none(),
        "a `_`-prefix that COLLIDES with a same-function `_count` must be REFUSED; got titles {:?}",
        action_titles(&actions)
    );
    shutdown(&client, t);
}

/// SOUNDNESS (#119): the narrowed firewall must refuse when `_name` lives in a NESTED LAMBDA inside the
/// renamed binding's OWN function. A lambda body parses as a nested `Function`, so the `_name` local's
/// enclosing-function span is INSIDE (not disjoint from) the renamed binding's function span — the fix
/// must NOT be offered. Locks in the nested-`Function`-span reasoning the disjoint-span check relies on.
#[test]
fn underscore_prefix_refused_when_name_in_nested_lambda() {
    // Outer unused local `count` in `f`; `_count` is a local of a lambda nested INSIDE `f`.
    const SRC: &str =
        "extends Node\n\nfunc f() -> void:\n\tvar count = 1\n\tvar cb = func() -> void:\n\t\tvar _count = 2\n\t\tprint(_count)\n\tcb.call()\n";
    let p = base_project();
    let (server, client) = Connection::memory();
    let t = std::thread::spawn(move || gd_server::serve(server));
    let (_r, diags) = init_open(&p, &client, &[("a.gd", SRC)], caps(true, true, true));
    let uri = file_uri(&p.root.join("a.gd"));
    let diag = diags
        .diagnostics
        .iter()
        .find(|d| {
            d.code == Some(NumberOrString::String("UNUSED_VARIABLE".to_string()))
                && d.range.start.line == 3
        })
        .cloned()
        .unwrap_or_else(|| {
            panic!(
                "the unused local `count` (line 3) must warn; got {:?}",
                diags.diagnostics
            )
        });
    let actions = request_code_action(&client, 10, &uri, diag.range, vec![diag], None);
    assert!(
        find_action(&actions, "Prefix unused name").is_none(),
        "a `_`-prefix whose `_count` lives in a nested lambda of the SAME function must be REFUSED \
         (the lambda span is nested, not disjoint); got titles {:?}",
        action_titles(&actions)
    );
    shutdown(&client, t);
}

/// SOUNDNESS (#119, reverse direction): the renamed binding is an unused PARAM of a NESTED LAMBDA, and
/// `_name` is a local of the ENCLOSING function. The lambda's enclosing-function span (the lambda) is
/// CONTAINED IN the enclosing function's span, so the two are NOT disjoint — the fix must NOT be offered.
/// Locks in the smallest-`Function` selection from the lambda-inward side (the mirror of
/// `underscore_prefix_refused_when_name_in_nested_lambda`).
#[test]
fn underscore_prefix_refused_when_name_in_enclosing_function() {
    // `_count` is a local of the enclosing `f`; the unused PARAM `count` belongs to a lambda nested in `f`.
    const SRC: &str =
        "extends Node\n\nfunc f() -> void:\n\tvar _count = 1\n\tprint(_count)\n\tvar cb = func(count: int) -> void:\n\t\tprint(0)\n\tcb.call(1)\n";
    let p = base_project();
    let (server, client) = Connection::memory();
    let t = std::thread::spawn(move || gd_server::serve(server));
    let (_r, diags) = init_open(&p, &client, &[("a.gd", SRC)], caps(true, true, true));
    let uri = file_uri(&p.root.join("a.gd"));
    // The unused PARAM `count` of the lambda is on line 5 (`func(count: int)`).
    let diag = diags
        .diagnostics
        .iter()
        .find(|d| d.code == Some(NumberOrString::String("UNUSED_PARAMETER".to_string())))
        .cloned()
        .unwrap_or_else(|| {
            panic!(
                "the unused lambda PARAM `count` must warn; got {:?}",
                diags.diagnostics
            )
        });
    let actions = request_code_action(&client, 10, &uri, diag.range, vec![diag], None);
    assert!(
        find_action(&actions, "Prefix unused name").is_none(),
        "a `_`-prefix whose `_count` lives in the ENCLOSING function must be REFUSED (the lambda span \
         is nested in the enclosing span, not disjoint); got titles {:?}",
        action_titles(&actions)
    );
    shutdown(&client, t);
}
