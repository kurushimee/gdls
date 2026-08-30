//! M9 (#68) gate: `textDocument/declaration` and `textDocument/typeDefinition` over an in-memory
//! connection.
//!
//! - `declaration` has no separate declare/define construct in GDScript, so it must return
//!   byte-identical targets to `definition` for the same cursor — these tests query both handlers
//!   at the same positions and assert equality (a local-style member use, a native class name, and
//!   an unresolved identifier where both are `null`).
//! - `typeDefinition` resolves the cursor symbol's *type* to that type's declaring site: a project
//!   `class_name` script's identifier (Script kind), a native class's or builtin type's stub header
//!   (Native and Builtin kinds), an enum's own `enum E {` token (Enum kind), or `null` for
//!   Variant/unresolved and for an enum with no locatable owner (never a guess — W10).
//!
//! Native cases REQUIRE a populated `NativeDb`: the bare `boot()` rig used by hover/definition tests
//! has an empty DB (every native name resolves Unresolved → typeDefinition would return `null` for
//! the wrong reason). So this file boots with an `extension_api.json` covering Object←Node←CanvasItem
//! ←Node2D and a small project on disk.
//!
//! Known gap (analyzer pinning, NOT a typeDefinition bug): a cross-file *inferred* script binding —
//! `var e := Enemy.new()` where `Enemy` is a `class_name` in another file — resolves to `null`. The
//! analyzer pins no `smallest_typed_containing`-reachable type on that binding (`definition` on the
//! same symbol is also blind), so the gd_server-only handler has nothing to map. A same-file
//! inferred binding (`var made := make()` with a local `make() -> Enemy`) DOES resolve and is the
//! "inferred" case exercised below; explicit annotations (`var x: Enemy`) resolve regardless.

use std::time::Duration;

use lsp_server::{Connection, Message, Notification, Request, RequestId};
use lsp_types::{
    request::{GotoDeclarationParams, GotoDeclarationResponse},
    ClientCapabilities, DeclarationCapability, GeneralClientCapabilities, GotoDefinitionParams,
    GotoDefinitionResponse, InitializeParams, InitializeResult, InitializedParams,
    PartialResultParams, Position, PositionEncodingKind, TextDocumentIdentifier, TextDocumentItem,
    TextDocumentPositionParams, TypeDefinitionProviderCapability, Uri, WorkDoneProgressParams,
};

fn recv(conn: &Connection) -> Message {
    conn.receiver
        .recv_timeout(Duration::from_secs(10))
        .expect("timed out waiting for a message from the server")
}

/// `recv`, skipping server-initiated notifications (a late `publishDiagnostics` can land where a
/// response was expected on slow hosts) until a `Response` arrives.
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

/// A throwaway on-disk project with a native-class dump, removed on drop. The dump
/// (`Object←Node←CanvasItem←Node2D`) is what makes the analyzer resolve native types to `Native`
/// (rather than the permissive `Unresolved` of the empty-DB `boot()` rig) so typeDefinition's
/// native arm has a real class to anchor.
struct NativeProject {
    root: camino::Utf8PathBuf,
    _dir: tempfile::TempDir,
}

impl NativeProject {
    fn new(files: &[(&str, &str)]) -> Self {
        let dir = tempfile::Builder::new()
            .prefix("gdls_typedef_")
            .tempdir()
            .expect("create temp dir");
        let root = camino::Utf8PathBuf::from_path_buf(dir.path().to_path_buf())
            .expect("temp dir is UTF-8");
        let p = NativeProject { root, _dir: dir };
        p.write("project.godot", "");
        p.write(
            "extension_api.json",
            r#"{
                "header": { "version_major": 4, "version_minor": 6, "version_patch": 3 },
                "classes": [
                    {"name": "Object"},
                    {"name": "Node", "inherits": "Object",
                     "enums": [{"name": "ProcessMode", "is_bitfield": false,
                                "values": [{"name": "PROCESS_MODE_INHERIT", "value": 0},
                                           {"name": "PROCESS_MODE_ALWAYS", "value": 3}]}]},
                    {"name": "CanvasItem", "inherits": "Node"},
                    {"name": "Node2D", "inherits": "CanvasItem"}
                ],
                "builtin_classes": [
                    {"name": "int"},
                    {"name": "Vector2",
                     "members": [{"name": "x", "type": "float"}],
                     "enums": [{"name": "Axis", "is_bitfield": false,
                                "values": [{"name": "AXIS_X", "value": 0},
                                           {"name": "AXIS_Y", "value": 1}]}]}
                ]
            }"#,
        );
        for (rel, contents) in files {
            p.write(rel, contents);
        }
        p
    }

    fn write(&self, rel: &str, contents: &str) {
        let path = self.root.join(rel);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, contents).unwrap();
    }

    fn uri(&self, rel: &str) -> Uri {
        gd_server::uri::path_to_file_uri(&self.root.join(rel)).expect("valid file URI")
    }
}

/// Boot a server over an in-memory connection pointed at `project`'s root + dump, UTF-8 negotiated
/// (LSP characters == bytes for ASCII docs). Returns the connection, the thread handle, and the
/// parsed `InitializeResult` so capability assertions can read the advertised providers.
fn boot(project: &NativeProject) -> (Connection, std::thread::JoinHandle<()>, InitializeResult) {
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
        initialization_options: Some(serde_json::json!({
            "projectRoot": project.root.as_str(),
            "autoDumpExtensionApi": false,
            "extensionApiPath": project.root.join("extension_api.json").as_str(),
        })),
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
    let Message::Response(resp) = recv(&client) else {
        panic!("expected initialize response");
    };
    let result: InitializeResult =
        serde_json::from_value(resp.result.expect("initialize result")).unwrap();
    client
        .sender
        .send(notification(
            "initialized",
            serde_json::to_value(InitializedParams {}).unwrap(),
        ))
        .unwrap();
    (client, handle, result)
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
    // Drain the implicit didOpen diagnostics push.
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

fn position_params(uri: &Uri, position: Position) -> TextDocumentPositionParams {
    TextDocumentPositionParams {
        text_document: TextDocumentIdentifier { uri: uri.clone() },
        position,
    }
}

fn definition_at(
    client: &Connection,
    uri: &Uri,
    position: Position,
) -> Option<GotoDefinitionResponse> {
    let params = GotoDefinitionParams {
        text_document_position_params: position_params(uri, position),
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
    serde_json::from_value(resp.result.expect("definition result is always present"))
        .expect("valid Option<GotoDefinitionResponse>")
}

fn declaration_at(
    client: &Connection,
    uri: &Uri,
    position: Position,
) -> Option<GotoDeclarationResponse> {
    let params = GotoDeclarationParams {
        text_document_position_params: position_params(uri, position),
        work_done_progress_params: WorkDoneProgressParams::default(),
        partial_result_params: PartialResultParams::default(),
    };
    client
        .sender
        .send(request(
            12,
            "textDocument/declaration",
            serde_json::to_value(params).unwrap(),
        ))
        .unwrap();
    let resp = recv_response(client);
    serde_json::from_value(resp.result.expect("declaration result is always present"))
        .expect("valid Option<GotoDeclarationResponse>")
}

fn type_definition_at(
    client: &Connection,
    uri: &Uri,
    position: Position,
) -> Option<GotoDefinitionResponse> {
    // `GotoTypeDefinitionParams`/`-Response` are aliases of the `GotoDefinition*` types.
    let params = GotoDefinitionParams {
        text_document_position_params: position_params(uri, position),
        work_done_progress_params: WorkDoneProgressParams::default(),
        partial_result_params: PartialResultParams::default(),
    };
    client
        .sender
        .send(request(
            13,
            "textDocument/typeDefinition",
            serde_json::to_value(params).unwrap(),
        ))
        .unwrap();
    let resp = recv_response(client);
    serde_json::from_value(
        resp.result
            .expect("typeDefinition result is always present"),
    )
    .expect("valid Option<GotoTypeDefinitionResponse>")
}

fn scalar(resp: GotoDefinitionResponse) -> lsp_types::Location {
    match resp {
        GotoDefinitionResponse::Scalar(loc) => loc,
        other => panic!("expected scalar Location, got {other:?}"),
    }
}

#[test]
fn both_providers_are_advertised() {
    let project = NativeProject::new(&[("a.gd", "extends Node\n")]);
    let (client, handle, init) = boot(&project);

    assert!(
        matches!(
            init.capabilities.declaration_provider,
            Some(DeclarationCapability::Simple(true))
        ),
        "declarationProvider must be advertised as Simple(true), got {:?}",
        init.capabilities.declaration_provider
    );
    assert!(
        matches!(
            init.capabilities.type_definition_provider,
            Some(TypeDefinitionProviderCapability::Simple(true))
        ),
        "typeDefinitionProvider must be advertised as Simple(true), got {:?}",
        init.capabilities.type_definition_provider
    );

    shutdown(&client, handle);
}

#[test]
fn declaration_returns_byte_identical_targets_to_definition() {
    // GDScript has no separate declare/define form: `declaration` must equal `definition` exactly,
    // for every cursor shape. Positions cover the spec's local / member / native cases plus an
    // unresolved name; the assertion compares the full `Option<…>` so the equality holds whether the
    // result is a `Location` or `null`. (declaration delegates to definition by construction, so the
    // point is to exercise that delegation across the distinct cursor pipelines, not just one.)
    let consumer = concat!(
        "extends Node\n",        // line 0 — `Node` is a native class name (cols 8..12)
        "\n",                    // line 1
        "var speed := 1.0\n",    // line 2 — a class member `speed` (cols 4..9)
        "\n",                    // line 3
        "func go() -> void:\n",  // line 4
        "\tvar local := 1\n",    // line 5 — a body-local `local` (cols 5..10)
        "\tprint(local)\n",      // line 6 — body-local use (cols 7..12)
        "\tprint(speed)\n",      // line 7 — member use `speed` (cols 8..13)
        "\tprint(does_not_x)\n", // line 8 — unresolved identifier (cols 8..16)
    );
    let project = NativeProject::new(&[("consumer.gd", consumer)]);
    let (client, handle, _) = boot(&project);
    let uri = project.uri("consumer.gd");
    did_open(&client, &uri, consumer);

    // member-style: a class member used at line 7 → its declaration in this file (a real Location).
    // native-style: the `Node` native class name in `extends Node` at line 0 → native stub header.
    for (line, character, what) in [(7u32, 10u32, "member use"), (0, 9, "native class name")] {
        let def = definition_at(&client, &uri, Position::new(line, character));
        let decl = declaration_at(&client, &uri, Position::new(line, character));
        assert!(
            def.is_some(),
            "{what}: definition should resolve at {line}:{character} (sanity)"
        );
        assert_eq!(
            decl, def,
            "{what}: declaration must be byte-identical to definition at {line}:{character}"
        );
    }

    // local-style and the unresolved name: `definition` returns `null` for a body-local use (the
    // in-file arm covers class members, not function-body locals) and for an unknown identifier;
    // `declaration` must agree on `null` in both — equality holds for the null branch too.
    for (line, character, what) in [(6u32, 8u32, "body-local use"), (8, 10, "unresolved name")] {
        let def = definition_at(&client, &uri, Position::new(line, character));
        let decl = declaration_at(&client, &uri, Position::new(line, character));
        assert!(
            def.is_none(),
            "{what}: definition is null at {line}:{character}"
        );
        assert_eq!(
            decl, def,
            "{what}: declaration must also be null (== definition) at {line}:{character}"
        );
    }

    shutdown(&client, handle);
}

#[test]
fn type_definition_on_script_typed_symbol_jumps_to_class_name_site() {
    // A symbol whose type is a project `class_name` script → that script's `class_name` identifier
    // site (the Script-kind arm). Two shapes, both with the cursor on the SYMBOL (not the type
    // name), so they genuinely exercise type resolution rather than re-testing `definition`:
    //   - `made` — an INFERRED member (`var made := make()`, where `make() -> Enemy`): matches the
    //     spec's "inferred type is a project script" wording.
    //   - `e` — an explicitly annotated member (`var e: Enemy`): the hard-typed path, plus the
    //     discriminating contrast below.
    // (A cross-file *inferred* `var e := Enemy.new()` does NOT resolve — see the module note /
    // report: the analyzer pins no `smallest_typed_containing`-reachable type on that binding. That
    // is an analyzer-pinning gap, gd_server-only code can't close it without touching the frontend.)
    let consumer = concat!(
        "extends Node\n",          // line 0
        "\n",                      // line 1
        "var e: Enemy\n",          // line 2 — annotated member `e` (cols 4..5)
        "var made := make()\n",    // line 3 — inferred member `made` (cols 4..8)
        "\n",                      // line 4
        "func go() -> void:\n",    // line 5
        "\tprint(e)\n",            // line 6 — member use `e` (cols 7..8)
        "\n",                      // line 7
        "func make() -> Enemy:\n", // line 8 — same-file factory returning the project class
        "\treturn Enemy.new()\n",  // line 9
    );
    let project = NativeProject::new(&[
        ("enemy.gd", "class_name Enemy\nextends Node2D\n"),
        ("consumer.gd", consumer),
    ]);
    let (client, handle, _) = boot(&project);
    let uri = project.uri("consumer.gd");
    did_open(&client, &uri, consumer);

    let enemy_uri = project.uri("enemy.gd");
    // `class_name Enemy` — identifier `Enemy` at line 0, cols 11..16 in enemy.gd.
    let enemy_class_name_start = Position::new(0, 11);
    let enemy_class_name_end = Position::new(0, 16);

    // INFERRED member `made` (line 3, col 4) → Enemy's class_name site.
    let loc_inferred = scalar(
        type_definition_at(&client, &uri, Position::new(3, 4))
            .expect("typeDefinition resolves the inferred script type of `made`"),
    );
    assert_eq!(
        loc_inferred.uri,
        enemy_uri,
        "inferred type should point at enemy.gd, got {}",
        loc_inferred.uri.as_str()
    );
    assert_eq!(loc_inferred.range.start, enemy_class_name_start);
    assert_eq!(loc_inferred.range.end, enemy_class_name_end);

    // ANNOTATED member `e` at its declaration (line 2, col 4) → same class_name site.
    let loc = scalar(
        type_definition_at(&client, &uri, Position::new(2, 4))
            .expect("typeDefinition resolves the annotated script type of `e`"),
    );
    assert_eq!(loc.uri, enemy_uri);
    assert_eq!(loc.range.start, enemy_class_name_start);
    assert_eq!(loc.range.end, enemy_class_name_end);

    // And on a USE of `e` (line 6, col 7) — same type target.
    let loc_use = scalar(
        type_definition_at(&client, &uri, Position::new(6, 7))
            .expect("typeDefinition resolves on a use site too"),
    );
    assert_eq!(
        loc_use, loc,
        "use-site type target == declaration-site target"
    );

    // Discriminating contrast: `definition` on the same use lands on `var e` in consumer.gd, NOT
    // enemy.gd — proving typeDefinition resolved the TYPE, not the symbol's own declaration.
    let def = scalar(
        definition_at(&client, &uri, Position::new(6, 7)).expect("definition resolves `e`'s decl"),
    );
    assert_eq!(def.uri, uri, "definition stays in consumer.gd");
    assert_ne!(
        def, loc_use,
        "typeDefinition (Enemy class_name) must differ from definition (var e)"
    );

    shutdown(&client, handle);
}

#[test]
fn type_definition_cross_file_inferred_is_currently_null() {
    // Pins the known analyzer-pinning gap (see the module note): a cross-file *inferred* binding
    // `var x := Enemy.new()` (Enemy a `class_name` in another file) currently resolves to `null` —
    // the analyzer pins no `smallest_typed_containing`-reachable type on it. `definition` still
    // finds the symbol's own binding (proving `x` is a real symbol; only its TYPE is unpinned).
    // This asserts CURRENT behavior: if the frontend later pins cross-file inferred types, this
    // `is_none()` flips — an intentional, visible change to update here, not a silent regression.
    let consumer = concat!(
        "extends Node\n",         // line 0
        "\n",                     // line 1
        "var x := Enemy.new()\n", // line 2 — cross-file inferred member `x` (cols 4..5)
        "\n",                     // line 3
        "func go() -> void:\n",   // line 4
        "\tprint(x)\n",           // line 5 — use of `x` (cols 7..8)
    );
    let project = NativeProject::new(&[
        ("enemy.gd", "class_name Enemy\nextends Node2D\n"),
        ("consumer.gd", consumer),
    ]);
    let (client, handle, _) = boot(&project);
    let uri = project.uri("consumer.gd");
    did_open(&client, &uri, consumer);

    // Sanity: `x` is a resolvable member — `definition` on its use lands on the `var x` declaration.
    assert!(
        definition_at(&client, &uri, Position::new(5, 7)).is_some(),
        "`x` should be a resolvable binding (definition on the use is non-null)"
    );
    // The gap: the cross-file inferred TYPE isn't pinned, so typeDefinition is currently null.
    assert!(
        type_definition_at(&client, &uri, Position::new(2, 4)).is_none(),
        "cross-file inferred `var x := Enemy.new()` currently yields null typeDefinition \
         (analyzer-pinning gap); if this ever resolves, the frontend improved — update intentionally"
    );

    shutdown(&client, handle);
}

#[test]
fn type_definition_on_native_typed_symbol_jumps_to_stub_header() {
    // A symbol declared with a native type (`var n: Node`) → that engine class's stub header
    // (the Native-kind arm). Requires the populated dump (`boot` here points at one).
    let consumer = concat!(
        "extends Node\n", // line 0
        "\n",             // line 1
        "var n: Node\n",  // line 2 — `n` typed Node (cols 4..5)
    );
    let project = NativeProject::new(&[("consumer.gd", consumer)]);
    let (client, handle, _) = boot(&project);
    let uri = project.uri("consumer.gd");
    did_open(&client, &uri, consumer);

    // typeDefinition on `n` (line 2, col 4) → Node's stub `class_name` header.
    let loc = scalar(
        type_definition_at(&client, &uri, Position::new(2, 4))
            .expect("typeDefinition resolves the native type of `n`"),
    );
    assert!(
        loc.uri.as_str().ends_with("/Node.gd"),
        "native type should point at the Node stub, got {}",
        loc.uri.as_str()
    );
    // Stub headers open `class_name <Name>` → the identifier starts at col 11 on line 0.
    assert_eq!(loc.range.start, Position::new(0, 11));
    assert_eq!(loc.range.end, Position::new(0, 15)); // "Node" is 4 chars

    shutdown(&client, handle);
}

#[test]
fn type_definition_on_variant_or_unresolved_returns_null() {
    // A symbol whose type names no single declaring document answers `null` rather than guessing
    // (W10). `Variant` is the whole point of the rule — it stands for every type at once — and an
    // unresolved identifier has no type to declare at all.
    let consumer = concat!(
        "extends Node\n",         // line 0
        "\n",                     // line 1
        "func go() -> void:\n",   // line 2
        "\tvar v: Variant = 1\n", // line 3 — hard Variant (cols 5..6)
        "\tprint(v)\n",           // line 4
        "\tprint(mystery_q)\n",   // line 5 — unresolved identifier (cols 8..17)
    );
    let project = NativeProject::new(&[("consumer.gd", consumer)]);
    let (client, handle, _) = boot(&project);
    let uri = project.uri("consumer.gd");
    did_open(&client, &uri, consumer);

    assert!(
        type_definition_at(&client, &uri, Position::new(3, 5)).is_none(),
        "Variant local ⇒ typeDefinition null (no guess)"
    );
    assert!(
        type_definition_at(&client, &uri, Position::new(4, 8)).is_none(),
        "use of a Variant local ⇒ typeDefinition null"
    );
    assert!(
        type_definition_at(&client, &uri, Position::new(5, 10)).is_none(),
        "unresolved identifier ⇒ typeDefinition null"
    );

    shutdown(&client, handle);
}

/// #393: a builtin type is a document like any other since #370 renders one per `builtin_classes`
/// entry, so `typeDefinition` anchors at that page's header the way the native arm does. An
/// INFERRED builtin answers too — `var v = 1` is a soft `int`, and soft is still a type, so the
/// jump is honest rather than a guess. The one builtin that stays `null` is `Nil`: the absence of
/// a type is not a type.
#[test]
fn type_definition_on_a_builtin_typed_symbol_jumps_to_its_stub_header() {
    let consumer = concat!(
        "extends Node\n",       // line 0
        "\n",                   // line 1
        "var pos: Vector2\n",   // line 2 — annotated (cols 4..7)
        "\n",                   // line 3
        "func go() -> void:\n", // line 4
        "\tvar v = 1\n",        // line 5 — inferred soft int (cols 5..6)
        "\tprint(v)\n",         // line 6
    );
    let project = NativeProject::new(&[("consumer.gd", consumer)]);
    let (client, handle, _) = boot(&project);
    let uri = project.uri("consumer.gd");
    did_open(&client, &uri, consumer);

    let loc = scalar(
        type_definition_at(&client, &uri, Position::new(2, 4))
            .expect("typeDefinition resolves the builtin type of `pos`"),
    );
    assert!(
        loc.uri.as_str().ends_with("/Vector2.gd"),
        "expected the Vector2 stub, got {}",
        loc.uri.as_str()
    );
    // A stub header opens `class_name <Name>` → the identifier starts at col 11 on line 0.
    assert_eq!(loc.range.start, Position::new(0, 11));
    assert_eq!(loc.range.end, Position::new(0, 18)); // "Vector2" is 7 chars

    let inferred = scalar(
        type_definition_at(&client, &uri, Position::new(5, 5))
            .expect("an inferred soft builtin resolves too"),
    );
    assert!(
        inferred.uri.as_str().ends_with("/int.gd"),
        "expected the int stub, got {}",
        inferred.uri.as_str()
    );

    shutdown(&client, handle);
}

/// #393: an enum resolves to its own `enum E {` token, in whichever document declares it. The four
/// shapes below cover every way the owner is recovered — the declaring `FileId` a cross-file enum
/// carries outright, and the `<owner>.<enum>` fqcn every other kind records.
#[test]
fn type_definition_on_an_enum_typed_symbol_jumps_to_the_enum_declaration() {
    let host = concat!(
        "class_name EnumHost\n", // line 0
        "extends Node\n",        // line 1
        "\n",                    // line 2
        "enum Local { A, B }\n", // line 3 — `Local` at cols 5..10
        "\n",                    // line 4
        "class Nested:\n",       // line 5
        "\tenum Deep { X }\n",   // line 6 — `Deep` at cols 6..10
    );
    let consumer = concat!(
        "extends Node\n",                                    // line 0
        "\n",                                                // line 1
        "func go() -> void:\n",                              // line 2
        "\tvar a := EnumHost.Local.A\n",                     // line 3 — `a` at cols 5..6
        "\tvar b := EnumHost.Nested.Deep.X\n",               // line 4 — `b` at cols 5..6
        "\tvar c := Node.ProcessMode.PROCESS_MODE_ALWAYS\n", // line 5 — `c` at cols 5..6
        "\tvar d := Vector2.Axis.AXIS_X\n",                  // line 6 — `d` at cols 5..6
        "\tprint(a, b, c, d)\n",                             // line 7
    );
    let project = NativeProject::new(&[("host.gd", host), ("consumer.gd", consumer)]);
    let (client, handle, _) = boot(&project);
    let uri = project.uri("consumer.gd");
    did_open(&client, &uri, consumer);

    let host_uri = project.uri("host.gd").as_str().to_owned();
    for (label, line, want_uri, want_start, want_end) in [
        (
            "a cross-file head-class enum",
            3u32,
            host_uri.clone(),
            Position::new(3, 5),
            Position::new(3, 10),
        ),
        (
            "a cross-file INNER-class enum",
            4,
            host_uri.clone(),
            Position::new(6, 6),
            Position::new(6, 10),
        ),
    ] {
        let loc = scalar(
            type_definition_at(&client, &uri, Position::new(line, 5))
                .unwrap_or_else(|| panic!("{label} resolves")),
        );
        assert_eq!(loc.uri.as_str(), want_uri, "{label}");
        assert_eq!(loc.range.start, want_start, "{label}");
        assert_eq!(loc.range.end, want_end, "{label}");
    }

    // A native enum and a builtin enum both anchor at their stub page's `enum <Name>` line, which
    // `stubs` renders and keys by the enum's own name.
    for (label, line, stub, name_len) in [
        ("a native-class enum", 5u32, "/Node.gd", 11u32),
        ("a builtin-type enum", 6, "/Vector2.gd", 4),
    ] {
        let loc = scalar(
            type_definition_at(&client, &uri, Position::new(line, 5))
                .unwrap_or_else(|| panic!("{label} resolves")),
        );
        assert!(
            loc.uri.as_str().ends_with(stub),
            "{label}: expected {stub}, got {}",
            loc.uri.as_str()
        );
        // Stub enum lines open `enum <Name> {`, so the identifier starts at col 5.
        assert_eq!(loc.range.start.character, 5, "{label}");
        assert_eq!(loc.range.end.character, 5 + name_len, "{label} name length");
    }

    shutdown(&client, handle);
}

/// The `null` half of #393's enum arm. A `@GlobalScope` enum records a bare enum name with no
/// owner, and the dump publishes no `@GlobalScope` page to anchor in, so it stays `null` rather
/// than borrowing some other document that happens to declare the name (W10).
#[test]
fn type_definition_on_a_global_scope_enum_stays_null() {
    let consumer = concat!(
        "extends Node\n",       // line 0
        "\n",                   // line 1
        "func go() -> void:\n", // line 2
        "\tvar e := OK\n",      // line 3 — `e` at cols 5..6
        "\tprint(e)\n",         // line 4
    );
    let project = NativeProject::new(&[("consumer.gd", consumer)]);
    let (client, handle, _) = boot(&project);
    let uri = project.uri("consumer.gd");
    did_open(&client, &uri, consumer);

    assert!(
        type_definition_at(&client, &uri, Position::new(3, 5)).is_none(),
        "a @GlobalScope enum has no page to anchor in"
    );

    shutdown(&client, handle);
}

#[test]
fn type_definition_inner_class_instance_jumps_to_inner_class() {
    // #146 family: an inner-class INSTANCE (`var x := Inner.new()`) is typed `Script` with a non-empty
    // `inner` chain (the same producer #146 fixed for hover/definition). typeDefinition on that
    // instance must land on the INNER class's identifier (`class Inner:`), not the file ROOT class —
    // the root-lie that persisted in this read-only consumer. Same-file inferred, so the analyzer
    // pins the type (cross-file inferred is the separate, unpinned gap above).
    let src = concat!(
        "class_name Root\n",        // line 0 — root identifier `Root` at cols 11..15
        "\n",                       // line 1
        "class Inner:\n",           // line 2 — inner identifier `Inner` at cols 6..11
        "\tvar field := 0\n",       // line 3
        "\n",                       // line 4
        "func use_it() -> void:\n", // line 5
        "\tvar x := Inner.new()\n", // line 6 — inferred inner instance `x` (cols 5..6)
        "\tprint(x)\n",             // line 7 — use of `x` (cols 7..8)
    );
    let project = NativeProject::new(&[("inner.gd", src)]);
    let (client, handle, _) = boot(&project);
    let uri = project.uri("inner.gd");
    did_open(&client, &uri, src);

    // typeDefinition on the instance `x` (declaration site, line 6 col 5) → the INNER class header.
    let loc = scalar(
        type_definition_at(&client, &uri, Position::new(6, 5))
            .expect("typeDefinition resolves the inner-class instance type of `x`"),
    );
    assert_eq!(loc.uri, uri, "stays in the same file");
    assert_eq!(
        loc.range.start,
        Position::new(2, 6),
        "typeDefinition must land on the INNER class identifier `Inner` (line 2, col 6), \
         not the file root class `Root` (line 0); got {:?}",
        loc.range.start
    );
    assert_eq!(loc.range.end, Position::new(2, 11), "`Inner` is 5 chars");

    // And on a USE of `x` (line 7, col 7) — same inner-class type target.
    let loc_use = scalar(
        type_definition_at(&client, &uri, Position::new(7, 7))
            .expect("typeDefinition resolves the inner-class type on a use site too"),
    );
    assert_eq!(
        loc_use, loc,
        "use-site inner-class type target == declaration-site target"
    );

    shutdown(&client, handle);
}

#[test]
fn type_definition_doubly_nested_inner_class_instance_descends_full_chain() {
    // Depth-2 twin of the test above: a doubly-nested inner-class instance (`Outer.Inner.new()`) is
    // typed `Script` with `inner = ["Outer", "Inner"]`. typeDefinition must descend the FULL chain
    // and land on the innermost `class Inner:` identifier, not `Outer` and not the file root — pinning
    // the per-segment descent loop in `inner_class_identifier_span` (depth-1 alone can't catch a loop
    // that stops one segment early).
    let src = concat!(
        "class_name Root\n",              // line 0 — root identifier `Root`
        "\n",                             // line 1
        "class Outer:\n",                 // line 2 — outer identifier `Outer`
        "\tclass Inner:\n",               // line 3 — inner identifier `Inner` at cols 7..12
        "\t\tvar field := 0\n",           // line 4
        "\n",                             // line 5
        "func use_it() -> void:\n",       // line 6
        "\tvar x := Outer.Inner.new()\n", // line 7 — inferred instance `x` (cols 5..6)
    );
    let project = NativeProject::new(&[("nested.gd", src)]);
    let (client, handle, _) = boot(&project);
    let uri = project.uri("nested.gd");
    did_open(&client, &uri, src);

    // typeDefinition on the instance `x` (line 7, col 5) → the innermost `Inner` class header.
    let loc = scalar(
        type_definition_at(&client, &uri, Position::new(7, 5))
            .expect("typeDefinition resolves the doubly-nested inner-class instance type of `x`"),
    );
    assert_eq!(loc.uri, uri, "stays in the same file");
    assert_eq!(
        loc.range.start,
        Position::new(3, 7),
        "typeDefinition must descend to the innermost `Inner` (line 3, col 7), not `Outer` \
         (line 2) or the file root; got {:?}",
        loc.range.start
    );
    assert_eq!(loc.range.end, Position::new(3, 12), "`Inner` is 5 chars");

    shutdown(&client, handle);
}
