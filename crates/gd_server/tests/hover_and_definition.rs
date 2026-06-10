//! WP-H gate: drive the server over an in-memory connection and verify that
//! `textDocument/hover` returns analyzer-resolved type info + native doc text, and that
//! `textDocument/definition` jumps to the declaration site of an in-file identifier. Both run
//! through the same parse/analyze caches `publishDiagnostics` uses; these tests pin the
//! protocol-boundary slice (request → handler → wire response).

use std::time::Duration;

use lsp_server::{Connection, Message, Notification, Request, RequestId};
use lsp_types::{
    ClientCapabilities, GeneralClientCapabilities, GotoDefinitionParams, GotoDefinitionResponse,
    Hover, HoverContents, HoverParams, InitializeParams, InitializedParams, MarkupKind,
    PartialResultParams, Position, PositionEncodingKind, TextDocumentIdentifier, TextDocumentItem,
    TextDocumentPositionParams, Uri, WorkDoneProgressParams,
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

/// Boot a server over an in-memory connection with UTF-8 negotiated (so LSP characters equal bytes
/// for the ASCII test docs). The test boot uses no `extensionApiPath`, which means the workspace's
/// `NativeDb` is empty and the analyzer is permissive on unknown native names (WP-G's
/// `resolve_extends` fallback) — that's exactly the right shape for testing hover/definition over
/// the analyzer's *own* type table without needing a 10MB JSON fixture.
fn boot() -> (Connection, std::thread::JoinHandle<()>) {
    boot_with_options(None)
}

/// Boot the server with an optional `initializationOptions` payload (used by the docs test to
/// point `extensionApiPath` at a tiny JSON fixture).
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
    let Message::Response(_) = recv(&client) else {
        panic!("expected initialize response");
    };
    client
        .sender
        .send(notification(
            "initialized",
            serde_json::to_value(InitializedParams {}).unwrap(),
        ))
        .unwrap();
    (client, handle)
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
    // Drain the implicit `publishDiagnostics` push that follows didOpen, so subsequent receives
    // line up with our own request responses.
    let _ = recv(client);
}

fn shutdown(client: &Connection, handle: std::thread::JoinHandle<()>) {
    client
        .sender
        .send(request(99, "shutdown", serde_json::Value::Null))
        .unwrap();
    let _ = recv(client);
    client
        .sender
        .send(notification("exit", serde_json::Value::Null))
        .unwrap();
    handle.join().expect("server thread panicked");
}

fn hover_at(client: &Connection, uri: &Uri, position: Position) -> Option<Hover> {
    let params = HoverParams {
        text_document_position_params: TextDocumentPositionParams {
            text_document: TextDocumentIdentifier { uri: uri.clone() },
            position,
        },
        work_done_progress_params: WorkDoneProgressParams::default(),
    };
    client
        .sender
        .send(request(
            10,
            "textDocument/hover",
            serde_json::to_value(params).unwrap(),
        ))
        .unwrap();
    let resp = recv_response(client);
    let value = resp.result.expect("hover result is always present");
    serde_json::from_value(value).expect("valid Option<Hover>")
}

fn definition_at(
    client: &Connection,
    uri: &Uri,
    position: Position,
) -> Option<GotoDefinitionResponse> {
    let params = GotoDefinitionParams {
        text_document_position_params: TextDocumentPositionParams {
            text_document: TextDocumentIdentifier { uri: uri.clone() },
            position,
        },
        work_done_progress_params: WorkDoneProgressParams::default(),
        partial_result_params: PartialResultParams::default(),
    };
    client
        .sender
        .send(request(
            11,
            "textDocument/definition",
            serde_json::to_value(params).unwrap(),
        ))
        .unwrap();
    let resp = recv_response(client);
    let value = resp.result.expect("definition result is always present");
    serde_json::from_value(value).expect("valid Option<GotoDefinitionResponse>")
}

fn hover_markdown(hover: &Hover) -> &str {
    match &hover.contents {
        HoverContents::Markup(m) => {
            assert_eq!(m.kind, MarkupKind::Markdown);
            &m.value
        }
        other => panic!("expected MarkupContent, got {other:?}"),
    }
}

#[test]
fn hover_on_typed_variable_renders_resolved_type() {
    // `var x: int = 0` declares an explicit `int`; hovering on `x` should show that type. The
    // analyzer's type table is what surfaces here — pinned at WP-E's `resolve_assignable`.
    let (client, handle) = boot();
    let uri: Uri = "file:///test/typed.gd".parse().unwrap();
    let src = "var x: int = 0\n";
    did_open(&client, &uri, src);

    // Hover on the `x` identifier (line 0, column 4).
    let hover = hover_at(&client, &uri, Position::new(0, 4)).expect("hover on x");
    let md = hover_markdown(&hover);
    assert!(
        md.contains("int"),
        "hover should render the resolved int type, got {md:?}"
    );
    assert!(
        md.contains("```gdscript"),
        "hover wraps the type in a gdscript code block, got {md:?}"
    );

    shutdown(&client, handle);
}

/// v1.0.2 (issue #26): hover on a DECLARATION NAME renders the member's signature via the same
/// formatter as the call-site hover — previously the typed-ancestor fallback surfaced the
/// enclosing class's `<Script #N>` meta placeholder.
#[test]
fn hover_on_function_declaration_name_renders_signature() {
    let (client, handle) = boot();
    let uri: Uri = "file:///test/decl_sig.gd".parse().unwrap();
    let src = "class_name DeclSig\nextends Node\n\n\nfunc spawn(parent: Node, at: Vector3) -> DeclSig:\n\treturn self\n\n\nstatic func make() -> DeclSig:\n\treturn null\n";
    did_open(&client, &uri, src);

    // Cursor on `spawn` in the declaration (line 4, col 5..10).
    let hover = hover_at(&client, &uri, Position::new(4, 6)).expect("hover on func decl name");
    let md = hover_markdown(&hover);
    assert!(
        md.contains("func spawn(parent: Node, at: Vector3) -> DeclSig"),
        "declaration hover must render the signature, got {md:?}"
    );
    assert!(
        !md.contains("<Script #"),
        "the Display placeholder must never reach hover output, got {md:?}"
    );

    // Cursor on `make` in the static declaration (line 8).
    let hover = hover_at(&client, &uri, Position::new(8, 12)).expect("hover on static func decl");
    let md = hover_markdown(&hover);
    assert!(
        md.contains("static func make() -> DeclSig"),
        "static declarations render the static keyword, got {md:?}"
    );

    shutdown(&client, handle);
}

/// v1.0.2 (issue #26): script-typed values render the script's `class_name` (or file basename),
/// never `<Script #N>` — pinned on a member var whose type is inferred from a self-returning
/// factory.
#[test]
fn hover_on_script_typed_member_renders_class_name() {
    let (client, handle) = boot();
    let uri: Uri = "file:///test/script_typed.gd".parse().unwrap();
    let src = "class_name ScriptTyped\nextends Node\n\nvar made := factory()\n\n\nfunc factory() -> ScriptTyped:\n\treturn self\n";
    did_open(&client, &uri, src);

    // Cursor on `made` (line 3, col 4..8) — an untyped member var with an inferred script type.
    let hover = hover_at(&client, &uri, Position::new(3, 5)).expect("hover on script-typed var");
    let md = hover_markdown(&hover);
    assert!(
        md.contains("ScriptTyped"),
        "script types must render their class_name, got {md:?}"
    );
    assert!(
        !md.contains("<Script #"),
        "the Display placeholder must never reach hover output, got {md:?}"
    );

    shutdown(&client, handle);
}

/// v1.0.2 (issue #26): hover on an inner `class X` declaration name renders `class X extends …`
/// from the AST (inner classes aren't in the `class_name` registry).
#[test]
fn hover_on_inner_class_declaration_renders_class_line() {
    let (client, handle) = boot();
    let uri: Uri = "file:///test/inner_class.gd".parse().unwrap();
    let src = "extends Node\n\nclass Accumulator extends RefCounted:\n\tvar total := 0\n\n\tfunc add(n: int) -> void:\n\t\ttotal += n\n";
    did_open(&client, &uri, src);

    // Cursor on `Accumulator` (line 2, col 6..17).
    let hover = hover_at(&client, &uri, Position::new(2, 8)).expect("hover on inner class name");
    let md = hover_markdown(&hover);
    assert!(
        md.contains("class Accumulator extends RefCounted"),
        "inner-class declaration hover, got {md:?}"
    );

    // And an inner-class MEMBER declaration resolves through the inner interface scope:
    // cursor on `add` (line 5, col 6..9).
    let hover = hover_at(&client, &uri, Position::new(5, 7)).expect("hover on inner member decl");
    let md = hover_markdown(&hover);
    assert!(
        md.contains("func add(n: int) -> void"),
        "inner-class member declaration hover, got {md:?}"
    );

    shutdown(&client, handle);
}

#[test]
fn hover_returns_none_outside_any_node() {
    // A hover request whose position lands past the source end (a whitespace-only file) must
    // resolve to a null wire response, not a panic.
    let (client, handle) = boot();
    let uri: Uri = "file:///test/empty.gd".parse().unwrap();
    did_open(&client, &uri, "\n");

    let hover = hover_at(&client, &uri, Position::new(10, 10));
    assert!(hover.is_none(), "out-of-range hover ⇒ null");

    shutdown(&client, handle);
}

#[test]
fn definition_jumps_to_in_file_variable_declaration() {
    // A program that declares `var speed := 1.0` at line 0 and references `speed` somewhere else.
    // Hovering definition on the reference should jump to the declaration's identifier span.
    let (client, handle) = boot();
    let uri: Uri = "file:///test/def.gd".parse().unwrap();
    let src = concat!(
        "var speed := 1.0\n", // line 0
        "\n",                 // line 1
        "func use():\n",      // line 2
        "\tprint(speed)\n",   // line 3 — the reference to `speed` is at columns 8..13
    );
    did_open(&client, &uri, src);

    // Click on `speed` inside print(...) at line 3 col 10.
    let response = definition_at(&client, &uri, Position::new(3, 10))
        .expect("a `class_name`-style member should resolve");
    let location = match response {
        GotoDefinitionResponse::Scalar(loc) => loc,
        other => panic!("expected scalar Location, got {other:?}"),
    };
    assert_eq!(location.uri, uri);
    // The declaration's identifier `speed` is at line 0, columns 4..9.
    assert_eq!(location.range.start, Position::new(0, 4));
    assert_eq!(location.range.end, Position::new(0, 9));

    shutdown(&client, handle);
}

#[test]
fn definition_jumps_to_in_file_function_declaration() {
    // `func double(x)` declared then called. The definition lookup walks members, finds the
    // function, returns the identifier span of `double`.
    let (client, handle) = boot();
    let uri: Uri = "file:///test/func.gd".parse().unwrap();
    let src = concat!(
        "func double(x):\n",    // line 0 — identifier `double` at columns 5..11
        "\treturn x * 2\n",     // line 1
        "\n",                   // line 2
        "func caller():\n",     // line 3
        "\treturn double(2)\n", // line 4 — reference at col 8..14
    );
    did_open(&client, &uri, src);

    let response = definition_at(&client, &uri, Position::new(4, 10))
        .expect("function reference resolves to its declaration");
    let location = match response {
        GotoDefinitionResponse::Scalar(loc) => loc,
        other => panic!("expected scalar Location, got {other:?}"),
    };
    assert_eq!(location.uri, uri);
    assert_eq!(location.range.start, Position::new(0, 5));
    assert_eq!(location.range.end, Position::new(0, 11));

    shutdown(&client, handle);
}

#[test]
fn definition_on_unknown_name_returns_null() {
    // `extends Node` references `Node`. With no `extension_api.json` loaded the boot has an empty
    // native DB, the analyzer is permissive (WP-G), and the index has no project `class_name`
    // matching "Node". The definition request must resolve to `null` (no jump target), not error.
    let (client, handle) = boot();
    let uri: Uri = "file:///test/unknown.gd".parse().unwrap();
    did_open(&client, &uri, "extends Node\n");

    // `Node` is at line 0, columns 8..12.
    let response = definition_at(&client, &uri, Position::new(0, 10));
    assert!(
        response.is_none(),
        "unknown native + no class_name ⇒ null, got {response:?}"
    );

    shutdown(&client, handle);
}

#[test]
fn hover_includes_native_doc_text_when_dump_carries_it() {
    // Materialize a minimal `extension_api.json` fixture next to a unique temp dir, with one class
    // (`Widget`) that ships a `brief_description` + `description`. Boot the server pointed at it,
    // then hover on `extends Widget`'s `Widget` identifier — the markdown must include both the
    // type code block (the bare class name, since the analyzer pinned the type on the surrounding
    // class header) and the brief/long descriptions from the dump.
    let fixture_dir = std::env::temp_dir().join("gdls_wph_docs");
    std::fs::create_dir_all(&fixture_dir).expect("create temp fixture dir");
    let api_path = fixture_dir.join("extension_api.json");
    let api_json = r#"{
        "header": { "version_major": 4, "version_minor": 6, "version_patch": 3 },
        "classes": [
            {
                "name": "Widget",
                "brief_description": "A clickable UI thing.",
                "description": "Widgets are the primary interactive surface of the editor."
            }
        ]
    }"#;
    std::fs::write(&api_path, api_json).expect("write fixture JSON");

    let init_options = serde_json::json!({
        "extensionApiPath": api_path.to_string_lossy().as_ref(),
    });
    let (client, handle) = boot_with_options(Some(init_options));
    let uri: Uri = "file:///test/widget.gd".parse().unwrap();
    did_open(&client, &uri, "extends Widget\n");

    // `Widget` identifier is at line 0, columns 8..14. Click anywhere inside it.
    let hover = hover_at(&client, &uri, Position::new(0, 11)).expect("hover on Widget");
    let md = hover_markdown(&hover);

    assert!(
        md.contains("Widget"),
        "hover renders the class name, got {md:?}"
    );
    assert!(
        md.contains("A clickable UI thing."),
        "hover includes the dump's brief_description, got {md:?}"
    );
    assert!(
        md.contains("primary interactive surface"),
        "hover includes the dump's long description, got {md:?}"
    );

    shutdown(&client, handle);
}

#[test]
fn definition_on_a_literal_returns_null() {
    // The innermost node at a literal position is the literal itself, not an identifier. Per the
    // handler's contract (`NodeKind::Identifier` gate), non-identifier positions resolve to null.
    let (client, handle) = boot();
    let uri: Uri = "file:///test/literal.gd".parse().unwrap();
    did_open(&client, &uri, "var x := 42\n");

    let response = definition_at(&client, &uri, Position::new(0, 10));
    assert!(response.is_none(), "literal position ⇒ null");

    shutdown(&client, handle);
}

#[test]
fn definition_jumps_across_files_via_class_name() {
    // Cross-file `class_name` jump: `b.gd` extends a class declared in `a.gd` via `class_name`.
    // Hover-definition on `FooHero` inside `b.gd`'s `extends` must return a Location whose URI
    // points at `a.gd`'s class header. Pinned because the find_global_class_definition path
    // (handlers.rs) was previously untested at the wire — index resolution was covered, but the
    // open-file-via-parse-cache + closed-file-via-fs branches weren't.
    let fixture_dir = std::env::temp_dir().join("gdls_def_xfile");
    let _ = std::fs::remove_dir_all(&fixture_dir);
    std::fs::create_dir_all(&fixture_dir).expect("create fixture dir");
    // Empty project.godot anchors `res://`; the index walks the directory regardless.
    std::fs::write(fixture_dir.join("project.godot"), "").expect("write project.godot");
    let a_path = fixture_dir.join("a.gd");
    let b_path = fixture_dir.join("b.gd");
    // `a.gd` declares the class; the identifier `FooHero` is at line 0, columns 11..18.
    std::fs::write(&a_path, "class_name FooHero\n").expect("write a.gd");
    std::fs::write(&b_path, "extends FooHero\n").expect("write b.gd");

    let init_options = serde_json::json!({
        "projectRoot": fixture_dir.to_string_lossy().as_ref(),
    "autoDumpExtensionApi": false,
    });
    let (client, handle) = boot_with_options(Some(init_options));

    // Open b.gd and request definition on the `FooHero` reference at line 0, column 10.
    let b_uri: Uri = format!("file:///{}", b_path.to_string_lossy().replace('\\', "/"))
        .parse()
        .unwrap();
    did_open(&client, &b_uri, "extends FooHero\n");

    let response = definition_at(&client, &b_uri, Position::new(0, 10))
        .expect("cross-file class_name should resolve");
    let location = match response {
        GotoDefinitionResponse::Scalar(loc) => loc,
        other => panic!("expected scalar Location, got {other:?}"),
    };
    // The URI should point at a.gd (closed; we read it from disk through the fallback path).
    let loc_str = location.uri.as_str();
    assert!(
        loc_str.ends_with("/a.gd"),
        "definition should jump to a.gd, got {loc_str}"
    );
    // The identifier `FooHero` in `class_name FooHero` is at columns 11..18 on line 0.
    assert_eq!(location.range.start, Position::new(0, 11));
    assert_eq!(location.range.end, Position::new(0, 18));

    shutdown(&client, handle);
    let _ = std::fs::remove_dir_all(&fixture_dir);
}

/// Fix 1: hover on a cross-file method call with parameters shows param NAMES in the signature.
/// `func helper(amount: int, who: String) -> int` must render with `amount` and `who`, not just
/// `int, String`.
#[test]
fn hover_cross_file_method_shows_param_names() {
    let fixture_dir = std::env::temp_dir().join("gdls_hover_param_names");
    let _ = std::fs::remove_dir_all(&fixture_dir);
    std::fs::create_dir_all(&fixture_dir).expect("create fixture dir");
    std::fs::write(fixture_dir.join("project.godot"), "").expect("write project.godot");
    // lib.gd: class_name Lib, func helper(amount: int, who: String) -> int
    std::fs::write(
        fixture_dir.join("lib.gd"),
        "class_name Lib\nextends Node\n\nfunc helper(amount: int, who: String) -> int:\n\treturn amount\n",
    )
    .expect("write lib.gd");
    // caller.gd: calls l.helper(5, \"Bob\")
    // Line 0: `extends Node`
    // Line 2: `func test(l: Lib):`
    // Line 3: `\tvar x = l.helper(5, \"Bob\")`  — `helper` at col 12..18
    let caller_src = "extends Node\n\nfunc test(l: Lib):\n\tvar x = l.helper(5, \"Bob\")\n";
    std::fs::write(fixture_dir.join("caller.gd"), caller_src).expect("write caller.gd");

    let init_options = serde_json::json!({
        "projectRoot": fixture_dir.to_string_lossy().as_ref(),
    "autoDumpExtensionApi": false,
    });
    let (client, handle) = boot_with_options(Some(init_options));

    let caller_path = fixture_dir.join("caller.gd");
    let caller_uri: Uri = format!(
        "file:///{}",
        caller_path.to_string_lossy().replace('\\', "/")
    )
    .parse()
    .unwrap();
    did_open(&client, &caller_uri, caller_src);

    // Hover on `helper` at line 3, col 13.
    let hover = hover_at(&client, &caller_uri, Position::new(3, 13))
        .expect("hover on method call should return something");
    let md = hover_markdown(&hover);
    assert!(
        md.contains("amount"),
        "hover signature must include param name 'amount', got {md:?}"
    );
    assert!(
        md.contains("who"),
        "hover signature must include param name 'who', got {md:?}"
    );
    assert!(
        md.contains("amount: int"),
        "hover signature must render 'amount: int', got {md:?}"
    );
    assert!(
        md.contains("who: String"),
        "hover signature must render 'who: String', got {md:?}"
    );
    assert!(
        md.contains("func helper("),
        "hover signature must render the `func helper(` prefix, got {md:?}"
    );
    assert!(
        md.contains("-> int"),
        "hover signature must render the `-> int` return type, got {md:?}"
    );

    shutdown(&client, handle);
    let _ = std::fs::remove_dir_all(&fixture_dir);
}

/// Regression for the M6-F hover-scope gate: in `l.helper(5, "Bob")`, hovering the *base receiver*
/// `l` must fall through to the ordinary type-label hover, NOT surface `helper`'s signature. Before
/// the gate, `hover_member_signature` matched the whole enclosing Call span, so the base (and any
/// argument) wrongly rendered `func helper(...)` instead of its own type.
#[test]
fn hover_on_call_base_does_not_show_callee_signature() {
    let fixture_dir = std::env::temp_dir().join("gdls_hover_base_not_sig");
    let _ = std::fs::remove_dir_all(&fixture_dir);
    std::fs::create_dir_all(&fixture_dir).expect("create fixture dir");
    std::fs::write(fixture_dir.join("project.godot"), "").expect("write project.godot");
    std::fs::write(
        fixture_dir.join("lib.gd"),
        "class_name Lib\nextends Node\n\nfunc helper(amount: int, who: String) -> int:\n\treturn amount\n",
    )
    .expect("write lib.gd");
    // Line 3: `\tvar x = l.helper(5, "Bob")` — base `l` at col 9, `helper` at col 11..17.
    let caller_src = "extends Node\n\nfunc test(l: Lib):\n\tvar x = l.helper(5, \"Bob\")\n";
    std::fs::write(fixture_dir.join("caller.gd"), caller_src).expect("write caller.gd");

    let init_options = serde_json::json!({
        "projectRoot": fixture_dir.to_string_lossy().as_ref(),
    "autoDumpExtensionApi": false,
    });
    let (client, handle) = boot_with_options(Some(init_options));

    let caller_path = fixture_dir.join("caller.gd");
    let caller_uri: Uri = format!(
        "file:///{}",
        caller_path.to_string_lossy().replace('\\', "/")
    )
    .parse()
    .unwrap();
    did_open(&client, &caller_uri, caller_src);

    // Hover on the base receiver `l` at line 3, col 9 — must show its own type, never the callee sig.
    let base_hover = hover_at(&client, &caller_uri, Position::new(3, 9))
        .expect("hover on the typed base receiver should return its type label");
    assert!(
        !hover_markdown(&base_hover).contains("func helper("),
        "hover on base receiver `l` must not surface the callee signature, got {:?}",
        hover_markdown(&base_hover)
    );

    // Sanity: hovering the callee `helper` (col 13) DOES still render the signature, proving the
    // gate narrowed scope rather than disabling the M6-F feature.
    let callee_hover = hover_at(&client, &caller_uri, Position::new(3, 13))
        .expect("hover on the callee must still return the signature");
    assert!(
        hover_markdown(&callee_hover).contains("func helper("),
        "hover on callee `helper` must still render the signature, got {:?}",
        hover_markdown(&callee_hover)
    );

    shutdown(&client, handle);
    let _ = std::fs::remove_dir_all(&fixture_dir);
}

/// Hover on a `preload("res://foo.gd")` string literal shows the resolved script's basename.
#[test]
fn hover_on_preload_string_shows_resolved_script() {
    let fixture_dir = std::env::temp_dir().join("gdls_hover_preload");
    let _ = std::fs::remove_dir_all(&fixture_dir);
    std::fs::create_dir_all(&fixture_dir).expect("create fixture dir");
    std::fs::write(fixture_dir.join("project.godot"), "").expect("write project.godot");
    std::fs::write(fixture_dir.join("lib.gd"), "extends Node\n").expect("write lib.gd");
    // caller.gd has: const Lib = preload("res://lib.gd")
    // The string literal starts at col 20.
    let caller_src = "const Lib = preload(\"res://lib.gd\")\n";
    std::fs::write(fixture_dir.join("caller.gd"), caller_src).expect("write caller.gd");

    let init_options = serde_json::json!({
        "projectRoot": fixture_dir.to_string_lossy().as_ref(),
    "autoDumpExtensionApi": false,
    });
    let (client, handle) = boot_with_options(Some(init_options));

    let caller_path = fixture_dir.join("caller.gd");
    let caller_uri: Uri = format!(
        "file:///{}",
        caller_path.to_string_lossy().replace('\\', "/")
    )
    .parse()
    .unwrap();
    did_open(&client, &caller_uri, caller_src);

    // Hover inside "res://lib.gd" at line 0, col 25.
    let hover = hover_at(&client, &caller_uri, Position::new(0, 25))
        .expect("hover on preload string must return something");
    let md = hover_markdown(&hover);
    assert!(
        md.contains("lib.gd"),
        "hover on preload string must show the resolved filename 'lib.gd', got {md:?}"
    );

    shutdown(&client, handle);
    let _ = std::fs::remove_dir_all(&fixture_dir);
}

/// Hover on a `preload("res://…")` whose target is a real **non-GDScript** resource (`.tscn`/
/// `.tres`/asset) shows that file's basename, labelled "Resource" (not "GDScript"). The index holds
/// only `.gd`, so this resolves via the on-disk fallback — matching what `document_link` links.
#[test]
fn hover_on_preload_non_gd_resource_shows_basename() {
    let fixture_dir = std::env::temp_dir().join("gdls_hover_preload_tscn");
    let _ = std::fs::remove_dir_all(&fixture_dir);
    std::fs::create_dir_all(fixture_dir.join("scenes")).expect("create fixture dir");
    std::fs::write(fixture_dir.join("project.godot"), "").expect("write project.godot");
    // A real scene file on disk — NOT a `.gd`, so it never enters the index.
    std::fs::write(fixture_dir.join("scenes/main.tscn"), "[gd_scene]\n").expect("write main.tscn");
    // caller.gd has: const Main = preload("res://scenes/main.tscn")
    let caller_src = "const Main = preload(\"res://scenes/main.tscn\")\n";
    std::fs::write(fixture_dir.join("caller.gd"), caller_src).expect("write caller.gd");

    let init_options = serde_json::json!({
        "projectRoot": fixture_dir.to_string_lossy().as_ref(),
    "autoDumpExtensionApi": false,
    });
    let (client, handle) = boot_with_options(Some(init_options));

    let caller_path = fixture_dir.join("caller.gd");
    let caller_uri: Uri = format!(
        "file:///{}",
        caller_path.to_string_lossy().replace('\\', "/")
    )
    .parse()
    .unwrap();
    did_open(&client, &caller_uri, caller_src);

    // Hover inside the `"res://scenes/main.tscn"` literal (col 30 lands in the path).
    let hover = hover_at(&client, &caller_uri, Position::new(0, 30))
        .expect("hover on a preload of an on-disk .tscn must return something");
    let md = hover_markdown(&hover);
    assert!(
        md.contains("main.tscn"),
        "hover must show the resolved resource basename 'main.tscn', got {md:?}"
    );
    assert!(
        md.contains("Resource") && !md.contains("GDScript"),
        "a non-.gd resource hover must be labelled 'Resource', not 'GDScript', got {md:?}"
    );

    shutdown(&client, handle);
    let _ = std::fs::remove_dir_all(&fixture_dir);
}

/// M6-F: hover on a cross-file method call with no parameters shows the function signature from
/// the callee's interface (`func helper() -> int`). Param-name rendering is tested in
/// `hover_cross_file_method_shows_param_names`; preload-string hover in
/// `hover_on_preload_string_shows_resolved_script`.
#[test]
fn hover_cross_file_method_shows_signature() {
    let fixture_dir = std::env::temp_dir().join("gdls_hover_m6f");
    let _ = std::fs::remove_dir_all(&fixture_dir);
    std::fs::create_dir_all(&fixture_dir).expect("create fixture dir");
    std::fs::write(fixture_dir.join("project.godot"), "").expect("write project.godot");
    // lib.gd: class_name Lib, func helper() -> int
    std::fs::write(
        fixture_dir.join("lib.gd"),
        "class_name Lib\nextends Node\n\nfunc helper() -> int:\n\treturn 1\n",
    )
    .expect("write lib.gd");
    // caller.gd: calls l.helper()
    // Line 0: `extends Node`
    // Line 2: `func test(l: Lib):`
    // Line 3: `\tvar x = l.helper()`  — `helper` at col 12..18
    let caller_src = "extends Node\n\nfunc test(l: Lib):\n\tvar x = l.helper()\n";
    std::fs::write(fixture_dir.join("caller.gd"), caller_src).expect("write caller.gd");

    let init_options = serde_json::json!({
        "projectRoot": fixture_dir.to_string_lossy().as_ref(),
    "autoDumpExtensionApi": false,
    });
    let (client, handle) = boot_with_options(Some(init_options));

    let caller_path = fixture_dir.join("caller.gd");
    let caller_uri: Uri = format!(
        "file:///{}",
        caller_path.to_string_lossy().replace('\\', "/")
    )
    .parse()
    .unwrap();
    did_open(&client, &caller_uri, caller_src);

    // Hover on `helper` at line 3, col 13 (inside the identifier).
    let hover = hover_at(&client, &caller_uri, Position::new(3, 13))
        .expect("hover on method call should return something");
    let md = hover_markdown(&hover);
    assert!(
        md.contains("helper"),
        "hover on cross-file method call must show 'helper' in the signature, got {md:?}"
    );
    assert!(
        md.contains("func"),
        "hover on cross-file method call must show 'func' keyword, got {md:?}"
    );

    shutdown(&client, handle);
    let _ = std::fs::remove_dir_all(&fixture_dir);
}

/// M6-F regression (greptile r3372647839): with a nested call `a.outer(b.inner())`, hovering the
/// *inner* callee `inner` must render `inner`'s signature, not the enclosing `outer`'s. DFS
/// pre-order visits the outer Call first, so the handler must pick the smallest-span (innermost)
/// enclosing Call, not the first match.
#[test]
fn hover_nested_call_picks_innermost_callee() {
    let fixture_dir = std::env::temp_dir().join("gdls_hover_m6f_nested");
    let _ = std::fs::remove_dir_all(&fixture_dir);
    std::fs::create_dir_all(&fixture_dir).expect("create fixture dir");
    std::fs::write(fixture_dir.join("project.godot"), "").expect("write project.godot");
    std::fs::write(
        fixture_dir.join("lib.gd"),
        "class_name Lib\nextends Node\n\nfunc outer(n: int) -> int:\n\treturn n\n\nfunc inner() -> int:\n\treturn 1\n",
    )
    .expect("write lib.gd");
    // Line 3: `\tvar x = a.outer(b.inner())` — `inner` at cols 19..24, nested inside `a.outer(…)`.
    let caller_src = "extends Node\n\nfunc test(a: Lib, b: Lib):\n\tvar x = a.outer(b.inner())\n";
    std::fs::write(fixture_dir.join("caller.gd"), caller_src).expect("write caller.gd");

    let init_options = serde_json::json!({
        "projectRoot": fixture_dir.to_string_lossy().as_ref(),
    "autoDumpExtensionApi": false,
    });
    let (client, handle) = boot_with_options(Some(init_options));

    let caller_path = fixture_dir.join("caller.gd");
    let caller_uri: Uri = format!(
        "file:///{}",
        caller_path.to_string_lossy().replace('\\', "/")
    )
    .parse()
    .unwrap();
    did_open(&client, &caller_uri, caller_src);

    // Hover on `inner` at line 3, col 21 (inside the inner callee identifier).
    let hover = hover_at(&client, &caller_uri, Position::new(3, 21))
        .expect("hover on nested inner call should return something");
    let md = hover_markdown(&hover);
    assert!(
        md.contains("inner"),
        "hover on the inner callee must show 'inner', got {md:?}"
    );
    assert!(
        !md.contains("outer"),
        "hover on the inner callee must NOT show the enclosing 'outer' signature, got {md:?}"
    );

    shutdown(&client, handle);
    let _ = std::fs::remove_dir_all(&fixture_dir);
}

/// M6-B: definition on a `class_name`-registered identifier used in expression position
/// (`Foo.bar()` or `Foo.CONST`). When the cursor is on `Foo` (the base of an attribute-access
/// subscript), go-to-definition must jump to `foo.gd`'s `class_name Foo` declaration.
#[test]
fn definition_on_class_name_in_expression_position() {
    let fixture_dir = std::env::temp_dir().join("gdls_def_m6b");
    let _ = std::fs::remove_dir_all(&fixture_dir);
    std::fs::create_dir_all(&fixture_dir).expect("create fixture dir");
    std::fs::write(fixture_dir.join("project.godot"), "").expect("write project.godot");
    // foo.gd declares the class; `class_name Foo` at line 0, cols 11..14.
    std::fs::write(fixture_dir.join("foo.gd"), "class_name Foo\n").expect("write foo.gd");
    // caller.gd references Foo in expression position: `Foo.new()`.
    // Line 0: `extends Node`
    // Line 2: `func test():`
    // Line 3: `\tvar x = Foo.new()` — `Foo` at col 9..12
    let caller_src = "extends Node\n\nfunc test():\n\tvar x = Foo.new()\n";
    std::fs::write(fixture_dir.join("caller.gd"), caller_src).expect("write caller.gd");

    let init_options = serde_json::json!({
        "projectRoot": fixture_dir.to_string_lossy().as_ref(),
    "autoDumpExtensionApi": false,
    });
    let (client, handle) = boot_with_options(Some(init_options));

    let caller_path = fixture_dir.join("caller.gd");
    let caller_uri: Uri = format!(
        "file:///{}",
        caller_path.to_string_lossy().replace('\\', "/")
    )
    .parse()
    .unwrap();
    did_open(&client, &caller_uri, caller_src);

    // Click on `Foo` at line 3, col 10 (inside `Foo.new()`).
    let response = definition_at(&client, &caller_uri, Position::new(3, 10))
        .expect("class_name in expression position must resolve");
    let location = match response {
        GotoDefinitionResponse::Scalar(loc) => loc,
        other => panic!("expected scalar Location, got {other:?}"),
    };
    let loc_str = location.uri.as_str();
    assert!(
        loc_str.ends_with("/foo.gd"),
        "definition should jump to foo.gd, got {loc_str}"
    );
    // `class_name Foo` — identifier `Foo` at cols 11..14.
    assert_eq!(location.range.start, Position::new(0, 11));
    assert_eq!(location.range.end, Position::new(0, 14));

    shutdown(&client, handle);
    let _ = std::fs::remove_dir_all(&fixture_dir);
}

/// M6-C1: definition on a preload/load path string literal jumps to the target file.
#[test]
fn definition_on_preload_path_string_jumps_to_file() {
    let fixture_dir = std::env::temp_dir().join("gdls_def_m6c1");
    let _ = std::fs::remove_dir_all(&fixture_dir);
    std::fs::create_dir_all(&fixture_dir).expect("create fixture dir");
    std::fs::write(fixture_dir.join("project.godot"), "").expect("write project.godot");
    std::fs::write(fixture_dir.join("foo.gd"), "extends Node\n").expect("write foo.gd");
    // caller.gd has: const Foo = preload("res://foo.gd")
    // line 0: `const Foo = preload("res://foo.gd")`
    // The string literal `"res://foo.gd"` starts at col 20.
    let caller_src = "const Foo = preload(\"res://foo.gd\")\n";
    std::fs::write(fixture_dir.join("caller.gd"), caller_src).expect("write caller.gd");

    let init_options = serde_json::json!({
        "projectRoot": fixture_dir.to_string_lossy().as_ref(),
    "autoDumpExtensionApi": false,
    });
    let (client, handle) = boot_with_options(Some(init_options));

    let caller_path = fixture_dir.join("caller.gd");
    let caller_uri: Uri = format!(
        "file:///{}",
        caller_path.to_string_lossy().replace('\\', "/")
    )
    .parse()
    .unwrap();
    did_open(&client, &caller_uri, caller_src);

    // Click inside the string literal "res://foo.gd" at line 0, col 25.
    let response = definition_at(&client, &caller_uri, Position::new(0, 25))
        .expect("preload string must resolve to the target file");
    let location = match response {
        GotoDefinitionResponse::Scalar(loc) => loc,
        other => panic!("expected scalar Location, got {other:?}"),
    };
    let loc_str = location.uri.as_str();
    assert!(
        loc_str.ends_with("/foo.gd"),
        "definition on preload path should jump to foo.gd, got {loc_str}"
    );
    // Location points at the beginning of foo.gd (line 0, col 0).
    assert_eq!(location.range.start, Position::new(0, 0));

    shutdown(&client, handle);
    let _ = std::fs::remove_dir_all(&fixture_dir);
}

/// M6-D: definition on an autoload name jumps to the autoload script (last-fallback branch).
/// Also tests shadowing: a local var `Save` shadows the autoload named `Save`; definition on
/// the local ref must resolve to the in-file declaration, not the autoload.
#[test]
fn definition_on_autoload_jumps_to_script() {
    let fixture_dir = std::env::temp_dir().join("gdls_def_m6d");
    let _ = std::fs::remove_dir_all(&fixture_dir);
    std::fs::create_dir_all(&fixture_dir).expect("create fixture dir");
    std::fs::write(fixture_dir.join("save.gd"), "extends Node\n").expect("write save.gd");
    // project.godot declares `Save` as an autoload pointing at res://save.gd.
    let project_godot = "[application]\nconfig/name=\"Test\"\nconfig_version=5\n\n[autoload]\nSave=\"*res://save.gd\"\n";
    std::fs::write(fixture_dir.join("project.godot"), project_godot).expect("write project.godot");
    // user.gd references the autoload `Save` in expression position.
    // Line 0: `extends Node`
    // Line 2: `func test():`
    // Line 3: `\tSave.do_thing()`  — `Save` at col 1..5
    let user_src = "extends Node\n\nfunc test():\n\tSave.do_thing()\n";
    std::fs::write(fixture_dir.join("user.gd"), user_src).expect("write user.gd");

    let init_options = serde_json::json!({
        "projectRoot": fixture_dir.to_string_lossy().as_ref(),
    "autoDumpExtensionApi": false,
    });
    let (client, handle) = boot_with_options(Some(init_options));

    let user_path = fixture_dir.join("user.gd");
    let user_uri: Uri = format!("file:///{}", user_path.to_string_lossy().replace('\\', "/"))
        .parse()
        .unwrap();
    did_open(&client, &user_uri, user_src);

    // Click on `Save` at line 3, col 2 — should jump to save.gd.
    let response = definition_at(&client, &user_uri, Position::new(3, 2))
        .expect("autoload name must resolve to its script");
    let location = match response {
        GotoDefinitionResponse::Scalar(loc) => loc,
        other => panic!("expected scalar Location, got {other:?}"),
    };
    let loc_str = location.uri.as_str();
    assert!(
        loc_str.ends_with("/save.gd"),
        "definition on autoload should jump to save.gd, got {loc_str}"
    );

    shutdown(&client, handle);
    let _ = std::fs::remove_dir_all(&fixture_dir);
}

/// M6-D negative: when project.godot declares an autoload pointing at `res://save.gd` but
/// `save.gd` is NOT present on disk (not indexed), `go-to-definition` on the autoload name must
/// return no Location — not crash, not emit a dangling URI. The existence-gate in
/// `find_autoload_definition` (`resolve_res_path` returning None for unindexed files) covers this.
#[test]
fn definition_autoload_missing_script_returns_none() {
    let fixture_dir = std::env::temp_dir().join("gdls_def_m6d_missing");
    let _ = std::fs::remove_dir_all(&fixture_dir);
    std::fs::create_dir_all(&fixture_dir).expect("create fixture dir");
    // Declare `Save` autoload pointing at res://save.gd — but do NOT write save.gd to disk.
    let project_godot = "[application]\nconfig/name=\"Test\"\nconfig_version=5\n\n[autoload]\nSave=\"*res://save.gd\"\n";
    std::fs::write(fixture_dir.join("project.godot"), project_godot).expect("write project.godot");
    // user.gd references Save in expression position.
    // Line 0: `extends Node`
    // Line 2: `func test():`
    // Line 3: `\tSave.do_thing()`  — `Save` at col 1..5
    let user_src = "extends Node\n\nfunc test():\n\tSave.do_thing()\n";
    std::fs::write(fixture_dir.join("user.gd"), user_src).expect("write user.gd");

    let init_options = serde_json::json!({
        "projectRoot": fixture_dir.to_string_lossy().as_ref(),
    "autoDumpExtensionApi": false,
    });
    let (client, handle) = boot_with_options(Some(init_options));

    let user_path = fixture_dir.join("user.gd");
    let user_uri: Uri = format!("file:///{}", user_path.to_string_lossy().replace('\\', "/"))
        .parse()
        .unwrap();
    did_open(&client, &user_uri, user_src);

    // Click on `Save` at line 3, col 2 — autoload exists in project.godot but script is absent.
    // Must return None (null wire response), not a dangling file:// URI.
    let response = definition_at(&client, &user_uri, Position::new(3, 2));
    assert!(
        response.is_none(),
        "definition on autoload with missing script must return None; got {response:?}"
    );

    shutdown(&client, handle);
    let _ = std::fs::remove_dir_all(&fixture_dir);
}

/// M6-D shadowing: a local var named `Save` takes priority over the autoload named `Save`.
/// Definition on the local reference must stay in-file, not jump to the autoload script.
#[test]
fn definition_autoload_shadowed_by_local_stays_in_file() {
    let fixture_dir = std::env::temp_dir().join("gdls_def_m6d_shadow");
    let _ = std::fs::remove_dir_all(&fixture_dir);
    std::fs::create_dir_all(&fixture_dir).expect("create fixture dir");
    std::fs::write(fixture_dir.join("save.gd"), "extends Node\n").expect("write save.gd");
    let project_godot = "[application]\nconfig/name=\"Test\"\nconfig_version=5\n\n[autoload]\nSave=\"*res://save.gd\"\n";
    std::fs::write(fixture_dir.join("project.godot"), project_godot).expect("write project.godot");
    // shadow.gd has a member `var Save := 1` and a func that references it.
    // Line 0: `extends Node`
    // Line 1: `var Save := 1`   — `Save` (decl) at col 4..8
    // Line 3: `func test():`
    // Line 4: `\tprint(Save)`   — `Save` at col 8..12
    let shadow_src = "extends Node\nvar Save := 1\n\nfunc test():\n\tprint(Save)\n";
    std::fs::write(fixture_dir.join("shadow.gd"), shadow_src).expect("write shadow.gd");

    let init_options = serde_json::json!({
        "projectRoot": fixture_dir.to_string_lossy().as_ref(),
    "autoDumpExtensionApi": false,
    });
    let (client, handle) = boot_with_options(Some(init_options));

    let shadow_path = fixture_dir.join("shadow.gd");
    let shadow_uri: Uri = format!(
        "file:///{}",
        shadow_path.to_string_lossy().replace('\\', "/")
    )
    .parse()
    .unwrap();
    did_open(&client, &shadow_uri, shadow_src);

    // Click on `Save` at line 4 col 9 — the `print(Save)` reference.
    let response = definition_at(&client, &shadow_uri, Position::new(4, 9))
        .expect("shadowed name resolves to in-file decl");
    let location = match response {
        GotoDefinitionResponse::Scalar(loc) => loc,
        other => panic!("expected scalar Location, got {other:?}"),
    };
    // Must stay in shadow.gd, pointing at the `var Save` declaration (col 4..8 on line 1).
    assert_eq!(
        location.uri, shadow_uri,
        "shadowed name must resolve in-file"
    );
    assert_eq!(location.range.start, Position::new(1, 4));

    shutdown(&client, handle);
    let _ = std::fs::remove_dir_all(&fixture_dir);
}

#[test]
fn did_save_triggers_a_diagnostic_republish() {
    // `didSave` should re-publish diagnostics for the open buffer (same content, idempotent),
    // not silently drop the request. Pinned because the path was untested — a regression that
    // skipped the publish would only show as user-facing "diagnostics went stale after save."
    let (client, handle) = boot();
    let uri: Uri = "file:///test/save.gd".parse().unwrap();
    did_open(&client, &uri, "var x: int = 0\n");

    // Send didSave; the server should push a publishDiagnostics in response (per the
    // dispatch_notification arm in server.rs).
    client
        .sender
        .send(notification(
            "textDocument/didSave",
            serde_json::to_value(lsp_types::DidSaveTextDocumentParams {
                text_document: TextDocumentIdentifier { uri: uri.clone() },
                text: None,
            })
            .unwrap(),
        ))
        .unwrap();
    // The publish should arrive within the recv timeout.
    let msg = recv(&client);
    let Message::Notification(note) = msg else {
        panic!("expected publishDiagnostics notification, got {msg:?}");
    };
    assert_eq!(note.method, "textDocument/publishDiagnostics");

    shutdown(&client, handle);
}
