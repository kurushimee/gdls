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
        "config_version=5\n\n[application]\n\nconfig/name=\"Test\"\nconfig/features=PackedStringArray(\"4.6\")\n",
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
            "autoDumpExtensionApi": false,
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

const USER_GD: &str = "\
extends Node
func use_lib() -> void:
\tvar b: NavBase = NavBase.new()
\tb.boost(1)
";

/// v1.0.3 real-project walk regression: `definition` on the attribute callee of a dotted
/// method call through a typed var (`b.boost(1)`, b: NavBase) must jump to the declaring file.
/// Hover resolved the signature here while definition returned null — the reducer's attribute
/// paths record no `Binding::Use`, so the handler now projects the `Binding::Call` whose callee
/// identifier contains the cursor (the same projection references' call-site click uses).
#[test]
fn definition_on_dotted_method_call_jumps_to_declaring_file() {
    let p = project();
    p.write("user.gd", USER_GD);
    let (client, handle) = boot_with_api(&p);
    did_open(&client, &p, "user.gd");

    let user_uri = file_uri(&p.root.join("user.gd"));
    let params = GotoDefinitionParams {
        text_document_position_params: TextDocumentPositionParams {
            text_document: TextDocumentIdentifier {
                uri: user_uri.clone(),
            },
            // line 3 `\tb.boost(1)`, character 4 inside `boost`.
            position: Position {
                line: 3,
                character: 4,
            },
        },
        work_done_progress_params: WorkDoneProgressParams::default(),
        partial_result_params: PartialResultParams::default(),
    };
    client
        .sender
        .send(request(41, "textDocument/definition", params))
        .unwrap();
    let resp = recv_response(&client);
    assert!(resp.error.is_none(), "definition errored: {:?}", resp.error);
    let result: Option<GotoDefinitionResponse> =
        serde_json::from_value(resp.result.unwrap()).unwrap();
    let Some(GotoDefinitionResponse::Scalar(loc)) = result else {
        panic!("expected a scalar definition location for `b.boost(1)`, got {result:?}");
    };
    assert!(
        loc.uri.as_str().ends_with("base.gd"),
        "must jump to the declaring file, got {}",
        loc.uri.as_str()
    );
    shutdown(&client, handle);
}

/// v1.0.3 real-project walk regression: `callHierarchy/incomingCalls` on a method declaration
/// must surface CROSS-FILE callers that reach the method through a typed var. The candidate set
/// used to come from the interface-level `name_referencers` index, which never contains
/// body-only method names — incoming calls were structurally empty across files.
#[test]
fn incoming_calls_surface_cross_file_dotted_callers() {
    use lsp_types::{
        CallHierarchyIncomingCall, CallHierarchyIncomingCallsParams, CallHierarchyItem,
        CallHierarchyPrepareParams,
    };
    let p = project();
    p.write("user.gd", USER_GD);
    let (client, handle) = boot_with_api(&p);
    did_open(&client, &p, "base.gd");
    did_open(&client, &p, "user.gd");

    let base_uri = file_uri(&p.root.join("base.gd"));
    let prepare = CallHierarchyPrepareParams {
        text_document_position_params: TextDocumentPositionParams {
            text_document: TextDocumentIdentifier {
                uri: base_uri.clone(),
            },
            // line 5 `func boost(amount: int) -> void:`, character 6 inside `boost`.
            position: Position {
                line: 5,
                character: 6,
            },
        },
        work_done_progress_params: WorkDoneProgressParams::default(),
    };
    client
        .sender
        .send(request(42, "textDocument/prepareCallHierarchy", prepare))
        .unwrap();
    let resp = recv_response(&client);
    let items: Option<Vec<CallHierarchyItem>> =
        serde_json::from_value(resp.result.unwrap()).unwrap();
    let item = items
        .and_then(|v| v.into_iter().next())
        .expect("prepare returns the boost item");
    assert_eq!(item.name, "boost");

    let incoming = CallHierarchyIncomingCallsParams {
        item,
        work_done_progress_params: WorkDoneProgressParams::default(),
        partial_result_params: PartialResultParams::default(),
    };
    client
        .sender
        .send(request(43, "callHierarchy/incomingCalls", incoming))
        .unwrap();
    let resp = recv_response(&client);
    assert!(resp.error.is_none(), "incoming errored: {:?}", resp.error);
    let calls: Option<Vec<CallHierarchyIncomingCall>> =
        serde_json::from_value(resp.result.unwrap()).unwrap();
    let calls = calls.unwrap_or_default();
    let cross_file = calls
        .iter()
        .find(|c| c.from.uri.as_str().ends_with("user.gd"))
        .unwrap_or_else(|| panic!("expected a caller from user.gd, got {calls:?}"));
    assert_eq!(cross_file.from.name, "use_lib");
    assert!(
        !cross_file.from_ranges.is_empty(),
        "the call site range must be reported"
    );
    shutdown(&client, handle);
}

/// v1.0.3 real-project walk regression: `prepareCallHierarchy` with the cursor on a CALL-SITE
/// callee identifier must prepare the CALLEE's item (here `boost`, declared in base.gd), not
/// the function the cursor happens to sit inside — the old enclosing-only walk returned
/// `use_lib` for this position.
#[test]
fn prepare_call_hierarchy_at_call_site_targets_the_callee() {
    use lsp_types::{CallHierarchyItem, CallHierarchyPrepareParams};
    let p = project();
    p.write("user.gd", USER_GD);
    let (client, handle) = boot_with_api(&p);
    did_open(&client, &p, "user.gd");

    let user_uri = file_uri(&p.root.join("user.gd"));
    let prepare = CallHierarchyPrepareParams {
        text_document_position_params: TextDocumentPositionParams {
            text_document: TextDocumentIdentifier {
                uri: user_uri.clone(),
            },
            // line 3 `\tb.boost(1)`, character 4 inside `boost` (the callee identifier).
            position: Position {
                line: 3,
                character: 4,
            },
        },
        work_done_progress_params: WorkDoneProgressParams::default(),
    };
    client
        .sender
        .send(request(44, "textDocument/prepareCallHierarchy", prepare))
        .unwrap();
    let resp = recv_response(&client);
    let items: Option<Vec<CallHierarchyItem>> =
        serde_json::from_value(resp.result.unwrap()).unwrap();
    let item = items
        .and_then(|v| v.into_iter().next())
        .expect("prepare returns an item for the callee");
    assert_eq!(item.name, "boost", "the callee, not the enclosing function");
    assert!(
        item.uri.as_str().ends_with("base.gd"),
        "the item must locate the callee's declaring file, got {}",
        item.uri.as_str()
    );
    shutdown(&client, handle);
}

// ===================================================================================================
// #541 — a bare call to a cross-file inherited method. `boost(SPEED)` on CHILD_GD line 4, column 1.
// ===================================================================================================

/// A definition request at `(line, character)` in `child.gd`, as a scalar location.
fn definition_at(
    client: &Connection,
    uri: &lsp_types::Uri,
    id: i32,
    line: u32,
    character: u32,
) -> Option<Location> {
    client
        .sender
        .send(request(
            id,
            "textDocument/definition",
            GotoDefinitionParams {
                text_document_position_params: TextDocumentPositionParams {
                    text_document: TextDocumentIdentifier { uri: uri.clone() },
                    position: Position::new(line, character),
                },
                work_done_progress_params: WorkDoneProgressParams::default(),
                partial_result_params: PartialResultParams::default(),
            },
        ))
        .unwrap();
    let resp = recv_response(client);
    let result: Option<GotoDefinitionResponse> =
        serde_json::from_value(resp.result.unwrap_or(serde_json::Value::Null)).unwrap_or(None);
    match result {
        Some(GotoDefinitionResponse::Scalar(loc)) => Some(loc),
        Some(GotoDefinitionResponse::Array(mut v)) => v.pop(),
        _ => None,
    }
}

/// References at `(line, character)` in `uri`, as `(file name, start line)` pairs, sorted.
fn references_at(
    client: &Connection,
    uri: &lsp_types::Uri,
    id: i32,
    line: u32,
    character: u32,
) -> Vec<(String, u32)> {
    client
        .sender
        .send(request(
            id,
            "textDocument/references",
            ReferenceParams {
                text_document_position: TextDocumentPositionParams {
                    text_document: TextDocumentIdentifier { uri: uri.clone() },
                    position: Position::new(line, character),
                },
                work_done_progress_params: WorkDoneProgressParams::default(),
                partial_result_params: PartialResultParams::default(),
                context: ReferenceContext {
                    include_declaration: true,
                },
            },
        ))
        .unwrap();
    let resp = recv_response(client);
    let locs: Vec<Location> =
        serde_json::from_value(resp.result.unwrap_or(serde_json::Value::Null)).unwrap_or_default();
    let mut out: Vec<(String, u32)> = locs
        .iter()
        .map(|l| {
            let s = l.uri.as_str();
            (
                s.rsplit('/').next().unwrap_or(s).to_owned(),
                l.range.start.line,
            )
        })
        .collect();
    out.sort();
    out
}

/// Every edit a rename at `(line, character)` produces, as `(file name, start line)` pairs, sorted.
fn rename_at(
    client: &Connection,
    uri: &lsp_types::Uri,
    id: i32,
    line: u32,
    character: u32,
    new_name: &str,
) -> Vec<(String, u32)> {
    client
        .sender
        .send(request(
            id,
            "textDocument/rename",
            lsp_types::RenameParams {
                text_document_position: TextDocumentPositionParams {
                    text_document: TextDocumentIdentifier { uri: uri.clone() },
                    position: Position::new(line, character),
                },
                new_name: new_name.to_owned(),
                work_done_progress_params: WorkDoneProgressParams::default(),
            },
        ))
        .unwrap();
    let resp = recv_response(client);
    assert!(resp.error.is_none(), "rename errored: {:?}", resp.error);
    let edit: lsp_types::WorkspaceEdit =
        serde_json::from_value(resp.result.expect("rename result")).unwrap();
    let mut out: Vec<(String, u32)> = Vec::new();
    // This harness negotiates a minimal client, so the server answers with `changes` rather than
    // `documentChanges`. Read both, so the helper does not silently see an empty set.
    if let Some(changes) = edit.changes {
        for (uri, edits) in changes {
            let s = uri.as_str();
            let name = s.rsplit('/').next().unwrap_or(s).to_owned();
            for e in edits {
                out.push((name.clone(), e.range.start.line));
            }
        }
    }
    if let Some(lsp_types::DocumentChanges::Edits(docs)) = edit.document_changes {
        for doc in docs {
            let s = doc.text_document.uri.as_str();
            let name = s.rsplit('/').next().unwrap_or(s).to_owned();
            for e in doc.edits {
                let range = match e {
                    lsp_types::OneOf::Left(t) => t.range,
                    lsp_types::OneOf::Right(t) => t.text_edit.range,
                };
                out.push((name.clone(), range.start.line));
            }
        }
    }
    out.sort();
    out
}

/// The repro. `definition` on the bare `boost(SPEED)` answered null, because
/// `reduce_identifier` skips its use record in callee position and the cross-file half of that
/// record was never made anywhere else.
#[test]
fn definition_on_a_bare_inherited_call_jumps_to_the_declaring_file() {
    let p = project();
    let (client, handle) = boot_with_api(&p);
    let _ = did_open(&client, &p, "child.gd");
    let child_uri = file_uri(&p.root.join("child.gd"));

    // line 4 = `\tboost(SPEED)`, character 1 is the `b` of `boost`.
    let loc = definition_at(&client, &child_uri, 2, 4, 1).expect("a definition location");
    assert!(
        loc.uri.as_str().ends_with("base.gd"),
        "expected base.gd, got {}",
        loc.uri.as_str()
    );
    // `func boost(amount: int) -> void:` is line 5 (0-based) of BASE_GD.
    assert_eq!(loc.range.start.line, 5);
    shutdown(&client, handle);
}

/// The corruption case: renaming from the DECLARATION has to reach the bare call. Before this,
/// the edit set covered the declaration and every dotted site and silently skipped the bare one,
/// so applying it left a call to a method that no longer existed.
#[test]
fn renaming_the_declaration_reaches_the_bare_inherited_call() {
    let p = project();
    let (client, handle) = boot_with_api(&p);
    let _ = did_open(&client, &p, "base.gd");
    let _ = did_open(&client, &p, "child.gd");
    let base_uri = file_uri(&p.root.join("base.gd"));

    // line 5 = `func boost(amount: int) -> void:`, character 6 is inside `boost`.
    let edits = rename_at(&client, &base_uri, 3, 5, 6, "accelerate");
    assert_eq!(
        edits,
        vec![("base.gd".to_owned(), 5), ("child.gd".to_owned(), 4)],
        "the declaration and the bare call, both"
    );
    shutdown(&client, handle);
}

/// Renaming FROM the bare call site gives the identical set: rename canonicalizes through
/// `definition`, so the answer cannot depend on which end the user clicked.
#[test]
fn renaming_from_the_bare_call_gives_the_same_edits() {
    let p = project();
    let (client, handle) = boot_with_api(&p);
    let _ = did_open(&client, &p, "base.gd");
    let _ = did_open(&client, &p, "child.gd");
    let base_uri = file_uri(&p.root.join("base.gd"));
    let child_uri = file_uri(&p.root.join("child.gd"));

    let from_decl = rename_at(&client, &base_uri, 3, 5, 6, "accelerate");
    let from_call = rename_at(&client, &child_uri, 4, 4, 1, "accelerate");
    assert!(!from_call.is_empty(), "two empty sets are not agreement");
    assert_eq!(from_call, from_decl);
    shutdown(&client, handle);
}

/// References from the declaration include the bare call.
#[test]
fn references_on_the_declaration_include_the_bare_inherited_call() {
    let p = project();
    let (client, handle) = boot_with_api(&p);
    let _ = did_open(&client, &p, "base.gd");
    let _ = did_open(&client, &p, "child.gd");
    let base_uri = file_uri(&p.root.join("base.gd"));

    let refs = references_at(&client, &base_uri, 5, 5, 6);
    assert!(
        refs.contains(&("child.gd".to_owned(), 4)),
        "expected the bare call among {refs:?}"
    );
    shutdown(&client, handle);
}

/// A bare NATIVE inherited call anchors nothing new, so it keeps answering from the native side
/// and rename still refuses it. The fail-closed half.
#[test]
fn a_bare_native_call_is_unchanged() {
    let p = project();
    p.write(
        "native_caller.gd",
        "extends Node\nfunc go() -> void:\n\tset_process(true)\n",
    );
    let (client, handle) = boot_with_api(&p);
    let _ = did_open(&client, &p, "native_caller.gd");
    let uri = file_uri(&p.root.join("native_caller.gd"));

    client
        .sender
        .send(request(
            6,
            "textDocument/rename",
            lsp_types::RenameParams {
                text_document_position: TextDocumentPositionParams {
                    text_document: TextDocumentIdentifier { uri: uri.clone() },
                    position: Position::new(2, 1),
                },
                new_name: "nope".to_owned(),
                work_done_progress_params: WorkDoneProgressParams::default(),
            },
        ))
        .unwrap();
    let resp = recv_response(&client);
    assert!(
        resp.error.is_some(),
        "renaming a native method must be refused, got {:?}",
        resp.result
    );
    shutdown(&client, handle);
}
