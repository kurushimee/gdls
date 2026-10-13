//! #595 — a diagnostic whose range ends on a line break stays on the line that break closes.
//!
//! Godot's tokenizer gives a NEWLINE token a column one past the line's last character, so the
//! editor underlines to the end of that line and no further. gdls carries the same token as a byte
//! span whose end is the first byte of the *next* line, which used to publish as `(line + 1, 0)`
//! and highlight a whole extra line of indentation.
//!
//! #604 — the root class's range ends on the file's last line, not one past it as in the engine.

use crate::common;

use std::time::Duration;

use common::{file_uri, notification, recv, request, sample_project, shutdown, try_recv};
use lsp_server::{Connection, Message};
use lsp_types::{
    DidOpenTextDocumentParams, InitializeParams, InitializedParams, Position,
    PublishDiagnosticsParams, TextDocumentItem, Uri,
};

fn boot_project(project: &common::TempProject, client: &Connection) {
    let init = InitializeParams {
        initialization_options: Some(serde_json::json!({
            "projectRoot": project.root.as_str(),
            "autoDumpExtensionApi": false,
            "extensionApiPath": project.root.join("extension_api.json").as_str(),
        })),
        ..Default::default()
    };
    client.sender.send(request(1, "initialize", init)).unwrap();
    let _ = recv(client);
    client
        .sender
        .send(notification("initialized", InitializedParams {}))
        .unwrap();
}

fn open_and_diags(client: &Connection, uri: &Uri, text: &str) -> PublishDiagnosticsParams {
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
    loop {
        let Some(Message::Notification(note)) = try_recv(client, Duration::from_secs(5)) else {
            panic!("expected a publishDiagnostics notification for {uri:?}");
        };
        if note.method != "textDocument/publishDiagnostics" {
            continue;
        }
        let params: PublishDiagnosticsParams = serde_json::from_value(note.params).unwrap();
        if &params.uri == uri {
            return params;
        }
    }
}

/// The end of the diagnostic carrying `message` that opening `file` (written into `project`) reports.
fn diagnostic_end(project: common::TempProject, file: &str, src: &str, message: &str) -> Position {
    project.write(file, src);

    let (server, client) = Connection::memory();
    let server_thread = std::thread::spawn(move || gd_server::serve(server));
    boot_project(&project, &client);

    let uri = file_uri(&project.root.join(file));
    let diags = open_and_diags(&client, &uri, src);
    let end = diags
        .diagnostics
        .iter()
        .find(|d| d.message.contains(message))
        .unwrap_or_else(|| panic!("expected {message:?}; got {diags:?}"))
        .range
        .end;

    shutdown(&client, server_thread);
    end
}

fn missing_return_end(file: &str, src: &str) -> Position {
    diagnostic_end(
        sample_project(),
        file,
        src,
        "Not all code paths return a value.",
    )
}

#[test]
fn an_analyzer_diagnostic_ending_on_a_newline_stays_on_its_line() {
    let src = "extends Node\nfunc f() -> int:\n\tpass\nfunc g() -> void:\n\tprint(f())\n";
    // The body is `\tpass` on LSP line 2: five characters, and Godot ends the range at 6.
    assert_eq!(
        missing_return_end("src/newline_end.gd", src),
        Position {
            line: 2,
            character: 6
        }
    );
}

#[test]
fn a_body_ending_on_the_files_last_newline_stays_on_its_line() {
    let src = "extends Node\nfunc f() -> int:\n\tpass\n";
    assert_eq!(
        missing_return_end("src/newline_eof.gd", src),
        Position {
            line: 2,
            character: 6
        }
    );
}

#[test]
fn a_root_class_range_ends_on_the_files_last_line() {
    let project = sample_project();
    project.write("src/tool_base.gd", "@tool\nextends Node\n");
    let src = "extends \"res://src/tool_base.gd\"\n\nfunc test():\n\tpass\n";
    // #604: the engine ends it at (4, 0), a line past the file; docs/04 §5 explains why gdls does not.
    assert_eq!(
        diagnostic_end(
            project,
            "src/non_tool.gd",
            src,
            "The base class script has the \"@tool\" annotation, but this script does not have it.",
        ),
        Position {
            line: 3,
            character: 6
        }
    );
}

#[test]
fn a_parser_diagnostic_ending_on_a_newline_stays_on_its_line() {
    let project = sample_project();
    let src = "extends Node\n\tstray\nfunc test() -> void:\n\tpass\n";
    project.write("src/parse_newline_end.gd", src);

    let (server, client) = Connection::memory();
    let server_thread = std::thread::spawn(move || gd_server::serve(server));
    boot_project(&project, &client);

    let uri = file_uri(&project.root.join("src/parse_newline_end.gd"));
    let diags = open_and_diags(&client, &uri, src);

    assert!(
        !diags.diagnostics.is_empty(),
        "the fixture must not parse cleanly"
    );
    for d in &diags.diagnostics {
        assert!(
            d.range.end.character > 0 || d.range.end == d.range.start,
            "a range must not end at column 0 of the line after the one it covers: {d:?}"
        );
    }

    shutdown(&client, server_thread);
}
