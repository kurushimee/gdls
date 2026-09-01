//! #572 — the path a class's fully-qualified name carries.
//!
//! Godot threads `parser->script_path` into the head class's `fqcn` (`gdscript_parser.cpp:993`),
//! and that fqcn is what names a base class in a shadowing warning. An editor session loads a
//! script by its `res://` path, so a Godot user reads
//! `… in the base class "res://src/w/g1.gd::A".` The server used to hand the analyzer only the
//! basename, which reached the user as `"g1.gd::A"`.
//!
//! Enum rendering strips the path back down through the `String::get_file()` mirror in
//! `Display for DataType`, so the full path costs it nothing — the rows below pin both halves.

mod common;

use std::time::Duration;

use common::{file_uri, notification, recv, request, sample_project, shutdown, try_recv};
use lsp_server::{Connection, Message};
use lsp_types::{
    DidOpenTextDocumentParams, InitializeParams, InitializedParams, PublishDiagnosticsParams,
    TextDocumentItem, Uri,
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

const SRC: &str = "extends Node\n\nclass A:\n\tvar foo := 1\n\nclass B extends A:\n\tfunc f() -> void:\n\t\tvar foo := 2\n\t\tprint(foo)\n";

/// The base is an inner class of an unnamed head class, so its fqcn is built from the script path.
/// Verbatim against Godot 4.7.2's editor language server.
#[test]
fn an_inner_base_class_is_named_by_its_res_path() {
    let project = sample_project();
    project.write("src/w/g1.gd", SRC);

    let (server, client) = Connection::memory();
    let server_thread = std::thread::spawn(move || gd_server::serve(server));
    boot_project(&project, &client);

    let uri = file_uri(&project.root.join("src/w/g1.gd"));
    let diags = open_and_diags(&client, &uri, SRC);

    let messages: Vec<&str> = diags
        .diagnostics
        .iter()
        .map(|d| d.message.as_str())
        .collect();
    assert!(
        messages.contains(
            &r#"The local variable "foo" is shadowing an already-declared variable at line 4 in the base class "res://src/w/g1.gd::A"."#
        ),
        "got {messages:?}"
    );

    shutdown(&client, server_thread);
}

/// The other half of the same value. An enum's rendering strips the path at the last `/`, so the
/// head class's enum stays `<file>.<Enum>` and an inner class's stays `<file>::<Inner>.<Enum>` —
/// the forms Godot's own corpus goldens pin.
#[test]
fn an_enum_type_still_renders_as_a_bare_file_name() {
    let src = "extends Node\n\nenum Outer { A }\n\nclass Inner:\n\tenum IE { B }\n\nfunc f() -> void:\n\tvar x: Outer = Inner.IE.B\n\tprint(x)\n";
    let project = sample_project();
    project.write("src/deep/e.gd", src);

    let (server, client) = Connection::memory();
    let server_thread = std::thread::spawn(move || gd_server::serve(server));
    boot_project(&project, &client);

    let uri = file_uri(&project.root.join("src/deep/e.gd"));
    let diags = open_and_diags(&client, &uri, src);

    let messages: Vec<&str> = diags
        .diagnostics
        .iter()
        .map(|d| d.message.as_str())
        .collect();
    assert!(
        messages
            .iter()
            .any(|m| m.contains("e.gd::Inner.IE") && m.contains("e.gd.Outer")),
        "an enum keeps its bare-file rendering; got {messages:?}"
    );
    assert!(
        !messages.iter().any(|m| m.contains("res://")),
        "no enum rendering leaks the full path; got {messages:?}"
    );

    shutdown(&client, server_thread);
}

/// A buffer outside the project has no `res://` form. It falls back to the basename rather than
/// putting an absolute host path in front of the user.
#[test]
fn a_file_outside_the_project_falls_back_to_its_basename() {
    let project = sample_project();
    let outside = tempfile::tempdir().expect("temp dir");
    let path = camino::Utf8PathBuf::from_path_buf(outside.path().join("loose.gd"))
        .expect("utf-8 temp path");
    std::fs::write(path.as_std_path(), SRC).expect("write loose.gd");

    let (server, client) = Connection::memory();
    let server_thread = std::thread::spawn(move || gd_server::serve(server));
    boot_project(&project, &client);

    let uri = file_uri(&path);
    let diags = open_and_diags(&client, &uri, SRC);

    let messages: Vec<&str> = diags
        .diagnostics
        .iter()
        .map(|d| d.message.as_str())
        .collect();
    assert!(
        messages.contains(
            &r#"The local variable "foo" is shadowing an already-declared variable at line 4 in the base class "loose.gd::A"."#
        ),
        "got {messages:?}"
    );

    shutdown(&client, server_thread);
}

/// The path spelling is shared: an enum's `native_type` is built from the script path on the
/// in-file side and from the cross-file link on the other, and the two must agree or the identity
/// check rejects a value against its own enum (#286). The shape is Pixelorama's — a peer file
/// holds a member typed by this file's inner class, so reading that member's enum-typed field
/// comes back through the cross-file path while the parameter it feeds was built in place.
#[test]
fn an_enum_keeps_one_identity_across_a_file_boundary() {
    let project = sample_project();
    project.write(
        "src/holder.gd",
        "class_name Holder\nextends Node\n\nconst E = preload(\"res://src/deep/e.gd\")\n\nvar profile: E.Profile\n",
    );
    let src = "extends Node\n\nenum FileFormat { PNG, GIF }\n\nclass Profile:\n\tvar fmt: FileFormat\n\nfunc uses(f: FileFormat) -> bool:\n\treturn f == FileFormat.PNG\n\nfunc go(h: Holder) -> void:\n\tprint(uses(h.profile.fmt))\n";
    project.write("src/deep/e.gd", src);

    let (server, client) = Connection::memory();
    let server_thread = std::thread::spawn(move || gd_server::serve(server));
    boot_project(&project, &client);

    let uri = file_uri(&project.root.join("src/deep/e.gd"));
    let diags = open_and_diags(&client, &uri, src);

    let errors: Vec<&str> = diags
        .diagnostics
        .iter()
        .filter(|d| d.severity == Some(lsp_types::DiagnosticSeverity::ERROR))
        .map(|d| d.message.as_str())
        .collect();
    assert!(
        errors.is_empty(),
        "the enum is its own type; got {errors:?}"
    );

    shutdown(&client, server_thread);
}
