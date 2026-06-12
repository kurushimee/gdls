//! M7 (#57) — wire-level races for the router thread: true `$/cancelRequest` preemption,
//! stale-by-edit `ContentModified`, FIFO ordering, and the shutdown path that replaced
//! lsp-server's `handle_shutdown`.
//!
//! Determinism: every session sets `initializationOptions.analyzer.checkpointDelayUs`, making
//! each analyze pass sleep at the analyzer's 256-node checkpoint gates. A `references` request
//! on a method-shaped name then analyzes every candidate file that mentions the name — the
//! heavy `filler` bodies below give it tens of gates (seconds of deterministic sleep), while
//! the router flips interrupt flags within microseconds of a control message arriving, so
//! "the flag lands mid-run" needs no timing luck. The cancelled run's latency is bounded by
//! one gate; the uncancelled run pays every gate — that gap is what the preemption assertions
//! measure.

mod common;

use std::time::{Duration, Instant};

use common::{file_uri, notification, recv, recv_response, request, TempProject};
use lsp_server::{Connection, Message, RequestId, Response};
use lsp_types::{
    CancelParams, ClientCapabilities, DidChangeTextDocumentParams, DidOpenTextDocumentParams,
    DocumentSymbolParams, InitializeParams, InitializedParams, NumberOrString, PartialResultParams,
    Position, ReferenceContext, ReferenceParams, TextDocumentContentChangeEvent,
    TextDocumentIdentifier, TextDocumentItem, TextDocumentPositionParams, Uri,
    VersionedTextDocumentIdentifier, WorkDoneProgressParams,
};

/// LSP 3.17 error codes the interrupt gate maps to.
const REQUEST_CANCELLED: i32 = -32800;
const CONTENT_MODIFIED: i32 = -32801;
const INVALID_REQUEST: i32 = -32600;

/// Per-checkpoint sleep. Each heavy file below crosses its 256-node gate several times, so a
/// full analyze of one family (3 heavy files) sleeps for many multiples of this — while a
/// cancelled run stops within ~one gate.
const CHECKPOINT_DELAY_US: u64 = 30_000;

/// A function body big enough to cross the analyzer's 256-node checkpoint gate many times.
fn filler(lines: usize) -> String {
    let mut body = String::from("\nfunc filler() -> void:\n");
    for i in 0..lines {
        body.push_str(&format!("\tvar _x{i} = {i} + {i}\n"));
    }
    body
}

/// Two independent "method families" (`target_a` in 3 heavy files, `target_b` in 3 others) so a
/// test can measure a full uncancelled run on one family with the other family's caches cold.
fn heavy_project() -> TempProject {
    let p = TempProject::new();
    p.write(
        "project.godot",
        "config_version=5\n\n[application]\n\nconfig/name=\"Test\"\n",
    );
    p.write("extension_api.json", common::MINI_API);
    for fam in ["a", "b"] {
        p.write(
            &format!("src/def_{fam}.gd"),
            &format!(
                "class_name Def{}\nextends Node\n\nfunc target_{fam}() -> void:\n\tpass\n{}",
                fam.to_uppercase(),
                filler(800)
            ),
        );
        for n in [1, 2] {
            p.write(
                &format!("src/use_{fam}{n}.gd"),
                &format!(
                    "extends Node\n\nfunc go(v: Def{}) -> void:\n\tv.target_{fam}()\n{}",
                    fam.to_uppercase(),
                    filler(800)
                ),
            );
        }
    }
    // The tiny open buffer every request positions into. Line 3 col 3 is inside `target_a`,
    // line 4 col 3 inside `target_b`.
    p.write("probe.gd", PROBE_TEXT);
    p
}

const PROBE_TEXT: &str =
    "extends Node\n\nfunc probe(a: DefA, b: DefB) -> void:\n\ta.target_a()\n\tb.target_b()\n";

/// Boot a session over `heavy_project()` with the checkpoint-delay governor armed, open
/// `probe.gd`, and drain its didOpen publish. Returns (project, client, server thread, probe URI).
fn boot() -> (
    TempProject,
    Connection,
    std::thread::JoinHandle<anyhow::Result<()>>,
    Uri,
) {
    let p = heavy_project();
    let (server, client) = Connection::memory();
    let server_thread = std::thread::spawn(move || gd_server::serve(server));

    let init = InitializeParams {
        capabilities: ClientCapabilities::default(),
        initialization_options: Some(serde_json::json!({
            "projectRoot": p.root.as_str(),
            "autoDumpExtensionApi": false,
            "extensionApiPath": p.root.join("extension_api.json").as_str(),
            "analyzer": { "checkpointDelayUs": CHECKPOINT_DELAY_US },
        })),
        ..Default::default()
    };
    client.sender.send(request(1, "initialize", init)).unwrap();
    let _initialize_response = recv_response(&client);
    client
        .sender
        .send(notification("initialized", InitializedParams {}))
        .unwrap();

    let uri = file_uri(&p.root.join("probe.gd"));
    client
        .sender
        .send(notification(
            "textDocument/didOpen",
            DidOpenTextDocumentParams {
                text_document: TextDocumentItem {
                    uri: uri.clone(),
                    language_id: "gdscript".to_string(),
                    version: 1,
                    text: PROBE_TEXT.to_string(),
                },
            },
        ))
        .unwrap();
    // Drain the didOpen publish (probe is tiny — a gate or two of delay at most).
    let _publish = recv(&client);

    (p, client, server_thread, uri)
}

/// `textDocument/references` on `target_a` (line 3) or `target_b` (line 4) in the probe buffer.
fn references_msg(id: i32, uri: &Uri, line: u32) -> Message {
    request(
        id,
        "textDocument/references",
        ReferenceParams {
            text_document_position: TextDocumentPositionParams {
                text_document: TextDocumentIdentifier { uri: uri.clone() },
                position: Position { line, character: 3 },
            },
            work_done_progress_params: WorkDoneProgressParams::default(),
            partial_result_params: PartialResultParams::default(),
            context: ReferenceContext {
                include_declaration: true,
            },
        },
    )
}

fn document_symbol_msg(id: i32, uri: &Uri) -> Message {
    request(
        id,
        "textDocument/documentSymbol",
        DocumentSymbolParams {
            text_document: TextDocumentIdentifier { uri: uri.clone() },
            work_done_progress_params: WorkDoneProgressParams::default(),
            partial_result_params: PartialResultParams::default(),
        },
    )
}

fn cancel_msg(id: i32) -> Message {
    notification(
        "$/cancelRequest",
        CancelParams {
            id: NumberOrString::Number(id),
        },
    )
}

/// Full-document didChange on the probe buffer.
fn did_change_msg(uri: &Uri, version: i32, text: &str) -> Message {
    notification(
        "textDocument/didChange",
        DidChangeTextDocumentParams {
            text_document: VersionedTextDocumentIdentifier {
                uri: uri.clone(),
                version,
            },
            content_changes: vec![TextDocumentContentChangeEvent {
                range: None,
                range_length: None,
                text: text.to_string(),
            }],
        },
    )
}

/// `recv_response` with a longer ceiling for the deliberately slow uncancelled runs.
fn recv_response_slow(conn: &Connection) -> Response {
    let deadline = Duration::from_secs(60);
    let start = Instant::now();
    loop {
        match conn
            .receiver
            .recv_timeout(deadline.saturating_sub(start.elapsed()))
        {
            Ok(Message::Response(r)) => return r,
            Ok(_) => continue,
            Err(e) => panic!("timed out waiting for a (slow) response: {e}"),
        }
    }
}

fn err_code(resp: &Response) -> Option<i32> {
    resp.error.as_ref().map(|e| e.code)
}

/// The headline #57 behavior: a cancel arriving while `references` is mid-analysis preempts it
/// at the next checkpoint — the response is `RequestCancelled` and arrives in a fraction of the
/// uncancelled runtime (post-hoc gating, the pre-M7 behavior, would pay the full runtime first).
#[test]
fn cancel_mid_analysis_preempts_instead_of_running_to_completion() {
    let (_p, client, server_thread, uri) = boot();

    // Cancelled run on the `target_a` family: cancel lands while candidate analysis sleeps at
    // a checkpoint gate (the router flips the token in microseconds; the first gate alone
    // sleeps 30 ms).
    let started = Instant::now();
    client.sender.send(references_msg(10, &uri, 3)).unwrap();
    client.sender.send(cancel_msg(10)).unwrap();
    let cancelled = recv_response_slow(&client);
    let cancelled_elapsed = started.elapsed();
    assert_eq!(cancelled.id, RequestId::from(10));
    assert_eq!(
        err_code(&cancelled),
        Some(REQUEST_CANCELLED),
        "a cancelled references run must answer RequestCancelled; got {:?}",
        cancelled.error
    );

    // Full run on the untouched `target_b` family (its candidate analyses are cache-cold).
    let started = Instant::now();
    client.sender.send(references_msg(11, &uri, 4)).unwrap();
    let full = recv_response_slow(&client);
    let full_elapsed = started.elapsed();
    assert_eq!(full.id, RequestId::from(11));
    assert!(
        full.error.is_none(),
        "the uncancelled run must succeed; got {:?}",
        full.error
    );

    // The governor must actually have engaged (each heavy file sleeps many 30 ms gates) …
    assert!(
        full_elapsed > Duration::from_millis(200),
        "uncancelled run finished in {full_elapsed:?} — the checkpoint-delay governor did not engage"
    );
    // … and preemption must beat it by a wide margin: the cancelled run stops within ~one gate.
    assert!(
        cancelled_elapsed < full_elapsed / 3,
        "cancellation was not preemptive: cancelled run took {cancelled_elapsed:?} vs full run {full_elapsed:?}"
    );

    // The session stays healthy after a preempted handler.
    client.sender.send(document_symbol_msg(12, &uri)).unwrap();
    let after = recv_response(&client);
    assert_eq!(after.id, RequestId::from(12));
    assert!(after.error.is_none());

    common::shutdown(&client, server_thread);
}

/// A cancel for a request still sitting in the forward queue short-circuits it: the response is
/// `RequestCancelled` and the handler never runs.
#[test]
fn cancel_before_start_short_circuits_queued_request() {
    let (_p, client, server_thread, uri) = boot();

    client.sender.send(references_msg(20, &uri, 3)).unwrap(); // slow — occupies the worker
    client.sender.send(document_symbol_msg(21, &uri)).unwrap(); // queued behind it
    client.sender.send(cancel_msg(21)).unwrap(); // cancels the QUEUED request
    client.sender.send(cancel_msg(20)).unwrap(); // then unblocks the worker quickly

    let first = recv_response_slow(&client);
    assert_eq!(first.id, RequestId::from(20));
    assert_eq!(err_code(&first), Some(REQUEST_CANCELLED));

    let second = recv_response(&client);
    assert_eq!(second.id, RequestId::from(21));
    assert_eq!(
        err_code(&second),
        Some(REQUEST_CANCELLED),
        "a documentSymbol would have succeeded — RequestCancelled proves the queued request \
         was short-circuited; got {:?}",
        second.error
    );

    common::shutdown(&client, server_thread);
}

/// An edit arriving while a request is mid-run invalidates its result: the handler bails at the
/// next checkpoint and the response is `ContentModified` (the client retries against new text).
#[test]
fn edit_mid_request_returns_content_modified() {
    let (_p, client, server_thread, uri) = boot();

    client.sender.send(references_msg(30, &uri, 3)).unwrap();
    let edited = format!("{PROBE_TEXT}# trailing comment\n");
    client
        .sender
        .send(did_change_msg(&uri, 2, &edited))
        .unwrap();

    let resp = recv_response_slow(&client);
    assert_eq!(resp.id, RequestId::from(30));
    assert_eq!(
        err_code(&resp),
        Some(CONTENT_MODIFIED),
        "a mid-run edit must invalidate the in-flight references run; got {:?}",
        resp.error
    );

    common::shutdown(&client, server_thread);
}

/// An edit also invalidates requests still in the queue — they answer `ContentModified` without
/// running (the client's retry, which follows the edit in wire order, sees the new text).
#[test]
fn queued_request_behind_edit_is_shed_with_content_modified() {
    let (_p, client, server_thread, uri) = boot();

    client.sender.send(references_msg(40, &uri, 3)).unwrap(); // slow — occupies the worker
    client.sender.send(document_symbol_msg(41, &uri)).unwrap(); // queued
    let edited = format!("{PROBE_TEXT}# v2\n");
    client
        .sender
        .send(did_change_msg(&uri, 2, &edited))
        .unwrap(); // stales BOTH

    let first = recv_response_slow(&client);
    assert_eq!(first.id, RequestId::from(40));
    assert_eq!(err_code(&first), Some(CONTENT_MODIFIED));

    let second = recv_response(&client);
    assert_eq!(second.id, RequestId::from(41));
    assert_eq!(err_code(&second), Some(CONTENT_MODIFIED));

    common::shutdown(&client, server_thread);
}

/// When a request is both cancelled and staled by an edit, the cancel wins — the client
/// retracted the request and discards the response either way. The cancel is sent FIRST: in the
/// edit-then-cancel order the worker can legitimately bail Stale and finish in the microseconds
/// between the router processing the two messages, so that order is not wire-deterministic —
/// both flag orders are pinned deterministically by the `RequestLifecycle` unit tests instead.
#[test]
fn cancel_beats_stale_when_both_land() {
    let (_p, client, server_thread, uri) = boot();

    client.sender.send(references_msg(50, &uri, 3)).unwrap();
    client.sender.send(cancel_msg(50)).unwrap();
    let edited = format!("{PROBE_TEXT}# stale\n");
    client
        .sender
        .send(did_change_msg(&uri, 2, &edited))
        .unwrap();

    let resp = recv_response_slow(&client);
    assert_eq!(resp.id, RequestId::from(50));
    assert_eq!(
        err_code(&resp),
        Some(REQUEST_CANCELLED),
        "cancelled wins over stale; got {:?}",
        resp.error
    );

    common::shutdown(&client, server_thread);
}

/// FIFO pin: a request sent AFTER an edit runs against the post-edit text — the router's stale
/// sweep must never blast requests that follow the mutation in wire order.
#[test]
fn request_after_edit_sees_new_text_and_is_not_stale() {
    let (_p, client, server_thread, uri) = boot();

    let edited = format!("{PROBE_TEXT}\nfunc brand_new() -> void:\n\tpass\n");
    client
        .sender
        .send(did_change_msg(&uri, 2, &edited))
        .unwrap();
    client.sender.send(document_symbol_msg(60, &uri)).unwrap();

    let resp = recv_response(&client);
    assert_eq!(resp.id, RequestId::from(60));
    assert!(
        resp.error.is_none(),
        "a request following the edit must not be stale; got {:?}",
        resp.error
    );
    let rendered = serde_json::to_string(&resp.result).unwrap();
    assert!(
        rendered.contains("brand_new"),
        "documentSymbol must reflect the post-edit text; got {rendered}"
    );

    common::shutdown(&client, server_thread);
}

/// The shutdown path that replaced lsp-server's `handle_shutdown` (which would block 30 s on the
/// receiver the router now owns): shutdown queued behind in-flight work is answered after it,
/// later requests get `InvalidRequest`, `exit` ends the session, and the whole sequence
/// completes far inside the old 30 s hang.
#[test]
fn shutdown_with_inflight_work_answers_everything_and_exits_quickly() {
    let (_p, client, server_thread, uri) = boot();

    let started = Instant::now();
    client.sender.send(references_msg(70, &uri, 3)).unwrap(); // slow, uncancelled
    client
        .sender
        .send(request(71, "shutdown", serde_json::Value::Null))
        .unwrap();
    client.sender.send(document_symbol_msg(72, &uri)).unwrap(); // post-shutdown
    client
        .sender
        .send(notification("exit", serde_json::Value::Null))
        .unwrap();

    let inflight = recv_response_slow(&client);
    assert_eq!(inflight.id, RequestId::from(70));
    assert!(
        inflight.error.is_none(),
        "the in-flight request received before shutdown must still be answered; got {:?}",
        inflight.error
    );

    let shutdown_resp = recv_response(&client);
    assert_eq!(shutdown_resp.id, RequestId::from(71));
    assert!(shutdown_resp.error.is_none());

    let refused = recv_response(&client);
    assert_eq!(refused.id, RequestId::from(72));
    assert_eq!(
        err_code(&refused),
        Some(INVALID_REQUEST),
        "requests after shutdown must answer InvalidRequest (-32600); got {:?}",
        refused.error
    );

    server_thread
        .join()
        .expect("server thread panicked")
        .expect("serve() returned an error");
    assert!(
        started.elapsed() < Duration::from_secs(20),
        "shutdown sequence took {:?} — the old handle_shutdown 30 s hang is back",
        started.elapsed()
    );
}
