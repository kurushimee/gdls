//! #262 — the process exit status LSP 3.17 §exit specifies.
//!
//! > The server should exit with `success` code 0 if the shutdown request has been received
//! > before; otherwise with `error` code 1.
//!
//! That distinction is the only way a supervising client can tell a clean stop from an abrupt
//! one, and it is observable ONLY on the real binary — `serve()` over an in-memory `Connection`
//! never touches the process status. So these tests spawn `gdls` itself and drive a minimal
//! handshake down its stdin.
//!
//! A transport close (stdin EOF with no `exit` at all) stays 0: the client is gone, which is not
//! a protocol violation.

mod common;

use std::io::{BufRead, BufReader, Read, Write};
use std::process::{Child, ChildStdout, Command, Stdio};

use common::{file_uri, sample_project};

fn gdls_bin() -> &'static str {
    env!("CARGO_BIN_EXE_gdls")
}

/// Frame one JSON-RPC message the way the LSP wire does.
fn frame(body: &serde_json::Value) -> Vec<u8> {
    let text = serde_json::to_string(body).expect("serialize");
    format!("Content-Length: {}\r\n\r\n{text}", text.len()).into_bytes()
}

/// Spawn `gdls` on stdio and complete `initialize` / `initialized` against `root`, returning the
/// child with its stdout already positioned past the `initialize` response.
///
/// That response IS read back, and it is the whole point of reading it: without it a server that
/// died during the handshake is indistinguishable from a live one, because a dead process and a
/// clean `exit` both end in a closed pipe. Reading it turns "the handshake never happened" into a
/// named failure on every platform instead of a status code that only looks wrong on one of them.
/// stdout is piped (not inherited) so the harness's own output stays clean.
fn spawn_initialized(root: &camino::Utf8Path) -> Child {
    let mut child = Command::new(gdls_bin())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn gdls binary");
    let stdin = child.stdin.as_mut().expect("stdin piped");
    let init = serde_json::json!({
        "jsonrpc": "2.0", "id": 1, "method": "initialize",
        "params": {
            "processId": std::process::id(),
            // Built through the production URI helper, not `format!("file://{root}")`: a Windows
            // temp root is a drive path, and pasting it after the scheme yields `file://C:\\Users\\...`
            // — an unparseable URI that used to take the whole handshake down (#279).
            "rootUri": file_uri(root).as_str(),
            "capabilities": {},
        }
    });
    stdin.write_all(&frame(&init)).expect("write initialize");
    stdin
        .write_all(&frame(&serde_json::json!({
            "jsonrpc": "2.0", "method": "initialized", "params": {}
        })))
        .expect("write initialized");
    stdin.flush().expect("flush");
    expect_initialize_response(&mut child);
    child
}

/// Read frames off the child's stdout until the `initialize` response arrives, so every test below
/// starts from a server that is demonstrably up. Panics with the server's own words if the pipe
/// closes first — that is what a handshake-time death looks like from the client side.
fn expect_initialize_response(child: &mut Child) {
    let stdout = child.stdout.as_mut().expect("stdout piped");
    let mut reader = BufReader::new(stdout);
    for _ in 0..8 {
        let Some(body) = read_frame(&mut reader) else {
            panic!(
                "gdls closed stdout before answering `initialize` — the handshake died (see #279)"
            );
        };
        let msg: serde_json::Value = serde_json::from_slice(&body).expect("server sent valid JSON");
        if msg.get("id").and_then(serde_json::Value::as_i64) == Some(1) {
            assert!(
                msg.get("result").is_some(),
                "`initialize` must answer with a result, got {msg}"
            );
            return;
        }
    }
    panic!("no `initialize` response within the first 8 frames from the server");
}

/// Read one `Content-Length`-framed message body, or `None` at end of stream.
fn read_frame(reader: &mut BufReader<&mut ChildStdout>) -> Option<Vec<u8>> {
    let mut len = None;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).ok()? == 0 {
            return None;
        }
        let trimmed = line.trim_end_matches(['\r', '\n']);
        if trimmed.is_empty() {
            break;
        }
        if let Some(v) = trimmed.strip_prefix("Content-Length: ") {
            len = v.parse::<usize>().ok();
        }
    }
    let mut body = vec![0u8; len?];
    reader.read_exact(&mut body).ok()?;
    Some(body)
}

fn send(child: &mut Child, msg: serde_json::Value) {
    let stdin = child.stdin.as_mut().expect("stdin piped");
    stdin.write_all(&frame(&msg)).expect("write");
    stdin.flush().expect("flush");
}

/// Drain stdout on a helper thread while waiting, so a server that fills the pipe can't deadlock
/// against a test that is only waiting on the status.
fn wait_status(mut child: Child) -> Option<i32> {
    let mut out = child.stdout.take().expect("stdout piped");
    let drain = std::thread::spawn(move || {
        let mut sink = Vec::new();
        let _ = out.read_to_end(&mut sink);
    });
    let status = child.wait().expect("wait for gdls");
    let _ = drain.join();
    status.code()
}

#[test]
fn exit_after_shutdown_returns_zero() {
    let project = sample_project();
    let mut child = spawn_initialized(&project.root);
    send(
        &mut child,
        serde_json::json!({"jsonrpc": "2.0", "id": 2, "method": "shutdown", "params": null}),
    );
    send(
        &mut child,
        serde_json::json!({"jsonrpc": "2.0", "method": "exit", "params": null}),
    );
    assert_eq!(
        wait_status(child),
        Some(0),
        "`exit` after `shutdown` is the clean handshake — LSP 3.17 §exit requires status 0"
    );
}

#[test]
fn exit_without_shutdown_returns_one() {
    let project = sample_project();
    let mut child = spawn_initialized(&project.root);
    send(
        &mut child,
        serde_json::json!({"jsonrpc": "2.0", "method": "exit", "params": null}),
    );
    assert_eq!(
        wait_status(child),
        Some(1),
        "`exit` with NO prior `shutdown` is the spec's error case — status 1 is how a supervising \
         client detects that it tore the server down without a handshake"
    );
}

#[test]
fn stdin_eof_without_exit_returns_zero() {
    let project = sample_project();
    let mut child = spawn_initialized(&project.root);
    // Close stdin without any `exit`: the transport went away. Not a protocol violation.
    drop(child.stdin.take());
    assert_eq!(
        wait_status(child),
        Some(0),
        "a transport close is the client leaving, not a missing handshake — it must not be \
         reported as the §exit error case"
    );
}
