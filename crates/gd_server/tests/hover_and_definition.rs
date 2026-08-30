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
            // These tests assert hover CONTENT, so they ask for markdown the way every real
            // editor profile does. Without the request the server answers plaintext (#261) —
            // the correct floor for a client that declared nothing, but not what is under test.
            text_document: Some(lsp_types::TextDocumentClientCapabilities {
                hover: Some(lsp_types::HoverClientCapabilities {
                    content_format: Some(vec![MarkupKind::Markdown]),
                    ..Default::default()
                }),
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

/// #411: a member named bare in value position renders the same declaration card the `self.`
/// spelling has rendered since #258 — the doc a user wrote must be visible in the position their
/// code actually uses.
#[test]
fn hover_on_a_bare_member_renders_its_declaration_and_doc() {
    let (client, handle) = boot();
    let uri: Uri = "file:///test/bare.gd".parse().unwrap();
    //  0: extends Node
    //  1:
    //  2: ## The speed doc.
    //  3: var speed: int = 3
    //  4:
    //  5: ## The cap doc.
    //  6: const CAP := 9
    //  7:
    //  8: ## The sig doc.
    //  9: signal pinged(x: int)
    // 10:
    // 11: ## The mode doc.
    // 12: enum Mode { A, B }
    // 13:
    // 14: func _ready() -> void:
    // 15: \tprint(speed)
    // 16: \tprint(CAP)
    // 17: \tprint(pinged)
    // 18: \tprint(Mode)
    did_open(
        &client,
        &uri,
        concat!(
            "extends Node\n\n",
            "## The speed doc.\nvar speed: int = 3\n\n",
            "## The cap doc.\nconst CAP := 9\n\n",
            "## The sig doc.\nsignal pinged(x: int)\n\n",
            "## The mode doc.\nenum Mode { A, B }\n\n",
            "func _ready() -> void:\n",
            "\tprint(speed)\n\tprint(CAP)\n\tprint(pinged)\n\tprint(Mode)\n",
        ),
    );

    for (line, sig, doc) in [
        (15, "var speed: int", "The speed doc."),
        (16, "const CAP: int", "The cap doc."),
        (17, "signal pinged(x: int)", "The sig doc."),
        (18, "enum Mode", "The mode doc."),
    ] {
        let hover = hover_at(&client, &uri, Position::new(line, 8))
            .unwrap_or_else(|| panic!("expected a hover at line {line}"));
        let md = hover_markdown(&hover);
        assert!(md.contains(sig), "line {line}: wanted {sig:?}, got {md:?}");
        assert!(md.contains(doc), "line {line}: wanted {doc:?}, got {md:?}");
    }

    shutdown(&client, handle);
}

/// A local, a parameter, and a `for` variable have no declaration card and no doc, so they keep
/// the plain type label — including when one shadows a member of the same name.
#[test]
fn hover_on_a_local_keeps_the_plain_type_label() {
    let (client, handle) = boot();
    let uri: Uri = "file:///test/bareloc.gd".parse().unwrap();
    //  0: extends Node
    //  1:
    //  2: ## The speed doc.
    //  3: var speed: int = 3
    //  4:
    //  5: func take(amount: int) -> void:
    //  6: \tvar speed = "shadow"
    //  7: \tprint(speed)
    //  8: \tprint(amount)
    did_open(
        &client,
        &uri,
        concat!(
            "extends Node\n\n",
            "## The speed doc.\nvar speed: int = 3\n\n",
            "func take(amount: int) -> void:\n",
            "\tvar speed = \"shadow\"\n\tprint(speed)\n\tprint(amount)\n",
        ),
    );

    let shadowed = hover_at(&client, &uri, Position::new(7, 8)).expect("hover on the shadow");
    let md = hover_markdown(&shadowed);
    assert!(md.contains("String"), "got {md:?}");
    assert!(!md.contains("The speed doc."), "got {md:?}");

    let param = hover_at(&client, &uri, Position::new(8, 8)).expect("hover on the parameter");
    assert!(hover_markdown(&param).contains("int"));

    shutdown(&client, handle);
}

/// #412: the type-label fallback answers "what type is the node around here". Around a tab, an
/// operator, a delimiter, or a structural keyword that node is the enclosing statement, so hovering
/// blank space used to pop a card naming the statement's type — most often `Nil`.
#[test]
fn hover_returns_none_on_a_position_that_holds_no_symbol() {
    let (client, handle) = boot();
    let uri: Uri = "file:///test/nosym.gd".parse().unwrap();
    // 0: extends Node
    // 1:
    // 2: func go() -> void:
    // 3: \tpass
    // 4:
    // 5: func _ready() -> void:
    // 6: \tvar a = 1 + 2
    // 7: \tgo()
    did_open(
        &client,
        &uri,
        "extends Node\n\nfunc go() -> void:\n\tpass\n\nfunc _ready() -> void:\n\tvar a = 1 + 2\n\tgo()\n",
    );

    for (line, character, what) in [
        (1, 0, "an empty line"),
        (3, 0, "the indent before a statement"),
        (6, 0, "the indent before a declaration"),
        (6, 7, "the `=` of an assignment"),
        (6, 11, "the `+` of a binary op"),
        (7, 4, "the `)` of a void call"),
        (2, 1, "the `func` keyword"),
        (0, 2, "the `extends` keyword"),
    ] {
        assert!(
            hover_at(&client, &uri, Position::new(line, character)).is_none(),
            "hover on {what} must be null"
        );
    }

    shutdown(&client, handle);
}

/// The other half of the same gate: every position that DOES hold a symbol still answers.
#[test]
fn hover_still_answers_on_names_and_literals() {
    let (client, handle) = boot();
    let uri: Uri = "file:///test/sym.gd".parse().unwrap();
    did_open(
        &client,
        &uri,
        "extends Node\n\nfunc _ready() -> void:\n\tvar a = 1 + 2\n\tvar s = \"hi\"\n\tvar b = true\n\tprint(a, s, b)\n",
    );

    for (line, character, want) in [
        (0, 9, "Node"),
        (3, 5, "int"),
        (3, 9, "int"),
        (4, 10, "String"),
        (5, 10, "bool"),
    ] {
        let hover = hover_at(&client, &uri, Position::new(line, character))
            .unwrap_or_else(|| panic!("expected a hover at {line}:{character}"));
        assert!(
            hover_markdown(&hover).contains(want),
            "at {line}:{character} wanted {want}, got {:?}",
            hover_markdown(&hover)
        );
    }

    shutdown(&client, handle);
}

/// #412, second half: a builtin `NIL` is spelled `null` by `DataType::to_string()`
/// (gdscript_parser.cpp:5341). `Nil` is `Variant::get_type_name`'s spelling, which is what error
/// messages use — never what a type label should read.
#[test]
fn a_void_typed_expression_reads_as_null_not_nil() {
    let (client, handle) = boot();
    let uri: Uri = "file:///test/voidty.gd".parse().unwrap();
    // 0: extends Node
    // 1:
    // 2: const N = null
    // 3:
    // 4: func _ready() -> void:
    // 5: \tpass
    did_open(
        &client,
        &uri,
        "extends Node\n\nconst N = null\n\nfunc _ready() -> void:\n\tpass\n",
    );

    let hover = hover_at(&client, &uri, Position::new(2, 6)).expect("hover on the declaration");
    let md = hover_markdown(&hover);
    assert!(md.contains("const N: null"), "got {md:?}");
    assert!(!md.contains("Nil"), "got {md:?}");

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
    // `extends FrobnicateWidget` references a name that is neither a project `class_name` nor a
    // class in the native DB (the default boot serves the embedded stock fallback since v1.0.2).
    // The definition request must resolve to `null` (no jump target), not error. KNOWN native
    // names are no longer null — they jump into a materialized API stub (#34; pinned by
    // `definition_on_native_symbols_jumps_into_materialized_stubs`).
    let (client, handle) = boot();
    let uri: Uri = "file:///test/unknown.gd".parse().unwrap();
    did_open(&client, &uri, "extends FrobnicateWidget\n");

    // `FrobnicateWidget` is at line 0, columns 8..24.
    let response = definition_at(&client, &uri, Position::new(0, 10));
    assert!(response.is_none(), "unknown name ⇒ null, got {response:?}");

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

#[test]
fn definition_on_type_position_const_alias_collision_jumps_to_global_195() {
    // #195: a TYPE-POSITION base segment (`: Foo` annotation / `extends Foo`) that names a global
    // `class_name Foo` resolves to the GLOBAL class — even when the SAME file declares a class-scope
    // `const Foo = preload(...)`. Godot's `resolve_datatype` checks `is_global_class`
    // (gdscript_analyzer.cpp:787) before current-/inherited-scope class members (:866), so the global
    // binds in type position. The old `definition` matched the class-scope `const Foo` first (step 1),
    // diverging from the analyzer (and from `rename`, which canonicalizes through `definition`).
    let fixture_dir = std::env::temp_dir().join("gdls_def_const_alias_195");
    let _ = std::fs::remove_dir_all(&fixture_dir);
    std::fs::create_dir_all(&fixture_dir).expect("create fixture dir");
    std::fs::write(fixture_dir.join("project.godot"), "").expect("write project.godot");
    // foo.gd declares the global `class_name Foo` (the type the annotations bind to).
    std::fs::write(fixture_dir.join("foo.gd"), "class_name Foo\nextends Node\n")
        .expect("write foo.gd");
    // other.gd is the const-alias target (a class_name-less script).
    std::fs::write(fixture_dir.join("other.gd"), "extends Node\n").expect("write other.gd");
    // consumer extends the global Foo AND declares a class-scope `const Foo` alias + a `: Foo` use.
    //   line 0 `extends Foo`                              `Foo`@8
    //   line 1 `const Foo = preload("res://other.gd")`    the class-scope alias (NOT the type target)
    //   line 2 `var x: Foo = null`                        `Foo`@7
    let consumer = "extends Foo\nconst Foo = preload(\"res://other.gd\")\nvar x: Foo = null\n";
    std::fs::write(fixture_dir.join("consumer.gd"), consumer).expect("write consumer.gd");

    let init_options = serde_json::json!({
        "projectRoot": fixture_dir.to_string_lossy().as_ref(),
        "autoDumpExtensionApi": false,
    });
    let (client, handle) = boot_with_options(Some(init_options));
    let foo_uri: Uri = format!(
        "file:///{}",
        fixture_dir
            .join("foo.gd")
            .to_string_lossy()
            .replace('\\', "/")
    )
    .parse()
    .unwrap();
    let consumer_uri: Uri = format!(
        "file:///{}",
        fixture_dir
            .join("consumer.gd")
            .to_string_lossy()
            .replace('\\', "/")
    )
    .parse()
    .unwrap();
    did_open(&client, &consumer_uri, consumer);

    // (a) `: Foo` annotation base (line 2, col 7) → the GLOBAL class decl in foo.gd, NOT the
    // class-scope `const Foo` on line 1.
    let ann = definition_at(&client, &consumer_uri, Position::new(2, 7))
        .expect("`: Foo` annotation resolves");
    let GotoDefinitionResponse::Scalar(ann_loc) = ann else {
        panic!("expected scalar Location for `: Foo`");
    };
    assert!(
        ann_loc.uri.as_str().ends_with("/foo.gd"),
        "`: Foo` must jump to the global class in foo.gd, not the local const; got {}",
        ann_loc.uri.as_str()
    );
    assert_eq!(ann_loc.range.start, Position::new(0, 11));

    // (b) `extends Foo` base (line 0, col 8) → the same GLOBAL class decl (inheritance is
    // global-before-class-scope too).
    let ext =
        definition_at(&client, &consumer_uri, Position::new(0, 8)).expect("`extends Foo` resolves");
    let GotoDefinitionResponse::Scalar(ext_loc) = ext else {
        panic!("expected scalar Location for `extends Foo`");
    };
    assert!(
        ext_loc.uri.as_str().ends_with("/foo.gd"),
        "`extends Foo` must jump to the global class in foo.gd; got {}",
        ext_loc.uri.as_str()
    );
    assert_eq!(ext_loc.range.start, Position::new(0, 11));

    let _ = foo_uri;
    shutdown(&client, handle);
    let _ = std::fs::remove_dir_all(&fixture_dir);
}

#[test]
fn definition_on_cross_file_member_covers_the_name_token() {
    // Cross-file member jumps must anchor the NAME token — the same shape the in-file arm
    // returns — not the whole declaration node that `MemberDecl::span` covers: editors select
    // the returned range, so a whole-func range visibly selects the entire function body.
    // Covers both cross-file arms: the call-binding path (`l.helper(1)`) and the use-binding
    // path (`l.sig`).
    let fixture_dir = std::env::temp_dir().join("gdls_def_xfile_member");
    let _ = std::fs::remove_dir_all(&fixture_dir);
    std::fs::create_dir_all(&fixture_dir).expect("create fixture dir");
    std::fs::write(fixture_dir.join("project.godot"), "").expect("write project.godot");
    let lib_path = fixture_dir.join("lib.gd");
    let b_path = fixture_dir.join("b.gd");
    // `sig` ident on line 1 cols 7..10; `helper` ident on line 2 cols 5..11.
    std::fs::write(
        &lib_path,
        "class_name DefLib\nsignal sig\nfunc helper(amount: int) -> int:\n\treturn amount\n",
    )
    .expect("write lib.gd");
    let b_src = "extends Node\n\
                 func go() -> void:\n\
                 \tvar l: DefLib = DefLib.new()\n\
                 \tl.helper(1)\n\
                 \tprint(l.sig)\n";
    std::fs::write(&b_path, b_src).expect("write b.gd");

    let init_options = serde_json::json!({
        "projectRoot": fixture_dir.to_string_lossy().as_ref(),
        "autoDumpExtensionApi": false,
    });
    let (client, handle) = boot_with_options(Some(init_options));
    let b_uri: Uri = format!("file:///{}", b_path.to_string_lossy().replace('\\', "/"))
        .parse()
        .unwrap();
    did_open(&client, &b_uri, b_src);

    let location_at = |line: u32, character: u32, what: &str| -> lsp_types::Location {
        match definition_at(&client, &b_uri, Position::new(line, character))
            .unwrap_or_else(|| panic!("definition must answer on {what} at {line}:{character}"))
        {
            GotoDefinitionResponse::Scalar(loc) => loc,
            other => panic!("{what}: expected scalar Location, got {other:?}"),
        }
    };

    // Call-binding arm: `helper` inside `l.helper(1)` → exactly the `helper` identifier in
    // lib.gd (line 2, cols 5..11), not the whole `func helper(...)` node.
    let loc = location_at(3, 4, "cross-file method callee");
    assert!(
        loc.uri.as_str().ends_with("/lib.gd"),
        "jump must land in lib.gd, got {}",
        loc.uri.as_str()
    );
    assert_eq!(loc.range.start, Position::new(2, 5));
    assert_eq!(loc.range.end, Position::new(2, 11));

    // Use-binding arm: `sig` inside `print(l.sig)` → exactly the `sig` identifier (line 1,
    // cols 7..10).
    let loc = location_at(4, 9, "cross-file signal attribute");
    assert!(
        loc.uri.as_str().ends_with("/lib.gd"),
        "jump must land in lib.gd, got {}",
        loc.uri.as_str()
    );
    assert_eq!(loc.range.start, Position::new(1, 7));
    assert_eq!(loc.range.end, Position::new(1, 10));

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
    let project_godot = "[application]\nconfig/name=\"Test\"\nconfig/features=PackedStringArray(\"4.6\")\nconfig_version=5\n\n[autoload]\nSave=\"*res://save.gd\"\n";
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
    let project_godot = "[application]\nconfig/name=\"Test\"\nconfig/features=PackedStringArray(\"4.6\")\nconfig_version=5\n\n[autoload]\nSave=\"*res://save.gd\"\n";
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
    let project_godot = "[application]\nconfig/name=\"Test\"\nconfig/features=PackedStringArray(\"4.6\")\nconfig_version=5\n\n[autoload]\nSave=\"*res://save.gd\"\n";
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

/// v1.0.4 (#35): hover on native members renders the editor-LSP declaration line — never the
/// bare expression type (`stop()` used to hover as `Nil`, `volume_db` as `float`). Covers every
/// repro row from the issue: attribute callee, property, class name, enum constant, bare
/// implicit-self call, builtin method, and a `@GlobalScope` utility — plus member docs after
/// the fence when the dump carries them.
#[test]
fn hover_native_members_render_declaration_lines() {
    let fixture = tempfile::tempdir().expect("create fixture dir");
    let fixture_dir = fixture.path().to_path_buf();
    std::fs::write(fixture_dir.join("project.godot"), "").expect("write project.godot");
    let api_path = fixture_dir.join("extension_api.json");
    std::fs::write(
        &api_path,
        r#"{
        "header": { "version_major": 4, "version_minor": 6, "version_patch": 3 },
        "builtin_classes": [
            {"name": "Vector2", "is_keyed": false,
             "methods": [{"name": "length", "is_const": true, "is_static": false,
                          "is_vararg": false, "return_type": "float", "arguments": []}]}
        ],
        "classes": [
            {"name": "Object", "is_instantiable": true},
            {"name": "Node", "inherits": "Object", "is_instantiable": true},
            {"name": "AudioStreamPlayer", "inherits": "Node", "is_instantiable": true,
             "properties": [{"name": "volume_db", "type": "float",
                             "setter": "set_volume_db", "getter": "get_volume_db"}],
             "methods": [{"name": "stop", "is_const": false, "is_static": false,
                          "is_vararg": false, "is_virtual": false, "hash": 1, "arguments": [],
                          "description": "Stops the audio."}]},
            {"name": "Input", "inherits": "Object", "is_instantiable": true,
             "enums": [{"name": "MouseMode", "is_bitfield": false,
                        "values": [{"name": "MOUSE_MODE_CAPTURED", "value": 2}]}]}
        ],
        "utility_functions": [
            {"name": "print", "category": "general", "is_vararg": true, "arguments": []}
        ]
    }"#,
    )
    .expect("write fixture JSON");

    let src = "extends AudioStreamPlayer\n\
               func _ready() -> void:\n\
               \tvar player := AudioStreamPlayer.new()\n\
               \tplayer.stop()\n\
               \tplayer.volume_db = 0.0\n\
               \tvar mode := Input.MOUSE_MODE_CAPTURED\n\
               \tvar v: Vector2\n\
               \tstop()\n\
               \tprint([mode, v.length()])\n";
    let script_path = fixture_dir.join("main.gd");
    std::fs::write(&script_path, src).expect("write main.gd");

    let init_options = serde_json::json!({
        "projectRoot": fixture_dir.to_string_lossy().as_ref(),
        "extensionApiPath": api_path.to_string_lossy().as_ref(),
        "autoDumpExtensionApi": false,
    });
    let (client, handle) = boot_with_options(Some(init_options));
    let uri: Uri = format!(
        "file:///{}",
        script_path.to_string_lossy().replace('\\', "/")
    )
    .parse()
    .unwrap();
    did_open(&client, &uri, src);

    let md_at = |line: u32, character: u32, what: &str| -> String {
        let hover = hover_at(&client, &uri, Position::new(line, character))
            .unwrap_or_else(|| panic!("hover must answer on {what} at {line}:{character}"));
        hover_markdown(&hover).to_string()
    };

    // Class name (`extends AudioStreamPlayer`) → the editor-LSP class line + nothing lost.
    let md = md_at(0, 12, "class name");
    assert!(
        md.contains("<Native> class AudioStreamPlayer extends Node"),
        "class hover renders the declaration line, got {md:?}"
    );

    // Attribute callee: `player.stop()` → the full signature, with the member doc after.
    let md = md_at(3, 9, "player.stop callee");
    assert!(
        md.contains("func AudioStreamPlayer.stop() -> void"),
        "method hover renders the signature, not `Nil`, got {md:?}"
    );
    assert!(
        md.contains("Stops the audio."),
        "member description renders after the fence, got {md:?}"
    );

    // Property attribute: `player.volume_db` → var line, not `float`.
    let md = md_at(4, 10, "player.volume_db");
    assert!(
        md.contains("var AudioStreamPlayer.volume_db: float"),
        "property hover renders the declaration, got {md:?}"
    );

    // Native class meta + enum value: `Input.MOUSE_MODE_CAPTURED`.
    let md = md_at(5, 14, "Input class name");
    assert!(
        md.contains("<Native> class Input extends Object"),
        "got {md:?}"
    );
    let md = md_at(5, 25, "MOUSE_MODE_CAPTURED");
    assert!(
        md.contains("const Input.MOUSE_MODE_CAPTURED: MouseMode = 2"),
        "enum-value hover renders the const line, got {md:?}"
    );

    // Bare implicit-self call: `stop()` under `extends AudioStreamPlayer`.
    let md = md_at(7, 2, "bare stop()");
    assert!(
        md.contains("func AudioStreamPlayer.stop() -> void"),
        "bare inherited call resolves through the chain root, got {md:?}"
    );

    // Builtin method + utility function.
    let md = md_at(8, 17, "v.length()");
    assert!(
        md.contains("func Vector2.length() -> float"),
        "builtin method hover renders the signature, got {md:?}"
    );
    let md = md_at(8, 2, "print utility");
    assert!(
        md.contains("func print(...) -> void"),
        "utility hover renders the vararg signature, got {md:?}"
    );

    shutdown(&client, handle);
}

/// v1.0.4 (#34): definition on native symbols materializes the class API as a real document
/// under the (test-overridden) stub cache and returns a plain `file://` Location into it —
/// class names anchor at the `class_name` header, member access anchors at the member's
/// rendered declaration line (in the DECLARING class's stub), and implicit-self bare calls
/// resolve through the chain root. Opening a stub publishes EMPTY diagnostics (an API page
/// need not be analyzable GDScript). Project classes keep shadowing the native arm.
#[test]
fn definition_on_native_symbols_jumps_into_materialized_stubs() {
    let fixture = tempfile::tempdir().expect("create fixture dir");
    let fixture_dir = fixture.path().to_path_buf();
    std::fs::write(fixture_dir.join("project.godot"), "").expect("write project.godot");
    let stub_cache = fixture_dir.join("stub-cache");
    let api_path = fixture_dir.join("extension_api.json");
    std::fs::write(
        &api_path,
        r#"{
        "header": { "version_major": 4, "version_minor": 6, "version_patch": 3 },
        "classes": [
            {"name": "Object", "is_instantiable": true},
            {"name": "Node", "inherits": "Object", "is_instantiable": true,
             "methods": [{"name": "queue_free", "is_const": false, "is_static": false,
                          "is_vararg": false, "is_virtual": false, "hash": 1, "arguments": []}]},
            {"name": "AudioStreamPlayer", "inherits": "Node", "is_instantiable": true,
             "properties": [{"name": "volume_db", "type": "float",
                             "setter": "set_volume_db", "getter": "get_volume_db"}],
             "methods": [{"name": "stop", "is_const": false, "is_static": false,
                          "is_vararg": false, "is_virtual": false, "hash": 2, "arguments": []}]}
        ]
    }"#,
    )
    .expect("write fixture JSON");
    // A project class that shares a workflow with natives — must keep shadowing them.
    std::fs::write(
        fixture_dir.join("shadow.gd"),
        "class_name ShadowHero\nextends Node\n",
    )
    .expect("write shadow.gd");

    let src = "extends AudioStreamPlayer\n\
               func _ready() -> void:\n\
               \tvar player: AudioStreamPlayer\n\
               \tplayer.stop()\n\
               \tqueue_free()\n\
               \tvar h: ShadowHero\n\
               \tprint_debug([player, h])\n";
    let script_path = fixture_dir.join("main.gd");
    std::fs::write(&script_path, src).expect("write main.gd");

    let init_options = serde_json::json!({
        "projectRoot": fixture_dir.to_string_lossy().as_ref(),
        "extensionApiPath": api_path.to_string_lossy().as_ref(),
        "autoDumpExtensionApi": false,
        "stubCacheDir": stub_cache.to_string_lossy().as_ref(),
    });
    let (client, handle) = boot_with_options(Some(init_options));
    let uri: Uri = format!(
        "file:///{}",
        script_path.to_string_lossy().replace('\\', "/")
    )
    .parse()
    .unwrap();
    did_open(&client, &uri, src);

    let location_at = |line: u32, character: u32, what: &str| -> lsp_types::Location {
        match definition_at(&client, &uri, Position::new(line, character))
            .unwrap_or_else(|| panic!("definition must answer on {what} at {line}:{character}"))
        {
            GotoDefinitionResponse::Scalar(loc) => loc,
            other => panic!("{what}: expected scalar Location, got {other:?}"),
        }
    };

    // 1. Class name (`extends AudioStreamPlayer`) → the stub's class_name header.
    let loc = location_at(0, 12, "native class name");
    let stub_path = gd_server::uri::uri_to_path(&loc.uri).expect("stub uri is a file path");
    assert!(
        stub_path
            .as_std_path()
            .starts_with(std::path::Path::new(&stub_cache)),
        "stub lands under the overridden cache root, got {stub_path:?}"
    );
    assert!(stub_path.as_str().ends_with("AudioStreamPlayer.gd"));
    let stub_text =
        std::fs::read_to_string(stub_path.as_std_path()).expect("stub file exists on disk");
    let header_line = stub_text
        .lines()
        .nth(loc.range.start.line as usize)
        .expect("class_line within the stub");
    assert_eq!(header_line, "class_name AudioStreamPlayer");
    // The range covers exactly the class NAME token, not column 0 of the header line.
    assert_eq!(loc.range.start.character, 11);
    assert_eq!(
        loc.range.end.character,
        11 + "AudioStreamPlayer".len() as u32
    );
    assert!(
        stub_text.contains("func stop() -> void") && stub_text.contains("var volume_db: float"),
        "the page renders the whole API, got:\n{stub_text}"
    );

    // 2. Member attribute (`player.stop`) → the member's line in the stub.
    let loc = location_at(3, 9, "member attribute");
    let asp_stub_uri = loc.uri.clone();
    let stub_text = std::fs::read_to_string(
        gd_server::uri::uri_to_path(&loc.uri)
            .expect("file uri")
            .as_std_path(),
    )
    .expect("stub readable");
    assert_eq!(
        stub_text
            .lines()
            .nth(loc.range.start.line as usize)
            .unwrap(),
        "func stop() -> void",
        "member access anchors at the rendered declaration"
    );
    // `func stop() -> void` — the member's name token sits at cols 5..9.
    assert_eq!(loc.range.start.character, 5);
    assert_eq!(loc.range.end.character, 9);

    // 3. Implicit-self bare call (`queue_free()`) → the DECLARING class's stub (Node).
    let loc = location_at(4, 3, "bare inherited call");
    let path = gd_server::uri::uri_to_path(&loc.uri).expect("file uri");
    assert!(
        path.as_str().ends_with("Node.gd"),
        "declaring class owns the stub, got {path:?}"
    );
    let stub_text = std::fs::read_to_string(path.as_std_path()).expect("Node stub readable");
    assert_eq!(
        stub_text
            .lines()
            .nth(loc.range.start.line as usize)
            .unwrap(),
        "func queue_free() -> void"
    );
    // `func queue_free() -> void` — the name token sits at cols 5..15.
    assert_eq!(loc.range.start.character, 5);
    assert_eq!(loc.range.end.character, 15);

    // 4. A project class keeps shadowing the native arm.
    let loc = location_at(5, 9, "project class");
    assert!(
        loc.uri.as_str().ends_with("/shadow.gd"),
        "project class_name wins over stub materialization, got {:?}",
        loc.uri
    );

    // 5. Opening a stub publishes EMPTY diagnostics.
    let stub_uri = asp_stub_uri;
    let stub_doc = std::fs::read_to_string(
        gd_server::uri::uri_to_path(&stub_uri)
            .expect("file uri")
            .as_std_path(),
    )
    .unwrap();
    client
        .sender
        .send(notification(
            "textDocument/didOpen",
            serde_json::to_value(lsp_types::DidOpenTextDocumentParams {
                text_document: TextDocumentItem {
                    uri: stub_uri.clone(),
                    language_id: "gdscript".to_string(),
                    version: 1,
                    text: stub_doc,
                },
            })
            .unwrap(),
        ))
        .unwrap();
    let Message::Notification(n) = recv(&client) else {
        panic!("expected publishDiagnostics after didOpen");
    };
    assert_eq!(n.method, "textDocument/publishDiagnostics");
    let params: lsp_types::PublishDiagnosticsParams = serde_json::from_value(n.params).unwrap();
    assert_eq!(params.uri.as_str(), stub_uri.as_str());
    assert!(
        params.diagnostics.is_empty(),
        "a stub buffer must not self-diagnose, got {:?}",
        params.diagnostics
    );

    shutdown(&client, handle);
}

// --- #146: inner-class instance hover/definition resolve the INNER member, not the file root ---

/// #146: hover on a method of an inner-class INSTANCE (`var x := Inner.new()`) that name-collides
/// with a root method must show the INNER signature, not the root one. Fail-OPEN before the fix
/// (showed the root `collide(a)`); the producer (context.rs finish) now populates the value's
/// inner-class chain and the hover consumer must descend it.
#[test]
fn hover_inner_class_instance_member_uses_inner_not_root() {
    let (client, handle) = boot();
    let uri: Uri = "file:///test/inner_hover.gd".parse().unwrap();
    let src = concat!(
        "func collide(a: int) -> void:\n",               // 0  root collide
        "\tpass\n",                                      // 1
        "\n",                                            // 2
        "class Inner:\n",                                // 3
        "\tfunc collide(a: int, extra: int) -> void:\n", // 4  inner collide
        "\t\tpass\n",                                    // 5
        "\n",                                            // 6
        "func use_it() -> void:\n",                      // 7
        "\tvar x := Inner.new()\n",                      // 8
        "\tx.collide(1, 2)\n",                           // 9
    );
    did_open(&client, &uri, src);
    // `\tx.collide(1, 2)` — `collide` at cols 3..10; hover at col 5.
    let hover = hover_at(&client, &uri, Position::new(9, 5)).expect("hover on x.collide");
    let md = hover_markdown(&hover);
    assert!(
        md.contains("extra"),
        "hover on an inner-class instance member must show the INNER signature (param `extra`), \
         not the root collide(a); got {md:?}"
    );
    shutdown(&client, handle);
}

/// #146: definition on an inner-class instance member jumps to the INNER declaration (line 4), not
/// the root one (line 0).
#[test]
fn definition_inner_class_instance_member_jumps_to_inner() {
    let (client, handle) = boot();
    let uri: Uri = "file:///test/inner_def.gd".parse().unwrap();
    let src = concat!(
        "func collide(a: int) -> void:\n",               // 0  root collide
        "\tpass\n",                                      // 1
        "\n",                                            // 2
        "class Inner:\n",                                // 3
        "\tfunc collide(a: int, extra: int) -> void:\n", // 4  inner collide (identifier at col 6)
        "\t\tpass\n",                                    // 5
        "\n",                                            // 6
        "func use_it() -> void:\n",                      // 7
        "\tvar x := Inner.new()\n",                      // 8
        "\tx.collide(1, 2)\n",                           // 9
    );
    did_open(&client, &uri, src);
    let response = definition_at(&client, &uri, Position::new(9, 5))
        .expect("definition on x.collide resolves");
    let location = match response {
        GotoDefinitionResponse::Scalar(loc) => loc,
        other => panic!("expected scalar Location, got {other:?}"),
    };
    assert_eq!(
        location.range.start.line, 4,
        "definition must jump to the INNER collide (line 4), not the root (line 0); got {:?}",
        location.range.start
    );
    shutdown(&client, handle);
}

/// #146 (non-call attribute): definition on an inner-class instance **property** (`x.field`, not a
/// call) jumps to the INNER `field`, not the same-named ROOT one. Exercises definition step (0.5)
/// for a bare attribute (no Call binding) + `member_decl_location` resolving a var member.
#[test]
fn definition_inner_class_property_jumps_to_inner() {
    let (client, handle) = boot();
    let uri: Uri = "file:///test/inner_prop.gd".parse().unwrap();
    let src = concat!(
        "var field := 0\n",         // 0  root field
        "\n",                       // 1
        "class Inner:\n",           // 2
        "\tvar field := 0\n",       // 3  inner field (the target, line 3)
        "\n",                       // 4
        "func use_it() -> void:\n", // 5
        "\tvar x := Inner.new()\n", // 6
        "\tvar y := x.field\n",     // 7  `field` at cols 11..16; cursor at col 13
    );
    did_open(&client, &uri, src);
    let response =
        definition_at(&client, &uri, Position::new(7, 13)).expect("definition on x.field resolves");
    let location = match response {
        GotoDefinitionResponse::Scalar(loc) => loc,
        other => panic!("expected scalar Location, got {other:?}"),
    };
    assert_eq!(
        location.range.start.line, 3,
        "definition must jump to the INNER field (line 3), not the root (line 0); got {:?}",
        location.range.start
    );
    shutdown(&client, handle);
}

/// #146 (deep nesting, CALL path): a **doubly**-nested inner-class instance method
/// (`Outer.Inner.new(); x.deep(1)`) resolves on `Outer.Inner` — locking `iface_at_inner`'s depth-2
/// descent via definition step (1.6)'s `CalleeTarget` `class_path` (built in `reduce_call`, so this
/// does NOT exercise the `finish()` producer chain — see the non-call test below for that).
#[test]
fn definition_doubly_nested_inner_class_member_descends_full_chain() {
    let (client, handle) = boot();
    let uri: Uri = "file:///test/depth2.gd".parse().unwrap();
    let src = concat!(
        "class Outer:\n",                   // 0
        "\tclass Inner:\n",                 // 1
        "\t\tfunc deep(a: int) -> void:\n", // 2  target (line 2)
        "\t\t\tpass\n",                     // 3
        "\n",                               // 4
        "func use_it() -> void:\n",         // 5
        "\tvar x := Outer.Inner.new()\n",   // 6
        "\tx.deep(1)\n",                    // 7  `deep` cursor at col 4
    );
    did_open(&client, &uri, src);
    let response =
        definition_at(&client, &uri, Position::new(7, 4)).expect("definition on x.deep resolves");
    let location = match response {
        GotoDefinitionResponse::Scalar(loc) => loc,
        other => panic!("expected scalar Location, got {other:?}"),
    };
    assert_eq!(
        location.range.start.line, 2,
        "definition must descend to Outer.Inner.deep (line 2); got {:?}",
        location.range.start
    );
    shutdown(&client, handle);
}

/// #146 (deep nesting, PRODUCER path): a **doubly**-nested inner-class instance **property**
/// (`Outer.Inner.new(); x.deep_field`, not a call) resolves on `Outer.Inner`. Unlike the call
/// variant, a bare attribute goes through definition step (0.5), which reads `base_dt.script_type`
/// — the `finish()`-produced `ScriptRef` — so this LOCKS the producer building the full
/// `["Outer","Inner"]` chain (a depth-1-only producer would fail to descend / find the member).
#[test]
fn definition_doubly_nested_inner_class_property_locks_producer_chain() {
    let (client, handle) = boot();
    let uri: Uri = "file:///test/depth2_prop.gd".parse().unwrap();
    let src = concat!(
        "class Outer:\n",                 // 0
        "\tclass Inner:\n",               // 1
        "\t\tvar deep_field := 0\n",      // 2  target (line 2)
        "\n",                             // 3
        "func use_it() -> void:\n",       // 4
        "\tvar x := Outer.Inner.new()\n", // 5
        "\tvar y := x.deep_field\n",      // 6  `deep_field` starts col 12; cursor at col 14
    );
    did_open(&client, &uri, src);
    let response = definition_at(&client, &uri, Position::new(6, 14))
        .expect("definition on x.deep_field resolves");
    let location = match response {
        GotoDefinitionResponse::Scalar(loc) => loc,
        other => panic!("expected scalar Location, got {other:?}"),
    };
    assert_eq!(
        location.range.start.line, 2,
        "definition must descend to Outer.Inner.deep_field (line 2) via the producer chain; got {:?}",
        location.range.start
    );
    shutdown(&client, handle);
}

/// #356: a member a project script INHERITS — from a script link up its `extends` chain, or from
/// the native root the chain bottoms out in — reaches hover and definition. Completion already
/// walked that chain, so before this the three surfaces disagreed about the same expression:
/// `slider.value` completed fine, hovered as the bare `float`, and went nowhere.
#[test]
fn hover_and_definition_reach_a_scripts_inherited_members() {
    let fixture = tempfile::tempdir().expect("create fixture dir");
    let fixture_dir = fixture.path().to_path_buf();
    std::fs::write(fixture_dir.join("project.godot"), "").expect("write project.godot");
    let api_path = fixture_dir.join("extension_api.json");
    std::fs::write(
        &api_path,
        r#"{
        "header": { "version_major": 4, "version_minor": 6, "version_patch": 3 },
        "classes": [
            {"name": "Object", "is_instantiable": true},
            {"name": "Node", "inherits": "Object", "is_instantiable": true,
             "methods": [{"name": "get_parent", "is_const": true, "is_static": false,
                          "is_vararg": false, "is_virtual": false, "hash": 1, "arguments": [],
                          "return_value": {"type": "Node"},
                          "description": "Returns this node's parent node."}]},
            {"name": "Range", "inherits": "Node", "is_instantiable": true,
             "properties": [{"name": "value", "type": "float",
                             "setter": "set_value", "getter": "get_value",
                             "description": "The current value."}]}
        ]
    }"#,
    )
    .expect("write fixture JSON");

    // `Slider extends Range` (native), and `FineSlider extends Slider` (a script link), so the
    // walk has to cross a script hop before it reaches the native tail.
    std::fs::write(
        fixture_dir.join("slider.gd"),
        "class_name Slider\nextends Range\n\n## How fine the steps are.\nvar precision := 1\n",
    )
    .expect("write slider.gd");
    std::fs::write(
        fixture_dir.join("fine_slider.gd"),
        "class_name FineSlider\nextends Slider\n",
    )
    .expect("write fine_slider.gd");

    let src = "extends Node\n\
               func _ready() -> void:\n\
               \tvar s: FineSlider = null\n\
               \ts.value = 1.0\n\
               \ts.get_parent()\n\
               \tprint(s.precision)\n";
    let script_path = fixture_dir.join("main.gd");
    std::fs::write(&script_path, src).expect("write main.gd");

    let stub_dir = fixture_dir.join("stubs");
    let init_options = serde_json::json!({
        "projectRoot": fixture_dir.to_string_lossy().as_ref(),
        "extensionApiPath": api_path.to_string_lossy().as_ref(),
        "autoDumpExtensionApi": false,
        "stubCacheDir": stub_dir.to_string_lossy().as_ref(),
    });
    let (client, handle) = boot_with_options(Some(init_options));
    let uri: Uri = format!(
        "file:///{}",
        script_path.to_string_lossy().replace('\\', "/")
    )
    .parse()
    .unwrap();
    did_open(&client, &uri, src);

    let md_at = |line: u32, character: u32, what: &str| -> String {
        let hover = hover_at(&client, &uri, Position::new(line, character))
            .unwrap_or_else(|| panic!("hover must answer on {what} at {line}:{character}"));
        hover_markdown(&hover).to_string()
    };

    // A property inherited from the chain's NATIVE root, two hops up.
    let md = md_at(3, 4, "s.value");
    assert!(
        md.contains("var Range.value: float"),
        "an inherited native property hovers as its declaration, got {md:?}"
    );
    assert!(
        md.contains("The current value."),
        "the dump's description renders after the fence, got {md:?}"
    );

    // A method inherited from further up the same native chain.
    let md = md_at(4, 4, "s.get_parent()");
    assert!(
        md.contains("func Node.get_parent() -> Node"),
        "an inherited native method hovers as its signature, got {md:?}"
    );

    // A member declared by the SCRIPT link above — the head interface does not carry it.
    let md = md_at(5, 10, "s.precision");
    assert!(
        md.contains("precision"),
        "a member from the script base hovers as its declaration, got {md:?}"
    );
    assert!(
        md.contains("How fine the steps are."),
        "the base script's `##` doc renders, got {md:?}"
    );

    // Definition lands in the DECLARING class's stub, not nowhere.
    let Some(GotoDefinitionResponse::Scalar(loc)) =
        definition_at(&client, &uri, Position::new(3, 4))
    else {
        panic!("definition on an inherited native property must resolve");
    };
    assert!(
        loc.uri.as_str().ends_with("Range.gd"),
        "expected the Range stub, got {}",
        loc.uri.as_str()
    );

    // And on the script link, in that script's own file.
    let Some(GotoDefinitionResponse::Scalar(loc)) =
        definition_at(&client, &uri, Position::new(5, 10))
    else {
        panic!("definition on a script-inherited member must resolve");
    };
    assert!(
        loc.uri.as_str().ends_with("slider.gd"),
        "expected slider.gd, got {}",
        loc.uri.as_str()
    );

    shutdown(&client, handle);
}

/// #370: a Variant type's symbols get the same docs and the same stub jump an engine class's do.
/// `Array`, `Vector2`, `String` and the rest are what most GDScript touches most often, and their
/// prose was dropped at ingestion while `definition` had no page to anchor on.
#[test]
fn builtin_type_symbols_carry_docs_and_jump_into_a_stub() {
    let fixture = tempfile::tempdir().expect("create fixture dir");
    let fixture_dir = fixture.path().to_path_buf();
    std::fs::write(fixture_dir.join("project.godot"), "").expect("write project.godot");
    let stub_cache = fixture_dir.join("stub-cache");
    let api_path = fixture_dir.join("extension_api.json");
    std::fs::write(
        &api_path,
        r#"{
        "header": { "version_major": 4, "version_minor": 6, "version_patch": 3 },
        "classes": [
            {"name": "Object", "is_instantiable": true},
            {"name": "Node", "inherits": "Object", "is_instantiable": true}
        ],
        "builtin_classes": [
            {"name": "Vector2",
             "brief_description": "A 2D vector using floating-point coordinates.",
             "description": "A 2-element structure that can be used to represent 2D coordinates.",
             "members": [{"name": "x", "type": "float",
                          "description": "The vector's X component."}],
             "constants": [{"name": "UP", "type": "Vector2", "value": "Vector2(0, -1)",
                            "description": "Up unit vector."}],
             "methods": [{"name": "length", "return_type": "float", "is_const": true,
                          "is_static": false, "is_vararg": false, "hash": 1, "arguments": [],
                          "description": "Returns the length (magnitude) of this vector."}]}
        ]
    }"#,
    )
    .expect("write fixture JSON");

    let src = "extends Node\n\
               func _ready() -> void:\n\
               \tvar v: Vector2\n\
               \tprint(v.length())\n\
               \tprint(v.x)\n\
               \tprint(Vector2.UP)\n";
    let script_path = fixture_dir.join("main.gd");
    std::fs::write(&script_path, src).expect("write main.gd");

    let init_options = serde_json::json!({
        "projectRoot": fixture_dir.to_string_lossy().as_ref(),
        "extensionApiPath": api_path.to_string_lossy().as_ref(),
        "autoDumpExtensionApi": false,
        "stubCacheDir": stub_cache.to_string_lossy().as_ref(),
    });
    let (client, handle) = boot_with_options(Some(init_options));
    let uri: Uri = format!(
        "file:///{}",
        script_path.to_string_lossy().replace('\\', "/")
    )
    .parse()
    .unwrap();
    did_open(&client, &uri, src);

    let hover_text = |line: u32, character: u32, what: &str| -> String {
        let hover = hover_at(&client, &uri, Position::new(line, character))
            .unwrap_or_else(|| panic!("hover must answer on {what}"));
        match hover.contents {
            HoverContents::Markup(m) => m.value,
            other => panic!("{what}: expected markup, got {other:?}"),
        }
    };

    // The type itself keeps its bare label — Godot has no `<Native> class …` form for a builtin —
    // and gains the prose that used to be dropped.
    let ty = hover_text(2, 8, "the Vector2 annotation");
    assert!(
        ty.contains("A 2D vector using floating-point coordinates."),
        "the type's brief must render: {ty}"
    );
    for (line, ch, what, doc) in [
        (
            3,
            10,
            "a builtin method",
            "Returns the length (magnitude) of this vector.",
        ),
        (4, 9, "a builtin member", "The vector's X component."),
        (5, 16, "a builtin constant", "Up unit vector."),
    ] {
        let md = hover_text(line, ch, what);
        assert!(md.contains(doc), "{what} must carry its doc: {md}");
    }

    let location_at = |line: u32, character: u32, what: &str| -> lsp_types::Location {
        match definition_at(&client, &uri, Position::new(line, character))
            .unwrap_or_else(|| panic!("definition must answer on {what} at {line}:{character}"))
        {
            GotoDefinitionResponse::Scalar(loc) => loc,
            other => panic!("{what}: expected scalar Location, got {other:?}"),
        }
    };

    // Every one of these used to answer null: a builtin had no page to anchor on.
    for (line, ch, what) in [
        (2, 8, "the Vector2 annotation"),
        (3, 10, "a builtin method"),
        (4, 9, "a builtin member"),
        (5, 16, "a builtin constant"),
    ] {
        let loc = location_at(line, ch, what);
        let path = gd_server::uri::uri_to_path(&loc.uri).expect("stub uri is a file path");
        assert!(
            path.as_std_path()
                .starts_with(std::path::Path::new(&stub_cache)),
            "{what} must land inside the stub cache, got {path}"
        );
        assert!(
            path.as_str().ends_with("Vector2.gd"),
            "{what} must land on the Vector2 page, got {path}"
        );
        let page = std::fs::read_to_string(path.as_std_path()).expect("read the stub page");
        let hit = page
            .lines()
            .nth(loc.range.start.line as usize)
            .unwrap_or("");
        assert!(
            hit.contains("Vector2")
                || hit.contains("length")
                || hit.contains("x")
                || hit.contains("UP"),
            "{what} anchored on {hit:?}"
        );
    }

    shutdown(&client, handle);
}

/// #405: an enum value hovers like its native counterpart — qualified by its enum, typed as that
/// enum, carrying its integer — at the declaration and at every reference.
#[test]
fn hover_on_an_enum_value_renders_its_type_and_value() {
    let (client, handle) = boot();
    let uri: Uri = "file:///test/enum_value.gd".parse().unwrap();
    let src = "extends Node\n\nenum Mode { IDLE, RUN = 5, JUMP }\n\nfunc f() -> void:\n\tprint(Mode.JUMP)\n";
    did_open(&client, &uri, src);

    // The declaration of an implicit value, of an explicit one, and of the value after it.
    for (pos, want) in [
        (Position::new(2, 13), "const Mode.IDLE: Mode = 0"),
        (Position::new(2, 20), "const Mode.RUN: Mode = 5"),
        (Position::new(2, 28), "const Mode.JUMP: Mode = 6"),
        // The reference reads the same as the declaration.
        (Position::new(5, 13), "const Mode.JUMP: Mode = 6"),
    ] {
        let hover = hover_at(&client, &uri, pos).unwrap_or_else(|| panic!("hover at {pos:?}"));
        let md = hover_markdown(&hover);
        assert!(md.contains(want), "wanted {want:?} at {pos:?}, got {md:?}");
    }

    shutdown(&client, handle);
}

/// #405: a value the index cannot read without evaluating keeps the shorter line. Guessing one
/// would be worse than omitting it, and the poison spreads to every implicit value after it.
#[test]
fn an_enum_value_that_needs_evaluating_hovers_without_a_number() {
    let (client, handle) = boot();
    let uri: Uri = "file:///test/enum_poison.gd".parse().unwrap();
    let src = "extends Node\n\nenum Flags { A = 1 << 0, B, C = -3 }\n";
    did_open(&client, &uri, src);

    for (pos, want) in [
        (Position::new(2, 13), "const Flags.A: Flags"),
        (Position::new(2, 25), "const Flags.B: Flags"),
        (Position::new(2, 28), "const Flags.C: Flags = -3"),
    ] {
        let hover = hover_at(&client, &uri, pos).unwrap_or_else(|| panic!("hover at {pos:?}"));
        let md = hover_markdown(&hover);
        assert!(md.contains(want), "wanted {want:?} at {pos:?}, got {md:?}");
    }
    // `A` and `B` carry no `=` at all, rather than a guessed one.
    let hover = hover_at(&client, &uri, Position::new(2, 13)).expect("hover on A");
    assert!(
        !hover_markdown(&hover).contains('='),
        "an unreadable value must not render a number, got {:?}",
        hover_markdown(&hover)
    );

    shutdown(&client, handle);
}

/// #405: a `const` carries its folded value, and one gdls cannot fold does not.
#[test]
fn hover_on_a_constant_renders_its_value_when_it_folds() {
    let (client, handle) = boot();
    let uri: Uri = "file:///test/const_value.gd".parse().unwrap();
    let src = "extends Node\n\nconst K := 7\nconst S := \"hi\"\nconst F := 1.5\nconst B := true\nconst D = K * 2\nconst ARR = [1, 2]\n";
    did_open(&client, &uri, src);

    for (pos, want) in [
        (Position::new(2, 6), "const K: int = 7"),
        (Position::new(3, 6), "const S: String = \"hi\""),
        (Position::new(4, 6), "const F: float = 1.5"),
        (Position::new(5, 6), "const B: bool = true"),
        (Position::new(6, 6), "const D: int = 14"),
    ] {
        let hover = hover_at(&client, &uri, pos).unwrap_or_else(|| panic!("hover at {pos:?}"));
        let md = hover_markdown(&hover);
        assert!(md.contains(want), "wanted {want:?} at {pos:?}, got {md:?}");
    }

    // An array literal has no `FoldedValue`, so the line stops at the type.
    let hover = hover_at(&client, &uri, Position::new(7, 7)).expect("hover on ARR");
    let md = hover_markdown(&hover);
    assert!(
        md.contains("const ARR") && !md.contains('='),
        "an unfoldable initializer must not render a value, got {md:?}"
    );

    shutdown(&client, handle);
}
