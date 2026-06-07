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
    let Message::Response(resp) = recv(client) else {
        panic!("expected hover response");
    };
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
    let Message::Response(resp) = recv(client) else {
        panic!("expected definition response");
    };
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

/// M6-F: hover on a cross-file method call shows the function signature from the callee's
/// interface (`func helper() -> void`). Hover on a preload string shows the target file basename.
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
