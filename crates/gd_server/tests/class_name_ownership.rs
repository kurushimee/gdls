//! Which file owns a `class_name` that more than one file declares.
//!
//! A 3-souls session reported `argument 1 should be "Node3D" but is "CharacterEntity"` for a class
//! that does extend Node3D. The type check was right; the index pointed `CharacterEntity` at the
//! wrong file. Two ways got it there, both pinned below:
//!
//! - a scratch copy in a `.gdignore`'d folder declared the same name with another base, and the
//!   registry let whichever file was indexed last own it;
//! - a new file whose `class_name` was being typed passed through the existing name for one
//!   keystroke, took it over, and on the next keystroke dropped it, with no fallback to the real
//!   owner. The warm cache then carried the lost name into every later session.
//!
//! Each test checks a consumer that stores a `Hero` (which extends `Node2D`) in a `Node2D`
//! variable: clean when `Hero` resolves to the real file, an error when it resolves to an impostor,
//! and `Could not find type` when the name is lost.

use crate::common;

use common::{file_uri, notification, recv, request, sample_project, shutdown, TempProject};
use lsp_server::{Connection, Message};
use lsp_types::{
    DidChangeTextDocumentParams, DidOpenTextDocumentParams, InitializeParams, InitializedParams,
    PublishDiagnosticsParams, TextDocumentContentChangeEvent, TextDocumentItem, Uri,
    VersionedTextDocumentIdentifier,
};

const CONSUMER: &str =
    "extends Node\n\nfunc take(h: Hero) -> void:\n\tvar n: Node2D = h\n\tprint(n)\n";

/// A copy of `Hero` whose base is not a `Node2D`, as a stale or scratch copy might be.
const IMPOSTOR: &str = "class_name Hero\nextends Resource\n";

fn boot(project: &TempProject) -> (Connection, std::thread::JoinHandle<anyhow::Result<()>>) {
    let (server, client) = Connection::memory();
    let server_thread = std::thread::spawn(move || gd_server::serve(server));
    let init = InitializeParams {
        initialization_options: Some(serde_json::json!({
            "projectRoot": project.root.as_str(),
            "autoDumpExtensionApi": false,
            "extensionApiPath": project.root.join("extension_api.json").as_str(),
        })),
        ..Default::default()
    };
    client.sender.send(request(1, "initialize", init)).unwrap();
    while !matches!(recv(&client), Message::Response(_)) {}
    client
        .sender
        .send(notification("initialized", InitializedParams {}))
        .unwrap();
    (client, server_thread)
}

fn open(client: &Connection, uri: &Uri, text: &str) {
    client
        .sender
        .send(notification(
            "textDocument/didOpen",
            DidOpenTextDocumentParams {
                text_document: TextDocumentItem {
                    uri: uri.clone(),
                    language_id: "gdscript".to_string(),
                    version: 1,
                    text: text.to_string(),
                },
            },
        ))
        .unwrap();
}

fn change(client: &Connection, uri: &Uri, version: i32, text: &str) {
    client
        .sender
        .send(notification(
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
        ))
        .unwrap();
}

/// The next diagnostics the server publishes for `uri`.
fn next_diagnostics(client: &Connection, uri: &Uri) -> Vec<String> {
    diagnostics_matching(client, uri, |_| true)
}

/// The first diagnostics published for `uri` whose document version satisfies `wanted`. Earlier
/// publishes (a republish while another file was mid-edit) are skipped.
fn diagnostics_matching(
    client: &Connection,
    uri: &Uri,
    wanted: impl Fn(Option<i32>) -> bool,
) -> Vec<String> {
    loop {
        let Message::Notification(note) = recv(client) else {
            continue;
        };
        if note.method != "textDocument/publishDiagnostics" {
            continue;
        }
        let params: PublishDiagnosticsParams = serde_json::from_value(note.params).unwrap();
        if &params.uri == uri && wanted(params.version) {
            return params.diagnostics.into_iter().map(|d| d.message).collect();
        }
    }
}

/// The consumer's diagnostics, freshly analyzed: an edit forces a re-analysis whatever the
/// server cached, and only the publish for that edit's version counts.
fn consumer_diagnostics(client: &Connection, uri: &Uri, version: i32) -> Vec<String> {
    change(client, uri, version, &format!("{CONSUMER}\n"));
    diagnostics_matching(client, uri, |v| v == Some(version))
}

fn errors(diagnostics: &[String]) -> Vec<&String> {
    diagnostics
        .iter()
        .filter(|m| !m.contains("is declared but never used"))
        .collect()
}

#[test]
fn a_gdignored_copy_never_owns_the_class() {
    let p = sample_project();
    p.write("outputs/.gdignore", "");
    p.write("outputs/hero_copy.gd", IMPOSTOR);
    p.write("src/consumer.gd", CONSUMER);
    let (client, server_thread) = boot(&p);

    let consumer = file_uri(&p.root.join("src/consumer.gd"));
    open(&client, &consumer, CONSUMER);
    assert_eq!(
        errors(&next_diagnostics(&client, &consumer)),
        Vec::<&String>::new()
    );

    // Opening the copy indexes it, but Godot never registers a class from an ignored folder.
    let copy = file_uri(&p.root.join("outputs/hero_copy.gd"));
    open(&client, &copy, IMPOSTOR);
    let _ = next_diagnostics(&client, &copy);
    assert_eq!(
        errors(&consumer_diagnostics(&client, &consumer, 2)),
        Vec::<&String>::new()
    );

    shutdown(&client, server_thread);
}

#[test]
fn a_scanned_duplicate_resolves_as_godot_registers_it() {
    // Godot registers subdirectory scripts before a directory's own, and the last registration
    // wins, so `src/hero.gd` owns `Hero` over `src/old/hero_copy.gd` whatever the index order.
    let p = sample_project();
    p.write("src/old/hero_copy.gd", IMPOSTOR);
    p.write("src/consumer.gd", CONSUMER);
    let (client, server_thread) = boot(&p);

    let consumer = file_uri(&p.root.join("src/consumer.gd"));
    open(&client, &consumer, CONSUMER);
    assert_eq!(
        errors(&next_diagnostics(&client, &consumer)),
        Vec::<&String>::new()
    );

    // Editing the loser re-indexes it last; it must still lose.
    let copy = file_uri(&p.root.join("src/old/hero_copy.gd"));
    open(&client, &copy, IMPOSTOR);
    let _ = next_diagnostics(&client, &copy);
    change(&client, &copy, 2, &format!("{IMPOSTOR}\n"));
    let _ = next_diagnostics(&client, &copy);
    assert_eq!(
        errors(&consumer_diagnostics(&client, &consumer, 2)),
        Vec::<&String>::new()
    );

    shutdown(&client, server_thread);
}

#[test]
fn typing_a_class_name_through_an_existing_one_never_loses_it() {
    let p = sample_project();
    p.write("src/consumer.gd", CONSUMER);
    p.write("src/zz_new.gd", "extends Node\n");

    {
        let (client, server_thread) = boot(&p);
        let consumer = file_uri(&p.root.join("src/consumer.gd"));
        open(&client, &consumer, CONSUMER);
        assert_eq!(
            errors(&next_diagnostics(&client, &consumer)),
            Vec::<&String>::new()
        );

        let new_file = file_uri(&p.root.join("src/zz_new.gd"));
        open(&client, &new_file, "extends Node\n");
        let _ = next_diagnostics(&client, &new_file);
        for (version, typed) in [(2, "Her"), (3, "Hero"), (4, "HeroA")] {
            change(
                &client,
                &new_file,
                version,
                &format!("class_name {typed}\nextends Resource\n"),
            );
        }
        assert_eq!(
            errors(&consumer_diagnostics(&client, &consumer, 2)),
            Vec::<&String>::new(),
            "Hero must still resolve to src/hero.gd once the typing moved past it"
        );
        shutdown(&client, server_thread);
    }

    // The next session warm-starts from the cache the first one saved.
    let (client, server_thread) = boot(&p);
    let consumer = file_uri(&p.root.join("src/consumer.gd"));
    open(&client, &consumer, CONSUMER);
    assert_eq!(
        errors(&next_diagnostics(&client, &consumer)),
        Vec::<&String>::new(),
        "a restart must not bring back a lost or hijacked class name"
    );
    shutdown(&client, server_thread);
}
