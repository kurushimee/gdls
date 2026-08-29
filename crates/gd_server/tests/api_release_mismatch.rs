//! A native dump that names a different Godot release than the project declares (#329).
//!
//! Every dump source lands stamped `ApiProvenance::Exact` — the claim that this dump IS the engine
//! surface, and the claim that unlocks `Identifier "X" not declared in the current scope.` and its
//! siblings. A dump from an OLDER release cannot carry it: each API the newer release added is
//! missing from it, and the miss reads as a user error. Pixelorama shipped exactly that, a
//! checked-in 4.6.3 dump under `config/features=("4.7")`, and gdls fabricated four errors out of
//! 4.7-only APIs.
//!
//! `workspace::version_mismatch_tests` pins the decision itself. This pins what a user sees:
//! diagnostics on the wire, and the message that explains them.

mod common;

use std::time::Duration;

use common::{file_uri, notification, recv, recv_response, request, shutdown, TempProject};
use lsp_server::{Connection, Message};
use lsp_types::{
    ClientCapabilities, DidOpenTextDocumentParams, InitializeParams, InitializedParams,
    PublishDiagnosticsParams, TextDocumentItem, Uri,
};

/// A minimal dump stamped 4.6.3. It has no `DrawableTexture2D`, which is exactly right: that class
/// is a 4.7 addition, absent from every real 4.6 dump too.
const API_4_6: &str = r#"{
    "header": {"version_major": 4, "version_minor": 6, "version_patch": 3},
    "utility_functions": [
        {"name": "print", "return_type": "void", "category": "general", "is_vararg": true, "hash": 1, "arguments": []}
    ],
    "builtin_classes": [],
    "global_enums": [],
    "global_constants": [],
    "singletons": [],
    "classes": [
        {"name": "Object", "is_refcounted": false, "is_instantiable": true, "api_type": "core"},
        {"name": "Node", "inherits": "Object", "is_refcounted": false, "is_instantiable": true, "api_type": "core"}
    ]
}"#;

/// A project that declares `features` — the evidenced case, where gdls is entitled to act on a
/// mismatch. `declared` is written verbatim into `config/features`.
fn project_declaring(declared: &str, api: &str) -> TempProject {
    let p = TempProject::new();
    p.write(
        "project.godot",
        &format!(
            "config_version=5\n\n[application]\n\nconfig/name=\"T\"\nconfig/features=PackedStringArray(\"{declared}\")\n"
        ),
    );
    p.write("extension_api.json", api);
    p
}

/// Boot against `p`'s root dump, open `text`, and return `(diagnostic messages, showMessage texts)`.
fn open_and_collect(p: &TempProject, text: &str) -> (Vec<String>, Vec<String>) {
    let uri: Uri = file_uri(&p.root.join("src/main.gd"));
    let options = serde_json::json!({
        "projectRoot": p.root.as_str(),
        "autoDumpExtensionApi": false,
        "extensionApiPath": p.root.join("extension_api.json").as_str(),
    });
    let (server, client) = Connection::memory();
    let server_thread = std::thread::spawn(move || gd_server::serve(server));

    client
        .sender
        .send(request(
            1,
            "initialize",
            InitializeParams {
                capabilities: ClientCapabilities::default(),
                initialization_options: Some(options),
                ..Default::default()
            },
        ))
        .unwrap();
    let resp = recv_response(&client);
    assert!(resp.error.is_none(), "initialize errored: {:?}", resp.error);
    client
        .sender
        .send(notification(
            "initialized",
            serde_json::to_value(InitializedParams {}).unwrap(),
        ))
        .unwrap();
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

    let mut diagnostics: Option<Vec<String>> = None;
    let mut messages: Vec<String> = Vec::new();
    // Diagnostics and the startup notice race; drain until the publish for this file arrives.
    while diagnostics.is_none() {
        match recv(&client) {
            Message::Notification(n) if n.method == "textDocument/publishDiagnostics" => {
                let params: PublishDiagnosticsParams =
                    serde_json::from_value(n.params).expect("valid publishDiagnostics");
                if params.uri == uri {
                    diagnostics = Some(params.diagnostics.into_iter().map(|d| d.message).collect());
                }
            }
            Message::Notification(n) if n.method == "window/showMessage" => {
                let params: lsp_types::ShowMessageParams =
                    serde_json::from_value(n.params).expect("valid showMessage");
                messages.push(params.message);
            }
            _ => {}
        }
    }
    // The notice may still be in flight behind the publish; give it a moment.
    while let Some(Message::Notification(n)) = common::try_recv(&client, Duration::from_millis(300))
    {
        if n.method == "window/showMessage" {
            let params: lsp_types::ShowMessageParams =
                serde_json::from_value(n.params).expect("valid showMessage");
            messages.push(params.message);
        }
    }

    shutdown(&client, server_thread);
    (
        diagnostics.expect("a publish for the opened file"),
        messages,
    )
}

/// `DrawableTexture2D` is a 4.7 addition — present in the stock 4.7 surface, absent from every 4.6
/// one. Under the old behavior a 4.6 dump on a 4.7 project drew
/// `Identifier "DrawableTexture2D" not declared in the current scope.` here, which is the exact
/// error Pixelorama saw. The `class_exists` fixture below pins that gdls did not simply stop
/// reporting undeclared names.
const SRC: &str =
    "extends Node\n\nfunc f() -> void:\n\tvar n = DrawableTexture2D.new()\n\tprint(n)\n";

/// A name no Godot release has, so it must read as undeclared under any correct surface.
const SRC_TYPO: &str =
    "extends Node\n\nfunc f() -> void:\n\tvar n = DrawbleTexture2D.new()\n\tprint(n)\n";

/// The issue's reproduction: 4.6 dump, project declares 4.7. The dump is dropped for the stock 4.7
/// surface, so the 4.7-only class resolves — and the user is told why.
#[test]
fn a_stale_dump_no_longer_invents_undeclared_identifiers() {
    let p = project_declaring("4.7", API_4_6);
    let (diags, messages) = open_and_collect(&p, SRC);

    assert!(
        !diags.iter().any(|m| m.contains("not declared")),
        "a 4.7 class must not read as undeclared under a project that declares 4.7: {diags:?}"
    );
    assert!(
        messages
            .iter()
            .any(|m| m.contains("4.6") && m.contains("4.7") && m.contains("config/features")),
        "the user must be told which dump was ignored and how to fix it: {messages:?}"
    );
}

/// The control. Replacing the surface must not have cost gdls the ability to report a real typo —
/// a name no release has is still undeclared, on the very same project.
#[test]
fn a_replaced_surface_still_reports_a_real_typo() {
    let p = project_declaring("4.7", API_4_6);
    let (diags, _) = open_and_collect(&p, SRC_TYPO);

    assert!(
        diags
            .iter()
            .any(|m| m == r#"Identifier "DrawbleTexture2D" not declared in the current scope."#),
        "a genuinely unknown name must still be reported: {diags:?}"
    );
}

/// A dump under the release it was made for is left alone, notice and all — and a 4.7-only class
/// is then correctly undeclared, because on a 4.6 project it really does not exist.
#[test]
fn a_matching_dump_is_kept_and_keeps_its_negatives() {
    let p = project_declaring("4.6", API_4_6);
    let (diags, messages) = open_and_collect(&p, SRC);

    assert!(
        diags
            .iter()
            .any(|m| m == r#"Identifier "DrawableTexture2D" not declared in the current scope."#),
        "a 4.7-only class on a 4.6 project is a real error: {diags:?}"
    );
    assert!(
        !messages.iter().any(|m| m.contains("extension_api.json")),
        "a matching dump warrants no notice: {messages:?}"
    );
}
