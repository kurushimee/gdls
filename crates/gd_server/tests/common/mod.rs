//! Shared rig for `gd_server` integration tests: a throwaway on-disk project, a minimal
//! `extension_api.json` covering the canonical four-class chain, a sample-project factory, plus
//! the LSP message-receive and `file://` URI helpers every integration test needs.
//!
//! Hoisted from `tests/indexing.rs` in M4 WP-S3 so the new `tests/watcher_and_nav.rs` can reuse
//! the same project rig without duplicating the directory cleanup logic.

#![allow(
    dead_code,
    reason = "different test binaries use different subsets of this rig"
)]

use std::time::{Duration, Instant};

use camino::{Utf8Path, Utf8PathBuf};
use gd_server::config::InitializationOptions;
use lsp_server::{Connection, Message, Notification, Request, RequestId, Response};
use lsp_types::Uri;

/// A throwaway project directory, removed on drop.
///
/// Backed by [`tempfile::TempDir`] for guaranteed unique directory names — the prior
/// `nanos + pid` collision-prone scheme could race when `cargo test`'s default thread pool
/// ran two `TempProject::new()` calls in the same process within a nanosecond. The `TempDir`
/// handle owns the on-disk directory and removes it when dropped.
pub struct TempProject {
    /// Forward-slash UTF-8 absolute path to the temp project root.
    pub root: Utf8PathBuf,
    /// Owned cleanup handle: `tempfile::TempDir`'s `Drop` removes the directory tree.
    _dir: tempfile::TempDir,
}

impl TempProject {
    pub fn new() -> Self {
        let dir = tempfile::Builder::new()
            .prefix("gdls_test_")
            .tempdir()
            .expect("create temp dir");
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("temp dir is UTF-8");
        TempProject { root, _dir: dir }
    }

    pub fn write(&self, rel: &str, contents: &str) {
        let path = self.root.join(rel);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, contents).unwrap();
    }

    pub fn remove(&self, rel: &str) {
        let _ = std::fs::remove_file(self.root.join(rel));
    }
}

impl Default for TempProject {
    fn default() -> Self {
        Self::new()
    }
}

/// A minimal `extension_api.json`: `Object ← Node ← CanvasItem ← Node2D`, plus the handful of
/// Variant utility functions the fixtures call.
///
/// A dump loaded from a path stamps [`gd_types::ApiProvenance::Exact`], which since #256 is the
/// claim that a bare `name()` gdls cannot resolve genuinely does not exist. So a minimal dump has
/// to carry the utilities its fixtures use, or every `print(...)` in them reads as a typo. Real
/// `--dump-extension-api` output lists all 114; these are the ones the suite reaches for.
pub const MINI_API: &str = r#"{
    "header": {"version_major": 4, "version_minor": 6, "version_patch": 3},
    "utility_functions": [
        {"name": "print", "return_type": "void", "category": "general", "is_vararg": true, "hash": 1, "arguments": []},
        {"name": "prints", "return_type": "void", "category": "general", "is_vararg": true, "hash": 2, "arguments": []},
        {"name": "printerr", "return_type": "void", "category": "general", "is_vararg": true, "hash": 3, "arguments": []},
        {"name": "push_error", "return_type": "void", "category": "general", "is_vararg": true, "hash": 4, "arguments": []},
        {"name": "push_warning", "return_type": "void", "category": "general", "is_vararg": true, "hash": 5, "arguments": []},
        {"name": "str", "return_type": "String", "category": "general", "is_vararg": true, "hash": 6, "arguments": []},
        {"name": "typeof", "return_type": "int", "category": "general", "is_vararg": false, "hash": 7,
         "arguments": [{"name": "variable", "type": "Variant"}]},
        {"name": "is_instance_valid", "return_type": "bool", "category": "general", "is_vararg": false, "hash": 8,
         "arguments": [{"name": "instance", "type": "Variant"}]},
        {"name": "abs", "return_type": "Variant", "category": "math", "is_vararg": false, "hash": 9,
         "arguments": [{"name": "x", "type": "Variant"}]},
        {"name": "min", "return_type": "Variant", "category": "math", "is_vararg": true, "hash": 10, "arguments": []},
        {"name": "max", "return_type": "Variant", "category": "math", "is_vararg": true, "hash": 11, "arguments": []}
    ],
    "classes": [
        {"name": "Object"},
        {"name": "Node", "inherits": "Object"},
        {"name": "CanvasItem", "inherits": "Node"},
        {"name": "Node2D", "inherits": "CanvasItem"}
    ]
}"#;

/// Lay down the canonical M2 sample project: a `class_name` base, a script that extends it, and a
/// native dump rooted at `extension_api.json`.
pub fn sample_project() -> TempProject {
    let p = TempProject::new();
    p.write(
        "project.godot",
        "config_version=5\n\n[application]\n\nconfig/name=\"Test\"\n",
    );
    p.write("extension_api.json", MINI_API);
    p.write(
        "src/hero.gd",
        "class_name Hero\nextends Node2D\n\nvar hp: int = 10\n\nfunc attack() -> void:\n\tpass\n",
    );
    // Enemy extends the *script* class Hero (cross-file), which itself extends a native class.
    p.write("src/enemy.gd", "extends Hero\n\nfunc flee():\n\tpass\n");
    p
}

/// Standard `InitializationOptions` for a sample project: root + dump path.
pub fn options_for(p: &TempProject) -> InitializationOptions {
    InitializationOptions::parse(Some(&serde_json::json!({
        "projectRoot": p.root.as_str(),
        "autoDumpExtensionApi": false,
        "extensionApiPath": p.root.join("extension_api.json").as_str(),
    })))
}

/// Receive one message from the server, failing the test rather than hanging if none arrives.
/// 10s timeout matches the cold-index-tolerant cap from `indexing.rs`.
pub fn recv(conn: &Connection) -> Message {
    conn.receiver
        .recv_timeout(Duration::from_secs(10))
        .expect("timed out waiting for a message from the server")
}

/// Try to receive a message within a timeout; return `None` on timeout instead of panicking.
/// Used by watcher tests that drain published diagnostics across an indeterminate event count.
pub fn try_recv(conn: &Connection, timeout: Duration) -> Option<Message> {
    conn.receiver.recv_timeout(timeout).ok()
}

/// Receive messages until a [`Response`] arrives, skipping any server-initiated notifications
/// (e.g. a late `publishDiagnostics` push) or requests in between. A bare [`recv`] returns the
/// *next* message, which races on slower hosts (windows CI) where a diagnostic can slip past a
/// timeout-based drain and land where the awaited response was expected. Use this whenever a test
/// sends a request and needs its response.
pub fn recv_response(conn: &Connection) -> Response {
    loop {
        if let Message::Response(r) = recv(conn) {
            return r;
        }
    }
}

/// Build an LSP request `Message` with the given id, method, and serializable params.
pub fn request<P: serde::Serialize>(id: i32, method: &str, params: P) -> Message {
    Message::Request(Request {
        id: RequestId::from(id),
        method: method.to_string(),
        params: serde_json::to_value(params).unwrap(),
    })
}

/// Build an LSP notification `Message` with the given method and serializable params.
pub fn notification<P: serde::Serialize>(method: &str, params: P) -> Message {
    Message::Notification(Notification {
        method: method.to_string(),
        params: serde_json::to_value(params).unwrap(),
    })
}

/// Drive the LSP `shutdown`/`exit` handshake, then join the server thread (swallowing its result —
/// callers asserting on a clean exit do their own join instead of using this).
pub fn shutdown(client: &Connection, server_thread: std::thread::JoinHandle<anyhow::Result<()>>) {
    client
        .sender
        .send(request(99, "shutdown", serde_json::Value::Null))
        .unwrap();
    let _ = recv(client);
    client
        .sender
        .send(notification("exit", serde_json::Value::Null))
        .unwrap();
    server_thread.join().expect("server panicked").ok();
}

/// Poll `check` every `poll_interval` until it returns `Some(value)` or `deadline` passes,
/// returning the value on success or `None` on timeout.
///
/// Replaces the prior `std::thread::sleep(Duration::from_secs(N))` patterns in watcher tests:
/// a fixed sleep wastes budget on fast machines and flakes on slow CI, while a poll loop
/// returns the moment the event actually propagates. Exponential-ish backoff would be better
/// still, but a fixed 100 ms interval is plenty for filesystem-event polling and keeps the
/// helper readable.
pub fn poll_until<T>(
    deadline: Duration,
    poll_interval: Duration,
    mut check: impl FnMut() -> Option<T>,
) -> Option<T> {
    let start = Instant::now();
    loop {
        if let Some(v) = check() {
            return Some(v);
        }
        if start.elapsed() >= deadline {
            return None;
        }
        std::thread::sleep(poll_interval);
    }
}

/// Build a `file://` URI the server's `uri_to_path` round-trips. Handles Windows drive paths and
/// percent-encodes reserved chars (spaces, `#`, etc.) so test projects with spaces in their
/// path are exercisable — mirrors the production [`gd_server::uri::path_to_file_uri`] helper.
pub fn file_uri(path: &Utf8Path) -> Uri {
    gd_server::uri::path_to_file_uri(path).expect("valid file URI")
}
