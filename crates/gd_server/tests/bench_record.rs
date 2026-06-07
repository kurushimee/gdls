//! WP-P3 (M5) round-trip test: record a synthetic JSON-RPC trace via an in-memory server, then
//! replay the captured artifact and confirm the replay returns metrics for every recorded entry.
//!
//! Uses [`gd_server::serve_with_recorder`] to inject the recorder explicitly — avoids mutating
//! the global env (`std::env::set_var` is unsafe under Rust 2024 thread-safety rules) and keeps
//! the test independent of any sibling test in this binary that might be reading env vars.

mod common;

use std::time::Duration;

use gd_server::bench::{
    BenchArtifact, BenchRecorder, TraceEntry, ARTIFACT_VERSION, DEFAULT_TRACE_CAPACITY,
};
use lsp_server::{Connection, Message, RequestId};
use lsp_types::{
    ClientCapabilities, DidOpenTextDocumentParams, DocumentSymbolParams, GeneralClientCapabilities,
    InitializeParams, InitializedParams, PositionEncodingKind, TextDocumentIdentifier,
    TextDocumentItem, Uri,
};

use common::{notification, recv, request, try_recv};

/// Drive a small session (initialize + didOpen + documentSymbol + shutdown/exit) with a recorder
/// attached, then load the artifact and replay it — asserting on the schema shape, the trace
/// contents, the open-buffer snapshot, and the replay metrics.
#[test]
fn record_then_replay_round_trips() {
    let tmpdir = tempfile::tempdir().expect("tempdir");
    let artifact_path = tmpdir.path().join("trace.json");

    // ----- record phase -----
    let recorder = BenchRecorder::new(DEFAULT_TRACE_CAPACITY, artifact_path.clone());
    let (server, client) = Connection::memory();
    let server_thread =
        std::thread::spawn(move || gd_server::serve_with_recorder(server, Some(recorder)));

    let init = InitializeParams {
        capabilities: ClientCapabilities {
            general: Some(GeneralClientCapabilities {
                position_encodings: Some(vec![PositionEncodingKind::UTF8]),
                ..Default::default()
            }),
            ..Default::default()
        },
        ..Default::default()
    };
    client
        .sender
        .send(request(1, "initialize", init))
        .expect("send initialize");
    let Message::Response(init_resp) = recv(&client) else {
        panic!("expected initialize response");
    };
    assert_eq!(init_resp.id, RequestId::from(1));

    client
        .sender
        .send(notification("initialized", InitializedParams {}))
        .expect("send initialized");

    let uri: Uri = "file:///bench/a.gd".parse().unwrap();
    client
        .sender
        .send(notification(
            "textDocument/didOpen",
            DidOpenTextDocumentParams {
                text_document: TextDocumentItem {
                    uri: uri.clone(),
                    language_id: "gdscript".to_string(),
                    version: 1,
                    text: "extends Node\n".to_string(),
                },
            },
        ))
        .expect("send didOpen");
    // Drain the publishDiagnostics push that follows didOpen.
    let _ = try_recv(&client, Duration::from_secs(2));

    client
        .sender
        .send(request(
            2,
            "textDocument/documentSymbol",
            DocumentSymbolParams {
                text_document: TextDocumentIdentifier { uri: uri.clone() },
                work_done_progress_params: Default::default(),
                partial_result_params: Default::default(),
            },
        ))
        .expect("send documentSymbol");
    let Message::Response(sym_resp) = recv(&client) else {
        panic!("expected documentSymbol response");
    };
    assert_eq!(sym_resp.id, RequestId::from(2));

    // Clean shutdown so the server thread joins and the recorder flushes.
    client
        .sender
        .send(request(3, "shutdown", serde_json::Value::Null))
        .expect("send shutdown");
    let _ = recv(&client);
    client
        .sender
        .send(notification("exit", serde_json::Value::Null))
        .expect("send exit");
    server_thread
        .join()
        .expect("server thread panicked")
        .expect("server returned an error");

    // ----- assert artifact -----
    assert!(
        artifact_path.is_file(),
        "recorder did not flush to {}",
        artifact_path.display()
    );
    let raw = std::fs::read_to_string(&artifact_path).expect("read artifact");
    let artifact: BenchArtifact = serde_json::from_str(&raw).expect("parse artifact");
    assert_eq!(artifact.version, ARTIFACT_VERSION);
    assert!(
        artifact.captured_at_unix_secs > 0,
        "captured_at_unix_secs unset"
    );

    // didOpen + documentSymbol + shutdown should all be in the ring. (initialize + initialized fire
    // BEFORE the recorder field is checked in the dispatch loop, but they pass through it too —
    // we don't depend on initialize specifically being recorded, only that the substantive ops are.)
    let methods: Vec<&str> = artifact
        .trace
        .iter()
        .map(|e| match e {
            TraceEntry::Request { method, .. } | TraceEntry::Notification { method, .. } => {
                method.as_str()
            }
        })
        .collect();
    assert!(
        methods.contains(&"textDocument/didOpen"),
        "trace missing didOpen: {methods:?}"
    );
    assert!(
        methods.contains(&"textDocument/documentSymbol"),
        "trace missing documentSymbol: {methods:?}"
    );

    // The buffer was open at shutdown, so the snapshot should carry it (didClose was never sent).
    assert_eq!(artifact.open_buffers.len(), 1);
    assert_eq!(artifact.open_buffers[0].uri, "file:///bench/a.gd");
    assert_eq!(artifact.open_buffers[0].text, "extends Node\n");
    assert_eq!(artifact.open_buffers[0].version, 1);

    // ----- replay -----
    let metrics = gd_server::bench::replay(&artifact_path).expect("replay");
    // One metric row per trace entry (notifications report elapsed=tiny, requests report a real
    // request_id from the replay's renumbering).
    assert_eq!(
        metrics.len(),
        artifact.trace.len(),
        "metrics count mismatch"
    );
    let request_count = metrics.iter().filter(|m| !m.notification).count();
    assert!(
        request_count >= 1,
        "expected at least one request metric in {metrics:?}"
    );
}

/// Eviction: a recorder at capacity must drop the oldest entry, not the newest.
#[test]
fn ring_buffer_evicts_oldest() {
    let tmpdir = tempfile::tempdir().expect("tempdir");
    let mut rec = BenchRecorder::new(2, tmpdir.path().join("trace.json"));
    rec.record(TraceEntry::Notification {
        method: "first".to_string(),
        params: serde_json::Value::Null,
    });
    rec.record(TraceEntry::Notification {
        method: "second".to_string(),
        params: serde_json::Value::Null,
    });
    rec.record(TraceEntry::Notification {
        method: "third".to_string(),
        params: serde_json::Value::Null,
    });
    assert_eq!(rec.len(), 2);
    rec.flush(Vec::new()).expect("flush");

    let raw = std::fs::read_to_string(tmpdir.path().join("trace.json")).expect("read trace.json");
    let artifact: BenchArtifact = serde_json::from_str(&raw).expect("parse");
    let methods: Vec<&str> = artifact
        .trace
        .iter()
        .map(|e| match e {
            TraceEntry::Request { method, .. } | TraceEntry::Notification { method, .. } => {
                method.as_str()
            }
        })
        .collect();
    assert_eq!(methods, vec!["second", "third"], "oldest entry not evicted");
}

/// `--record` is opt-in: with no env var and no injected recorder, the server runs without ever
/// touching the bench module. Smoke-tests the lifecycle path matches the M0 baseline.
#[test]
fn unrecorded_serve_is_identical_to_baseline() {
    let (server, client) = Connection::memory();
    let server_thread = std::thread::spawn(move || gd_server::serve_with_recorder(server, None));

    client
        .sender
        .send(request(1, "initialize", InitializeParams::default()))
        .expect("send initialize");
    let _ = recv(&client);
    client
        .sender
        .send(notification("initialized", InitializedParams {}))
        .expect("send initialized");
    client
        .sender
        .send(request(2, "shutdown", serde_json::Value::Null))
        .expect("send shutdown");
    let _ = recv(&client);
    client
        .sender
        .send(notification("exit", serde_json::Value::Null))
        .expect("send exit");
    server_thread
        .join()
        .expect("panic in server thread")
        .expect("serve_with_recorder returned err");
}
