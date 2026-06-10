//! Integration tests for cross-file inheritance typing + member navigation (v1.0.1, #15/#13).
//!
//! A `class_name` base in one file, a child extending it in another:
//!   - the child publishes ZERO diagnostics (`$`/`@onready`/inherited members/self-compat all
//!     used to false-positive),
//!   - `textDocument/definition` on a member-access site jumps to the declaring file,
//!   - `textDocument/references` on the signal declaration includes the cross-file emit site,
//!   - `textDocument/hover` renders `var`/`const` member declarations.

mod common;

use common::{file_uri, notification, recv, recv_response, request, shutdown, TempProject};
use lsp_server::{Connection, Message};
use lsp_types::{
    DidOpenTextDocumentParams, GotoDefinitionParams, GotoDefinitionResponse, Hover, HoverContents,
    HoverParams, InitializeParams, InitializedParams, Location, PartialResultParams, Position,
    ReferenceContext, ReferenceParams, TextDocumentIdentifier, TextDocumentItem,
    TextDocumentPositionParams, WorkDoneProgressParams,
};

const BASE_GD: &str = "\
class_name NavBase
extends Node
signal ping(x: int)
var hp: int = 10
const SPEED: int = 10
func boost(amount: int) -> void:
\tpass
";

// Positions are 0-based (line, character) into this exact text, UTF-8 encoding negotiated.
const CHILD_GD: &str = "\
extends NavBase
@onready var lbl = $Label
func _ready() -> void:
\tping.emit(1)
\tboost(SPEED)
\thp += 1
\tvar _h = self.hp
\tvar _s = self.SPEED
";

fn project() -> TempProject {
    let p = TempProject::new();
    p.write(
        "project.godot",
        "config_version=5\n\n[application]\n\nconfig/name=\"Test\"\n",
    );
    p.write("extension_api.json", common::MINI_API);
    p.write("base.gd", BASE_GD);
    p.write("child.gd", CHILD_GD);
    p
}

fn boot_with_api(p: &TempProject) -> (Connection, std::thread::JoinHandle<anyhow::Result<()>>) {
    let (server, client) = Connection::memory();
    let handle = std::thread::spawn(move || gd_server::serve(server));
    let init = InitializeParams {
        initialization_options: Some(serde_json::json!({
            "projectRoot": p.root.as_str(),
            "extensionApiPath": p.root.join("extension_api.json").as_str(),
        })),
        capabilities: lsp_types::ClientCapabilities {
            general: Some(lsp_types::GeneralClientCapabilities {
                position_encodings: Some(vec![lsp_types::PositionEncodingKind::UTF8]),
                ..Default::default()
            }),
            ..Default::default()
        },
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

fn did_open(client: &Connection, project: &TempProject, rel: &str) -> Vec<lsp_types::Diagnostic> {
    let abs = project.root.join(rel);
    let text = std::fs::read_to_string(abs.as_std_path()).expect("read file");
    let uri = file_uri(&abs);
    client
        .sender
        .send(notification(
            "textDocument/didOpen",
            DidOpenTextDocumentParams {
                text_document: TextDocumentItem {
                    uri,
                    language_id: "gdscript".to_string(),
                    version: 1,
                    text,
                },
            },
        ))
        .unwrap();
    let msg = recv(client);
    let Message::Notification(notif) = msg else {
        panic!("expected publishDiagnostics after didOpen, got {msg:?}");
    };
    assert_eq!(notif.method, "textDocument/publishDiagnostics");
    let params: lsp_types::PublishDiagnosticsParams = serde_json::from_value(notif.params).unwrap();
    params.diagnostics
}

/// The Family-A acceptance shape: every construct in CHILD_GD used to produce an error.
#[test]
fn child_extending_cross_file_base_publishes_no_errors() {
    let p = project();
    let (client, handle) = boot_with_api(&p);
    let diags = did_open(&client, &p, "child.gd");
    let errors: Vec<_> = diags
        .iter()
        .filter(|d| d.severity == Some(lsp_types::DiagnosticSeverity::ERROR))
        .collect();
    assert!(errors.is_empty(), "expected zero errors, got {errors:#?}");
    shutdown(&client, handle);
}

/// #13: definition on the attribute in `self.hp` (line 6, col 15 = inside `hp`) jumps to the
/// declaring file's member declaration.
#[test]
fn definition_on_member_access_jumps_to_declaring_file() {
    let p = project();
    let (client, handle) = boot_with_api(&p);
    let _ = did_open(&client, &p, "child.gd");

    let child_uri = file_uri(&p.root.join("child.gd"));
    client
        .sender
        .send(request(
            2,
            "textDocument/definition",
            GotoDefinitionParams {
                text_document_position_params: TextDocumentPositionParams {
                    text_document: TextDocumentIdentifier {
                        uri: child_uri.clone(),
                    },
                    // line 6 = `\tvar _h = self.hp`, character 15 is inside `hp`.
                    position: Position::new(6, 15),
                },
                work_done_progress_params: WorkDoneProgressParams::default(),
                partial_result_params: PartialResultParams::default(),
            },
        ))
        .unwrap();
    let resp = recv_response(&client);
    let result: Option<GotoDefinitionResponse> =
        serde_json::from_value(resp.result.expect("definition result")).unwrap();
    let Some(GotoDefinitionResponse::Scalar(loc)) = result else {
        panic!("expected a scalar definition location, got {result:?}");
    };
    assert!(
        loc.uri.as_str().ends_with("base.gd"),
        "definition must land in base.gd, got {}",
        loc.uri.as_str()
    );
    // `var hp: int = 10` is line 3 (0-based) of BASE_GD.
    assert_eq!(loc.range.start.line, 3, "expected the hp declaration line");
    shutdown(&client, handle);
}

/// #13: references on the signal DECLARATION include the cross-file `ping.emit(1)` site.
#[test]
fn references_on_signal_declaration_include_cross_file_emit_site() {
    let p = project();
    let (client, handle) = boot_with_api(&p);
    let _ = did_open(&client, &p, "base.gd");
    let _ = did_open(&client, &p, "child.gd");

    let base_uri = file_uri(&p.root.join("base.gd"));
    client
        .sender
        .send(request(
            3,
            "textDocument/references",
            ReferenceParams {
                text_document_position: TextDocumentPositionParams {
                    text_document: TextDocumentIdentifier {
                        uri: base_uri.clone(),
                    },
                    // line 2 = `signal ping(x: int)`, character 8 is inside `ping`.
                    position: Position::new(2, 8),
                },
                work_done_progress_params: WorkDoneProgressParams::default(),
                partial_result_params: PartialResultParams::default(),
                context: ReferenceContext {
                    include_declaration: false,
                },
            },
        ))
        .unwrap();
    let resp = recv_response(&client);
    let locs: Vec<Location> =
        serde_json::from_value(resp.result.expect("references result")).unwrap_or_default();
    assert!(
        locs.iter()
            .any(|l| l.uri.as_str().ends_with("child.gd") && l.range.start.line == 3),
        "expected the child.gd ping.emit site among references, got {locs:#?}"
    );
    shutdown(&client, handle);
}

/// #13: hover on `self.hp` / `self.SPEED` renders the var/const declaration shapes.
#[test]
fn hover_on_var_and_const_members_renders_declarations() {
    let p = project();
    let (client, handle) = boot_with_api(&p);
    let _ = did_open(&client, &p, "child.gd");

    let child_uri = file_uri(&p.root.join("child.gd"));
    let hover_at = |id: i32, line: u32, character: u32| -> String {
        client
            .sender
            .send(request(
                id,
                "textDocument/hover",
                HoverParams {
                    text_document_position_params: TextDocumentPositionParams {
                        text_document: TextDocumentIdentifier {
                            uri: child_uri.clone(),
                        },
                        position: Position::new(line, character),
                    },
                    work_done_progress_params: WorkDoneProgressParams::default(),
                },
            ))
            .unwrap();
        let resp = recv_response(&client);
        let hover: Option<Hover> =
            serde_json::from_value(resp.result.expect("hover result")).unwrap();
        match hover.expect("hover content").contents {
            HoverContents::Markup(m) => m.value,
            other => panic!("expected markup hover, got {other:?}"),
        }
    };

    // line 6 `\tvar _h = self.hp`, character 15 inside `hp`.
    let hp = hover_at(4, 6, 15);
    assert!(
        hp.contains("var hp: int"),
        "var member hover must render the declaration, got {hp:?}"
    );
    // line 7 `\tvar _s = self.SPEED`, character 16 inside `SPEED`.
    let speed = hover_at(5, 7, 16);
    assert!(
        speed.contains("const SPEED: int"),
        "const member hover must render the declaration, got {speed:?}"
    );
    shutdown(&client, handle);
}
