//! WP-RD3: deterministic coverage of the watcher event-loop "dark branches" — the load-bearing
//! `select!` arms that, before this WP, were reachable only through the real `serve` loop fed by a
//! live OS event stream (and so were tested best-effort / never-fails in `watcher_and_nav.rs`).
//!
//! [`gd_server::serve_with_injected_watcher`] runs the full server lifecycle but takes the
//! watcher's event receiver as a parameter, so this test holds the `Sender` half and feeds it
//! exactly the post-debounce `DebounceEventResult`s each branch keys on:
//!   - **channel death** — drop the sender; the `recv(watcher_arm) -> Err(_)` arm disables the
//!     watcher and the LSP session keeps serving.
//!   - **fatal notify errors** — a `MaxFilesWatch` batch trips the `Ok(Err(_))` arm's fatal path;
//!     the arm disables, the session survives.
//!   - **`need_rescan` overflow** — a rescan-flagged event drives the full `reconcile()` path.
//!   - **`Ok(events)` reaction batch** — a `.gd` modify event flows through `handle_watcher` →
//!     `apply_reaction` and the session keeps serving.
//!
//! Every assertion is on *session survival* (a `publishDiagnostics` still flows after the branch
//! fires), which is deterministic — unlike the real-FS timing the `watcher_and_nav.rs` suite
//! tolerates.

mod common;

use std::time::{Duration, Instant};

use camino::Utf8Path;
use common::{file_uri, notification, sample_project, try_recv, TempProject};
use crossbeam_channel::{Receiver, Sender};
use lsp_server::{Connection, Message};
use lsp_types::{DidOpenTextDocumentParams, InitializeParams, InitializedParams, TextDocumentItem};
use notify_debouncer_full::{DebounceEventResult, DebouncedEvent};

/// Start the server with an injected watcher receiver. Returns the client connection, the watcher
/// event sender (the test feeds dark-branch events through it), and the server thread handle.
fn start(
    project: &TempProject,
) -> (
    Connection,
    Sender<DebounceEventResult>,
    std::thread::JoinHandle<anyhow::Result<()>>,
) {
    let (server, client) = Connection::memory();
    let (watcher_tx, watcher_rx): (Sender<DebounceEventResult>, Receiver<DebounceEventResult>) =
        crossbeam_channel::unbounded();
    let thread =
        std::thread::spawn(move || gd_server::serve_with_injected_watcher(server, watcher_rx));

    // initialize handshake.
    let init = InitializeParams {
        initialization_options: Some(serde_json::json!({
            "projectRoot": project.root.as_str(),
            "autoDumpExtensionApi": false,
            "extensionApiPath": project.root.join("extension_api.json").as_str(),
        })),
        ..Default::default()
    };
    client
        .sender
        .send(common::request(1, "initialize", init))
        .unwrap();
    let _ = common::recv(&client); // initialize response
    client
        .sender
        .send(notification("initialized", InitializedParams {}))
        .unwrap();
    (client, watcher_tx, thread)
}

/// Open `rel` via `didOpen`; the server publishes diagnostics for it. Returns true if at least one
/// `publishDiagnostics` arrived within the budget — the session-alive signal.
fn open_and_expect_publish(
    project: &TempProject,
    client: &Connection,
    rel: &str,
    version: i32,
) -> bool {
    let abs = project.root.join(rel);
    let text = std::fs::read_to_string(abs.as_std_path()).unwrap();
    client
        .sender
        .send(notification(
            "textDocument/didOpen",
            DidOpenTextDocumentParams {
                text_document: TextDocumentItem {
                    uri: file_uri(&abs),
                    language_id: "gdscript".to_string(),
                    version,
                    text,
                },
            },
        ))
        .unwrap();
    // Drain until a publishDiagnostics arrives (the open path always publishes) or we time out.
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if let Some(Message::Notification(n)) = try_recv(client, Duration::from_millis(200)) {
            if n.method == "textDocument/publishDiagnostics" {
                return true;
            }
        }
    }
    false
}

/// Cleanly shut the server down and join, surfacing a server-thread panic as a test failure.
fn shutdown(client: &Connection, thread: std::thread::JoinHandle<anyhow::Result<()>>) {
    client
        .sender
        .send(common::request(99, "shutdown", serde_json::Value::Null))
        .unwrap();
    // Drain until the shutdown response arrives.
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if let Some(Message::Response(_)) = try_recv(client, Duration::from_millis(200)) {
            break;
        }
    }
    client
        .sender
        .send(notification("exit", serde_json::Value::Null))
        .unwrap();
    thread.join().expect("server thread panicked").ok();
}

/// A debounced event carrying notify's `Rescan` flag — the live-stream analog of a kernel
/// event-queue overflow. `need_rescan()` is true for it.
fn rescan_event() -> DebouncedEvent {
    let event = notify::Event::new(notify::EventKind::Any).set_flag(notify::event::Flag::Rescan);
    DebouncedEvent::new(event, Instant::now())
}

/// A debounced `Modify` event on `path` — the shape `classify_event` maps to `Reaction::GdSource`
/// for a `.gd` file under the project root.
fn gd_modify_event(path: &Utf8Path) -> DebouncedEvent {
    let event = notify::Event::new(notify::EventKind::Modify(notify::event::ModifyKind::Data(
        notify::event::DataChange::Content,
    )))
    .add_path(path.as_std_path().to_path_buf());
    DebouncedEvent::new(event, Instant::now())
}

#[test]
fn channel_death_disables_watcher_but_session_survives() {
    let project = sample_project();
    let (client, watcher_tx, thread) = start(&project);
    assert!(
        open_and_expect_publish(&project, &client, "src/hero.gd", 1),
        "precondition: the server publishes for the first open"
    );

    // Drop the watcher sender → the loop's `recv(watcher_arm) -> Err(_)` arm fires and disables
    // the watcher. The LSP session must keep serving.
    drop(watcher_tx);

    assert!(
        open_and_expect_publish(&project, &client, "src/enemy.gd", 2),
        "after the watcher channel died, the LSP session must still publish diagnostics"
    );
    shutdown(&client, thread);
}

#[test]
fn fatal_max_files_watch_disables_watcher_but_session_survives() {
    let project = sample_project();
    let (client, watcher_tx, thread) = start(&project);
    assert!(open_and_expect_publish(&project, &client, "src/hero.gd", 1));

    // A `MaxFilesWatch` batch trips the `Ok(Err(errors))` arm's fatal path → the watcher arm is
    // disabled. The session keeps serving open buffers.
    watcher_tx
        .send(Err(vec![notify::Error::new(
            notify::ErrorKind::MaxFilesWatch,
        )]))
        .unwrap();

    assert!(
        open_and_expect_publish(&project, &client, "src/enemy.gd", 2),
        "after a fatal MaxFilesWatch error disabled the watcher, the session must still serve"
    );
    shutdown(&client, thread);
}

#[test]
fn non_fatal_notify_error_keeps_watcher_armed() {
    let project = sample_project();
    let (client, watcher_tx, thread) = start(&project);
    assert!(open_and_expect_publish(&project, &client, "src/hero.gd", 1));

    // A generic (non-fatal) notify error is logged and swallowed; the watcher stays armed and a
    // subsequent real event still flows. Send a non-fatal error then a valid rescan batch.
    watcher_tx
        .send(Err(vec![notify::Error::new(notify::ErrorKind::Generic(
            "transient glitch".to_string(),
        ))]))
        .unwrap();
    watcher_tx.send(Ok(vec![rescan_event()])).unwrap();

    assert!(
        open_and_expect_publish(&project, &client, "src/enemy.gd", 2),
        "a non-fatal notify error must not disable the watcher or kill the session"
    );
    shutdown(&client, thread);
}

/// The Windows-with-`NoCache` rename shape (issue #14): without the debouncer's FileIdMap there
/// is no rename pairing, so `From`/`To` arrive as separate unpaired halves. The vanished-path
/// arm removes the old file, the `To` arm reindexes the new one, and the session keeps serving —
/// the contract that made dropping the (tree-walking, handle-per-file) cache safe.
#[test]
fn unpaired_rename_halves_keep_session_serving() {
    let project = sample_project();
    let (client, watcher_tx, thread) = start(&project);
    assert!(open_and_expect_publish(&project, &client, "src/hero.gd", 1));

    let old_abs = project.root.join("src/enemy.gd");
    let new_abs = project.root.join("src/enemy_renamed.gd");
    std::fs::rename(old_abs.as_std_path(), new_abs.as_std_path()).unwrap();

    let from = notify::Event::new(notify::EventKind::Modify(notify::event::ModifyKind::Name(
        notify::event::RenameMode::From,
    )))
    .add_path(old_abs.as_std_path().to_path_buf());
    let to = notify::Event::new(notify::EventKind::Modify(notify::event::ModifyKind::Name(
        notify::event::RenameMode::To,
    )))
    .add_path(new_abs.as_std_path().to_path_buf());
    watcher_tx
        .send(Ok(vec![
            DebouncedEvent::new(from, Instant::now()),
            DebouncedEvent::new(to, Instant::now()),
        ]))
        .unwrap();

    assert!(
        open_and_expect_publish(&project, &client, "src/enemy_renamed.gd", 2),
        "after unpaired rename halves, the renamed file must open and publish"
    );
    shutdown(&client, thread);
}

#[test]
fn need_rescan_event_drives_reconcile_and_session_survives() {
    let project = sample_project();
    let (client, watcher_tx, thread) = start(&project);
    assert!(open_and_expect_publish(&project, &client, "src/hero.gd", 1));

    // A rescan-flagged event drives the full `reconcile()` overflow path inside `handle_watcher`.
    watcher_tx.send(Ok(vec![rescan_event()])).unwrap();

    assert!(
        open_and_expect_publish(&project, &client, "src/enemy.gd", 2),
        "the need_rescan reconcile path must run and leave the session serving"
    );
    shutdown(&client, thread);
}

#[test]
fn project_and_native_reactions_coalesce_and_session_survives() {
    let project = sample_project();
    let (client, watcher_tx, thread) = start(&project);
    assert!(open_and_expect_publish(&project, &client, "src/hero.gd", 1));

    // A batch touching BOTH project.godot AND extension_api.json at the project root: WP-RD11 (3)
    // scans the batch and coalesces the native-DB reload + republish into ONE post-batch pass
    // (rather than reloading per reaction). The session must still serve afterward.
    let pg = project.root.join("project.godot");
    let api = project.root.join("extension_api.json");
    watcher_tx
        .send(Ok(vec![gd_modify_event(&pg), gd_modify_event(&api)]))
        .unwrap();

    assert!(
        open_and_expect_publish(&project, &client, "src/enemy.gd", 2),
        "the coalesced project/native reload (WP-RD11 (3)) must leave the session serving"
    );
    shutdown(&client, thread);
}

#[test]
fn gd_source_modify_reaction_batch_keeps_session_serving() {
    let project = sample_project();
    let (client, watcher_tx, thread) = start(&project);
    assert!(open_and_expect_publish(&project, &client, "src/hero.gd", 1));

    // Modify hero.gd on disk and feed a debounced `Modify` event for it: `handle_watcher` →
    // `classify_event` → `Reaction::GdSource` → `Workspace::reindex`. A multi-event batch
    // (modify + a stray Other) exercises the per-batch loop too.
    let hero = project.root.join("src/hero.gd");
    std::fs::write(
        hero.as_std_path(),
        "class_name Hero\nextends Node2D\n\nvar hp: int = 20\n\nfunc attack() -> void:\n\tpass\n",
    )
    .unwrap();
    watcher_tx
        .send(Ok(vec![gd_modify_event(&hero), rescan_event()]))
        .unwrap();

    assert!(
        open_and_expect_publish(&project, &client, "src/enemy.gd", 2),
        "a GdSource reaction batch must flow through apply_reaction without killing the session"
    );
    shutdown(&client, thread);
}
