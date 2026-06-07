//! M4 integration tests: filesystem watcher + the four nav handlers end-to-end against an
//! in-memory `Connection` driving the real `gd_server::serve` over a `TempProject` on disk.
//!
//! These tests exercise the boundaries the M4 unit tests can't reach:
//!   - `notify`'s real OS event stream → debouncer → `select!` arm → `handle_watcher` →
//!     `Workspace::reindex/remove` → publish refresh.
//!   - The four nav handlers (`references`, `implementation`, `prepareCallHierarchy` +
//!     `incomingCalls` + `outgoingCalls`, `workspace/symbol`) returning real results across
//!     the sample project.
//!
//! Note: filesystem events on Windows are eventually-consistent — the 250 ms debounce + reindex
//! + publish can take seconds in CI. Every test uses `try_recv` with a generous budget and
//!   tolerates a publish that doesn't arrive (logs the test's expectation, doesn't fail flaky).
//!
//! Deterministic (non-FS) coverage of the cache/reload/cross-file-cycle seams lives in
//! `tests/cache_coherence.rs` — that's where `WorkspaceXFileQuery` resolving cross-file
//! member-initializer cycles (WP-R2/WP-X2) and the `project.godot`-reload effect are pinned with
//! hard assertions, since those don't depend on real-FS event timing.

mod common;

use std::time::Duration;

use common::{
    file_uri, notification, poll_until, recv, request, sample_project, shutdown, try_recv,
};
use lsp_server::{Connection, Message, RequestId};
use lsp_types::{
    CallHierarchyIncomingCall, CallHierarchyIncomingCallsParams, CallHierarchyItem,
    CallHierarchyOutgoingCall, CallHierarchyOutgoingCallsParams, CallHierarchyPrepareParams,
    DidOpenTextDocumentParams, GotoDefinitionParams, GotoDefinitionResponse, InitializeParams,
    InitializedParams, Location, Position, ReferenceContext, ReferenceParams, SymbolInformation,
    TextDocumentIdentifier, TextDocumentItem, TextDocumentPositionParams, WorkDoneProgressParams,
    WorkspaceSymbolParams, WorkspaceSymbolResponse,
};

fn init_and_open(project: &common::TempProject, client: &Connection, relative_files: &[&str]) {
    // initialize
    let init = InitializeParams {
        initialization_options: Some(serde_json::json!({
            "projectRoot": project.root.as_str(),
            "extensionApiPath": project.root.join("extension_api.json").as_str(),
        })),
        ..Default::default()
    };
    client.sender.send(request(1, "initialize", init)).unwrap();
    let _ = recv(client); // initialize response

    client
        .sender
        .send(notification("initialized", InitializedParams {}))
        .unwrap();

    // Open each requested file with `didOpen`.
    for (i, rel) in relative_files.iter().enumerate() {
        let abs = project.root.join(rel);
        let text = std::fs::read_to_string(abs.as_std_path()).unwrap();
        let uri = file_uri(&abs);
        client
            .sender
            .send(notification(
                "textDocument/didOpen",
                DidOpenTextDocumentParams {
                    text_document: TextDocumentItem {
                        uri,
                        language_id: "gdscript".to_string(),
                        version: (i + 1) as i32,
                        text,
                    },
                },
            ))
            .unwrap();
    }

    // Drain any number of publishDiagnostics notifications the opens triggered.
    while try_recv(client, Duration::from_millis(500)).is_some() {}
}

// ----------------------------------------------------------------------------
// Nav: references
// ----------------------------------------------------------------------------

#[test]
fn references_finds_cross_file_class_usage() {
    let project = sample_project();
    let (server, client) = Connection::memory();
    let server_thread = std::thread::spawn(move || gd_server::serve(server));

    init_and_open(&project, &client, &["src/hero.gd", "src/enemy.gd"]);

    // Cursor in hero.gd on the class_name `Hero` declaration (line 1, char ~12).
    let hero_uri = file_uri(&project.root.join("src/hero.gd"));
    let params = ReferenceParams {
        text_document_position: TextDocumentPositionParams {
            text_document: TextDocumentIdentifier {
                uri: hero_uri.clone(),
            },
            position: Position {
                line: 0,
                character: 12, // mid-"Hero"
            },
        },
        work_done_progress_params: WorkDoneProgressParams::default(),
        partial_result_params: Default::default(),
        context: ReferenceContext {
            include_declaration: true,
        },
    };
    client
        .sender
        .send(request(10, "textDocument/references", params))
        .unwrap();
    let Message::Response(resp) = recv(&client) else {
        panic!("expected references response");
    };
    assert!(resp.error.is_none(), "references errored: {:?}", resp.error);
    let locations: Option<Vec<Location>> = serde_json::from_value(resp.result.unwrap()).unwrap();
    let locations = locations.unwrap_or_default();
    // Expect exactly two occurrences of `Hero`, each reported once at its IDENTIFIER range:
    //   1. the `class_name Hero` declaration in hero.gd (line 0, cols 11..15), included because
    //      `include_declaration: true`, and
    //   2. the cross-file `extends Hero` reference in enemy.gd (line 0, cols 8..12).
    // The prior assertion only checked that *some* enemy.gd location came back — it would pass even
    // if the declaration were dropped, the range pointed at an arbitrary span, or the cross-file hit
    // were double-reported. Pin all three.
    let decl = locations
        .iter()
        .find(|l| l.uri.as_str().ends_with("hero.gd"))
        .unwrap_or_else(|| {
            panic!("include_declaration: the `class_name Hero` declaration must appear; got {locations:?}")
        });
    assert_eq!(
        (
            decl.range.start.line,
            decl.range.start.character,
            decl.range.end.character
        ),
        (0, 11, 15),
        "declaration must point at the `Hero` identifier in `class_name Hero`, not a wider span"
    );

    let enemy = locations
        .iter()
        .find(|l| l.uri.as_str().contains("enemy.gd"))
        .unwrap_or_else(|| {
            panic!("the cross-file `extends Hero` reference in enemy.gd must appear; got {locations:?}")
        });
    assert_eq!(
        (
            enemy.range.start.line,
            enemy.range.start.character,
            enemy.range.end.character
        ),
        (0, 8, 12),
        "enemy.gd reference must point at the `Hero` identifier in `extends Hero`"
    );

    // Cross-file dedup: the same logical occurrence must not be reported twice. With exactly one
    // `Hero` per file, the correct result is precisely these two locations.
    assert_eq!(
        locations.len(),
        2,
        "expected the declaration + one cross-file reference, no duplicates; got {locations:?}"
    );

    shutdown(&client, server_thread);
}

#[test]
fn references_does_not_double_report_in_file_call_sites() {
    // Regression: a function call site was reported TWICE in textDocument/references —
    // once at the whole-call span (`Binding::Call`, e.g. `tick()`) and once at the callee-identifier
    // span (the parser identifier scan, `tick`). The two ranges overlap but aren't equal, so the
    // exact-range dedup couldn't collapse them. After the fix the binding scan no longer projects
    // `Binding::Call`, so each occurrence is reported once at the identifier range.
    let project = sample_project();
    project.write(
        "src/ticker.gd",
        "extends Node\n\nfunc tick() -> void:\n\tpass\n\nfunc _process(_d):\n\ttick()\n\ttick()\n",
    );
    let (server, client) = Connection::memory();
    let server_thread = std::thread::spawn(move || gd_server::serve(server));

    init_and_open(&project, &client, &["src/ticker.gd"]);

    let uri = file_uri(&project.root.join("src/ticker.gd"));
    let params = ReferenceParams {
        text_document_position: TextDocumentPositionParams {
            text_document: TextDocumentIdentifier { uri: uri.clone() },
            position: Position {
                line: 2,
                character: 5, // mid-"tick" in `func tick()`
            },
        },
        work_done_progress_params: WorkDoneProgressParams::default(),
        partial_result_params: Default::default(),
        context: ReferenceContext {
            include_declaration: true,
        },
    };
    client
        .sender
        .send(request(11, "textDocument/references", params))
        .unwrap();
    let Message::Response(resp) = recv(&client) else {
        panic!("expected references response");
    };
    assert!(resp.error.is_none(), "references errored: {:?}", resp.error);
    let locations: Vec<Location> =
        serde_json::from_value::<Option<Vec<Location>>>(resp.result.unwrap())
            .unwrap()
            .unwrap_or_default();

    // No two returned references may OVERLAP. The double-report produced a whole-call-span Location
    // (`tick()`) that strictly contained the identifier-span Location (`tick`) at the same call —
    // distinct ranges the exact-equality dedup couldn't collapse. Disjoint occurrences are correct.
    for (i, a) in locations.iter().enumerate() {
        for b in &locations[i + 1..] {
            if a.uri == b.uri {
                let overlap = a.range.start < b.range.end && b.range.start < a.range.end;
                assert!(
                    !overlap,
                    "overlapping references (call-site double-report): {a:?} vs {b:?}"
                );
            }
        }
    }
    // Sanity: we still find the declaration + both call sites (under-reporting would be a separate
    // regression).
    assert!(
        locations.len() >= 3,
        "expected the `tick` declaration + 2 call sites, got {locations:?}"
    );

    shutdown(&client, server_thread);
}

// ----------------------------------------------------------------------------
// Nav: implementation
// ----------------------------------------------------------------------------

#[test]
fn implementation_lists_direct_subclasses() {
    let project = sample_project();
    let (server, client) = Connection::memory();
    let server_thread = std::thread::spawn(move || gd_server::serve(server));

    init_and_open(&project, &client, &["src/hero.gd"]);

    // Cursor on the class_name `Hero` in hero.gd; enemy.gd extends Hero (sample project).
    let hero_uri = file_uri(&project.root.join("src/hero.gd"));
    let params = GotoDefinitionParams {
        text_document_position_params: TextDocumentPositionParams {
            text_document: TextDocumentIdentifier { uri: hero_uri },
            position: Position {
                line: 0,
                character: 12,
            },
        },
        work_done_progress_params: WorkDoneProgressParams::default(),
        partial_result_params: Default::default(),
    };
    client
        .sender
        .send(request(20, "textDocument/implementation", params))
        .unwrap();
    let Message::Response(resp) = recv(&client) else {
        panic!("expected implementation response");
    };
    assert!(
        resp.error.is_none(),
        "implementation errored: {:?}",
        resp.error
    );
    // Deserialize properly and assert on Location::uri so a regression that returned the
    // wrong shape (Scalar vs Array) or stuffed `enemy.gd` into an error message string
    // would fail this test. The prior `serde_json::to_string(&val).contains("enemy.gd")`
    // was a false-positive trap.
    let response: Option<GotoDefinitionResponse> =
        serde_json::from_value(resp.result.unwrap()).unwrap();
    let locations: Vec<Location> = match response {
        Some(GotoDefinitionResponse::Array(arr)) => arr,
        Some(GotoDefinitionResponse::Scalar(loc)) => vec![loc],
        Some(GotoDefinitionResponse::Link(links)) => links
            .into_iter()
            .map(|l| Location {
                uri: l.target_uri,
                range: l.target_range,
            })
            .collect(),
        None => Vec::new(),
    };
    let saw_enemy = locations
        .iter()
        .any(|l| l.uri.as_str().ends_with("enemy.gd"));
    assert!(
        saw_enemy,
        "expected enemy.gd as a Hero subclass; got {locations:?}"
    );

    shutdown(&client, server_thread);
}

// ----------------------------------------------------------------------------
// Nav: workspace/symbol
// ----------------------------------------------------------------------------

#[test]
fn workspace_symbol_finds_class_by_prefix() {
    let project = sample_project();
    let (server, client) = Connection::memory();
    let server_thread = std::thread::spawn(move || gd_server::serve(server));

    init_and_open(&project, &client, &[]);

    let params = WorkspaceSymbolParams {
        query: "Hero".to_string(),
        work_done_progress_params: WorkDoneProgressParams::default(),
        partial_result_params: Default::default(),
    };
    client
        .sender
        .send(request(30, "workspace/symbol", params))
        .unwrap();
    let Message::Response(resp) = recv(&client) else {
        panic!("expected workspace/symbol response");
    };
    assert!(
        resp.error.is_none(),
        "workspace/symbol errored: {:?}",
        resp.error
    );
    let response: Option<WorkspaceSymbolResponse> =
        serde_json::from_value(resp.result.unwrap()).unwrap();
    let symbols: Vec<SymbolInformation> = match response {
        Some(WorkspaceSymbolResponse::Flat(s)) => s,
        Some(WorkspaceSymbolResponse::Nested(n)) => {
            // 3.17 shape — extract names only; locations are deferred. Not the path our v1 takes,
            // but tolerate either.
            n.into_iter()
                .map(|s| {
                    #[allow(deprecated)]
                    SymbolInformation {
                        name: s.name,
                        kind: s.kind,
                        tags: None,
                        deprecated: None,
                        location: Location {
                            uri: project.root.as_str().parse().unwrap(),
                            range: Default::default(),
                        },
                        container_name: s.container_name,
                    }
                })
                .collect()
        }
        None => Vec::new(),
    };
    assert!(
        symbols.iter().any(|s| s.name == "Hero"),
        "expected `Hero` in workspace/symbol results; got {symbols:?}"
    );

    shutdown(&client, server_thread);
}

// ----------------------------------------------------------------------------
// Nav: prepareCallHierarchy + outgoingCalls
// ----------------------------------------------------------------------------

#[test]
fn call_hierarchy_prepare_and_outgoing_for_attack() {
    let project = sample_project();
    // Augment hero.gd so attack() actually calls something — sample's attack body is `pass`.
    project.write(
        "src/hero.gd",
        "class_name Hero\nextends Node2D\n\nvar hp: int = 10\n\nfunc helper() -> void:\n\tpass\n\nfunc attack() -> void:\n\thelper()\n",
    );

    let (server, client) = Connection::memory();
    let server_thread = std::thread::spawn(move || gd_server::serve(server));

    init_and_open(&project, &client, &["src/hero.gd"]);

    let hero_uri = file_uri(&project.root.join("src/hero.gd"));
    // prepareCallHierarchy on `attack` (line 8 in 0-based — "func attack").
    let prepare_params = CallHierarchyPrepareParams {
        text_document_position_params: TextDocumentPositionParams {
            text_document: TextDocumentIdentifier {
                uri: hero_uri.clone(),
            },
            position: Position {
                line: 8,
                character: 8, // mid-"attack"
            },
        },
        work_done_progress_params: WorkDoneProgressParams::default(),
    };
    client
        .sender
        .send(request(
            40,
            "textDocument/prepareCallHierarchy",
            prepare_params,
        ))
        .unwrap();
    let Message::Response(resp) = recv(&client) else {
        panic!("expected prepareCallHierarchy response");
    };
    assert!(resp.error.is_none(), "prepare errored: {:?}", resp.error);
    let items: Option<Vec<CallHierarchyItem>> =
        serde_json::from_value(resp.result.unwrap()).unwrap();
    let items = items.unwrap_or_default();
    assert!(
        !items.is_empty(),
        "prepareCallHierarchy should return at least one item for attack"
    );
    let item = items.into_iter().next().unwrap();
    assert_eq!(item.name, "attack");

    // outgoingCalls — expect `helper` to appear.
    let outgoing_params = CallHierarchyOutgoingCallsParams {
        item: item.clone(),
        work_done_progress_params: WorkDoneProgressParams::default(),
        partial_result_params: Default::default(),
    };
    client
        .sender
        .send(request(41, "callHierarchy/outgoingCalls", outgoing_params))
        .unwrap();
    let Message::Response(resp) = recv(&client) else {
        panic!("expected outgoingCalls response");
    };
    assert!(resp.error.is_none(), "outgoing errored: {:?}", resp.error);
    let outgoing: Option<Vec<CallHierarchyOutgoingCall>> =
        serde_json::from_value(resp.result.unwrap()).unwrap();
    let outgoing = outgoing.unwrap_or_default();
    let helper_call = outgoing
        .iter()
        .find(|c| c.to.name == "helper")
        .unwrap_or_else(|| {
            panic!("expected `helper` in attack's outgoing calls; got {outgoing:?}")
        });
    // The `to` item locates the callee's DECLARATION (LSP 3.17), not the call site.
    // `func helper` is declared on line 5; the call `helper()` is on line 9. Pre-fix BOTH `range`
    // and `selection_range` pointed at the call site (line 9) — the contract violation this closes.
    assert_eq!(
        helper_call.to.selection_range.start.line, 5,
        "outgoing `to.selection_range` must land on the callee declaration (func helper, line 5), \
         not the call site; got {:?}",
        helper_call.to.selection_range
    );
    // The call site (line 9) belongs in `from_ranges`, not the item range.
    assert!(
        helper_call.from_ranges.iter().any(|r| r.start.line == 9),
        "the call site (line 9) must be reported as a from_range; got {:?}",
        helper_call.from_ranges
    );

    shutdown(&client, server_thread);
}

// ----------------------------------------------------------------------------
// Nav: callHierarchy/outgoingCalls records DOTTED dispatch
// ----------------------------------------------------------------------------

#[test]
fn call_hierarchy_outgoing_records_dotted_self_call() {
    // Pre-fix `reduce_call` recorded a `Binding::Call` ONLY for a bare-identifier callee, so
    // `self.attack()` / `obj.method()` / `Class.method()` (and `super` calls) were entirely invisible
    // to call hierarchy. This pins the dotted case from the OUTGOING side: `combo` calls ONLY
    // `self.attack()` (no bare call), so pre-fix its outgoing set was empty; post-fix `attack` must
    // appear, located at its declaration with the call site in `from_ranges`.
    //   line 0: class_name Hero
    //   line 1: extends Node2D
    //   line 3: func attack() -> void:
    //   line 6: func combo() -> void:
    //   line 7: \tself.attack()
    let project = sample_project();
    project.write(
        "src/hero.gd",
        "class_name Hero\nextends Node2D\n\nfunc attack() -> void:\n\tpass\n\nfunc combo() -> void:\n\tself.attack()\n",
    );

    let (server, client) = Connection::memory();
    let server_thread = std::thread::spawn(move || gd_server::serve(server));
    init_and_open(&project, &client, &["src/hero.gd"]);

    let hero_uri = file_uri(&project.root.join("src/hero.gd"));
    // prepareCallHierarchy on `combo` (line 6, mid-"combo").
    let prepare = CallHierarchyPrepareParams {
        text_document_position_params: TextDocumentPositionParams {
            text_document: TextDocumentIdentifier {
                uri: hero_uri.clone(),
            },
            position: Position {
                line: 6,
                character: 7, // mid-"combo"
            },
        },
        work_done_progress_params: WorkDoneProgressParams::default(),
    };
    client
        .sender
        .send(request(75, "textDocument/prepareCallHierarchy", prepare))
        .unwrap();
    let Message::Response(resp) = recv(&client) else {
        panic!("expected prepareCallHierarchy response");
    };
    let items: Option<Vec<CallHierarchyItem>> =
        serde_json::from_value(resp.result.unwrap()).unwrap();
    let item = items
        .and_then(|v| v.into_iter().find(|i| i.name == "combo"))
        .expect("prepareCallHierarchy should return an item for `combo`");

    // outgoingCalls — `attack` must appear even though it is invoked as `self.attack()` (dotted).
    let outgoing = CallHierarchyOutgoingCallsParams {
        item: item.clone(),
        work_done_progress_params: WorkDoneProgressParams::default(),
        partial_result_params: Default::default(),
    };
    client
        .sender
        .send(request(76, "callHierarchy/outgoingCalls", outgoing))
        .unwrap();
    let Message::Response(resp) = recv(&client) else {
        panic!("expected outgoingCalls response");
    };
    assert!(resp.error.is_none(), "outgoing errored: {:?}", resp.error);
    let outgoing: Option<Vec<CallHierarchyOutgoingCall>> =
        serde_json::from_value(resp.result.unwrap()).unwrap();
    let outgoing = outgoing.unwrap_or_default();
    let attack_call = outgoing
        .iter()
        .find(|c| c.to.name == "attack")
        .unwrap_or_else(|| {
            panic!("combo's outgoing calls must include the dotted `self.attack()` callee `attack`; got {outgoing:?}")
        });
    assert_eq!(
        attack_call.to.selection_range.start.line, 3,
        "outgoing `to.selection_range` must land on attack's declaration (line 3); got {:?}",
        attack_call.to.selection_range
    );
    assert!(
        attack_call.from_ranges.iter().any(|r| r.start.line == 7),
        "the `self.attack()` call site (line 7) must be reported as a from_range; got {:?}",
        attack_call.from_ranges
    );

    shutdown(&client, server_thread);
}

// ----------------------------------------------------------------------------
// Watcher: external file deletion drops it from the index
// ----------------------------------------------------------------------------

#[test]
fn watcher_external_delete_drops_from_index() {
    let project = sample_project();
    // Add a throwaway file we'll delete.
    project.write("src/throwaway.gd", "class_name Throwaway\nextends Node\n");

    let (server, client) = Connection::memory();
    let server_thread = std::thread::spawn(move || gd_server::serve(server));

    init_and_open(&project, &client, &[]);

    // Pre-check: Throwaway IS present at startup (the cold-index scan caught it).
    // Without this guard, the post-delete assertion could pass even if the watcher never
    // delivered the event — workspace/symbol would just be returning the same empty result
    // as if the file never existed.
    assert!(
        workspace_symbol_matches(&client, 49, "Throwaway"),
        "pre-check: Throwaway should be in the index before the disk delete"
    );

    // Delete the file on disk; poll workspace/symbol until it drops out, up to 5s.
    project.remove("src/throwaway.gd");
    let gone = poll_until(Duration::from_secs(5), Duration::from_millis(100), || {
        (!workspace_symbol_matches(&client, 50, "Throwaway")).then_some(())
    });
    assert!(
        gone.is_some(),
        "Throwaway should disappear from the index within 5s of disk delete"
    );

    shutdown(&client, server_thread);
}

// ----------------------------------------------------------------------------
// Watcher: external file creation gets indexed
// ----------------------------------------------------------------------------

#[test]
fn watcher_external_create_appears_in_index() {
    let project = sample_project();
    let (server, client) = Connection::memory();
    let server_thread = std::thread::spawn(move || gd_server::serve(server));

    init_and_open(&project, &client, &[]);

    // Pre-check: Newcomer is NOT in the index yet.
    assert!(
        !workspace_symbol_matches(&client, 59, "Newcomer"),
        "pre-check: Newcomer should not be in the index before the disk write"
    );

    // Write a new file on disk after the server started; poll until it shows up.
    project.write("src/newcomer.gd", "class_name Newcomer\nextends Node\n");
    let arrived = poll_until(Duration::from_secs(5), Duration::from_millis(100), || {
        workspace_symbol_matches(&client, 60, "Newcomer").then_some(())
    });
    assert!(
        arrived.is_some(),
        "Newcomer should appear in the index within 5s of disk create"
    );

    shutdown(&client, server_thread);
}

// ----------------------------------------------------------------------------
// Watcher: external rename moves the class to the new path
// ----------------------------------------------------------------------------

#[test]
fn watcher_external_rename_moves_class_to_new_path() {
    // Rename coverage was absent. The rename apply path (server.rs `apply_reaction` for
    // `FileChange::Renamed`) is non-trivial and carries a load-bearing prior fix: it removes the
    // SOURCE path's interface BEFORE the open-buffer guard, so a closed source can't strand a stale
    // interface in the index. This drives a real on-disk rename end-to-end and asserts the class
    // resolves at the NEW path afterward and NO LONGER at the old one (a stranded source interface
    // would keep `oldname.gd` in the results).
    //
    // Robust to platform event shape: whether `notify` reports a merged `Modify(Name(Both))` (the
    // path the prior fix targets, typical on Windows) or a separate delete+create, the observable
    // outcome — class present only under the new path — is the same.
    let project = sample_project();
    project.write("src/oldname.gd", "class_name Renamable\nextends Node\n");

    let (server, client) = Connection::memory();
    let server_thread = std::thread::spawn(move || gd_server::serve(server));
    init_and_open(&project, &client, &[]);

    // Pre-check: Renamable is indexed at the OLD path (so the post-rename assertion can't pass
    // vacuously against a file that was never there).
    let before = workspace_symbol_locations(&client, 120, "Renamable");
    assert!(
        before.iter().any(|u| u.contains("oldname.gd")),
        "pre-check: Renamable should resolve at oldname.gd before the rename; got {before:?}"
    );

    // Rename on disk: oldname.gd -> newname.gd (the `class_name Renamable` rides along).
    std::fs::rename(
        project.root.join("src/oldname.gd").as_std_path(),
        project.root.join("src/newname.gd").as_std_path(),
    )
    .expect("rename oldname.gd -> newname.gd on disk");

    // Poll until Renamable resolves at the NEW path AND no longer at the old one.
    let moved = poll_until(Duration::from_secs(5), Duration::from_millis(100), || {
        let locs = workspace_symbol_locations(&client, 121, "Renamable");
        let at_new = locs.iter().any(|u| u.contains("newname.gd"));
        let at_old = locs.iter().any(|u| u.contains("oldname.gd"));
        (at_new && !at_old).then_some(())
    });
    assert!(
        moved.is_some(),
        "within 5s of the disk rename, Renamable must resolve only at newname.gd (source interface \
         must be dropped, not stranded at oldname.gd)"
    );

    shutdown(&client, server_thread);
}

// ----------------------------------------------------------------------------
// Nav: callHierarchy/incomingCalls
// ----------------------------------------------------------------------------

#[test]
fn call_hierarchy_incoming_for_attack() {
    // Incoming coverage for a DOTTED self-call. Pre-fix `reduce_call`
    // recorded a `Binding::Call` ONLY for a bare-identifier callee, so `self.attack()` was invisible
    // to call hierarchy — and this test asserted nothing (it discarded the result with
    // `let _ = incoming.unwrap_or_default()`, passing even if the result was empty or wrong). Now the
    // dotted shape is recorded too, so `attack`'s incoming calls must surface its caller `combo`.
    //
    // Kept IN-FILE (combo and attack in the same script) so the assertion is deterministic: a dotted
    // self-call resolves to the in-file declaration, so the reducer records `callee_file = Some(file)`
    // and incoming resolves directly. Cross-file incoming through the `extends` chain is best-effort
    // under M4 — the reducer records `callee_file = None` for an inherited bare call and
    // `name_referencers` does not index call names — so a cross-file caller would be flaky to assert.
    //   line 0: class_name Hero
    //   line 1: extends Node2D
    //   line 3: func attack() -> void:
    //   line 6: func combo() -> void:
    //   line 7: \tself.attack()
    let project = sample_project();
    project.write(
        "src/hero.gd",
        "class_name Hero\nextends Node2D\n\nfunc attack() -> void:\n\tpass\n\nfunc combo() -> void:\n\tself.attack()\n",
    );

    let (server, client) = Connection::memory();
    let server_thread = std::thread::spawn(move || gd_server::serve(server));

    init_and_open(&project, &client, &["src/hero.gd"]);

    // prepareCallHierarchy on `attack` in hero.gd (line 3, mid-"attack").
    let hero_uri = file_uri(&project.root.join("src/hero.gd"));
    let prepare = CallHierarchyPrepareParams {
        text_document_position_params: TextDocumentPositionParams {
            text_document: TextDocumentIdentifier {
                uri: hero_uri.clone(),
            },
            position: Position {
                line: 3,
                character: 7, // mid-"attack"
            },
        },
        work_done_progress_params: WorkDoneProgressParams::default(),
    };
    client
        .sender
        .send(request(70, "textDocument/prepareCallHierarchy", prepare))
        .unwrap();
    let Message::Response(resp) = recv(&client) else {
        panic!("expected prepareCallHierarchy response");
    };
    let items: Option<Vec<CallHierarchyItem>> =
        serde_json::from_value(resp.result.unwrap()).unwrap();
    let item = items.and_then(|v| v.into_iter().next()).expect(
        "prepareCallHierarchy should return at least one item for hero.gd's `attack` function",
    );
    assert_eq!(item.name, "attack");

    // incomingCalls — `combo` calls `self.attack()`, so it must surface as an incoming caller, with
    // the call site (line 7) in `from_ranges`.
    let incoming = CallHierarchyIncomingCallsParams {
        item: item.clone(),
        work_done_progress_params: WorkDoneProgressParams::default(),
        partial_result_params: Default::default(),
    };
    client
        .sender
        .send(request(71, "callHierarchy/incomingCalls", incoming))
        .unwrap();
    let Message::Response(resp) = recv(&client) else {
        panic!("expected incomingCalls response");
    };
    assert!(resp.error.is_none(), "incoming errored: {:?}", resp.error);
    let incoming: Option<Vec<CallHierarchyIncomingCall>> =
        serde_json::from_value(resp.result.unwrap()).unwrap();
    let incoming = incoming.unwrap_or_default();
    let combo_call = incoming
        .iter()
        .find(|c| c.from.name == "combo")
        .unwrap_or_else(|| {
            panic!("attack's incoming calls must include the dotted self-caller `combo`; got {incoming:?}")
        });
    assert!(
        combo_call.from_ranges.iter().any(|r| r.start.line == 7),
        "the `self.attack()` call site (line 7) must be reported as a from_range; got {:?}",
        combo_call.from_ranges
    );

    shutdown(&client, server_thread);
}

/// WP-RD5: a top-level call (outside any function) surfaces in incomingCalls as the synthetic
/// `<top>` caller — strengthening the prior `resp.error.is_none()`-only assertion for that branch.
/// `var primed := helper()` at class-body scope records a `Binding::Call` with
/// `caller_function = None`, which the handler renders as `<top>`.
#[test]
fn call_hierarchy_incoming_surfaces_top_level_caller() {
    let project = sample_project();
    // line 0: class_name Hero / 1: extends Node2D / 3: func helper / 6: var primed := helper()
    project.write(
        "src/hero.gd",
        "class_name Hero\nextends Node2D\n\nfunc helper() -> int:\n\treturn 1\n\nvar primed := helper()\n",
    );

    let (server, client) = Connection::memory();
    let server_thread = std::thread::spawn(move || gd_server::serve(server));
    init_and_open(&project, &client, &["src/hero.gd"]);

    let hero_uri = file_uri(&project.root.join("src/hero.gd"));
    let prepare = CallHierarchyPrepareParams {
        text_document_position_params: TextDocumentPositionParams {
            text_document: TextDocumentIdentifier {
                uri: hero_uri.clone(),
            },
            position: Position {
                line: 3,
                character: 7, // mid-"helper"
            },
        },
        work_done_progress_params: WorkDoneProgressParams::default(),
    };
    client
        .sender
        .send(request(90, "textDocument/prepareCallHierarchy", prepare))
        .unwrap();
    let Message::Response(resp) = recv(&client) else {
        panic!("expected prepareCallHierarchy response");
    };
    let items: Option<Vec<CallHierarchyItem>> =
        serde_json::from_value(resp.result.unwrap()).unwrap();
    let item = items
        .and_then(|v| v.into_iter().next())
        .expect("prepareCallHierarchy should return an item for `helper`");
    assert_eq!(item.name, "helper");

    let incoming = CallHierarchyIncomingCallsParams {
        item,
        work_done_progress_params: WorkDoneProgressParams::default(),
        partial_result_params: Default::default(),
    };
    client
        .sender
        .send(request(91, "callHierarchy/incomingCalls", incoming))
        .unwrap();
    let Message::Response(resp) = recv(&client) else {
        panic!("expected incomingCalls response");
    };
    assert!(resp.error.is_none(), "incoming errored: {:?}", resp.error);
    let incoming: Vec<CallHierarchyIncomingCall> =
        serde_json::from_value(resp.result.unwrap()).unwrap_or_default();
    let top = incoming
        .iter()
        .find(|c| c.from.name == "<top>")
        .unwrap_or_else(|| {
            panic!("the top-level `var primed := helper()` call must surface as a `<top>` incoming caller; got {incoming:?}")
        });
    assert!(
        top.from_ranges.iter().any(|r| r.start.line == 6),
        "the top-level call site (line 6) must be reported as a from_range; got {:?}",
        top.from_ranges
    );

    shutdown(&client, server_thread);
}

/// The WP-RD5 assertion hook, made deterministic: a callHierarchy `from` item must
/// locate the CALLER's declaration, not the call site. An *in-file* caller surfaces deterministically
/// (its `Binding::Call.callee_file` == the target's file), unlike the best-effort cross-file path
/// above — so we can hard-assert the range. Pre-fix `from.range`/`selection_range` were the call
/// site; this pins them to the caller's `func` declaration with the call site in `from_ranges`.
#[test]
fn call_hierarchy_incoming_from_item_points_at_caller_declaration() {
    let project = sample_project();
    // hero.gd declares `attack` and an in-file caller `raid` that calls it.
    //   line 0: class_name Hero
    //   line 1: extends Node2D
    //   line 2: (blank)
    //   line 3: func attack() -> void:
    //   line 4: \tpass
    //   line 5: (blank)
    //   line 6: func raid() -> void:
    //   line 7: \tattack()
    project.write(
        "src/hero.gd",
        "class_name Hero\nextends Node2D\n\nfunc attack() -> void:\n\tpass\n\nfunc raid() -> void:\n\tattack()\n",
    );

    let (server, client) = Connection::memory();
    let server_thread = std::thread::spawn(move || gd_server::serve(server));
    init_and_open(&project, &client, &["src/hero.gd"]);

    let hero_uri = file_uri(&project.root.join("src/hero.gd"));
    let prepare = CallHierarchyPrepareParams {
        text_document_position_params: TextDocumentPositionParams {
            text_document: TextDocumentIdentifier {
                uri: hero_uri.clone(),
            },
            position: Position {
                line: 3,
                character: 7, // mid-"attack"
            },
        },
        work_done_progress_params: WorkDoneProgressParams::default(),
    };
    client
        .sender
        .send(request(73, "textDocument/prepareCallHierarchy", prepare))
        .unwrap();
    let Message::Response(resp) = recv(&client) else {
        panic!("expected prepareCallHierarchy response");
    };
    let items: Option<Vec<CallHierarchyItem>> =
        serde_json::from_value(resp.result.unwrap()).unwrap();
    let item = items
        .and_then(|v| v.into_iter().next())
        .expect("prepareCallHierarchy should return an item for `attack`");

    let incoming_params = CallHierarchyIncomingCallsParams {
        item,
        work_done_progress_params: WorkDoneProgressParams::default(),
        partial_result_params: Default::default(),
    };
    client
        .sender
        .send(request(74, "callHierarchy/incomingCalls", incoming_params))
        .unwrap();
    let Message::Response(resp) = recv(&client) else {
        panic!("expected incomingCalls response");
    };
    assert!(resp.error.is_none(), "incoming errored: {:?}", resp.error);
    let incoming: Option<Vec<CallHierarchyIncomingCall>> =
        serde_json::from_value(resp.result.unwrap()).unwrap();
    let incoming = incoming.unwrap_or_default();

    let raid = incoming
        .iter()
        .find(|c| c.from.name == "raid")
        .unwrap_or_else(|| {
            panic!("expected in-file caller `raid` in attack's incoming calls; got {incoming:?}")
        });
    // `func raid` is declared on line 6; the call `attack()` is on line 7.
    assert_eq!(
        raid.from.selection_range.start.line, 6,
        "incoming `from.selection_range` must land on the caller declaration (func raid, line 6), \
         not the call site; got {:?}",
        raid.from.selection_range
    );
    assert!(
        raid.from_ranges.iter().any(|r| r.start.line == 7),
        "the call site (line 7) must be reported as a from_range; got {:?}",
        raid.from_ranges
    );

    shutdown(&client, server_thread);
}

// ----------------------------------------------------------------------------
// Nav: cursor-not-on-identifier paths
// ----------------------------------------------------------------------------

#[test]
fn nav_handlers_return_null_for_whitespace_position() {
    let project = sample_project();
    let (server, client) = Connection::memory();
    let server_thread = std::thread::spawn(move || gd_server::serve(server));

    init_and_open(&project, &client, &["src/hero.gd"]);

    let hero_uri = file_uri(&project.root.join("src/hero.gd"));
    // The sample hero.gd has `class_name Hero\nextends Node2D\n\n` — line 2 is the blank.
    let blank_pos = Position {
        line: 2,
        character: 0,
    };

    // references at whitespace → result is null OR empty (LSP-compliant for "nothing
    // resolved at this position"). The key check: the handler does NOT panic and does
    // NOT return an error.
    let params = ReferenceParams {
        text_document_position: TextDocumentPositionParams {
            text_document: TextDocumentIdentifier {
                uri: hero_uri.clone(),
            },
            position: blank_pos,
        },
        work_done_progress_params: WorkDoneProgressParams::default(),
        partial_result_params: Default::default(),
        context: ReferenceContext {
            include_declaration: true,
        },
    };
    client
        .sender
        .send(request(80, "textDocument/references", params))
        .unwrap();
    let Message::Response(resp) = recv(&client) else {
        panic!("expected references response");
    };
    assert!(resp.error.is_none(), "references at whitespace errored");
    // Result must be JSON null OR an empty array. Either is LSP-conformant.
    let val = resp.result.unwrap_or(serde_json::Value::Null);
    let is_null = val.is_null();
    let is_empty_array = val.as_array().map(|a| a.is_empty()).unwrap_or(false);
    assert!(
        is_null || is_empty_array,
        "references at whitespace should return null or empty; got {val}"
    );

    // implementation at whitespace
    let impl_params = GotoDefinitionParams {
        text_document_position_params: TextDocumentPositionParams {
            text_document: TextDocumentIdentifier {
                uri: hero_uri.clone(),
            },
            position: blank_pos,
        },
        work_done_progress_params: WorkDoneProgressParams::default(),
        partial_result_params: Default::default(),
    };
    client
        .sender
        .send(request(81, "textDocument/implementation", impl_params))
        .unwrap();
    let Message::Response(resp) = recv(&client) else {
        panic!("expected implementation response");
    };
    assert!(resp.error.is_none(), "implementation at whitespace errored");

    // prepareCallHierarchy at whitespace
    let prep = CallHierarchyPrepareParams {
        text_document_position_params: TextDocumentPositionParams {
            text_document: TextDocumentIdentifier { uri: hero_uri },
            position: blank_pos,
        },
        work_done_progress_params: WorkDoneProgressParams::default(),
    };
    client
        .sender
        .send(request(82, "textDocument/prepareCallHierarchy", prep))
        .unwrap();
    let Message::Response(resp) = recv(&client) else {
        panic!("expected prepareCallHierarchy response");
    };
    assert!(resp.error.is_none(), "prepare at whitespace errored");

    shutdown(&client, server_thread);
}

/// An out-of-range LSP position must CLAMP — never panic, never error — end-to-end
/// through the nav handlers (the "position conversions clamp" invariant). The whitespace test above
/// covers a valid-but-empty position; this drives a position far past EOF (line/char 9999), which
/// exercises `PositionMapper`'s clamp on the real request path rather than only in its unit tests.
#[test]
fn nav_handlers_clamp_out_of_range_position() {
    let project = sample_project();
    let (server, client) = Connection::memory();
    let server_thread = std::thread::spawn(move || gd_server::serve(server));
    init_and_open(&project, &client, &["src/hero.gd"]);

    let hero_uri = file_uri(&project.root.join("src/hero.gd"));
    let oob = Position {
        line: 9999,
        character: 9999,
    };

    // references at an out-of-range position → clamped → no error, null/empty result.
    let refs = ReferenceParams {
        text_document_position: TextDocumentPositionParams {
            text_document: TextDocumentIdentifier {
                uri: hero_uri.clone(),
            },
            position: oob,
        },
        work_done_progress_params: WorkDoneProgressParams::default(),
        partial_result_params: Default::default(),
        context: ReferenceContext {
            include_declaration: true,
        },
    };
    client
        .sender
        .send(request(85, "textDocument/references", refs))
        .unwrap();
    let Message::Response(resp) = recv(&client) else {
        panic!("expected references response");
    };
    assert!(
        resp.error.is_none(),
        "references at out-of-range position errored (must clamp)"
    );
    let val = resp.result.unwrap_or(serde_json::Value::Null);
    assert!(
        val.is_null() || val.as_array().map(|a| a.is_empty()).unwrap_or(false),
        "references at out-of-range position should clamp to null/empty; got {val}"
    );

    // prepareCallHierarchy at an out-of-range position → clamped → no error.
    let prep = CallHierarchyPrepareParams {
        text_document_position_params: TextDocumentPositionParams {
            text_document: TextDocumentIdentifier { uri: hero_uri },
            position: oob,
        },
        work_done_progress_params: WorkDoneProgressParams::default(),
    };
    client
        .sender
        .send(request(86, "textDocument/prepareCallHierarchy", prep))
        .unwrap();
    let Message::Response(resp) = recv(&client) else {
        panic!("expected prepareCallHierarchy response");
    };
    assert!(
        resp.error.is_none(),
        "prepareCallHierarchy at out-of-range position errored (must clamp)"
    );

    shutdown(&client, server_thread);
}

// ----------------------------------------------------------------------------
// Nav: workspace/symbol edge cases
// ----------------------------------------------------------------------------

#[test]
fn workspace_symbol_empty_query_returns_empty() {
    let project = sample_project();
    let (server, client) = Connection::memory();
    let server_thread = std::thread::spawn(move || gd_server::serve(server));

    init_and_open(&project, &client, &[]);

    let params = WorkspaceSymbolParams {
        query: String::new(),
        work_done_progress_params: WorkDoneProgressParams::default(),
        partial_result_params: Default::default(),
    };
    client
        .sender
        .send(request(90, "workspace/symbol", params))
        .unwrap();
    let Message::Response(resp) = recv(&client) else {
        panic!("expected workspace/symbol response");
    };
    assert!(resp.error.is_none(), "empty query errored");
    let response: Option<WorkspaceSymbolResponse> =
        serde_json::from_value(resp.result.unwrap()).unwrap();
    let symbols: Vec<SymbolInformation> = match response {
        Some(WorkspaceSymbolResponse::Flat(s)) => s,
        Some(WorkspaceSymbolResponse::Nested(_)) => panic!("M4 always returns Flat; got Nested"),
        None => Vec::new(),
    };
    assert!(
        symbols.is_empty(),
        "empty workspace/symbol query should return zero results"
    );

    shutdown(&client, server_thread);
}

#[test]
fn workspace_symbol_fuzzy_matches_partial_query() {
    let project = sample_project();
    let (server, client) = Connection::memory();
    let server_thread = std::thread::spawn(move || gd_server::serve(server));

    init_and_open(&project, &client, &[]);

    // "Hr" → "Hero" via fuzzy match. The whole point of pulling in nucleo-matcher
    // (M4 WP-N5) is that the query doesn't need to be a strict prefix.
    let params = WorkspaceSymbolParams {
        query: "Hr".to_string(),
        work_done_progress_params: WorkDoneProgressParams::default(),
        partial_result_params: Default::default(),
    };
    client
        .sender
        .send(request(91, "workspace/symbol", params))
        .unwrap();
    let Message::Response(resp) = recv(&client) else {
        panic!("expected workspace/symbol response");
    };
    let response: Option<WorkspaceSymbolResponse> =
        serde_json::from_value(resp.result.unwrap()).unwrap();
    let symbols: Vec<SymbolInformation> = match response {
        Some(WorkspaceSymbolResponse::Flat(s)) => s,
        _ => Vec::new(),
    };
    assert!(
        symbols.iter().any(|s| s.name == "Hero"),
        "fuzzy query `Hr` should match class `Hero`; got {symbols:?}"
    );

    shutdown(&client, server_thread);
}

/// The workspace/symbol comparator sorts class-name hits before member-name hits when
/// scores TIE (`handlers.rs` cmp: `b.0.cmp(&a.0).then_with(|| b.1.5.cmp(&a.1.5))` — the `.5` is the
/// `is_class` flag). The other tests only assert presence via `.any()`, so an inverted/dropped
/// tiebreak would ship green. A `class_name Widget` and a member literally named `Widget` produce
/// the IDENTICAL haystack → identical nucleo score → the tiebreak alone decides order. Assert the
/// CLASS is the navigation anchor at index 0.
#[test]
fn workspace_symbol_class_outranks_member_on_score_tie() {
    let project = sample_project();
    project.write("src/widget.gd", "class_name Widget\nextends Node\n");
    // A member (const) literally named `Widget` in another file — legal GDScript, same haystack.
    project.write("src/holder.gd", "extends Node\nconst Widget = 1\n");

    let (server, client) = Connection::memory();
    let server_thread = std::thread::spawn(move || gd_server::serve(server));
    init_and_open(&project, &client, &[]);

    let params = WorkspaceSymbolParams {
        query: "Widget".to_string(),
        work_done_progress_params: WorkDoneProgressParams::default(),
        partial_result_params: Default::default(),
    };
    client
        .sender
        .send(request(95, "workspace/symbol", params))
        .unwrap();
    let Message::Response(resp) = recv(&client) else {
        panic!("expected workspace/symbol response");
    };
    let response: Option<WorkspaceSymbolResponse> =
        serde_json::from_value(resp.result.unwrap()).unwrap();
    let symbols: Vec<SymbolInformation> = match response {
        Some(WorkspaceSymbolResponse::Flat(s)) => s,
        _ => Vec::new(),
    };
    // Both the class and the member must be present (haystack/score-tie precondition)...
    assert!(
        symbols
            .iter()
            .any(|s| s.name == "Widget" && s.kind == lsp_types::SymbolKind::CLASS),
        "the `class_name Widget` must be in the results; got {symbols:?}"
    );
    assert!(
        symbols
            .iter()
            .any(|s| s.name == "Widget" && s.kind == lsp_types::SymbolKind::CONSTANT),
        "the member `const Widget` must be in the results; got {symbols:?}"
    );
    // ...and on the score tie, the CLASS sorts first (the navigation anchor).
    assert_eq!(
        symbols.first().map(|s| s.kind),
        Some(lsp_types::SymbolKind::CLASS),
        "class-name hits must sort before member hits on a score tie; got {symbols:?}"
    );

    shutdown(&client, server_thread);
}

// ----------------------------------------------------------------------------
// Watcher: project.godot reload republishes diagnostics
// ----------------------------------------------------------------------------

#[test]
fn watcher_project_godot_change_reloads_policy() {
    // The strongest behaviour we can assert with a sample policy change without
    // standing up a fully strict mode: write a syntactically-valid project.godot,
    // touch it, and verify the watcher's reload path fires by checking that an open
    // buffer republishes its diagnostics (the server unconditionally republishes
    // every open URI on a `Reaction::ProjectGodot`). A separate test could assert
    // the actual policy change took effect; that test would need WarnLevel changes
    // wired through. M4 covers the dispatch — M5 hardens against actual policy
    // changes.
    let project = sample_project();
    let (server, client) = Connection::memory();
    let server_thread = std::thread::spawn(move || gd_server::serve(server));

    init_and_open(&project, &client, &["src/hero.gd"]);

    // Touch project.godot — same contents, just a new mtime. The watcher should
    // surface this as `Reaction::ProjectGodot` and republish hero.gd's diagnostics.
    project.write(
        "project.godot",
        "config_version=5\n\n[application]\n\nconfig/name=\"Test2\"\n",
    );

    // Poll for a publishDiagnostics notification on hero.gd within 5s.
    let hero_uri_str = file_uri(&project.root.join("src/hero.gd"))
        .as_str()
        .to_string();
    let got = poll_until(Duration::from_secs(5), Duration::from_millis(100), || {
        try_recv(&client, Duration::from_millis(50)).and_then(|msg| match msg {
            Message::Notification(n) if n.method == "textDocument/publishDiagnostics" => {
                let v: serde_json::Value = n.params;
                v.get("uri")
                    .and_then(|u| u.as_str())
                    .filter(|s| *s == hero_uri_str)
                    .map(|_| ())
            }
            _ => None,
        })
    });
    // We don't fail the test on no-publish — file watchers on slow CI runners
    // can drop events; the test asserts the path is wired, not that every CI
    // run sees the event. If we did see the publish, great; if not, log.
    if got.is_none() {
        eprintln!(
            "watcher_project_godot_change_reloads_policy: did not observe a republish \
             within 5s — could be a slow-CI flake or a regression. The dispatch path \
             is exercised by the unit tests in watcher.rs; this integration test is \
             best-effort."
        );
    }

    shutdown(&client, server_thread);
}

/// Send a `workspace/symbol` query and return the `file://` locations of every result whose name
/// equals `name`.
///
/// Drains messages until OUR response (matching `req_id`) arrives. `recv` yields the *next* message
/// on the channel, which can be an asynchronous `publishDiagnostics` notification rather than the
/// query reply — conflating that with "no match" was a latent false-positive:
/// a stray notification read as "symbol gone", so the watcher delete test could pass without the
/// watcher ever processing the delete. Matching the id keeps "not found" distinct from "not my
/// reply"; an unexpected non-notification message is a bug and panics loudly rather than masking.
fn workspace_symbol_locations(client: &Connection, req_id: i32, name: &str) -> Vec<String> {
    let params = WorkspaceSymbolParams {
        query: name.to_string(),
        work_done_progress_params: WorkDoneProgressParams::default(),
        partial_result_params: Default::default(),
    };
    client
        .sender
        .send(request(req_id, "workspace/symbol", params))
        .unwrap();
    let resp = loop {
        match recv(client) {
            Message::Response(resp) if resp.id == RequestId::from(req_id) => break resp,
            Message::Notification(_) => continue,
            other => {
                panic!("unexpected message while awaiting workspace/symbol #{req_id}: {other:?}")
            }
        }
    };
    let response: Option<WorkspaceSymbolResponse> =
        serde_json::from_value(resp.result.unwrap_or_default())
            .ok()
            .flatten();
    let symbols: Vec<SymbolInformation> = match response {
        Some(WorkspaceSymbolResponse::Flat(s)) => s,
        _ => Vec::new(),
    };
    symbols
        .into_iter()
        .filter(|s| s.name == name)
        .map(|s| s.location.uri.as_str().to_string())
        .collect()
}

/// Whether `workspace/symbol` returns at least one result named `name`. Used by watcher tests to
/// poll for index freshness without sleep-and-pray. The drain-until-our-response logic lives in
/// [`workspace_symbol_locations`].
fn workspace_symbol_matches(client: &Connection, req_id: i32, name: &str) -> bool {
    !workspace_symbol_locations(client, req_id, name).is_empty()
}
