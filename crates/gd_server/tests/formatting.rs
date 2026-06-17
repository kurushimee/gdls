//! M11 (#80) gate: the external-formatter bridge (`textDocument/formatting`), over the in-memory
//! Connection rig (`tests/file_operations.rs` style).
//!
//! ## Cross-platform stub
//!
//! The formatter the tests bridge to is `gdls_format_stub` — a `[[bin]]` of THIS package located via
//! `env!("CARGO_BIN_EXE_gdls_format_stub")` (Cargo sets that for every bin of the package under
//! test). It is a deterministic stdin→stdout filter whose behavior is picked by its first arg
//! (`squeeze`/`replace`/`touchone`/`passthru`/`fail`/`sleep`/`binary`), so the exact same formatter
//! path runs on Linux and Windows — no `cat`/`sed`/`sleep` platform skew. gdls spawns it with NO
//! shell (argv vector), exactly as it would a real `gdformat`.
//!
//! Coverage (the prompt's matrix):
//!   * configured → `documentFormattingProvider` advertised; a request pipes text through the stub
//!     and returns edits that, applied, equal the stub's stdout.
//!   * unconfigured → capability NOT advertised; a request returns null/no-edits, no crash.
//!   * crash (non-zero exit) → no edits + exactly one showMessage(Warning); a second request does
//!     NOT re-warn (per-session-per-class dedupe).
//!   * timeout (stub sleeps past the bound) → killed, no edits + one warning.
//!   * non-UTF-8 stdout → no edits + one warning.
//!   * minimal-diff: a one-line reformat yields a TextEdit touching only that region, not the whole
//!     doc.

mod common;

use std::time::{Duration, Instant};

use common::{file_uri, notification, recv_response, request, shutdown, try_recv, TempProject};
use lsp_server::{Connection, Message, RequestId, Response};
use lsp_types::{
    CancelParams, ClientCapabilities, DidOpenTextDocumentParams, DocumentFormattingParams,
    DocumentSymbolParams, FormattingOptions, InitializeParams, InitializeResult, InitializedParams,
    NumberOrString, PartialResultParams, TextDocumentIdentifier, TextDocumentItem, TextEdit, Uri,
    WorkDoneProgressParams,
};

/// Absolute path to the cross-platform stub formatter bin (Cargo provides it for the package under
/// test). Used as `formatter.command`; the per-test behavior is the first `formatter.args` entry.
const STUB: &str = env!("CARGO_BIN_EXE_gdls_format_stub");

fn boot() -> (Connection, std::thread::JoinHandle<anyhow::Result<()>>) {
    let (server, client) = Connection::memory();
    let handle = std::thread::spawn(move || gd_server::serve(server));
    (client, handle)
}

/// A minimal project with a `project.godot` and a bundled-API-disabled init (no native dump needed —
/// formatting is parse-free / analyze-free).
fn bare_project() -> TempProject {
    let p = TempProject::new();
    p.write(
        "project.godot",
        "config_version=5\n\n[application]\n\nconfig/name=\"Fmt\"\n",
    );
    p
}

/// `initialize` (+`initialized`) with an optional `formatter` config block, then open `(rel, text)`
/// files. Returns the parsed `InitializeResult` so a test can assert the advertised formatting cap.
fn init_open(
    project: &TempProject,
    client: &Connection,
    formatter: Option<serde_json::Value>,
    files: &[(&str, &str)],
) -> InitializeResult {
    let mut opts = serde_json::json!({
        "projectRoot": project.root.as_str(),
        "autoDumpExtensionApi": false,
        "embeddedApiFallback": false,
    });
    if let Some(f) = formatter {
        opts["formatter"] = f;
    }
    let init = InitializeParams {
        initialization_options: Some(opts),
        capabilities: ClientCapabilities::default(),
        ..Default::default()
    };
    client.sender.send(request(1, "initialize", init)).unwrap();
    let init_resp = recv_response(client);
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

    for (rel, text) in files {
        let uri = file_uri(&project.root.join(rel));
        client
            .sender
            .send(notification(
                "textDocument/didOpen",
                DidOpenTextDocumentParams {
                    text_document: TextDocumentItem {
                        uri,
                        language_id: "gdscript".to_string(),
                        version: 1,
                        text: (*text).to_string(),
                    },
                },
            ))
            .unwrap();
    }
    // Drain the didOpen diagnostics + any stray notifications so later assertions on showMessage
    // start from a clean channel.
    while try_recv(client, Duration::from_millis(300)).is_some() {}
    result
}

/// A `formatter` config block running the stub in `mode`.
fn formatter_cfg(mode: &str) -> serde_json::Value {
    serde_json::json!({ "command": STUB, "args": [mode] })
}

/// Send a `textDocument/formatting` request for `uri` with the given request id.
fn send_format(client: &Connection, id: i32, uri: &Uri) {
    let params = DocumentFormattingParams {
        text_document: TextDocumentIdentifier { uri: uri.clone() },
        options: FormattingOptions {
            tab_size: 4,
            insert_spaces: false,
            properties: Default::default(),
            trim_trailing_whitespace: None,
            insert_final_newline: None,
            trim_final_newlines: None,
        },
        work_done_progress_params: WorkDoneProgressParams::default(),
    };
    client
        .sender
        .send(request(id, "textDocument/formatting", params))
        .unwrap();
}

/// Receive the formatting response for `id`, returning the deserialized edits (`None` for a `null`
/// result). Panics on a JSON-RPC error (formatting answers `null`, never an error, in v1). Skips any
/// notifications that arrive first (used by the happy-path tests, which don't assert on messages).
fn recv_edits(client: &Connection) -> Option<Vec<TextEdit>> {
    let resp = recv_response(client);
    edits_of_response(&resp)
}

/// Decode a formatting response into edits (`None` for `null`). Asserts the response is not an error.
fn edits_of_response(resp: &Response) -> Option<Vec<TextEdit>> {
    assert!(
        resp.error.is_none(),
        "formatting returned an error (should be null on failure): {:?}",
        resp.error
    );
    match &resp.result {
        Some(serde_json::Value::Null) | None => None,
        Some(v) => Some(serde_json::from_value(v.clone()).expect("Vec<TextEdit>")),
    }
}

/// Drive the channel until the formatting `Response` for `id` arrives, collecting EVERY
/// `window/showMessage` notification seen along the way (ordering-agnostic: a warning may arrive
/// before or after the response). Returns `(edits, warnings)` where each warning is `(type, message)`.
/// This is the correct rig for the failure tests — `recv_response` alone discards the notification
/// the test is asserting on.
fn format_with_messages(
    client: &Connection,
    id: i32,
    timeout: Duration,
) -> (Option<Vec<TextEdit>>, Vec<(i64, String)>) {
    let mut warnings = Vec::new();
    let mut edits = None;
    let mut got_response = false;
    let deadline = Instant::now() + timeout;
    // Phase 1: wait for the response, banking any showMessage that precedes it.
    while Instant::now() < deadline && !got_response {
        match try_recv(client, Duration::from_millis(100)) {
            Some(Message::Response(r)) if r.id == id.into() => {
                edits = edits_of_response(&r);
                got_response = true;
            }
            Some(Message::Notification(n)) if n.method == "window/showMessage" => {
                warnings.push(show_message_tuple(&n));
            }
            Some(_) | None => continue,
        }
    }
    assert!(
        got_response,
        "formatting response for id {id} never arrived"
    );
    // Phase 2: a short tail drain for a showMessage emitted just after the response (the handler
    // returns the response, then the worker may flush the notification a beat later — both are sent
    // from the same worker, so the order is implementation detail the test must not depend on).
    warnings.extend(collect_show_messages(client, Duration::from_millis(400)));
    (edits, warnings)
}

/// Extract `(type, message)` from a `window/showMessage` notification.
fn show_message_tuple(n: &lsp_server::Notification) -> (i64, String) {
    let typ = n.params["type"].as_i64().unwrap_or(0);
    let msg = n.params["message"].as_str().unwrap_or("").to_string();
    (typ, msg)
}

/// Apply LSP `TextEdit`s to `text` (non-overlapping; applied last-first by start offset so earlier
/// offsets stay valid). Uses a byte-offset mapper consistent with the server's UTF-16 default.
fn apply_edits(text: &str, edits: &[TextEdit]) -> String {
    let rope = ropey::Rope::from_str(text);
    // Mirror the server's default encoding (UTF-16) for position→byte mapping.
    let mut byte_edits: Vec<(usize, usize, &str)> = edits
        .iter()
        .map(|e| {
            let start = pos_to_byte(&rope, e.range.start);
            let end = pos_to_byte(&rope, e.range.end);
            (start, end, e.new_text.as_str())
        })
        .collect();
    byte_edits.sort_by_key(|(s, _, _)| *s);
    let mut out = text.to_string();
    for (start, end, new_text) in byte_edits.into_iter().rev() {
        out.replace_range(start..end, new_text);
    }
    out
}

/// UTF-16 LSP position → byte offset over `rope` (clamping), matching the server's default encoding.
fn pos_to_byte(rope: &ropey::Rope, pos: lsp_types::Position) -> usize {
    let line = (pos.line as usize).min(rope.len_lines().saturating_sub(1));
    let line_start_byte = rope.line_to_byte(line);
    let line_slice = rope.line(line);
    let cu = (pos.character as usize).min(line_slice.len_utf16_cu());
    let char_in_line = line_slice.utf16_cu_to_char(cu);
    let line_start_char = rope.byte_to_char(line_start_byte);
    rope.char_to_byte(line_start_char + char_in_line)
}

/// Collect every `window/showMessage` notification arriving within `window` (the channel is drained
/// at init, so these are all from the action under test). Returns each notification's `(type, message)`.
fn collect_show_messages(client: &Connection, window: Duration) -> Vec<(i64, String)> {
    let mut msgs = Vec::new();
    let deadline = Instant::now() + window;
    while Instant::now() < deadline {
        match try_recv(client, Duration::from_millis(100)) {
            Some(Message::Notification(n)) if n.method == "window/showMessage" => {
                msgs.push(show_message_tuple(&n));
            }
            Some(_) => continue,
            None => continue,
        }
    }
    msgs
}

/// True when `caps.document_formatting_provider` is advertised (`Some`).
fn advertises_formatting(result: &InitializeResult) -> bool {
    result.capabilities.document_formatting_provider.is_some()
}

// ---------------------------------------------------------------------------------------------
// configured → advertised + round-trip
// ---------------------------------------------------------------------------------------------

/// Configured: the capability is advertised, and a request pipes the buffer through the stub. The
/// returned edits, applied to the original, equal the stub's stdout (squeeze-spaces).
#[test]
fn configured_advertises_and_round_trips() {
    let p = bare_project();
    let src = "func  f():\n\tvar   x   =   1\n\treturn  x\n";
    let (client, thread) = boot();
    let result = init_open(
        &p,
        &client,
        Some(formatter_cfg("squeeze")),
        &[("a.gd", src)],
    );
    assert!(
        advertises_formatting(&result),
        "documentFormattingProvider must be advertised when a formatter is configured"
    );

    let uri = file_uri(&p.root.join("a.gd"));
    send_format(&client, 10, &uri);
    let edits = recv_edits(&client).expect("a reformat must produce edits");
    assert!(!edits.is_empty());

    // The stub squeezes runs of spaces; applying the edits reproduces exactly that output.
    let expected = "func f():\n\tvar x = 1\n\treturn x\n";
    assert_eq!(apply_edits(src, &edits), expected);

    shutdown(&client, thread);
}

/// A passthru formatter (output == input) yields NO edit (and no spurious cursor jump).
#[test]
fn passthru_yields_no_edit() {
    let p = bare_project();
    let src = "func f():\n\tpass\n";
    let (client, thread) = boot();
    init_open(
        &p,
        &client,
        Some(formatter_cfg("passthru")),
        &[("a.gd", src)],
    );

    let uri = file_uri(&p.root.join("a.gd"));
    send_format(&client, 10, &uri);
    assert!(
        recv_edits(&client).is_none(),
        "an unchanged formatter result must yield no edits"
    );

    shutdown(&client, thread);
}

// ---------------------------------------------------------------------------------------------
// unconfigured → NOT advertised + no edits
// ---------------------------------------------------------------------------------------------

/// Unconfigured (no `formatter` block): the capability is NOT advertised, and a stray request (a
/// non-conforming client) returns null/no-edits without crashing the session.
#[test]
fn unconfigured_not_advertised_and_no_edits() {
    let p = bare_project();
    let src = "func  f():\n\tpass\n";
    let (client, thread) = boot();
    let result = init_open(&p, &client, None, &[("a.gd", src)]);
    assert!(
        !advertises_formatting(&result),
        "documentFormattingProvider must NOT be advertised when unconfigured"
    );

    // A defensive request still answers null (never an error / panic).
    let uri = file_uri(&p.root.join("a.gd"));
    send_format(&client, 10, &uri);
    assert!(recv_edits(&client).is_none(), "unconfigured → no edits");

    // The session is still alive: a follow-up request answers too.
    send_format(&client, 11, &uri);
    assert!(recv_edits(&client).is_none());

    shutdown(&client, thread);
}

// ---------------------------------------------------------------------------------------------
// crash (non-zero exit) → no edits + exactly one warning, deduped on the second request
// ---------------------------------------------------------------------------------------------

#[test]
fn crash_yields_no_edits_and_one_warning_then_dedupes() {
    let p = bare_project();
    let src = "func f():\n\tpass\n";
    let (client, thread) = boot();
    init_open(&p, &client, Some(formatter_cfg("fail")), &[("a.gd", src)]);
    let uri = file_uri(&p.root.join("a.gd"));

    // First request: no edits + exactly one WARNING showMessage.
    send_format(&client, 10, &uri);
    let (edits, warnings) = format_with_messages(&client, 10, Duration::from_secs(10));
    assert!(edits.is_none(), "a failed format must yield no edits");
    assert_eq!(
        warnings.len(),
        1,
        "exactly one showMessage on first failure; got {warnings:?}"
    );
    assert_eq!(warnings[0].0, 2, "MessageType::WARNING is 2");

    // Second request (same failure class): no edits AND no new warning (per-session dedupe).
    send_format(&client, 11, &uri);
    let (edits2, again) = format_with_messages(&client, 11, Duration::from_secs(10));
    assert!(edits2.is_none());
    assert!(
        again.is_empty(),
        "a repeat of the same failure class must NOT re-warn; got {again:?}"
    );

    shutdown(&client, thread);
}

// ---------------------------------------------------------------------------------------------
// timeout → killed, no edits + one warning
// ---------------------------------------------------------------------------------------------

/// The stub sleeps far past the handler's bounded timeout; the handler kills it, returns no edits,
/// and warns once. (The handler's FORMAT_TIMEOUT is a few seconds; this test budgets generously.)
#[test]
fn timeout_is_killed_no_edits_one_warning() {
    let p = bare_project();
    let src = "func f():\n\tpass\n";
    let (client, thread) = boot();
    init_open(&p, &client, Some(formatter_cfg("sleep")), &[("a.gd", src)]);
    let uri = file_uri(&p.root.join("a.gd"));

    send_format(&client, 10, &uri);
    // The bounded timeout is a few seconds; allow margin for CI scheduling.
    let (edits, warnings) = format_with_messages(&client, 10, Duration::from_secs(20));
    assert!(
        edits.is_none(),
        "a timed-out format must yield no edits; got {edits:?}"
    );
    assert_eq!(
        warnings.len(),
        1,
        "one warning on timeout; got {warnings:?}"
    );
    assert_eq!(warnings[0].0, 2);

    shutdown(&client, thread);
}

// ---------------------------------------------------------------------------------------------
// non-UTF-8 stdout → no edits + one warning
// ---------------------------------------------------------------------------------------------

#[test]
fn non_utf8_output_yields_no_edits_one_warning() {
    let p = bare_project();
    let src = "func f():\n\tpass\n";
    let (client, thread) = boot();
    init_open(&p, &client, Some(formatter_cfg("binary")), &[("a.gd", src)]);
    let uri = file_uri(&p.root.join("a.gd"));

    send_format(&client, 10, &uri);
    let (edits, warnings) = format_with_messages(&client, 10, Duration::from_secs(10));
    assert!(
        edits.is_none(),
        "non-UTF-8 formatter output must be rejected → no edits"
    );
    assert_eq!(
        warnings.len(),
        1,
        "one warning on non-UTF-8 output; got {warnings:?}"
    );
    assert_eq!(warnings[0].0, 2);

    shutdown(&client, thread);
}

// ---------------------------------------------------------------------------------------------
// minimal-diff: a one-line reformat touches only that line
// ---------------------------------------------------------------------------------------------

/// The stub's `touchone` mode reformats EXACTLY the line(s) containing `MARK`. The returned edit must
/// touch only that region (not the whole document), and applying it must reproduce the stub's output.
#[test]
fn minimal_diff_touches_only_changed_region() {
    let p = bare_project();
    // Only the middle line changes (it has the MARK token + squeezable spaces).
    let src = "func f():\n\tvar   x   =   1 # MARK\n\treturn x\n";
    let (client, thread) = boot();
    init_open(
        &p,
        &client,
        Some(formatter_cfg("touchone")),
        &[("a.gd", src)],
    );
    let uri = file_uri(&p.root.join("a.gd"));

    send_format(&client, 10, &uri);
    let edits = recv_edits(&client).expect("a one-line change → one edit");
    assert_eq!(edits.len(), 1, "minimal-diff emits a single coalesced edit");

    // The edit is confined to the changed (middle) line — line 1 (0-based), NOT spanning line 0 or
    // the final line. This is the cursor-preservation property: a whole-doc replace would start at
    // line 0.
    let e = &edits[0];
    assert_eq!(e.range.start.line, 1, "edit starts at the changed line");
    assert_eq!(
        e.range.end.line, 2,
        "edit ends at the start of the next (unchanged) line, not the document end"
    );

    let expected = "func f():\n\tvar x = 1 # MARK\n\treturn x\n";
    assert_eq!(apply_edits(src, &edits), expected);

    shutdown(&client, thread);
}

/// Regression: a LARGE document must not deadlock the stdin/stdout pipes. A sequential
/// write-all-then-read deadlocked any doc whose formatter output exceeded the OS pipe buffer
/// (~64 KiB), silently failing with a misleading timeout on exactly the large files gdls targets.
/// With concurrent stdin-write / stdout-read it formats cleanly with no timeout warning.
#[test]
fn large_document_does_not_deadlock() {
    let p = bare_project();
    // ~640 KiB — far past any OS pipe buffer — with squeezable runs of spaces.
    let mut src = String::with_capacity(700_000);
    for i in 0..40_000 {
        src.push_str("\tvar    x    =    ");
        src.push_str(&(i % 10).to_string());
        src.push('\n');
    }
    let (client, thread) = boot();
    init_open(
        &p,
        &client,
        Some(formatter_cfg("squeeze")),
        &[("a.gd", &src)],
    );
    let uri = file_uri(&p.root.join("a.gd"));

    send_format(&client, 10, &uri);
    let (edits, warnings) = format_with_messages(&client, 10, Duration::from_secs(20));
    assert!(
        edits.as_ref().is_some_and(|e| !e.is_empty()),
        "a large squeezable doc must format (concurrent stdin/stdout — no pipe deadlock); got {edits:?}"
    );
    assert!(
        warnings.is_empty(),
        "a healthy formatter on a large doc must NOT warn (no spurious deadlock-timeout); got {warnings:?}"
    );
    // Sanity: the squeeze shrank the document (collapsed the space runs) and the edit round-trips.
    let formatted = apply_edits(&src, edits.as_ref().unwrap());
    assert!(
        formatted.len() < src.len() && !formatted.contains("    "),
        "squeeze must collapse the space runs across the whole large doc"
    );

    shutdown(&client, thread);
}

// ---------------------------------------------------------------------------------------------
// #135 — head-of-line blocking: a slow format must NOT stall an unrelated request
// ---------------------------------------------------------------------------------------------

/// A `formatter` config running the stub in `mode` with an extra `arg` (the delay / marker path).
fn formatter_cfg_arg(mode: &str, arg: &str) -> serde_json::Value {
    serde_json::json!({ "command": STUB, "args": [mode, arg] })
}

/// Send a `textDocument/documentSymbol` request — a fast, formatter-independent request used to
/// prove the worker is free while a slow format runs off-worker (#135).
fn send_document_symbol(client: &Connection, id: i32, uri: &Uri) {
    let params = DocumentSymbolParams {
        text_document: TextDocumentIdentifier { uri: uri.clone() },
        work_done_progress_params: WorkDoneProgressParams::default(),
        partial_result_params: PartialResultParams::default(),
    };
    client
        .sender
        .send(request(id, "textDocument/documentSymbol", params))
        .unwrap();
}

/// Send a `$/cancelRequest` for the numeric request `id`.
fn send_cancel(client: &Connection, id: i32) {
    client
        .sender
        .send(notification(
            "$/cancelRequest",
            CancelParams {
                id: NumberOrString::Number(id),
            },
        ))
        .unwrap();
}

/// Receive the next `Response` with `id`, skipping notifications/other ids, within `timeout`.
/// Returns the response and the elapsed time from the call.
fn recv_response_id(client: &Connection, id: i32, timeout: Duration) -> (Response, Duration) {
    let start = Instant::now();
    let deadline = start + timeout;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        assert!(
            !remaining.is_zero(),
            "response for id {id} never arrived within {timeout:?}"
        );
        match try_recv(client, remaining.min(Duration::from_millis(200))) {
            Some(Message::Response(r)) if r.id == RequestId::from(id) => {
                return (r, start.elapsed())
            }
            _ => continue,
        }
    }
}

/// #135 head-of-line blocking. A SLOW format (`delaysqueeze 2000` — sleeps 2s before formatting)
/// must run OFF the request worker, so a `documentSymbol` sent right after it is answered promptly
/// rather than waiting out the 2s format. Against the prior on-worker code the format blocked the
/// single worker for the full 2s, so `documentSymbol` could not be answered until the format
/// finished — i.e. the slow format's response would arrive FIRST. With the off-worker move the fast
/// request answers within a fraction of the format's runtime, and BEFORE the format response.
#[test]
fn slow_format_does_not_block_unrelated_request() {
    let p = bare_project();
    let src = "func  f():\n\tvar   x   =   1\n";
    let (client, thread) = boot();
    init_open(
        &p,
        &client,
        Some(formatter_cfg_arg("delaysqueeze", "2000")),
        &[("a.gd", src)],
    );
    let uri = file_uri(&p.root.join("a.gd"));

    // Fire the slow format (id 10), then immediately a fast documentSymbol (id 11).
    send_format(&client, 10, &uri);
    send_document_symbol(&client, 11, &uri);

    // The fast request must answer well inside the 2s format sleep — proof the worker is free.
    let (symbol_resp, symbol_elapsed) = recv_response_id(&client, 11, Duration::from_secs(2));
    assert!(
        symbol_resp.error.is_none(),
        "documentSymbol must succeed while the slow format runs off-worker; got {:?}",
        symbol_resp.error
    );
    assert!(
        symbol_elapsed < Duration::from_millis(1500),
        "documentSymbol took {symbol_elapsed:?} — the slow format blocked the worker (HOL); it \
         must answer well inside the 2s format sleep"
    );

    // The slow format still completes correctly afterwards (off-worker, on its own timeline).
    let (format_resp, _) = recv_response_id(&client, 10, Duration::from_secs(10));
    assert!(
        format_resp.error.is_none(),
        "the slow format must still succeed; got {:?}",
        format_resp.error
    );
    let edits = edits_of_response(&format_resp).expect("delaysqueeze changes the doc → edits");
    assert_eq!(apply_edits(src, &edits), "func f():\n\tvar x = 1\n");

    shutdown(&client, thread);
}

// ---------------------------------------------------------------------------------------------
// #136 — cancel preempts an in-flight format subprocess (prompt child-kill, no late edit, no orphan)
// ---------------------------------------------------------------------------------------------

/// #136. A `$/cancelRequest` for an in-flight format must KILL the subprocess promptly and answer
/// `RequestCancelled` — not run the child out to the handler's 5s timeout. The stub's `markerafter`
/// mode sleeps 30s, then (only if it survives) writes a MARKER file. The discriminators:
///   * the response is `RequestCancelled` (-32800), arriving WELL UNDER the 5s format timeout —
///     proving the child was killed by the cancel poll, not by the timeout backstop (the latency
///     discriminator: against the prior code with no poll-kill, a cancel landed only at the
///     post-handler gate AFTER the 5s timeout fired, so the response would take ~5s);
///   * the marker file NEVER appears — proving no orphaned subprocess completed and no late edit
///     was produced after the cancel (the mutating-consumer firewall).
#[test]
fn cancel_kills_inflight_format_promptly_no_late_edit() {
    let p = bare_project();
    let src = "func  f():\n\tpass\n";
    let marker = p.root.join("FORMAT_MARKER");
    let (client, thread) = boot();
    init_open(
        &p,
        &client,
        Some(formatter_cfg_arg("markerafter", marker.as_str())),
        &[("a.gd", src)],
    );
    let uri = file_uri(&p.root.join("a.gd"));

    // Fire the (30s-sleeping) format, then cancel it almost immediately.
    send_format(&client, 10, &uri);
    // A brief beat so the subprocess is actually spawned and the format thread is in its wait loop.
    std::thread::sleep(Duration::from_millis(150));
    send_cancel(&client, 10);

    // The cancel must be answered RequestCancelled FAR inside the 5s timeout (the poll-kill fired).
    let (resp, elapsed) = recv_response_id(&client, 10, Duration::from_secs(5));
    assert_eq!(
        resp.error.as_ref().map(|e| e.code),
        Some(-32800),
        "a cancelled in-flight format must answer RequestCancelled (-32800); got {:?}",
        resp.error
    );
    assert!(
        elapsed < Duration::from_secs(3),
        "cancel took {elapsed:?} — the child was not killed promptly (it ran toward the 5s \
         timeout instead of the cancel poll). #136 requires a prompt kill, not a timeout."
    );

    // No edit can have been applied: the response is an error, never edits. And the marker must
    // never appear — the child was killed mid-sleep, so it never reached its write. Poll a short
    // window to be sure no late write lands after the response.
    let marker_deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < marker_deadline {
        assert!(
            !marker.exists(),
            "the format subprocess wrote its marker AFTER cancel — an orphan completed / a late \
             edit was produced (no-orphan / mutating-consumer firewall violated)"
        );
        std::thread::sleep(Duration::from_millis(100));
    }

    // The session is still healthy: a follow-up request answers.
    send_document_symbol(&client, 11, &uri);
    let (after, _) = recv_response_id(&client, 11, Duration::from_secs(5));
    assert!(
        after.error.is_none(),
        "session must survive a cancelled format"
    );

    shutdown(&client, thread);
}
