//! v1.0.2 (issue #25) — end-to-end background auto-dump: the session starts serving on the
//! embedded stock fallback (no false positives), the dump runs off the critical path against a
//! fake Godot binary, and its adoption mid-session reloads the native DB and republishes open
//! buffers — the first run converges to the same diagnostics a warm session computes.
//!
//! Shell-script fake binary ⇒ unix-only (the run_dump mechanics have unit coverage in
//! `api_dump::tests::fake_binary`; Windows CI exercises the embedded/discovery layers).
#![cfg(unix)]

mod common;

use std::time::{Duration, Instant};

use common::{file_uri, notification, recv, request, shutdown, TempProject};
use lsp_server::{Connection, Message};
use lsp_types::{DidOpenTextDocumentParams, InitializeParams, InitializedParams, TextDocumentItem};

/// The fake engine's dump: knows `FakeCustomClass` (a "custom build" class the embedded stock
/// surface has never heard of) but NOT `Timer` — so adoption observably flips the diagnostics
/// from "Timer ok / FakeCustomClass silent" to "Timer unknown / FakeCustomClass ok".
const MINI_DUMP: &str = r#"{"header":{"version_major":4,"version_minor":6,"version_patch":3,"version_full_name":"Godot Engine v4.6.3.fake"},"classes":[{"name":"Object"},{"name":"Node","inherits":"Object"},{"name":"FakeCustomClass","inherits":"Object"}]}"#;

fn diagnostics_for(msg: &Message, uri_str: &str) -> Option<Vec<String>> {
    let Message::Notification(n) = msg else {
        return None;
    };
    if n.method != "textDocument/publishDiagnostics" {
        return None;
    }
    let params: lsp_types::PublishDiagnosticsParams =
        serde_json::from_value(n.params.clone()).ok()?;
    if params.uri.as_str() != uri_str {
        return None;
    }
    Some(params.diagnostics.into_iter().map(|d| d.message).collect())
}

#[test]
fn background_dump_adoption_republishes_open_buffers() {
    use std::os::unix::fs::PermissionsExt;

    let project = TempProject::new();
    project.write(
        "project.godot",
        "config_version=5\n\n[application]\nconfig/features=PackedStringArray(\"4.6\")\n",
    );
    project.write(
        "a.gd",
        "extends Node\n\nvar t: Timer = null\nvar c: FakeCustomClass = null\n",
    );
    // The fake binary blocks on a sentinel file the test writes only after publish #1 lands, so
    // the initial publish reflects the embedded fallback on any scheduler — a sync point, not a
    // timed sleep. The poll cap (~30 s) keeps an orphaned fake from outliving the test; the dump
    // thread's own deadline kill covers it regardless.
    let bin = project.root.join("fake-godot.sh");
    std::fs::write(
        bin.as_std_path(),
        format!(
            "#!/bin/sh\ni=0\nwhile [ ! -f go.flag ] && [ $i -lt 300 ]; do sleep 0.1; i=$((i+1)); done\n\
             cat > extension_api.json <<'EOF'\n{MINI_DUMP}\nEOF\nexit 0\n"
        ),
    )
    .unwrap();
    std::fs::set_permissions(bin.as_std_path(), std::fs::Permissions::from_mode(0o755)).unwrap();

    let (server, client) = Connection::memory();
    let server_thread = std::thread::spawn(move || gd_server::serve(server));

    let init = InitializeParams {
        initialization_options: Some(serde_json::json!({
            "projectRoot": project.root.as_str(),
            "godotBinaryPath": bin.as_str(),
        })),
        ..Default::default()
    };
    client.sender.send(request(1, "initialize", init)).unwrap();
    let _init_resp = recv(&client);
    client
        .sender
        .send(notification("initialized", InitializedParams {}))
        .unwrap();

    let uri = file_uri(&project.root.join("a.gd"));
    client
        .sender
        .send(notification(
            "textDocument/didOpen",
            DidOpenTextDocumentParams {
                text_document: TextDocumentItem {
                    uri: uri.clone(),
                    language_id: "gdscript".into(),
                    version: 1,
                    text: std::fs::read_to_string(project.root.join("a.gd").as_std_path()).unwrap(),
                },
            },
        ))
        .unwrap();

    // Publish #1 — embedded stock fallback (Generic): Timer resolves, FakeCustomClass degrades
    // silently. ZERO diagnostics; the v1.0.1 behavior was an error on every native name.
    let first = loop {
        let msg = recv(&client);
        if let Some(diags) = diagnostics_for(&msg, uri.as_str()) {
            break diags;
        }
    };
    assert!(
        first.is_empty(),
        "embedded-fallback session must not false-positive, got: {first:?}"
    );

    // Publish #1 observed — release the fake binary to produce its dump.
    project.write("go.flag", "");

    // The adoption republish — wait for the publish whose content reflects the Exact fake dump:
    // `Timer` is now a trustworthy unknown (error), `FakeCustomClass` resolves. Intermediate
    // publishes (watcher echoes) are allowed; the converged one must arrive.
    let deadline = Instant::now() + Duration::from_secs(30);
    let converged = loop {
        assert!(
            Instant::now() < deadline,
            "adoption republish never arrived"
        );
        let Some(msg) = common::try_recv(&client, Duration::from_secs(5)) else {
            continue;
        };
        if let Some(diags) = diagnostics_for(&msg, uri.as_str()) {
            if diags.iter().any(|m| m.contains(r#"type "Timer""#)) {
                break diags;
            }
        }
    };
    assert!(
        converged
            .iter()
            .any(|m| m == r#"Could not find type "Timer" in the current scope."#),
        "Exact provenance must restore trustworthy unknown-type errors, got: {converged:?}"
    );
    assert!(
        !converged.iter().any(|m| m.contains("FakeCustomClass")),
        "the custom class must resolve through the adopted dump, got: {converged:?}"
    );

    // The dump was adopted into the managed location and the root artifact cleaned up.
    assert!(project
        .root
        .join(".gdls/extension_api.json")
        .as_std_path()
        .exists());
    assert!(!project
        .root
        .join("extension_api.json")
        .as_std_path()
        .exists());

    shutdown(&client, server_thread);
}
