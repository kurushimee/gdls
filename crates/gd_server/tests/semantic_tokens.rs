//! M10 (#72): `textDocument/semanticTokens/{full,full/delta,range}` — Connection protocol-shape
//! coverage. Standard-legend-only (the #30 highlighting target).
//!
//! Covers the phase-2 acceptance criteria:
//!   1. The advertised legend is STANDARD token types/modifiers ONLY (snapshot — fails on any
//!      custom name).
//!   2. A `full` request over a representative `.gd` colors every mapped kind with the right
//!      (type, modifiers): type/class, enum/enumMember, function/method/static, signal/event,
//!      annotation/decorator, const readonly, parameter, property, local variable.
//!   3. `full` → edit → `full/delta` → apply-the-delta == a fresh `full` (delta round-trip on the
//!      flat integer array, exactly as a client applies it).
//!   4. `range` returns only the tokens intersecting the requested range.
//!   5. A reduced-legend client receives only its declared types/modifiers; `augmentsSyntaxTokens`
//!      true vs false produces identical output (gdls emits no base-grammar tokens, so there is
//!      nothing to suppress — the correct generic-LSP-first behavior).

mod common;

use common::{file_uri, notification, request, shutdown, TempProject};
use lsp_server::Connection;
use lsp_types::{
    ClientCapabilities, DidOpenTextDocumentParams, InitializeParams, InitializeResult,
    InitializedParams, PartialResultParams, Position, Range, SemanticToken, SemanticTokenModifier,
    SemanticTokenType, SemanticTokens, SemanticTokensClientCapabilities,
    SemanticTokensClientCapabilitiesRequests, SemanticTokensDelta, SemanticTokensFullDeltaResult,
    SemanticTokensFullOptions, SemanticTokensParams, SemanticTokensRangeParams,
    SemanticTokensRangeResult, SemanticTokensResult, SemanticTokensServerCapabilities,
    TextDocumentClientCapabilities, TextDocumentContentChangeEvent, TextDocumentIdentifier,
    TextDocumentItem, Uri, VersionedTextDocumentIdentifier, WorkDoneProgressParams,
};

/// Initialize against `project` with the given client capabilities, returning the parsed
/// `InitializeResult`, then send `initialized` and open `files` (draining diagnostics).
fn init_and_open_caps(
    project: &TempProject,
    client: &Connection,
    files: &[(&str, &str)],
    caps: ClientCapabilities,
) -> InitializeResult {
    let init = InitializeParams {
        initialization_options: Some(serde_json::json!({
            "projectRoot": project.root.as_str(),
            "autoDumpExtensionApi": false,
            "extensionApiPath": project.root.join("extension_api.json").as_str(),
        })),
        capabilities: caps,
        ..Default::default()
    };
    client.sender.send(request(1, "initialize", init)).unwrap();
    let init_resp = common::recv_response(client);
    assert!(
        init_resp.error.is_none(),
        "initialize errored: {:?}",
        init_resp.error
    );
    let result: InitializeResult =
        serde_json::from_value(init_resp.result.expect("initialize result")).unwrap();

    client
        .sender
        .send(notification("initialized", InitializedParams {}))
        .unwrap();

    for (i, (rel, text)) in files.iter().enumerate() {
        project.write(rel, text);
        let abs = project.root.join(rel);
        let uri = file_uri(&abs);
        client
            .sender
            .send(notification(
                "textDocument/didOpen",
                DidOpenTextDocumentParams {
                    text_document: TextDocumentItem {
                        uri,
                        language_id: "gdscript".to_string(),
                        version: (i + 2) as i32,
                        text: text.to_string(),
                    },
                },
            ))
            .unwrap();
    }
    while common::try_recv(client, std::time::Duration::from_millis(300)).is_some() {}
    result
}

/// A base project (project.godot + the mini native API), no source files — tests write their own.
fn base_project() -> TempProject {
    let p = TempProject::new();
    p.write("project.godot", "config_version=5\n");
    p.write("extension_api.json", common::MINI_API);
    p
}

/// Client capabilities advertising semantic tokens with gdls's FULL standard legend (the common
/// case — a client that supports every standard name). `augments` sets `augmentsSyntaxTokens`.
fn full_legend_caps(augments: Option<bool>) -> ClientCapabilities {
    client_caps_with_legend(
        &[
            SemanticTokenType::CLASS,
            SemanticTokenType::ENUM,
            SemanticTokenType::ENUM_MEMBER,
            SemanticTokenType::FUNCTION,
            SemanticTokenType::METHOD,
            SemanticTokenType::PROPERTY,
            SemanticTokenType::PARAMETER,
            SemanticTokenType::VARIABLE,
            SemanticTokenType::EVENT,
            SemanticTokenType::DECORATOR,
        ],
        &[
            SemanticTokenModifier::DECLARATION,
            SemanticTokenModifier::DEFINITION,
            SemanticTokenModifier::READONLY,
            SemanticTokenModifier::STATIC,
            SemanticTokenModifier::DEFAULT_LIBRARY,
            SemanticTokenModifier::DEPRECATED,
        ],
        augments,
    )
}

fn client_caps_with_legend(
    types: &[SemanticTokenType],
    modifiers: &[SemanticTokenModifier],
    augments: Option<bool>,
) -> ClientCapabilities {
    ClientCapabilities {
        text_document: Some(TextDocumentClientCapabilities {
            semantic_tokens: Some(SemanticTokensClientCapabilities {
                dynamic_registration: None,
                requests: SemanticTokensClientCapabilitiesRequests {
                    range: Some(true),
                    full: Some(SemanticTokensFullOptions::Delta { delta: Some(true) }),
                },
                token_types: types.to_vec(),
                token_modifiers: modifiers.to_vec(),
                formats: vec![],
                overlapping_token_support: None,
                multiline_token_support: None,
                server_cancel_support: None,
                augments_syntax_tokens: augments,
            }),
            ..Default::default()
        }),
        ..Default::default()
    }
}

fn full_params(uri: &Uri) -> SemanticTokensParams {
    SemanticTokensParams {
        work_done_progress_params: WorkDoneProgressParams::default(),
        partial_result_params: PartialResultParams::default(),
        text_document: TextDocumentIdentifier { uri: uri.clone() },
    }
}

/// Request `semanticTokens/full` and return the parsed `SemanticTokens` (asserts no error).
fn request_full(client: &Connection, id: i32, uri: &Uri) -> SemanticTokens {
    client
        .sender
        .send(request(
            id,
            "textDocument/semanticTokens/full",
            full_params(uri),
        ))
        .unwrap();
    let resp = common::recv_response(client);
    assert!(
        resp.error.is_none(),
        "semanticTokens/full errored: {:?}",
        resp.error
    );
    let result: SemanticTokensResult =
        serde_json::from_value(resp.result.expect("full result")).unwrap();
    match result {
        SemanticTokensResult::Tokens(t) => t,
        SemanticTokensResult::Partial(_) => panic!("expected full Tokens, got a partial result"),
    }
}

/// A decoded token (absolute line/char from the delta stream) with its resolved type/modifier names.
#[derive(Debug, Clone)]
struct Decoded {
    line: u32,
    start: u32,
    len: u32,
    ty: String,
    mods: Vec<String>,
}

/// Decode a delta-encoded `SemanticTokens` against the legend the SERVER advertised, into absolute
/// positions + human-readable type/modifier names. Mirrors how a client interprets the stream.
fn decode(tokens: &SemanticTokens, legend: &lsp_types::SemanticTokensLegend) -> Vec<Decoded> {
    let mut out = Vec::new();
    let mut line = 0u32;
    let mut start = 0u32;
    for t in &tokens.data {
        if t.delta_line != 0 {
            line += t.delta_line;
            start = t.delta_start;
        } else {
            start += t.delta_start;
        }
        let ty = legend.token_types[t.token_type as usize]
            .as_str()
            .to_string();
        let mut mods = Vec::new();
        for (bit, m) in legend.token_modifiers.iter().enumerate() {
            if t.token_modifiers_bitset & (1 << bit) != 0 {
                mods.push(m.as_str().to_string());
            }
        }
        out.push(Decoded {
            line,
            start,
            len: t.length,
            ty,
            mods,
        });
    }
    out
}

/// The server-advertised legend from an `InitializeResult` (so `decode` interprets indices exactly
/// as the wire defines them).
fn server_legend(init: &InitializeResult) -> lsp_types::SemanticTokensLegend {
    match init
        .capabilities
        .semantic_tokens_provider
        .as_ref()
        .expect("semanticTokensProvider must be advertised")
    {
        SemanticTokensServerCapabilities::SemanticTokensOptions(o) => o.legend.clone(),
        SemanticTokensServerCapabilities::SemanticTokensRegistrationOptions(o) => {
            o.semantic_tokens_options.legend.clone()
        }
    }
}

/// Criterion 1: the advertised legend is STANDARD names only — the exact standard set, in order. A
/// custom name added to either table fails this immediately.
#[test]
fn legend_is_standard_only_and_advertised() {
    let p = base_project();
    let (server, client) = Connection::memory();
    let server_thread = std::thread::spawn(move || gd_server::serve(server));
    let init = init_and_open_caps(
        &p,
        &client,
        &[("a.gd", "extends Node\n")],
        full_legend_caps(None),
    );
    let legend = server_legend(&init);
    let types: Vec<&str> = legend.token_types.iter().map(|t| t.as_str()).collect();
    assert_eq!(
        types,
        vec![
            "class",
            "enum",
            "enumMember",
            "function",
            "method",
            "property",
            "parameter",
            "variable",
            "event",
            "decorator",
        ],
        "advertised token types must be exactly the standard-only legend"
    );
    let mods: Vec<&str> = legend.token_modifiers.iter().map(|m| m.as_str()).collect();
    assert_eq!(
        mods,
        vec![
            "declaration",
            "definition",
            "readonly",
            "static",
            "defaultLibrary",
            "deprecated",
        ],
        "advertised token modifiers must be exactly the standard-only legend"
    );

    // The provider advertises full+delta+range.
    if let SemanticTokensServerCapabilities::SemanticTokensOptions(o) =
        init.capabilities.semantic_tokens_provider.as_ref().unwrap()
    {
        assert_eq!(o.range, Some(true), "range must be advertised");
        assert!(
            matches!(
                o.full,
                Some(SemanticTokensFullOptions::Delta { delta: Some(true) })
            ),
            "full+delta must be advertised; got {:?}",
            o.full
        );
    }
    shutdown(&client, server_thread);
}

/// The representative fixture exercising every mapped kind. Line numbers (0-based) are referenced in
/// the mapping assertions; keep this string and those numbers in lockstep.
///
/// ```text
/// 0: class_name Hero
/// 1: extends Node2D
/// 2:
/// 3: enum State { IDLE, RUN }
/// 4:
/// 5: signal died(by)
/// 6:
/// 7: const MAX_HP = 100
/// 8: @export var hp: int = 10
/// 9:
/// 10: static func make() -> int:
/// 11: \treturn 1
/// 12:
/// 13: func step(delta: int) -> void:
/// 14: \tvar local = delta
/// 15: \tprint(local)
/// ```
const RICH: &str = "class_name Hero\nextends Node2D\n\nenum State { IDLE, RUN }\n\nsignal died(by)\n\nconst MAX_HP = 100\n@export var hp: int = 10\n\nstatic func make() -> int:\n\treturn 1\n\nfunc step(delta: int) -> void:\n\tvar local = delta\n\tprint(local)\n";

/// Find the decoded token at a 0-based (line, character). Panics with the full set if absent.
fn at(decoded: &[Decoded], line: u32, start: u32) -> &Decoded {
    decoded
        .iter()
        .find(|d| d.line == line && d.start == start)
        .unwrap_or_else(|| panic!("no token at ({line},{start}); got {decoded:#?}"))
}

/// Criterion 2: the full docs/09 §6.5 mapping, spot-checked on `RICH`. Each mapped kind colors
/// with the right (type, modifiers).
#[test]
fn full_request_maps_every_kind() {
    let p = base_project();
    let (server, client) = Connection::memory();
    let server_thread = std::thread::spawn(move || gd_server::serve(server));
    let init = init_and_open_caps(&p, &client, &[("rich.gd", RICH)], full_legend_caps(None));
    let legend = server_legend(&init);
    let uri = file_uri(&p.root.join("rich.gd"));
    let tokens = request_full(&client, 10, &uri);
    let d = decode(&tokens, &legend);

    // class_name Hero → `Hero` at line 0, col 11 → class + declaration/definition.
    let hero = at(&d, 0, 11);
    assert_eq!(hero.ty, "class");
    assert!(hero.mods.contains(&"declaration".to_string()));
    assert_eq!(hero.len, 4, "the `Hero` identifier is 4 chars");

    // extends Node2D → `Node2D` at line 1, col 8 → class + defaultLibrary (native).
    let node2d = at(&d, 1, 8);
    assert_eq!(node2d.ty, "class");
    assert!(
        node2d.mods.contains(&"defaultLibrary".to_string()),
        "the native base `Node2D` must carry defaultLibrary; got {:?}",
        node2d.mods
    );

    // enum State → `State` at line 3, col 5 → enum + declaration.
    let state = at(&d, 3, 5);
    assert_eq!(state.ty, "enum");
    assert!(state.mods.contains(&"declaration".to_string()));
    // enum values IDLE (col 13) and RUN (col 19) → enumMember + readonly.
    let idle = at(&d, 3, 13);
    assert_eq!(idle.ty, "enumMember");
    assert!(idle.mods.contains(&"readonly".to_string()));
    let run = at(&d, 3, 19);
    assert_eq!(run.ty, "enumMember");

    // signal died → `died` at line 5, col 7 → event + declaration.
    let died = at(&d, 5, 7);
    assert_eq!(died.ty, "event");
    assert!(died.mods.contains(&"declaration".to_string()));

    // const MAX_HP → `MAX_HP` at line 7, col 6 → variable + readonly + declaration.
    let max_hp = at(&d, 7, 6);
    assert_eq!(max_hp.ty, "variable");
    assert!(max_hp.mods.contains(&"readonly".to_string()));
    assert!(max_hp.mods.contains(&"declaration".to_string()));

    // @export annotation → decorator at line 8, col 0 (covers `@export`, 7 chars).
    let export = at(&d, 8, 0);
    assert_eq!(export.ty, "decorator");
    assert_eq!(export.len, 7, "`@export` is 7 chars");
    // var hp → `hp` at line 8, col 12 → property + declaration (a class member).
    let hp = at(&d, 8, 12);
    assert_eq!(hp.ty, "property");
    assert!(hp.mods.contains(&"declaration".to_string()));

    // static func make → `make` at line 10, col 12 → method + static + declaration. (Every named
    // GDScript func is a class member → `method`; the script IS the root class. Only lambda bodies
    // are `function`.)
    let make = at(&d, 10, 12);
    assert_eq!(make.ty, "method");
    assert!(
        make.mods.contains(&"static".to_string()),
        "a static func must carry the static modifier; got {:?}",
        make.mods
    );
    assert!(make.mods.contains(&"declaration".to_string()));

    // func step → `step` at line 13, col 5 → method + declaration (NOT static).
    let step = at(&d, 13, 5);
    assert_eq!(step.ty, "method");
    assert!(!step.mods.contains(&"static".to_string()));
    // parameter delta → `delta` at line 13, col 10 → parameter + declaration.
    let delta_param = at(&d, 13, 10);
    assert_eq!(delta_param.ty, "parameter");
    assert!(delta_param.mods.contains(&"declaration".to_string()));

    // local var → `local` at line 14, col 5 → variable + declaration (a function-local).
    let local_decl = at(&d, 14, 5);
    assert_eq!(local_decl.ty, "variable");
    assert!(local_decl.mods.contains(&"declaration".to_string()));
    assert!(
        !local_decl.mods.contains(&"readonly".to_string()),
        "a plain local var is not readonly"
    );

    // USE sites (not just declarations) are colored too, resolved structurally from the enclosing
    // function's scope:
    //   - the `delta` parameter USE on line 14 (`var local = delta`, col 13) → parameter, no decl.
    let delta_use = at(&d, 14, 13);
    assert_eq!(delta_use.ty, "parameter");
    assert!(
        !delta_use.mods.contains(&"declaration".to_string()),
        "a parameter USE is not a declaration; got {:?}",
        delta_use.mods
    );
    //   - the `local` variable USE on line 15 (`print(local)`, col 7) → variable, no decl.
    let local_use = at(&d, 15, 7);
    assert_eq!(local_use.ty, "variable");
    assert!(
        !local_use.mods.contains(&"declaration".to_string()),
        "a local USE is not a declaration; got {:?}",
        local_use.mods
    );

    shutdown(&client, server_thread);
}

/// #184: a dotted call through a Callable property (`obj.cb()`) must color the callee as the
/// property it is, not as a method. A real dotted method call beside it stays `method`.
#[test]
fn dotted_callable_property_call_colors_property_not_method() {
    let p = base_project();
    let (server, client) = Connection::memory();
    let server_thread = std::thread::spawn(move || gd_server::serve(server));
    let source = "class_name Target\nextends Node\nvar cb: Callable\nfunc real() -> void:\n\tpass\nfunc use() -> void:\n\tvar obj: Target = Target.new()\n\tobj.cb()\n\tobj.real()\n";
    let init = init_and_open_caps(
        &p,
        &client,
        &[("target.gd", source)],
        full_legend_caps(None),
    );
    let legend = server_legend(&init);
    let uri = file_uri(&p.root.join("target.gd"));
    let tokens = request_full(&client, 10, &uri);
    let d = decode(&tokens, &legend);

    let cb = at(&d, 7, 5);
    assert_eq!(
        cb.ty, "property",
        "`obj.cb()` must color `cb` as the Callable property, not as a method; got {d:#?}"
    );
    let real = at(&d, 8, 5);
    assert_eq!(
        real.ty, "method",
        "`obj.real()` must keep coloring a genuine dotted method callee as method; got {d:#?}"
    );

    shutdown(&client, server_thread);
}

/// Apply a `SemanticTokensDelta` over the FLAT integer array exactly as a conformant client does
/// (the offsets are tokenIndex*5). This is the reference the round-trip is verified against — kept
/// independent of the server's own diff/apply so the test can't be self-consistent-but-wrong.
fn apply_wire_delta(base: &SemanticTokens, delta: &SemanticTokensDelta) -> Vec<SemanticToken> {
    let mut flat: Vec<u32> = Vec::new();
    for t in &base.data {
        flat.extend_from_slice(&[
            t.delta_line,
            t.delta_start,
            t.length,
            t.token_type,
            t.token_modifiers_bitset,
        ]);
    }
    // Edits must be applied in descending start order so earlier offsets stay valid.
    let mut edits = delta.edits.clone();
    edits.sort_by_key(|e| std::cmp::Reverse(e.start));
    for e in edits {
        let start = e.start as usize;
        let end = start + e.delete_count as usize;
        let mut ins = Vec::new();
        if let Some(data) = &e.data {
            for t in data {
                ins.extend_from_slice(&[
                    t.delta_line,
                    t.delta_start,
                    t.length,
                    t.token_type,
                    t.token_modifiers_bitset,
                ]);
            }
        }
        flat.splice(start..end, ins);
    }
    flat.as_chunks::<5>()
        .0
        .iter()
        .map(|c| SemanticToken {
            delta_line: c[0],
            delta_start: c[1],
            length: c[2],
            token_type: c[3],
            token_modifiers_bitset: c[4],
        })
        .collect()
}

/// Criterion 3: `full` → edit → `full/delta` → apply the delta to the previous array reproduces
/// exactly the array a fresh `full` returns for the edited document.
#[test]
fn full_delta_round_trips() {
    let p = base_project();
    let (server, client) = Connection::memory();
    let server_thread = std::thread::spawn(move || gd_server::serve(server));
    init_and_open_caps(&p, &client, &[("rich.gd", RICH)], full_legend_caps(None));
    let uri = file_uri(&p.root.join("rich.gd"));

    // 1) Initial full — note its result id.
    let first = request_full(&client, 10, &uri);
    let prev_id = first
        .result_id
        .clone()
        .expect("full must carry a result id");

    // 2) Edit the document: insert a new member line after the `@export var hp` line (line 8).
    //    Insert `\nstatic var count = 0` at the end of line 8 — shifts every later token down by one
    //    line and adds new tokens, exercising a real delta.
    let new_text = RICH.replace(
        "@export var hp: int = 10\n",
        "@export var hp: int = 10\nstatic var count = 0\n",
    );
    client
        .sender
        .send(notification(
            "textDocument/didChange",
            lsp_types::DidChangeTextDocumentParams {
                text_document: VersionedTextDocumentIdentifier {
                    uri: uri.clone(),
                    version: 100,
                },
                content_changes: vec![TextDocumentContentChangeEvent {
                    range: None, // full replace — simplest, unambiguous
                    range_length: None,
                    text: new_text.clone(),
                }],
            },
        ))
        .unwrap();
    while common::try_recv(&client, std::time::Duration::from_millis(300)).is_some() {}

    // 3) full/delta against the previous id.
    client
        .sender
        .send(request(
            11,
            "textDocument/semanticTokens/full/delta",
            lsp_types::SemanticTokensDeltaParams {
                work_done_progress_params: WorkDoneProgressParams::default(),
                partial_result_params: PartialResultParams::default(),
                text_document: TextDocumentIdentifier { uri: uri.clone() },
                previous_result_id: prev_id,
            },
        ))
        .unwrap();
    let resp = common::recv_response(&client);
    assert!(resp.error.is_none(), "delta errored: {:?}", resp.error);
    let delta_result: SemanticTokensFullDeltaResult =
        serde_json::from_value(resp.result.expect("delta result")).unwrap();
    let delta = match delta_result {
        SemanticTokensFullDeltaResult::TokensDelta(d) => d,
        other => panic!("expected a TokensDelta, got {other:?}"),
    };

    // 4) Apply the delta to the FIRST array, the way a client does.
    let applied = apply_wire_delta(&first, &delta);

    // 5) A fresh full on the edited document must equal the applied result.
    let fresh = request_full(&client, 12, &uri);
    assert_eq!(
        applied, fresh.data,
        "applying the delta to the previous tokens must reproduce a fresh full token array"
    );

    shutdown(&client, server_thread);
}

/// An unknown `previous_result_id` falls back to a fresh full token set (the spec's documented
/// behavior — gdls returns `Tokens`, not a delta).
#[test]
fn full_delta_unknown_previous_id_falls_back_to_full() {
    let p = base_project();
    let (server, client) = Connection::memory();
    let server_thread = std::thread::spawn(move || gd_server::serve(server));
    init_and_open_caps(&p, &client, &[("rich.gd", RICH)], full_legend_caps(None));
    let uri = file_uri(&p.root.join("rich.gd"));

    client
        .sender
        .send(request(
            11,
            "textDocument/semanticTokens/full/delta",
            lsp_types::SemanticTokensDeltaParams {
                work_done_progress_params: WorkDoneProgressParams::default(),
                partial_result_params: PartialResultParams::default(),
                text_document: TextDocumentIdentifier { uri: uri.clone() },
                previous_result_id: "st-does-not-exist".to_string(),
            },
        ))
        .unwrap();
    let resp = common::recv_response(&client);
    assert!(resp.error.is_none(), "delta errored: {:?}", resp.error);
    let result: SemanticTokensFullDeltaResult =
        serde_json::from_value(resp.result.expect("delta result")).unwrap();
    assert!(
        matches!(result, SemanticTokensFullDeltaResult::Tokens(_)),
        "an unknown previous id must fall back to a full Tokens result; got {result:?}"
    );
    shutdown(&client, server_thread);
}

/// Criterion 4: `range` returns only the tokens intersecting the requested range.
#[test]
fn range_returns_only_intersecting_tokens() {
    let p = base_project();
    let (server, client) = Connection::memory();
    let server_thread = std::thread::spawn(move || gd_server::serve(server));
    let init = init_and_open_caps(&p, &client, &[("rich.gd", RICH)], full_legend_caps(None));
    let legend = server_legend(&init);
    let uri = file_uri(&p.root.join("rich.gd"));

    // Request only lines 13..=15 (the `step` function): start line 13 col 0 .. end line 16 col 0.
    client
        .sender
        .send(request(
            10,
            "textDocument/semanticTokens/range",
            SemanticTokensRangeParams {
                work_done_progress_params: WorkDoneProgressParams::default(),
                partial_result_params: PartialResultParams::default(),
                text_document: TextDocumentIdentifier { uri: uri.clone() },
                range: Range {
                    start: Position {
                        line: 13,
                        character: 0,
                    },
                    end: Position {
                        line: 16,
                        character: 0,
                    },
                },
            },
        ))
        .unwrap();
    let resp = common::recv_response(&client);
    assert!(resp.error.is_none(), "range errored: {:?}", resp.error);
    let result: SemanticTokensRangeResult =
        serde_json::from_value(resp.result.expect("range result")).unwrap();
    let tokens = match result {
        SemanticTokensRangeResult::Tokens(t) => t,
        SemanticTokensRangeResult::Partial(_) => panic!("expected range Tokens, got partial"),
    };
    let d = decode(&tokens, &legend);

    assert!(!d.is_empty(), "the range should contain the `step` tokens");
    // Every returned token must be within lines 13..=15.
    for t in &d {
        assert!(
            (13..=15).contains(&t.line),
            "range returned an out-of-range token at line {}; got {d:#?}",
            t.line
        );
    }
    // And it must NOT contain tokens from before the range (e.g. `Hero` at line 0).
    assert!(
        !d.iter().any(|t| t.line == 0),
        "range must not include line-0 tokens; got {d:#?}"
    );
    // The `step` function token (line 13) is present.
    assert!(
        d.iter().any(|t| t.line == 13 && t.ty == "method"),
        "the `step` method must be in the range result; got {d:#?}"
    );

    shutdown(&client, server_thread);
}

/// Criterion 5a: a reduced-legend client's advertised legend is a pure ALLOW-FILTER. gdls emits its
/// own (server-advertised) legend indices — never a per-client remap (LSP 3.17: the wire integers
/// index the server-advertised legend, not the client's `tokenTypes` capability) — and DROPS any
/// type/modifier the client didn't advertise. A client supporting only `class` + `method` and only
/// the `static` modifier therefore: sees `class`/`method` at their SERVER indices (0 / 4), never sees
/// a `property`/`enum`/etc. token (dropped), and sees only the `static` bit at its SERVER bit (3).
#[test]
fn reduced_legend_client_gets_only_declared_types_and_modifiers() {
    let p = base_project();
    let (server, client) = Connection::memory();
    let server_thread = std::thread::spawn(move || gd_server::serve(server));
    // The CLIENT advertises only class + method (in its own order: class=0, method=1) and only the
    // static modifier. The SERVER still advertises the full legend; emission uses the SERVER indices
    // and drops anything the client didn't list. (`method` is in the reduced set so the static method
    // survives to prove modifier projection; `property`/`enum`/`event`/`function`/etc. are NOT, so
    // they must be dropped.)
    let caps = client_caps_with_legend(
        &[SemanticTokenType::CLASS, SemanticTokenType::METHOD],
        &[SemanticTokenModifier::STATIC],
        None,
    );
    let init = init_and_open_caps(&p, &client, &[("rich.gd", RICH)], caps);
    // Decode against the SERVER-advertised legend — that is what the wire indices mean (LSP 3.17).
    let legend = server_legend(&init);
    // The server advertises its full standard legend (stable wire indices); the client's reduced one
    // is only an allow-filter applied at emit time, never a remap and never a shrink of the legend.
    assert_eq!(
        legend.token_types.len(),
        10,
        "the server must advertise its full legend regardless of the client's reduced one"
    );
    let uri = file_uri(&p.root.join("rich.gd"));
    let tokens = request_full(&client, 10, &uri);

    // Raw-wire proof (independent of name-decoding): every emitted `tokenType` is a gdls SERVER index
    // (class=0, method=4) — NOT a client-list position (which would put method at 1 → decode as enum).
    let server_class = 0u32;
    let server_method = 4u32;
    for tok in &tokens.data {
        assert!(
            tok.token_type == server_class || tok.token_type == server_method,
            "a reduced-legend client must only receive its declared types, at gdls's SERVER indices \
             (class={server_class}, method={server_method}); got tokenType {}",
            tok.token_type
        );
    }
    assert!(
        tokens
            .data
            .iter()
            .any(|tok| tok.token_type == server_method),
        "the `method` tokens must be emitted at gdls's server index {server_method}"
    );

    let d = decode(&tokens, &legend);
    assert!(!d.is_empty(), "some class/method tokens should survive");
    // Decoded against the server legend, only `class` and `method` types appear — no property/enum/
    // event/etc. (those are dropped because the client didn't advertise them).
    for t in &d {
        assert!(
            t.ty == "class" || t.ty == "method",
            "a reduced-legend client must only receive its declared types; got `{}` in {d:#?}",
            t.ty
        );
        // Only the `static` modifier can appear; declaration/readonly/defaultLibrary are dropped.
        for m in &t.mods {
            assert_eq!(
                m, "static",
                "a reduced-legend client must only receive its declared modifiers; got `{m}`"
            );
        }
    }
    // The static method `make` is present and carries `static` at the SERVER bit (3) — the bitset
    // indexes the server legend, so `static` decodes correctly against it.
    assert!(
        d.iter()
            .any(|t| t.ty == "method" && t.mods.contains(&"static".to_string())),
        "the static method must still carry the declared `static` modifier; got {d:#?}"
    );
    // `Hero` (class) is present even though `declaration` (undeclared) was stripped from it.
    assert!(
        d.iter().any(|t| t.ty == "class" && t.mods.is_empty()),
        "the class token must survive with its undeclared modifiers stripped; got {d:#?}"
    );
    shutdown(&client, server_thread);

    // Strong negative proof: a FULL-legend client over the same fixture receives strictly MORE
    // tokens (the `property`/`enum`/`enumMember`/`event`/`decorator`/`parameter`/`variable` tokens
    // the reduced client dropped). The reduced set is a true subset, not the same set relabeled.
    let (server2, client2) = Connection::memory();
    let server_thread2 = std::thread::spawn(move || gd_server::serve(server2));
    init_and_open_caps(&p, &client2, &[("rich.gd", RICH)], full_legend_caps(None));
    let uri2 = file_uri(&p.root.join("rich.gd"));
    let full_tokens = request_full(&client2, 10, &uri2);
    assert!(
        full_tokens.data.len() > tokens.data.len(),
        "the full-legend client must receive more tokens than the reduced client \
         (undeclared types were genuinely dropped); full={} reduced={}",
        full_tokens.data.len(),
        tokens.data.len()
    );
    shutdown(&client2, server_thread2);
}

/// Order-independence (the proof the fix indexes the SERVER legend, not the client's): a client that
/// advertises gdls's types in a DIFFERENT order than the server legend still receives gdls's own
/// (server) indices on the wire — decoding against the server-advertised legend yields the correct
/// names. If gdls remapped to client positions, this client would mis-highlight (e.g. `method` would
/// arrive at the client's position and decode as the wrong type against the server legend).
#[test]
fn client_legend_in_a_different_order_still_gets_server_indices() {
    let p = base_project();
    let (server, client) = Connection::memory();
    let server_thread = std::thread::spawn(move || gd_server::serve(server));
    // The client lists method, class, event (positions 0, 1, 2) — a different order from the server
    // legend (class=0, method=4, event=8). It also lists `static`, `declaration` (positions 0, 1) —
    // again a different order from the server (declaration=0, static=3).
    let caps = client_caps_with_legend(
        &[
            SemanticTokenType::METHOD,
            SemanticTokenType::CLASS,
            SemanticTokenType::EVENT,
        ],
        &[
            SemanticTokenModifier::STATIC,
            SemanticTokenModifier::DECLARATION,
        ],
        None,
    );
    let init = init_and_open_caps(&p, &client, &[("rich.gd", RICH)], caps);
    let legend = server_legend(&init);
    let uri = file_uri(&p.root.join("rich.gd"));
    let tokens = request_full(&client, 10, &uri);

    // Raw-wire proof: every emitted `tokenType` is a SERVER index from the advertised set {0,4,8},
    // independent of the client's {0,1,2} ordering. (A remap would emit the client positions.)
    for tok in &tokens.data {
        assert!(
            [0u32, 4, 8].contains(&tok.token_type),
            "an order-different client must still receive gdls's SERVER indices (class=0, method=4, \
             event=8); got tokenType {}",
            tok.token_type
        );
    }

    // Decoded against the server legend, the `make`/`step` methods are `method` and `died` is `event`
    // — the names a correct client renders. The `static func make` carries `static` (server bit 3).
    let d = decode(&tokens, &legend);
    assert!(
        d.iter().any(|t| t.ty == "method"),
        "a method token must decode as `method` against the server legend; got {d:#?}"
    );
    assert!(
        d.iter().any(|t| t.ty == "event"),
        "the `died` signal must decode as `event` against the server legend; got {d:#?}"
    );
    assert!(
        d.iter()
            .any(|t| t.ty == "method" && t.mods.contains(&"static".to_string())),
        "the static method must carry `static` (at the server bit); got {d:#?}"
    );
    // A type the client did NOT advertise (e.g. `enum`, `property`) is absent.
    assert!(
        !d.iter().any(|t| t.ty == "enum" || t.ty == "property"),
        "types the client didn't advertise must be dropped; got {d:#?}"
    );
    shutdown(&client, server_thread);
}

/// Criterion 5b: `augmentsSyntaxTokens` true vs false produce IDENTICAL output. gdls's legend has
/// no keyword/operator/string/number/comment types, so it never emits a base-grammar token — there
/// is nothing to suppress, whichever way `augmentsSyntaxTokens` is set. Asserting equality is the
/// honest statement of that (the correct generic-LSP-first behavior).
#[test]
fn augments_syntax_tokens_true_and_false_are_identical() {
    let p = base_project();

    let run = |augments: bool| -> Vec<SemanticToken> {
        let (server, client) = Connection::memory();
        let server_thread = std::thread::spawn(move || gd_server::serve(server));
        init_and_open_caps(
            &p,
            &client,
            &[("rich.gd", RICH)],
            full_legend_caps(Some(augments)),
        );
        let uri = file_uri(&p.root.join("rich.gd"));
        let tokens = request_full(&client, 10, &uri);
        shutdown(&client, server_thread);
        tokens.data
    };

    let with_augment = run(true);
    let without_augment = run(false);
    assert_eq!(
        with_augment, without_augment,
        "gdls emits no base-grammar tokens, so augmentsSyntaxTokens must not change the output"
    );
    assert!(
        !with_augment.is_empty(),
        "the fixture should still produce identifier tokens"
    );
}

// ===================================================================================================
// #258 — the `deprecated` modifier, which the legend advertised but nothing ever emitted.
// ===================================================================================================

/// A library whose class, method, property and enum value are all `## @deprecated`, alongside
/// undeprecated siblings that must stay unmarked.
const DEP_LIB: &str = "\
## A widget.
##
## @deprecated: Gone in 2.0.
class_name DepWidget
extends Node

## @deprecated: Use resize().
func grow() -> void:
\tpass

func keep() -> void:
\tpass

## @deprecated: Old.
var width := 1

var height := 2

enum Kind {
\t## @deprecated: Old value.
\tONE,
\tTWO,
}
";

const DEP_USE: &str = "\
extends Node

func f(w: DepWidget) -> void:
\tw.grow()
\tw.keep()
\tprint(w.width, w.height)
\tvar other := DepWidget.new()
\tprint(other)
";

fn has_deprecated(d: &Decoded) -> bool {
    d.mods.iter().any(|m| m == "deprecated")
}

/// Every DECLARATION whose `##` block carries `@deprecated` gets the modifier — class, method,
/// property, and enum member — and every undeprecated sibling keeps its modifier set unchanged.
#[test]
fn deprecated_declarations_carry_the_modifier() {
    let p = base_project();
    let (server, client) = Connection::memory();
    let server_thread = std::thread::spawn(move || gd_server::serve(server));
    let init = init_and_open_caps(
        &p,
        &client,
        &[("src/deplib.gd", DEP_LIB)],
        full_legend_caps(None),
    );
    let legend = server_legend(&init);
    let uri = file_uri(&p.root.join("src/deplib.gd"));
    let decoded = decode(&request_full(&client, 2, &uri), &legend);

    assert!(has_deprecated(at(&decoded, 3, 11)), "class_name DepWidget");
    assert!(has_deprecated(at(&decoded, 7, 5)), "func grow");
    assert!(has_deprecated(at(&decoded, 14, 4)), "var width");
    assert!(has_deprecated(at(&decoded, 20, 1)), "enum value ONE");

    assert!(!has_deprecated(at(&decoded, 10, 5)), "func keep is clean");
    assert!(!has_deprecated(at(&decoded, 16, 4)), "var height is clean");
    assert!(
        !has_deprecated(at(&decoded, 21, 1)),
        "enum value TWO is clean"
    );

    shutdown(&client, server_thread);
}

/// A cross-file USE of a deprecated symbol carries it too — the call site, the property read, and
/// the class name — read from the DECLARING file's interface, never from a name match.
#[test]
fn deprecated_cross_file_uses_carry_the_modifier() {
    let p = base_project();
    let (server, client) = Connection::memory();
    let server_thread = std::thread::spawn(move || gd_server::serve(server));
    let init = init_and_open_caps(
        &p,
        &client,
        &[("src/deplib.gd", DEP_LIB), ("src/depuse.gd", DEP_USE)],
        full_legend_caps(None),
    );
    let legend = server_legend(&init);
    let uri = file_uri(&p.root.join("src/depuse.gd"));
    let decoded = decode(&request_full(&client, 2, &uri), &legend);

    assert!(has_deprecated(at(&decoded, 3, 3)), "w.grow() call site");
    assert!(has_deprecated(at(&decoded, 5, 9)), "w.width read");
    assert!(
        has_deprecated(at(&decoded, 6, 14)),
        "the DepWidget class name"
    );

    assert!(!has_deprecated(at(&decoded, 4, 3)), "w.keep() is clean");
    assert!(
        !has_deprecated(at(&decoded, 5, 18)),
        "w.height read is clean"
    );

    shutdown(&client, server_thread);
}
