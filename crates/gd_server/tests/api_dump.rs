//! Integration tests for the `extension_api.json` auto-dump (v1.0.1, issue #20), driven through
//! the full server lifecycle with a FAKE godot binary (a shell script that mimics the real
//! behavior: write `extension_api.json` into the `--path` project root, regardless of cwd).
//!
//! Unix-only by decision: a `.bat` fake under `std::process::Command` runs into Windows quoting /
//! BatBadBut hardening that has nothing to do with the code under test; Windows CI keeps the
//! pure-logic coverage (discovery order, staleness, resolution ladder) from `api_dump`'s unit
//! tests, and the spawn path itself is platform-uniform `std::process`.
#![cfg(unix)]

mod common;

use common::{file_uri, notification, recv, request, TempProject};
use lsp_server::{Connection, Message};
use lsp_types::{DidOpenTextDocumentParams, InitializeParams, InitializedParams, TextDocumentItem};

const CANNED_API: &str = r#"{
    "header": {"version_major": 4, "version_minor": 6, "version_patch": 3,
               "version_full_name": "Fake Godot v4.6.3.test"},
    "classes": [
        {"name": "Object"},
        {"name": "Node", "inherits": "Object"}
    ]
}"#;

/// Install the fake godot into the project dir: appends to `invocations.txt`, then writes the
/// canned dump into the `--path` argument's directory (real Godot's observed behavior).
fn install_fake_godot(p: &TempProject, exit_code: i32, write_dump: bool) -> String {
    use std::os::unix::fs::PermissionsExt;
    p.write("canned.json", CANNED_API);
    let write_part = if write_dump {
        r#"cat "$here/canned.json" > "$target/extension_api.json""#
    } else {
        "true"
    };
    let script = format!(
        "#!/bin/sh\nhere=$(dirname \"$0\")\nprev=\"\"\ntarget=\"\"\nfor a in \"$@\"; do\n  if [ \"$prev\" = \"--path\" ]; then target=\"$a\"; fi\n  prev=\"$a\"\ndone\necho run >> \"$here/invocations.txt\"\n{write_part}\nexit {exit_code}\n"
    );
    p.write("fake_godot.sh", &script);
    let path = p.root.join("fake_godot.sh");
    std::fs::set_permissions(path.as_std_path(), std::fs::Permissions::from_mode(0o755)).unwrap();
    path.as_str().to_owned()
}

fn boot_with_binary(
    p: &TempProject,
    binary: &str,
) -> (Connection, std::thread::JoinHandle<anyhow::Result<()>>) {
    let (server, client) = Connection::memory();
    let handle = std::thread::spawn(move || gd_server::serve(server));
    // autoDumpExtensionApi deliberately UNSET — this also pins the on-by-default contract.
    let init = InitializeParams {
        initialization_options: Some(serde_json::json!({
            "projectRoot": p.root.as_str(),
            "godotBinaryPath": binary,
        })),
        ..Default::default()
    };
    client.sender.send(request(1, "initialize", init)).unwrap();
    let _ = recv(&client);
    client
        .sender
        .send(notification("initialized", InitializedParams {}))
        .unwrap();
    (client, handle)
}

/// Open a trivial file and wait for its publish — proves the event loop armed (the dump runs
/// during workspace load, strictly before this).
fn open_and_drain(client: &Connection, p: &TempProject) {
    let abs = p.root.join("main.gd");
    let text = std::fs::read_to_string(abs.as_std_path()).unwrap();
    client
        .sender
        .send(notification(
            "textDocument/didOpen",
            DidOpenTextDocumentParams {
                text_document: TextDocumentItem {
                    uri: file_uri(&abs),
                    language_id: "gdscript".to_string(),
                    version: 1,
                    text,
                },
            },
        ))
        .unwrap();
    loop {
        if let Message::Notification(n) = recv(client) {
            if n.method == "textDocument/publishDiagnostics" {
                return;
            }
        }
    }
}

fn project() -> TempProject {
    let p = TempProject::new();
    p.write(
        "project.godot",
        "config_version=5\n\n[application]\n\nconfig/name=\"Test\"\n",
    );
    p.write("main.gd", "extends Node\nfunc _ready() -> void:\n\tpass\n");
    p
}

fn invocation_count(p: &TempProject) -> usize {
    std::fs::read_to_string(p.root.join("invocations.txt").as_std_path())
        .map(|t| t.lines().count())
        .unwrap_or(0)
}

#[test]
fn auto_dump_runs_once_and_is_fresh_on_second_boot() {
    let p = project();
    let binary = install_fake_godot(&p, 0, true);

    let (client, handle) = boot_with_binary(&p, &binary);
    open_and_drain(&client, &p);
    common::shutdown(&client, handle);

    let managed = p.root.join(".gdls/extension_api.json");
    let meta = p.root.join(".gdls/extension_api.meta.json");
    assert!(managed.as_std_path().exists(), "managed dump must exist");
    assert!(meta.as_std_path().exists(), "staleness meta must exist");
    assert!(
        !p.root.join("extension_api.json").as_std_path().exists(),
        "the root-level dump must have been moved into .gdls/"
    );
    assert_eq!(invocation_count(&p), 1, "exactly one dump on first boot");

    // Second boot: the meta is fresh (same binary, same gdextension set) — no re-dump.
    let (client, handle) = boot_with_binary(&p, &binary);
    open_and_drain(&client, &p);
    common::shutdown(&client, handle);
    assert_eq!(
        invocation_count(&p),
        1,
        "a fresh cached dump must not re-spawn godot"
    );
}

/// Godot 4.6.3 has been observed to abort on exit AFTER writing a complete dump — the artifact
/// decides, not the exit status.
#[test]
fn nonzero_exit_with_complete_dump_is_adopted() {
    let p = project();
    let binary = install_fake_godot(&p, 134, true); // SIGABRT-ish exit, dump written

    let (client, handle) = boot_with_binary(&p, &binary);
    open_and_drain(&client, &p);
    common::shutdown(&client, handle);

    assert!(
        p.root
            .join(".gdls/extension_api.json")
            .as_std_path()
            .exists(),
        "a complete dump must be adopted despite the exit status"
    );
    assert_eq!(invocation_count(&p), 1);
}

#[test]
fn dump_failure_degrades_and_server_keeps_serving() {
    let p = project();
    let binary = install_fake_godot(&p, 1, false); // fails, writes nothing

    let (client, handle) = boot_with_binary(&p, &binary);
    // The server must still arm the loop and serve diagnostics with a dynamic native DB.
    open_and_drain(&client, &p);
    common::shutdown(&client, handle);

    assert!(
        !p.root
            .join(".gdls/extension_api.json")
            .as_std_path()
            .exists(),
        "no dump must be adopted from a failed run"
    );
    assert_eq!(invocation_count(&p), 1, "the failed attempt did spawn once");
}

#[test]
fn pre_existing_root_file_is_never_clobbered_and_wins() {
    let p = project();
    let binary = install_fake_godot(&p, 0, true);
    // A user-managed dump already sits at the root.
    p.write("extension_api.json", CANNED_API);

    let (client, handle) = boot_with_binary(&p, &binary);
    open_and_drain(&client, &p);
    common::shutdown(&client, handle);

    assert_eq!(
        invocation_count(&p),
        0,
        "a pre-existing root extension_api.json must suppress the dump entirely"
    );
    assert!(
        p.root.join("extension_api.json").as_std_path().exists(),
        "the user file must survive untouched"
    );
}
