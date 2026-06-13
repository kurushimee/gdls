//! M9 (#71) integration tests: the lazy `workspaceSymbol/resolve` path.
//!
//! Drives `gd_server::serve` over an in-memory `Connection` against the on-disk `sample_project()`
//! (so `workspace/symbol` queries a real `class_name` registry + interface index). Covers the
//! gated-vs-ungated projection of `workspace/symbol` and the resolve round-trip:
//!   - With `workspace.symbol.resolveSupport` → the query returns `WorkspaceSymbol[]` with a
//!     location-SANS-full-range (uri only), and `workspaceSymbol/resolve` fills the precise range
//!     by touching one file.
//!   - Without it → the byte-identical `SymbolInformation[]` path with eager full ranges.
//!   - The resolved range EQUALS the eager flat-path range for the same symbol.
//!   - `workspaceSymbolProvider.resolveProvider == true` is advertised.
//!   - Malformed / absent `data` resolves to the item unchanged (never panics).

mod common;

use common::{file_uri, notification, recv_response, request, sample_project, shutdown, try_recv};
use lsp_server::Connection;
use lsp_types::{
    ClientCapabilities, GeneralClientCapabilities, InitializeParams, InitializeResult,
    InitializedParams, Location, OneOf, PositionEncodingKind, SymbolInformation,
    WorkDoneProgressParams, WorkspaceClientCapabilities, WorkspaceSymbol,
    WorkspaceSymbolClientCapabilities, WorkspaceSymbolOptions, WorkspaceSymbolParams,
    WorkspaceSymbolResolveSupportCapability, WorkspaceSymbolResponse,
};
use std::time::Duration;

/// UTF-8 so LSP character offsets equal byte offsets for the ASCII sample project.
fn utf8_general() -> Option<GeneralClientCapabilities> {
    Some(GeneralClientCapabilities {
        position_encodings: Some(vec![PositionEncodingKind::UTF8]),
        ..Default::default()
    })
}

/// Client capabilities that advertise `workspace.symbol.resolveSupport` (the 3.17 partial-symbol
/// opt-in), naming `location.range` as the property pulled lazily — the VS Code shape.
fn caps_with_resolve_support() -> ClientCapabilities {
    ClientCapabilities {
        general: utf8_general(),
        workspace: Some(WorkspaceClientCapabilities {
            symbol: Some(WorkspaceSymbolClientCapabilities {
                resolve_support: Some(WorkspaceSymbolResolveSupportCapability {
                    properties: vec!["location.range".to_string()],
                }),
                ..Default::default()
            }),
            ..Default::default()
        }),
        ..Default::default()
    }
}

/// Client capabilities WITHOUT `resolveSupport` — a 3.16-era client that must get the flat
/// `SymbolInformation[]` shape with eager ranges.
fn caps_without_resolve_support() -> ClientCapabilities {
    ClientCapabilities {
        general: utf8_general(),
        ..Default::default()
    }
}

/// Boot the server over an in-memory connection against `project`, completing the
/// `initialize`/`initialized` handshake with the given `capabilities` AND the project root
/// `initializationOptions` (both are needed: caps select the symbol shape, the options load the
/// real index `workspace/symbol` queries). Returns the client connection, the server thread, and
/// the deserialized `InitializeResult` (so a test can assert advertised capabilities).
fn boot(
    project: &common::TempProject,
    capabilities: ClientCapabilities,
) -> (
    Connection,
    std::thread::JoinHandle<anyhow::Result<()>>,
    InitializeResult,
) {
    let (server, client) = Connection::memory();
    let handle = std::thread::spawn(move || gd_server::serve(server));

    let init = InitializeParams {
        capabilities,
        initialization_options: Some(serde_json::json!({
            "projectRoot": project.root.as_str(),
            "autoDumpExtensionApi": false,
            "extensionApiPath": project.root.join("extension_api.json").as_str(),
        })),
        ..Default::default()
    };
    client.sender.send(request(1, "initialize", init)).unwrap();
    let resp = recv_response(&client);
    assert!(resp.error.is_none(), "initialize errored: {:?}", resp.error);
    let init_result: InitializeResult = serde_json::from_value(resp.result.unwrap()).unwrap();

    client
        .sender
        .send(notification("initialized", InitializedParams {}))
        .unwrap();
    // Drain any publishDiagnostics the handshake produced.
    while try_recv(&client, Duration::from_millis(300)).is_some() {}

    (client, handle, init_result)
}

fn query_params(query: &str) -> WorkspaceSymbolParams {
    WorkspaceSymbolParams {
        query: query.to_string(),
        work_done_progress_params: WorkDoneProgressParams::default(),
        partial_result_params: Default::default(),
    }
}

/// `resolveProvider == true` is advertised in `InitializeResult` (so a client knows it may issue
/// `workspaceSymbol/resolve`).
#[test]
fn advertises_workspace_symbol_resolve_provider() {
    let project = sample_project();
    let (client, handle, init_result) = boot(&project, caps_with_resolve_support());

    let provider = init_result
        .capabilities
        .workspace_symbol_provider
        .expect("workspace_symbol_provider must be advertised");
    match provider {
        OneOf::Right(WorkspaceSymbolOptions {
            resolve_provider, ..
        }) => assert_eq!(
            resolve_provider,
            Some(true),
            "resolveProvider must be advertised true"
        ),
        OneOf::Left(_) => panic!("expected the options form with resolveProvider, got a bare bool"),
    }

    shutdown(&client, handle);
}

/// With `resolveSupport`: `workspace/symbol` returns the 3.17 `WorkspaceSymbol[]` shape, each with
/// a location that carries ONLY a uri (no range), and `workspaceSymbol/resolve` on one item fills
/// the precise range — touching exactly one file (the path in the item's `data`).
#[test]
fn with_resolve_support_query_is_rangeless_and_resolve_fills_it() {
    let project = sample_project();
    let (client, handle, _init) = boot(&project, caps_with_resolve_support());

    client
        .sender
        .send(request(30, "workspace/symbol", query_params("Hero")))
        .unwrap();
    let resp = recv_response(&client);
    assert!(
        resp.error.is_none(),
        "workspace/symbol errored: {:?}",
        resp.error
    );
    let response: Option<WorkspaceSymbolResponse> =
        serde_json::from_value(resp.result.unwrap()).unwrap();
    let symbols = match response {
        Some(WorkspaceSymbolResponse::Nested(n)) => n,
        other => panic!("resolveSupport client must get the Nested shape; got {other:?}"),
    };

    // EVERY returned symbol carries a uri-only location (the partial shape) — no full range eagerly.
    for s in &symbols {
        match &s.location {
            OneOf::Right(loc) => assert!(
                loc.uri.as_str().ends_with(".gd"),
                "uri-only location should point at a .gd file; got {:?}",
                loc.uri
            ),
            OneOf::Left(loc) => panic!(
                "pre-resolve location must be uri-only (no range); got a full Location: {loc:?}"
            ),
        }
        assert!(
            s.data.is_some(),
            "each partial WorkspaceSymbol must carry a `data` resolve key; {} had none",
            s.name
        );
    }

    // Pick the `Hero` class symbol and resolve it.
    let hero = symbols
        .iter()
        .find(|s| s.name == "Hero")
        .cloned()
        .unwrap_or_else(|| panic!("expected `Hero`; got {symbols:?}"));

    // The data blob is the self-sufficient key: path + name span (W18 — extension rides `data`).
    let data = hero.data.clone().expect("Hero carries a data blob");
    let resolve_path = data.get("path").and_then(|v| v.as_str()).unwrap();
    assert!(
        resolve_path.ends_with("hero.gd"),
        "resolve key points at the declaring file; got {resolve_path}"
    );

    client
        .sender
        .send(request(31, "workspaceSymbol/resolve", hero))
        .unwrap();
    let resp = recv_response(&client);
    assert!(
        resp.error.is_none(),
        "workspaceSymbol/resolve errored: {:?}",
        resp.error
    );
    let resolved: WorkspaceSymbol = serde_json::from_value(resp.result.unwrap()).unwrap();

    // Post-resolve: a full Location with the precise name-token range.
    let loc = match resolved.location {
        OneOf::Left(loc) => loc,
        OneOf::Right(l) => panic!("resolve must fill a full Location; still uri-only: {l:?}"),
    };
    assert!(loc.uri.as_str().ends_with("hero.gd"));
    // `class_name Hero` — the identifier `Hero` is at line 0, cols 11..15 (UTF-8 = byte cols).
    assert_eq!(loc.range.start.line, 0, "Hero is declared on line 0");
    assert_eq!(loc.range.start.character, 11, "`Hero` starts at column 11");
    assert_eq!(loc.range.end.character, 15, "`Hero` ends at column 15");

    shutdown(&client, handle);
}

/// Without `resolveSupport`: `workspace/symbol` returns the flat `SymbolInformation[]` shape with
/// eager full ranges (the byte-identical pre-#71 behavior). A client that did not opt into the
/// partial shape must not receive it.
#[test]
fn without_resolve_support_query_is_flat_with_full_ranges() {
    let project = sample_project();
    let (client, handle, _init) = boot(&project, caps_without_resolve_support());

    client
        .sender
        .send(request(40, "workspace/symbol", query_params("Hero")))
        .unwrap();
    let resp = recv_response(&client);
    assert!(
        resp.error.is_none(),
        "workspace/symbol errored: {:?}",
        resp.error
    );
    let response: Option<WorkspaceSymbolResponse> =
        serde_json::from_value(resp.result.unwrap()).unwrap();
    let symbols: Vec<SymbolInformation> = match response {
        Some(WorkspaceSymbolResponse::Flat(s)) => s,
        other => panic!("no-resolveSupport client must get the Flat shape; got {other:?}"),
    };

    let hero = symbols
        .iter()
        .find(|s| s.name == "Hero")
        .unwrap_or_else(|| panic!("expected `Hero`; got {symbols:?}"));
    // Eager range is the precise `Hero` identifier span — NOT a zero-width fallback.
    assert!(hero.location.uri.as_str().ends_with("hero.gd"));
    assert_eq!(hero.location.range.start.line, 0);
    assert_eq!(hero.location.range.start.character, 11);
    assert_eq!(hero.location.range.end.character, 15);

    shutdown(&client, handle);
}

/// The range `workspaceSymbol/resolve` fills EQUALS the range the eager flat path returns for the
/// same symbol — the lazy path is a deferral, not a different answer. Queries both ways against the
/// same project and compares.
#[test]
fn resolved_range_equals_eager_flat_range() {
    let project = sample_project();

    // Eager: a no-resolveSupport client gets the flat range for `Hero`.
    let (flat_client, flat_handle, _) = boot(&project, caps_without_resolve_support());
    flat_client
        .sender
        .send(request(50, "workspace/symbol", query_params("Hero")))
        .unwrap();
    let resp = recv_response(&flat_client);
    let flat: Vec<SymbolInformation> = match serde_json::from_value(resp.result.unwrap()).unwrap() {
        Some(WorkspaceSymbolResponse::Flat(s)) => s,
        other => panic!("expected Flat; got {other:?}"),
    };
    let eager_loc: Location = flat
        .into_iter()
        .find(|s| s.name == "Hero")
        .map(|s| s.location)
        .expect("Hero in flat results");
    shutdown(&flat_client, flat_handle);

    // Lazy: a resolveSupport client gets the partial shape, then resolves `Hero`.
    let (lazy_client, lazy_handle, _) = boot(&project, caps_with_resolve_support());
    lazy_client
        .sender
        .send(request(51, "workspace/symbol", query_params("Hero")))
        .unwrap();
    let resp = recv_response(&lazy_client);
    let nested: Vec<WorkspaceSymbol> = match serde_json::from_value(resp.result.unwrap()).unwrap() {
        Some(WorkspaceSymbolResponse::Nested(n)) => n,
        other => panic!("expected Nested; got {other:?}"),
    };
    let hero = nested
        .into_iter()
        .find(|s| s.name == "Hero")
        .expect("Hero in nested results");
    lazy_client
        .sender
        .send(request(52, "workspaceSymbol/resolve", hero))
        .unwrap();
    let resp = recv_response(&lazy_client);
    let resolved: WorkspaceSymbol = serde_json::from_value(resp.result.unwrap()).unwrap();
    let lazy_loc = match resolved.location {
        OneOf::Left(loc) => loc,
        OneOf::Right(l) => panic!("resolve must fill a full Location; still uri-only: {l:?}"),
    };
    shutdown(&lazy_client, lazy_handle);

    assert_eq!(
        lazy_loc, eager_loc,
        "the resolved Location must equal the eager flat Location for the same symbol"
    );
}

/// A `member` symbol (not a class) also round-trips: the partial shape carries the member name +
/// container, and resolve fills its precise range. Guards that the path isn't class-only.
#[test]
fn member_symbol_resolves_its_range() {
    let project = sample_project();
    let (client, handle, _init) = boot(&project, caps_with_resolve_support());

    // `attack` is a func member of hero.gd (`func attack() -> void:` on line 5).
    client
        .sender
        .send(request(60, "workspace/symbol", query_params("attack")))
        .unwrap();
    let resp = recv_response(&client);
    let nested: Vec<WorkspaceSymbol> = match serde_json::from_value(resp.result.unwrap()).unwrap() {
        Some(WorkspaceSymbolResponse::Nested(n)) => n,
        other => panic!("expected Nested; got {other:?}"),
    };
    let attack = nested
        .into_iter()
        .find(|s| s.name == "attack")
        .unwrap_or_else(|| panic!("expected member `attack`"));
    assert!(
        matches!(attack.location, OneOf::Right(_)),
        "member location must be uri-only before resolve"
    );

    client
        .sender
        .send(request(61, "workspaceSymbol/resolve", attack))
        .unwrap();
    let resp = recv_response(&client);
    let resolved: WorkspaceSymbol = serde_json::from_value(resp.result.unwrap()).unwrap();
    let loc = match resolved.location {
        OneOf::Left(loc) => loc,
        OneOf::Right(l) => panic!("resolve must fill a full Location; got {l:?}"),
    };
    // `func attack()` is on line 5 (0-based); the identifier `attack` follows `func ` at col 5.
    assert!(loc.uri.as_str().ends_with("hero.gd"));
    assert_eq!(loc.range.start.line, 5, "attack is declared on line 5");
    assert_eq!(loc.range.start.character, 5, "`attack` starts at column 5");
    assert_eq!(loc.range.end.character, 11, "`attack` ends at column 11");

    shutdown(&client, handle);
}

/// Malformed / absent `data` (and a non-object data value) resolve to the item UNCHANGED — never a
/// panic, never a dropped symbol (workflow DoD §4: malformed/partial input must be tolerated). The
/// returned item keeps its original uri-only location.
#[test]
fn resolve_with_malformed_data_returns_item_unchanged() {
    let project = sample_project();
    let (client, handle, _init) = boot(&project, caps_with_resolve_support());

    let hero_uri = file_uri(&project.root.join("src/hero.gd"));

    // Three malformed inputs, each must come back with its location intact (uri-only) and no error.
    let cases = vec![
        // No `data` at all.
        WorkspaceSymbol {
            name: "Ghost".to_string(),
            kind: lsp_types::SymbolKind::CLASS,
            tags: None,
            container_name: None,
            location: OneOf::Right(lsp_types::WorkspaceLocation {
                uri: hero_uri.clone(),
            }),
            data: None,
        },
        // `data` present but missing the span fields.
        WorkspaceSymbol {
            name: "Ghost".to_string(),
            kind: lsp_types::SymbolKind::CLASS,
            tags: None,
            container_name: None,
            location: OneOf::Right(lsp_types::WorkspaceLocation {
                uri: hero_uri.clone(),
            }),
            data: Some(serde_json::json!({ "path": "/nope" })),
        },
        // `data` is not even an object.
        WorkspaceSymbol {
            name: "Ghost".to_string(),
            kind: lsp_types::SymbolKind::CLASS,
            tags: None,
            container_name: None,
            location: OneOf::Right(lsp_types::WorkspaceLocation { uri: hero_uri }),
            data: Some(serde_json::json!("not-an-object")),
        },
    ];

    for (i, item) in cases.into_iter().enumerate() {
        let before = item.location.clone();
        client
            .sender
            .send(request(70 + i as i32, "workspaceSymbol/resolve", item))
            .unwrap();
        let resp = recv_response(&client);
        assert!(
            resp.error.is_none(),
            "malformed resolve must not error (case {i}): {:?}",
            resp.error
        );
        let resolved: WorkspaceSymbol = serde_json::from_value(resp.result.unwrap()).unwrap();
        assert_eq!(
            resolved.location, before,
            "malformed resolve (case {i}) must return the location unchanged"
        );
    }

    shutdown(&client, handle);
}
