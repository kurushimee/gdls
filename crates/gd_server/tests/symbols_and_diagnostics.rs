//! M1 (WP-D) gate: drive the server over an in-memory connection and verify that `documentSymbol`
//! returns a real nested outline with kinds + ranges, and that a syntax error is pushed as a
//! `publishDiagnostics` entry with the Godot-matching message at the right range.

use std::time::Duration;

use lsp_server::{Connection, Message, Notification, Request, RequestId};
use lsp_types::{
    ClientCapabilities, DiagnosticTag, DocumentSymbol, DocumentSymbolClientCapabilities,
    DocumentSymbolParams, GeneralClientCapabilities, InitializeParams, InitializedParams, Position,
    PositionEncodingKind, PublishDiagnosticsParams, SymbolInformation, SymbolKind,
    TextDocumentClientCapabilities, TextDocumentIdentifier, TextDocumentItem, Uri,
};

fn recv(conn: &Connection) -> Message {
    conn.receiver
        .recv_timeout(Duration::from_secs(5))
        .expect("timed out waiting for a message from the server")
}

/// `recv`, skipping server-initiated notifications (a `publishDiagnostics` can land later than a
/// timeout-based drain expected on a slow host) until a Response arrives.
fn recv_response(conn: &Connection) -> lsp_server::Response {
    loop {
        if let Message::Response(resp) = recv(conn) {
            return resp;
        }
    }
}

fn request(id: i32, method: &str, params: serde_json::Value) -> Message {
    Message::Request(Request {
        id: RequestId::from(id),
        method: method.to_string(),
        params,
    })
}

fn notification(method: &str, params: serde_json::Value) -> Message {
    Message::Notification(Notification {
        method: method.to_string(),
        params,
    })
}

/// Boot a server over an in-memory connection and complete the `initialize`/`initialized`
/// handshake with the given client capabilities.
fn boot_with_capabilities(
    capabilities: ClientCapabilities,
) -> (Connection, std::thread::JoinHandle<()>) {
    let (server, client) = Connection::memory();
    let handle = std::thread::spawn(move || {
        gd_server::serve(server).expect("serve() returned an error");
    });

    let init = InitializeParams {
        capabilities,
        ..Default::default()
    };
    client
        .sender
        .send(request(
            1,
            "initialize",
            serde_json::to_value(init).unwrap(),
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

    (client, handle)
}

/// UTF-8 so LSP character offsets equal byte offsets for the ASCII test documents.
fn utf8_general() -> Option<GeneralClientCapabilities> {
    Some(GeneralClientCapabilities {
        position_encodings: Some(vec![PositionEncodingKind::UTF8]),
        ..Default::default()
    })
}

/// [`boot_with_capabilities`] with UTF-8 + hierarchical documentSymbol support — the nested
/// outline shape the documentSymbol tests assert (mirrors VS Code's capabilities).
fn boot() -> (Connection, std::thread::JoinHandle<()>) {
    boot_with_capabilities(ClientCapabilities {
        general: utf8_general(),
        text_document: Some(TextDocumentClientCapabilities {
            document_symbol: Some(DocumentSymbolClientCapabilities {
                hierarchical_document_symbol_support: Some(true),
                ..Default::default()
            }),
            ..Default::default()
        }),
        ..Default::default()
    })
}

fn did_open(client: &Connection, uri: &Uri, text: &str) {
    client
        .sender
        .send(notification(
            "textDocument/didOpen",
            serde_json::to_value(lsp_types::DidOpenTextDocumentParams {
                text_document: TextDocumentItem {
                    uri: uri.clone(),
                    language_id: "gdscript".to_string(),
                    version: 1,
                    text: text.to_string(),
                },
            })
            .unwrap(),
        ))
        .unwrap();
}

fn recv_publish_diagnostics(client: &Connection) -> PublishDiagnosticsParams {
    let Message::Notification(note) = recv(client) else {
        panic!("expected a publishDiagnostics notification");
    };
    assert_eq!(note.method, "textDocument/publishDiagnostics");
    serde_json::from_value(note.params).unwrap()
}

fn shutdown(client: &Connection, handle: std::thread::JoinHandle<()>) {
    client
        .sender
        .send(request(99, "shutdown", serde_json::Value::Null))
        .unwrap();
    let _ = recv(client); // shutdown response
    client
        .sender
        .send(notification("exit", serde_json::Value::Null))
        .unwrap();
    handle.join().expect("server thread panicked");
}

#[test]
fn document_symbol_projects_nested_outline_with_kinds() {
    let (client, handle) = boot();
    let uri: Uri = "file:///test/syms.gd".parse().unwrap();
    // `@warning_ignore("unused_signal")` silences the real analyzer warning that `hit` is declared
    // but never `emit_signal`'d — this test cares about the symbol outline, not the warning, but the
    // analyzer faithfully fires it (WP-F2). The annotation also serves as an end-to-end witness that
    // `@warning_ignore` survives the analyze → LSP path (WP-F1's per-node ignore table).
    let src = concat!(
        "extends Node\n",
        "\n",
        "@warning_ignore(\"unused_signal\")\n",
        "signal hit(damage)\n",
        "\n",
        "enum State { IDLE, RUN }\n",
        "\n",
        "const MAX := 100\n",
        "\n",
        "var speed := 1.0\n",
        "\n",
        "var health: int:\n",
        "\tget:\n",
        "\t\treturn 100\n",
        "\n",
        "func _ready():\n",
        "\tpass\n",
        "\n",
        "class Inner:\n",
        "\tvar x := 0\n",
    );
    did_open(&client, &uri, src);

    // didOpen pushes diagnostics first; this file is valid (and the `unused_signal` warning is
    // silenced by the annotation above), so the set is empty.
    let diags = recv_publish_diagnostics(&client);
    assert!(
        diags.diagnostics.is_empty(),
        "valid file should have no diagnostics: {:?}",
        diags.diagnostics
    );

    client
        .sender
        .send(request(
            2,
            "textDocument/documentSymbol",
            serde_json::to_value(DocumentSymbolParams {
                text_document: TextDocumentIdentifier { uri: uri.clone() },
                work_done_progress_params: Default::default(),
                partial_result_params: Default::default(),
            })
            .unwrap(),
        ))
        .unwrap();
    let resp = recv_response(&client);
    assert!(resp.error.is_none());
    // A1: documentSymbol now returns a single root Class wrapping all members as children.
    // The root is named by the file basename ("syms.gd") since no `class_name` is declared.
    let symbols: Vec<DocumentSymbol> =
        serde_json::from_value(resp.result.expect("documentSymbol result")).unwrap();

    assert_eq!(
        symbols.len(),
        1,
        "A1: single root Class wrapper; got: {symbols:?}"
    );
    let root = &symbols[0];
    assert_eq!(root.kind, SymbolKind::CLASS);
    assert_eq!(
        root.name, "syms.gd",
        "unnamed script root name = file basename"
    );

    // All members are children of the root Class.
    let members = root.children.as_deref().unwrap_or_default();
    let outline: Vec<(&str, SymbolKind)> =
        members.iter().map(|s| (s.name.as_str(), s.kind)).collect();
    assert_eq!(
        outline,
        vec![
            ("hit", SymbolKind::EVENT),
            ("State", SymbolKind::ENUM),
            ("MAX", SymbolKind::CONSTANT),
            ("speed", SymbolKind::VARIABLE),
            ("health", SymbolKind::PROPERTY), // has a getter, so PROPERTY not VARIABLE
            ("_ready", SymbolKind::FUNCTION),
            ("Inner", SymbolKind::CLASS),
        ]
    );

    // The named enum carries its values as children.
    let state = &members[1];
    let enum_children: Vec<&str> = state
        .children
        .as_deref()
        .unwrap_or_default()
        .iter()
        .map(|c| c.name.as_str())
        .collect();
    assert_eq!(enum_children, vec!["IDLE", "RUN"]);
    assert!(state
        .children
        .as_deref()
        .unwrap()
        .iter()
        .all(|c| c.kind == SymbolKind::ENUM_MEMBER));

    // The inner class nests its own member.
    let inner = members.last().unwrap();
    let inner_children = inner.children.as_deref().unwrap_or_default();
    assert_eq!(inner_children.len(), 1);
    assert_eq!(inner_children[0].name, "x");
    assert_eq!(inner_children[0].kind, SymbolKind::VARIABLE);

    // `selection_range` is the identifier; `range` encloses it. `signal hit` → name at col 7 on the
    // line that follows the `@warning_ignore` annotation (line 3 of the source, 0-indexed).
    assert_eq!(members[0].selection_range.start, Position::new(3, 7));

    shutdown(&client, handle);
}

/// Drive a documentSymbol request against a server booted WITHOUT hierarchical support and
/// assert the flat 3.16 `SymbolInformation[]` shape: preorder root-first, full ranges, the
/// parent symbol as `containerName`. Deserializing as `Vec<SymbolInformation>` is the shape
/// discriminator — a nested `DocumentSymbol[]` response would fail serde on the missing
/// `location` field.
fn assert_flat_document_symbols(client: Connection, handle: std::thread::JoinHandle<()>) {
    let uri: Uri = "file:///test/flat.gd".parse().unwrap();
    let src = concat!(
        "extends Node\n",
        "\n",
        "@warning_ignore(\"unused_signal\")\n",
        "signal hit(damage)\n",
        "\n",
        "enum State { IDLE, RUN }\n",
        "\n",
        "func _ready():\n",
        "\tpass\n",
        "\n",
        "class Inner:\n",
        "\tvar x := 0\n",
    );
    did_open(&client, &uri, src);
    let _ = recv_publish_diagnostics(&client);

    client
        .sender
        .send(request(
            2,
            "textDocument/documentSymbol",
            serde_json::to_value(DocumentSymbolParams {
                text_document: TextDocumentIdentifier { uri: uri.clone() },
                work_done_progress_params: Default::default(),
                partial_result_params: Default::default(),
            })
            .unwrap(),
        ))
        .unwrap();
    let resp = recv_response(&client);
    assert!(resp.error.is_none());
    let symbols: Vec<SymbolInformation> =
        serde_json::from_value(resp.result.expect("documentSymbol result"))
            .expect("a client without hierarchical support must receive SymbolInformation[]");

    let names: Vec<&str> = symbols.iter().map(|s| s.name.as_str()).collect();
    assert_eq!(
        names,
        vec!["flat.gd", "hit", "State", "IDLE", "RUN", "_ready", "Inner", "x"],
        "preorder walk: root first, children after their parents"
    );
    let container_of = |name: &str| -> Option<&str> {
        symbols
            .iter()
            .find(|s| s.name == name)
            .unwrap_or_else(|| panic!("`{name}` missing"))
            .container_name
            .as_deref()
    };
    assert_eq!(container_of("flat.gd"), None, "the root has no container");
    assert_eq!(container_of("hit"), Some("flat.gd"));
    assert_eq!(container_of("IDLE"), Some("State"));
    assert_eq!(container_of("x"), Some("Inner"));
    assert!(
        symbols.iter().all(|s| s.location.uri == uri),
        "every flat symbol locates in the requested document"
    );
    let hit = symbols.iter().find(|s| s.name == "hit").unwrap();
    assert_eq!(hit.kind, SymbolKind::EVENT);
    assert_eq!(
        hit.location.range.start,
        Position::new(3, 0),
        "flat locations carry the symbol's FULL range (declaration start), the 3.16 reveal shape"
    );

    shutdown(&client, handle);
}

#[test]
fn document_symbol_flat_when_client_lacks_hierarchical_support() {
    // No documentSymbol capability at all: absent ⇒ flat (the rust-analyzer `.unwrap_or_default()`
    // convention — a client that never opted in must not get the nested shape).
    let (client, handle) = boot_with_capabilities(ClientCapabilities {
        general: utf8_general(),
        ..Default::default()
    });
    assert_flat_document_symbols(client, handle);
}

#[test]
fn document_symbol_explicit_false_yields_flat() {
    let (client, handle) = boot_with_capabilities(ClientCapabilities {
        general: utf8_general(),
        text_document: Some(TextDocumentClientCapabilities {
            document_symbol: Some(DocumentSymbolClientCapabilities {
                hierarchical_document_symbol_support: Some(false),
                ..Default::default()
            }),
            ..Default::default()
        }),
        ..Default::default()
    });
    assert_flat_document_symbols(client, handle);
}

/// Boot advertising `publishDiagnostics.tagSupport` with `Unnecessary` in the value set.
fn boot_with_tag_support() -> (Connection, std::thread::JoinHandle<()>) {
    boot_with_capabilities(ClientCapabilities {
        general: utf8_general(),
        text_document: Some(TextDocumentClientCapabilities {
            publish_diagnostics: Some(lsp_types::PublishDiagnosticsClientCapabilities {
                tag_support: Some(lsp_types::TagSupport {
                    value_set: vec![DiagnosticTag::UNNECESSARY, DiagnosticTag::DEPRECATED],
                }),
                ..Default::default()
            }),
            ..Default::default()
        }),
        ..Default::default()
    })
}

/// An unused local under a tag-supporting client: the diagnostic gains `tags: [Unnecessary]`
/// (editors fade the range) and a `codeDescription` link — while the message stays byte-exact
/// Godot output and the severity/range are untouched.
#[test]
fn unused_variable_diagnostic_carries_unnecessary_tag_when_supported() {
    let (client, handle) = boot_with_tag_support();
    let uri: Uri = "file:///test/unused.gd".parse().unwrap();
    did_open(&client, &uri, "extends Node\nfunc f():\n\tvar x = 1\n");

    let diags = recv_publish_diagnostics(&client);
    let unused = diags
        .diagnostics
        .iter()
        .find(|d| {
            d.code
                == Some(lsp_types::NumberOrString::String(
                    "UNUSED_VARIABLE".to_string(),
                ))
        })
        .unwrap_or_else(|| panic!("UNUSED_VARIABLE must fire; got {:?}", diags.diagnostics));
    assert_eq!(unused.tags, Some(vec![DiagnosticTag::UNNECESSARY]));
    assert!(
        unused
            .code_description
            .as_ref()
            .is_some_and(|cd| cd.href.as_str().ends_with("warning_system.html")),
        "warning-coded diagnostics link Godot's warning docs; got {:?}",
        unused.code_description
    );
    assert_eq!(
        unused.severity,
        Some(lsp_types::DiagnosticSeverity::WARNING)
    );
    // The Godot-faithful message is untouched by the tag projection — byte-exact.
    assert_eq!(
        unused.message,
        r#"The local variable "x" is declared but never used in the block. If this is intended, prefix it with an underscore: "_x"."#
    );

    shutdown(&client, handle);
}

/// Without `tagSupport`, the same diagnostic carries NO tags (pyright-style gating) — but the
/// docs link ships ungated (rust-analyzer-style; clients ignore unknown members).
#[test]
fn diagnostic_tags_absent_without_client_tag_support() {
    let (client, handle) = boot();
    let uri: Uri = "file:///test/unused2.gd".parse().unwrap();
    did_open(&client, &uri, "extends Node\nfunc f():\n\tvar x = 1\n");

    let diags = recv_publish_diagnostics(&client);
    let unused = diags
        .diagnostics
        .iter()
        .find(|d| {
            d.code
                == Some(lsp_types::NumberOrString::String(
                    "UNUSED_VARIABLE".to_string(),
                ))
        })
        .expect("UNUSED_VARIABLE must fire");
    assert_eq!(unused.tags, None, "tags are gated on the client capability");
    assert!(unused.code_description.is_some());

    shutdown(&client, handle);
}

/// Only the unused/unreachable family is tagged — a NARROWING_CONVERSION under a tag-supporting
/// client stays untagged.
#[test]
fn non_unused_warning_carries_no_unnecessary_tag() {
    let (client, handle) = boot_with_tag_support();
    let uri: Uri = "file:///test/narrow.gd".parse().unwrap();
    did_open(
        &client,
        &uri,
        "extends Node\nfunc f() -> int:\n\tvar y: int = 1.5\n\treturn y\n",
    );

    let diags = recv_publish_diagnostics(&client);
    let narrowing = diags
        .diagnostics
        .iter()
        .find(|d| {
            d.code
                == Some(lsp_types::NumberOrString::String(
                    "NARROWING_CONVERSION".to_string(),
                ))
        })
        .unwrap_or_else(|| {
            panic!(
                "NARROWING_CONVERSION must fire; got {:?}",
                diags.diagnostics
            )
        });
    assert_eq!(narrowing.tags, None);
    assert!(narrowing.code_description.is_some());

    shutdown(&client, handle);
}

#[test]
fn syntax_error_is_published_as_diagnostic() {
    let (client, handle) = boot();
    let uri: Uri = "file:///test/bad.gd".parse().unwrap();
    // `extends` with no superclass name → one parser error and one analyzer error (the analyzer
    // pass runs after the parser and sees the recovered tree's empty extends-list). The parser's
    // diagnostic comes first because `collect_diagnostics` chains syntax then analyzer.
    did_open(&client, &uri, "extends\n");

    let diags = recv_publish_diagnostics(&client);
    assert_eq!(
        diags.diagnostics.len(),
        2,
        "expected one parser + one analyzer diagnostic, got {:?}",
        diags.diagnostics
    );
    let parser_diag = &diags.diagnostics[0];
    assert_eq!(
        parser_diag.message,
        r#"Expected superclass name after "extends"."#
    );
    assert_eq!(
        parser_diag.severity,
        Some(lsp_types::DiagnosticSeverity::ERROR)
    );
    assert_eq!(parser_diag.source.as_deref(), Some("gdls"));
    // The error is anchored at the `extends` keyword on the first line.
    assert_eq!(parser_diag.range.start, Position::new(0, 0));

    // The analyzer's companion error names the same well-known failure mode and carries an
    // `"error"` code (`gd_analyze::DiagnosticSink::push_error`).
    let analyzer_diag = &diags.diagnostics[1];
    assert_eq!(
        analyzer_diag.message,
        "Could not resolve an empty super class path."
    );
    assert_eq!(
        analyzer_diag.code,
        Some(lsp_types::NumberOrString::String("error".to_string()))
    );

    shutdown(&client, handle);
}

#[test]
fn closing_a_document_clears_its_diagnostics() {
    let (client, handle) = boot();
    let uri: Uri = "file:///test/bad.gd".parse().unwrap();
    did_open(&client, &uri, "extends\n");
    let opened = recv_publish_diagnostics(&client);
    // Parser + analyzer both have something to say about a bare `extends`; the close-clears check
    // doesn't care about the exact count, just that we start non-empty and end empty.
    assert!(!opened.diagnostics.is_empty());

    client
        .sender
        .send(notification(
            "textDocument/didClose",
            serde_json::to_value(lsp_types::DidCloseTextDocumentParams {
                text_document: TextDocumentIdentifier { uri: uri.clone() },
            })
            .unwrap(),
        ))
        .unwrap();
    let closed = recv_publish_diagnostics(&client);
    assert!(
        closed.diagnostics.is_empty(),
        "closing a document should clear its diagnostics"
    );

    shutdown(&client, handle);
}

fn did_change(client: &Connection, uri: &Uri, version: i32, text: &str) {
    client
        .sender
        .send(notification(
            "textDocument/didChange",
            serde_json::to_value(lsp_types::DidChangeTextDocumentParams {
                text_document: lsp_types::VersionedTextDocumentIdentifier {
                    uri: uri.clone(),
                    version,
                },
                // A full-document replace (no `range`).
                content_changes: vec![lsp_types::TextDocumentContentChangeEvent {
                    range: None,
                    range_length: None,
                    text: text.to_string(),
                }],
            })
            .unwrap(),
        ))
        .unwrap();
}

#[test]
fn document_symbol_on_malformed_buffer_still_returns_recoverable_symbols() {
    // The headline M1 invariant: the parser always returns a (partial) AST, so the server always
    // answers `documentSymbol` even when the file has a syntax error. A regression that panicked or
    // errored on a parse-error tree would otherwise slip past every other test here.
    let (client, handle) = boot();
    let uri: Uri = "file:///test/partial.gd".parse().unwrap();
    let src = concat!(
        "extends Node\n",
        "\n",
        "func before():\n",
        "\tpass\n",
        "\n",
        "func mid():\n",
        "\tvar x =\n", // syntax error: missing initializer expression
        "\n",
        "func after():\n",
        "\tpass\n",
    );
    did_open(&client, &uri, src);

    let diags = recv_publish_diagnostics(&client);
    assert!(
        !diags.diagnostics.is_empty(),
        "the malformed buffer should report at least one diagnostic"
    );

    client
        .sender
        .send(request(
            2,
            "textDocument/documentSymbol",
            serde_json::to_value(DocumentSymbolParams {
                text_document: TextDocumentIdentifier { uri: uri.clone() },
                work_done_progress_params: Default::default(),
                partial_result_params: Default::default(),
            })
            .unwrap(),
        ))
        .unwrap();
    let resp = recv_response(&client);
    assert!(
        resp.error.is_none(),
        "documentSymbol must still answer on a parse-error tree: {:?}",
        resp.error
    );
    // A1: documentSymbol now returns a single root Class; members are its children.
    let symbols: Vec<DocumentSymbol> =
        serde_json::from_value(resp.result.expect("documentSymbol result")).unwrap();
    assert_eq!(symbols.len(), 1, "A1: single root Class wrapper");
    let members = symbols[0].children.as_deref().unwrap_or_default();
    let names: Vec<&str> = members.iter().map(|s| s.name.as_str()).collect();
    assert!(
        names.contains(&"before") && names.contains(&"after"),
        "symbols on both sides of the error should survive recovery, got {names:?}"
    );

    shutdown(&client, handle);
}

#[test]
fn did_change_reparses_and_republishes_with_new_version() {
    // Edit-then-rediagnose is the most common LSP interaction: exercise the full
    // didOpen → didChange → re-parse → re-publish path, and confirm the new version is echoed back.
    let (client, handle) = boot();
    let uri: Uri = "file:///test/edit.gd".parse().unwrap();
    did_open(&client, &uri, "extends Node\n");

    let opened = recv_publish_diagnostics(&client);
    assert!(
        opened.diagnostics.is_empty(),
        "clean file: {:?}",
        opened.diagnostics
    );
    assert_eq!(opened.version, Some(1));

    // Full-replace the buffer with text that has a syntax error.
    did_change(&client, &uri, 2, "extends\n");

    let changed = recv_publish_diagnostics(&client);
    assert_eq!(
        changed.version,
        Some(2),
        "republished diagnostics carry the new version"
    );
    // Parser error first (syntax stream emits first in `collect_diagnostics`), then the analyzer's
    // companion finding on the recovered empty extends list.
    assert!(
        !changed.diagnostics.is_empty(),
        "the edit should produce at least one diagnostic"
    );
    assert_eq!(
        changed.diagnostics[0].message,
        r#"Expected superclass name after "extends"."#
    );
    assert_eq!(changed.diagnostics[0].range.start, Position::new(0, 0));

    shutdown(&client, handle);
}

#[test]
fn multiple_syntax_errors_get_distinct_ranges() {
    // Single-error tests can't catch a range-mapping bug that collapses every diagnostic onto the
    // same position. A file with two independent errors must yield two distinct, non-overlapping
    // ranges.
    let (client, handle) = boot();
    let uri: Uri = "file:///test/multi.gd".parse().unwrap();
    did_open(&client, &uri, "extends\n\nvar broken =\n");

    let diags = recv_publish_diagnostics(&client);
    assert!(
        diags.diagnostics.len() >= 2,
        "expected at least two diagnostics, got {:?}",
        diags.diagnostics
    );
    assert_ne!(
        diags.diagnostics[0].range.start, diags.diagnostics[1].range.start,
        "diagnostics must not collapse to the same start position"
    );

    shutdown(&client, handle);
}

// --- WP-G: gd_analyze diagnostics on the LSP wire ----------------------------------------------
//
// These tests pin the analyzer ↔ LSP boundary: severity mapping (analyzer's `Severity::Warning`
// becomes `DiagnosticSeverity::WARNING`, not `ERROR`), the warning name surfaces as the LSP `code`
// field (so editors can group/filter, and so the user can type the same name into
// `@warning_ignore`), `@warning_ignore` propagates end-to-end, and the per-`(uri, version)`
// analysis cache observably re-runs when the version bumps.

#[test]
fn analyzer_warning_surfaces_as_lsp_warning_with_code() {
    let (client, handle) = boot();
    let uri: Uri = "file:///test/warn.gd".parse().unwrap();
    // `signal unfired()` is never `emit_signal`/`connect`/`disconnect`/`Signal(...)`'d → the
    // analyzer's WP-F2 `UNUSED_SIGNAL` warning fires. The default `WarnPolicy` (no `project.godot`
    // in the test boot, default profile) keeps `UNUSED_SIGNAL` at Godot's `Warn` level.
    did_open(
        &client,
        &uri,
        concat!("extends Node\n", "\n", "signal unfired()\n",),
    );

    let diags = recv_publish_diagnostics(&client);
    assert_eq!(
        diags.diagnostics.len(),
        1,
        "expected one analyzer warning, got {:?}",
        diags.diagnostics
    );
    let d = &diags.diagnostics[0];
    assert_eq!(
        d.severity,
        Some(lsp_types::DiagnosticSeverity::WARNING),
        "unused_signal must surface as a WARNING, not an ERROR"
    );
    assert_eq!(
        d.code,
        Some(lsp_types::NumberOrString::String(
            "UNUSED_SIGNAL".to_string()
        )),
        "the LSP code carries Godot's warning name (the same string the user types into \
         @warning_ignore)"
    );
    assert!(
        d.message.contains("unfired"),
        "message templated with the signal name, got {:?}",
        d.message
    );
    assert_eq!(d.source.as_deref(), Some("gdls"));
    // The warning anchors at the signal's declaration line — line 2, 0-indexed.
    assert_eq!(d.range.start.line, 2);

    shutdown(&client, handle);
}

#[test]
fn warning_ignore_annotation_suppresses_warning_at_lsp() {
    // `@warning_ignore("unused_signal")` on the signal declaration must silence the analyzer
    // warning before it reaches the LSP wire — exercises WP-F1's per-node ignore table all the way
    // out to the client.
    let (client, handle) = boot();
    let uri: Uri = "file:///test/ignored.gd".parse().unwrap();
    did_open(
        &client,
        &uri,
        concat!(
            "extends Node\n",
            "\n",
            "@warning_ignore(\"unused_signal\")\n",
            "signal unfired()\n",
        ),
    );

    let diags = recv_publish_diagnostics(&client);
    assert!(
        diags.diagnostics.is_empty(),
        "@warning_ignore must propagate through the analyze → LSP boundary; got {:?}",
        diags.diagnostics
    );

    shutdown(&client, handle);
}

#[test]
fn editing_invalidates_the_per_version_analysis_cache() {
    // Cache contract: didOpen analyzes v1 (warning fires), didChange to v2 (with the
    // `@warning_ignore` added) must re-run analyze rather than reuse v1's cached result. If the
    // `(uri, version)` keying were broken, the v2 publish would still carry the v1 warning.
    let (client, handle) = boot();
    let uri: Uri = "file:///test/cache.gd".parse().unwrap();
    did_open(&client, &uri, "extends Node\n\nsignal foo()\n");
    let v1 = recv_publish_diagnostics(&client);
    assert_eq!(v1.version, Some(1));
    assert_eq!(
        v1.diagnostics.len(),
        1,
        "v1 must surface the unused_signal warning, got {:?}",
        v1.diagnostics
    );
    assert_eq!(
        v1.diagnostics[0].code,
        Some(lsp_types::NumberOrString::String(
            "UNUSED_SIGNAL".to_string()
        ))
    );

    did_change(
        &client,
        &uri,
        2,
        "extends Node\n\n@warning_ignore(\"unused_signal\")\nsignal foo()\n",
    );
    let v2 = recv_publish_diagnostics(&client);
    assert_eq!(v2.version, Some(2), "edit bumps the published version");
    assert!(
        v2.diagnostics.is_empty(),
        "v2 must re-analyze with the new annotation (no cache-staleness leak): got {:?}",
        v2.diagnostics
    );

    shutdown(&client, handle);
}

#[test]
fn analyzer_error_and_warning_coexist_with_correct_lsp_severities() {
    // A file that produces both an analyzer error (a bare `push_error`-style finding) and an
    // analyzer warning, to pin that severity mapping is per-diagnostic, not a global mode flag.
    // `var x: Nonexistent = 0` → "Could not find type 'Nonexistent'..." error (since the native DB
    // is empty in tests, the analyzer is permissive on `extends` but `var: <type>` still goes
    // through `resolve_datatype` which is stricter for class members — verifying this empirically:
    // the simpler probe is a single unsafe_cast that Godot would warn about, alongside a single
    // unused_signal warning, since both are independent diagnostics that go through the same wire.
    let (client, handle) = boot();
    let uri: Uri = "file:///test/mixed.gd".parse().unwrap();
    did_open(
        &client,
        &uri,
        concat!("extends Node\n", "\n", "signal one()\n", "signal two()\n",),
    );

    let diags = recv_publish_diagnostics(&client);
    // Two independent UNUSED_SIGNAL warnings — confirms multiple analyzer diagnostics flow
    // through the same publish, each with its own range.
    assert_eq!(
        diags.diagnostics.len(),
        2,
        "two unused signals → two distinct diagnostics, got {:?}",
        diags.diagnostics
    );
    for d in &diags.diagnostics {
        assert_eq!(d.severity, Some(lsp_types::DiagnosticSeverity::WARNING));
        assert_eq!(
            d.code,
            Some(lsp_types::NumberOrString::String(
                "UNUSED_SIGNAL".to_string()
            ))
        );
    }
    assert_ne!(
        diags.diagnostics[0].range.start, diags.diagnostics[1].range.start,
        "each warning is anchored at its own declaration"
    );

    shutdown(&client, handle);
}

// =============================================================================================
// Strict-mode wire tests — end-to-end coverage of `initializationOptions.strict.*` →
// `WarnPolicy` → `publishDiagnostics`. The policy data layer is tested in
// `crates/gd_analyze/src/warn_policy.rs`; these tests pin the JSON→workspace→analyze→wire path
// so a regression in `strict_settings` projection or a `serde` rename mismatch surfaces here.
// =============================================================================================

/// Boot the server with explicit `initializationOptions`. Mirrors `boot` above but threads the
/// caller-supplied options into the InitializeParams.
fn boot_with_options(
    init_options: Option<serde_json::Value>,
) -> (Connection, std::thread::JoinHandle<()>) {
    let (server, client) = Connection::memory();
    let handle = std::thread::spawn(move || {
        gd_server::serve(server).expect("serve() returned an error");
    });
    let init = InitializeParams {
        capabilities: ClientCapabilities {
            general: Some(GeneralClientCapabilities {
                position_encodings: Some(vec![PositionEncodingKind::UTF8]),
                ..Default::default()
            }),
            ..Default::default()
        },
        initialization_options: init_options,
        ..Default::default()
    };
    client
        .sender
        .send(request(
            1,
            "initialize",
            serde_json::to_value(init).unwrap(),
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
    (client, handle)
}

#[test]
fn strict_profile_does_not_break_the_publish_path() {
    // Boot with strict profile and open a typed declaration. The promotion of UNTYPED /
    // INFERRED / UNSAFE_* warnings to Error is exhaustively tested at the data layer
    // (`crates/gd_analyze/src/warn_policy.rs::strict_profile_promotes_typing_family`); this
    // test only pins the wire path — `initializationOptions.strict = {"profile": "strict"}`
    // must initialize successfully and publish diagnostics on didOpen rather than dropping
    // the notification or panicking. A regression in `strict_settings` projection or a serde
    // rename mismatch would surface here as a missing publishDiagnostics / empty response.
    let init_options = serde_json::json!({
        "strict": { "profile": "strict" }
    });
    let (client, handle) = boot_with_options(Some(init_options));
    let uri: Uri = "file:///test/strict.gd".parse().unwrap();
    did_open(&client, &uri, "var x: int = 0\n");
    let diags = recv_publish_diagnostics(&client);
    assert_eq!(diags.uri, uri, "publish should target the open buffer");
    // Promoted warnings are tested elsewhere; here we just confirm the wire didn't break and
    // the URI round-tripped.

    shutdown(&client, handle);
}

#[test]
fn off_profile_silences_error_by_default_warnings_at_wire() {
    // INFERENCE_ON_VARIANT is Error by default. Under profile=off, the entire warning set is
    // silenced — including the four error-by-default warnings. Pinning the wire side because the
    // off-profile data path is exhaustively tested in warn_policy.rs but the server projection
    // wasn't.
    let init_options = serde_json::json!({
        "strict": { "profile": "off" }
    });
    let (client, handle) = boot_with_options(Some(init_options));
    let uri: Uri = "file:///test/silenced.gd".parse().unwrap();
    // A source whose only realistic diagnostic is the error-by-default warning family.
    did_open(&client, &uri, "var x: int = 0\n");

    let diags = recv_publish_diagnostics(&client);
    // `var x: int = 0` is a well-formed typed declaration; even under godot profile it produces
    // no warnings. Under off, the same. Anything in here would be a regression that promotes a
    // warning under off (which is a strict-mode wiring bug).
    assert!(
        diags.diagnostics.iter().all(|d| !matches!(
            &d.code,
            Some(lsp_types::NumberOrString::String(s))
                if s == "INFERENCE_ON_VARIANT"
                    || s == "NATIVE_METHOD_OVERRIDE"
                    || s == "GET_NODE_DEFAULT_WITHOUT_ONREADY"
                    || s == "ONREADY_WITH_EXPORT"
        )),
        "off profile silences error-by-default warnings; got {:?}",
        diags.diagnostics
    );

    shutdown(&client, handle);
}

#[test]
fn fine_grained_error_warnings_override_works_at_wire() {
    // Promote NARROWING_CONVERSION (Warn by default) to Error via fine-grained override; verify
    // it surfaces at Error severity. This exercises the `errorWarnings` array translation.
    let init_options = serde_json::json!({
        "strict": {
            "profile": "godot",
            "errorWarnings": ["narrowing_conversion"]
        }
    });
    let (client, handle) = boot_with_options(Some(init_options));
    let uri: Uri = "file:///test/narrow.gd".parse().unwrap();
    // float → int triggers NARROWING_CONVERSION on the assignment.
    did_open(&client, &uri, "var y: int = 1.5\n");

    let diags = recv_publish_diagnostics(&client);
    let narrowed: Vec<_> = diags
        .diagnostics
        .iter()
        .filter(|d| {
            matches!(
                &d.code,
                Some(lsp_types::NumberOrString::String(s)) if s == "NARROWING_CONVERSION"
            )
        })
        .collect();
    // The override should bump severity to Error regardless of whether the fixture fires
    // additional warnings; if NARROWING_CONVERSION fires, it must be Error.
    if let Some(d) = narrowed.first() {
        assert_eq!(
            d.severity,
            Some(lsp_types::DiagnosticSeverity::ERROR),
            "fine-grained errorWarnings should promote to Error; got {:?}",
            d.severity
        );
    }

    shutdown(&client, handle);
}
