//! #570 — hover and `signatureHelp` spell a parameter list the same way.
//!
//! `signatureHelp` reads the declaring file's AST because the `Interface` seam carries parameter
//! types but no defaults. Hover only ever saw the interface, so `func work(n: int, mode: Mode)`
//! dropped the `= Mode.FAST` the popup was already showing. Both now build the list between the
//! parentheses from one builder, so they cannot drift.
//!
//! The frames around it stay different on purpose. Hover renders GDScript declaration syntax;
//! `signatureHelp` renders Godot's own call hint, `<return type> name(params)`, which is what
//! `_make_arguments_hint` builds (gdscript_editor.cpp:750-795). Godot's editor language server
//! answers `null` to `textDocument/signatureHelp`, so the hint popup is the only oracle for it.

mod common;

use std::time::Duration;

use common::{
    file_uri, notification, recv, recv_response, request, sample_project, shutdown, try_recv,
};
use lsp_server::{Connection, Message};
use lsp_types::{
    DidOpenTextDocumentParams, Hover, HoverContents, InitializeParams, InitializedParams,
    MarkupContent, Position, PublishDiagnosticsParams, SignatureHelp, TextDocumentItem, Uri,
};

const LIB: &str = "extends Node\nclass_name RcLib\n\n## A documented constant.\nconst LIMIT := 10\n\n## Does the thing.\nfunc work(n: int, mode: int = LIMIT) -> int:\n\treturn n + LIMIT\n\nfunc rest(a: int, ...more) -> void:\n\tprint(a, more)\n\nfunc plain(a, b := \"x\") -> void:\n\tprint(a, b)\n";

// The parameter, not `RcLib.new()`: the trimmed native dump these tests boot against marks `Node`
// abstract, so constructing one would type `lib` as Variant and defeat the fixture.
const USE: &str = "extends Node\n\nfunc run(lib: RcLib) -> void:\n\tprint(lib.work(1, 2))\n\tprint(lib.rest(1, 2))\n\tprint(lib.plain(1))\n\tprint(RcLib.LIMIT)\n";

fn boot(project: &common::TempProject, client: &Connection) {
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

fn did_open(client: &Connection, uri: &Uri, text: &str) {
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
    // Wait for THIS file's diagnostics: they only arrive once the project index is built, and a
    // hover asked before that types every cross-file read as `Variant`.
    loop {
        let Some(Message::Notification(note)) = try_recv(client, Duration::from_secs(5)) else {
            panic!("expected a publishDiagnostics notification for {uri:?}");
        };
        if note.method != "textDocument/publishDiagnostics" {
            continue;
        }
        let params: PublishDiagnosticsParams = serde_json::from_value(note.params).unwrap();
        if &params.uri == uri {
            return;
        }
    }
}

fn hover_line(client: &Connection, uri: &Uri, pos: Position) -> String {
    client
        .sender
        .send(request(
            90,
            "textDocument/hover",
            lsp_types::HoverParams {
                text_document_position_params: lsp_types::TextDocumentPositionParams {
                    text_document: lsp_types::TextDocumentIdentifier { uri: uri.clone() },
                    position: pos,
                },
                work_done_progress_params: Default::default(),
            },
        ))
        .unwrap();
    let resp = recv_response(client);
    let hover: Hover = serde_json::from_value(resp.result.expect("a hover result"))
        .unwrap_or_else(|e| panic!("hover at {pos:?}: {e}"));
    let HoverContents::Markup(MarkupContent { value, .. }) = hover.contents else {
        panic!("hover is markup");
    };
    // A client that advertises no markdown support gets the plain signature; one that does gets it
    // inside a ```gdscript fence. Take the first line that is neither.
    value
        .lines()
        .find(|l| !l.is_empty() && !l.starts_with("```"))
        .unwrap_or_else(|| panic!("a signature line at {pos:?}; got {value:?}"))
        .to_owned()
}

fn signature_label(client: &Connection, uri: &Uri, pos: Position) -> String {
    client
        .sender
        .send(request(
            91,
            "textDocument/signatureHelp",
            lsp_types::SignatureHelpParams {
                context: None,
                text_document_position_params: lsp_types::TextDocumentPositionParams {
                    text_document: lsp_types::TextDocumentIdentifier { uri: uri.clone() },
                    position: pos,
                },
                work_done_progress_params: Default::default(),
            },
        ))
        .unwrap();
    let resp = recv_response(client);
    let help: SignatureHelp = serde_json::from_value(resp.result.expect("a signatureHelp result"))
        .unwrap_or_else(|e| panic!("signatureHelp at {pos:?}: {e}"));
    help.signatures
        .first()
        .expect("one signature")
        .label
        .clone()
}

/// The text between the outermost parentheses.
fn params_of(label: &str) -> &str {
    let open = label.find('(').expect("an open paren");
    let close = label.rfind(')').expect("a close paren");
    &label[open + 1..close]
}

fn open_both(project: &common::TempProject, client: &Connection) -> Uri {
    project.write("src/rclib.gd", LIB);
    project.write("src/rcuse.gd", USE);
    boot(project, client);
    let uri = file_uri(&project.root.join("src/rcuse.gd"));
    did_open(client, &uri, USE);
    uri
}

#[test]
fn hover_on_a_call_shows_the_parameter_defaults() {
    let project = sample_project();
    let (server, client) = Connection::memory();
    let handle = std::thread::spawn(move || gd_server::serve(server));
    let uri = open_both(&project, &client);

    // `\tprint(lib.work(1, 2))` — the cursor on `work`.
    assert_eq!(
        hover_line(&client, &uri, Position::new(3, 12)),
        "func work(n: int, mode: int = LIMIT) -> int"
    );

    shutdown(&client, handle);
}

/// The contract: the two surfaces may frame a signature differently, but the parameter list
/// between the parentheses is one string built one way.
#[test]
fn hover_and_signature_help_agree_on_the_parameter_list() {
    let project = sample_project();
    let (server, client) = Connection::memory();
    let handle = std::thread::spawn(move || gd_server::serve(server));
    let uri = open_both(&project, &client);

    for (name, hover_at, help_at) in [
        ("work", Position::new(3, 12), Position::new(3, 17)),
        ("rest", Position::new(4, 12), Position::new(4, 17)),
        ("plain", Position::new(5, 12), Position::new(5, 18)),
    ] {
        let h = hover_line(&client, &uri, hover_at);
        let s = signature_label(&client, &uri, help_at);
        assert_eq!(
            params_of(&h),
            params_of(&s),
            "{name}: hover {h:?} vs signatureHelp {s:?}"
        );
        assert!(
            h.starts_with("func "),
            "{name}: hover frames as a declaration; got {h:?}"
        );
        assert!(
            !s.starts_with("func "),
            "{name}: signatureHelp keeps Godot's call hint; got {s:?}"
        );
    }

    shutdown(&client, handle);
}

/// A rest parameter and an untyped parameter with an inferred default are the two shapes the
/// interface renders differently from the declaring AST, so they pin the shared builder directly.
#[test]
fn a_rest_parameter_and_an_inferred_default_survive_the_shared_builder() {
    let project = sample_project();
    let (server, client) = Connection::memory();
    let handle = std::thread::spawn(move || gd_server::serve(server));
    let uri = open_both(&project, &client);

    assert_eq!(
        hover_line(&client, &uri, Position::new(4, 12)),
        "func rest(a: int, ...more: Variant) -> void"
    );
    assert_eq!(
        hover_line(&client, &uri, Position::new(5, 12)),
        r#"func plain(a: Variant, b: String = "x") -> void"#
    );

    shutdown(&client, handle);
}

/// A constant read in its own file carries the folded value the reducer already stamped on it, so
/// the use site shows what the declaration site has always shown.
#[test]
fn a_same_file_constant_read_shows_its_value() {
    let src = "extends Node\n\nconst CAP := 9\nconst NAME := \"x\"\nconst OPAQUE = Callable()\n\nfunc f() -> void:\n\tprint(CAP)\n\tprint(NAME)\n\tprint(OPAQUE)\n";
    let project = sample_project();
    project.write("src/consts.gd", src);
    let (server, client) = Connection::memory();
    let handle = std::thread::spawn(move || gd_server::serve(server));
    boot(&project, &client);
    let uri = file_uri(&project.root.join("src/consts.gd"));
    did_open(&client, &uri, src);

    assert_eq!(
        hover_line(&client, &uri, Position::new(7, 8)),
        "const CAP: int = 9"
    );
    assert_eq!(
        hover_line(&client, &uri, Position::new(8, 8)),
        r#"const NAME: String = "x""#
    );
    // Nothing to show is shown as nothing — never a guessed value.
    assert_eq!(
        hover_line(&client, &uri, Position::new(9, 8)),
        "const OPAQUE: Callable"
    );

    shutdown(&client, handle);
}

/// A constant read across a file boundary has no value anywhere on the gdls side — the interface
/// carries none — so it stays type-only rather than inventing one.
#[test]
fn a_cross_file_constant_read_stays_type_only() {
    let project = sample_project();
    let (server, client) = Connection::memory();
    let handle = std::thread::spawn(move || gd_server::serve(server));
    let uri = open_both(&project, &client);

    // `\tprint(RcLib.LIMIT)` — the cursor on `LIMIT`.
    assert_eq!(
        hover_line(&client, &uri, Position::new(6, 15)),
        "const LIMIT: int"
    );

    shutdown(&client, handle);
}
