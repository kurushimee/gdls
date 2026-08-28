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

use std::io::{Read, Write};
use std::process::{Child, Command, Stdio};

use common::sample_project;

fn gdls_bin() -> &'static str {
    env!("CARGO_BIN_EXE_gdls")
}

/// Frame one JSON-RPC message the way the LSP wire does.
fn frame(body: &serde_json::Value) -> Vec<u8> {
    let text = serde_json::to_string(body).expect("serialize");
    format!("Content-Length: {}\r\n\r\n{text}", text.len()).into_bytes()
}

/// Spawn `gdls` on stdio and complete `initialize` / `initialized` against `root`.
///
/// The `initialize` RESPONSE is not read back — these tests only care about the process status,
/// and leaving stdout unread is exactly what a client that dies mid-session does. stdout is piped
/// (not inherited) so the harness's own output stays clean; the pipe buffer is far larger than the
/// handshake, so the server never blocks writing into it.
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
            "rootUri": format!("file://{root}"),
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
    child
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
