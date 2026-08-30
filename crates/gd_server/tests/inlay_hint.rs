//! M10 (#73): `textDocument/inlayHint` + `inlayHint/resolve` + `workspace/inlayHint/refresh`.
//!
//! Covers the phase-3 acceptance criteria over a `Connection` (protocol-shape):
//!   1. `inlayHintProvider` advertised (with `resolveProvider`).
//!   2. A `var x := …` declaration yields a `: <Type>` TYPE hint; an inferred `for` loop var too.
//!   3. A multi-arg call yields PARAMETER hints (names before args); a single-arg call yields NONE.
//!   4. Each kind is independently config-toggleable; toggling emits `workspace/inlayHint/refresh`
//!      and the next request reflects the change (verified live over the same `Connection`).
//!   5. `inlayHint/resolve` fills the tooltip lazily ONLY for a `resolveSupport` client; a
//!      non-resolve client receives the tooltip eagerly (no resolve round-trip).
//!   6. A TYPE hint's `textEdit`, applied, re-analyzes with ZERO new diagnostics.
//!   7. Hints respect the requested range.

mod common;

use common::{file_uri, notification, recv, request, shutdown, try_recv, TempProject};
use lsp_server::{Connection, Message, Response};
use lsp_types::{
    ClientCapabilities, DidOpenTextDocumentParams, InitializeParams, InitializeResult,
    InitializedParams, InlayHint, InlayHintClientCapabilities, InlayHintKind,
    InlayHintResolveClientCapabilities, InlayHintWorkspaceClientCapabilities, Position,
    PublishDiagnosticsParams, Range, TextDocumentClientCapabilities, TextDocumentIdentifier,
    TextDocumentItem, Uri, WorkspaceClientCapabilities,
};
use std::time::Duration;

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
    while common::try_recv(client, Duration::from_millis(300)).is_some() {}
    result
}

/// Client caps with optional `inlayHint.resolveSupport` and `workspace.inlayHint.refreshSupport`.
fn inlay_caps(resolve: bool, refresh: bool) -> ClientCapabilities {
    ClientCapabilities {
        text_document: Some(TextDocumentClientCapabilities {
            inlay_hint: Some(InlayHintClientCapabilities {
                dynamic_registration: None,
                resolve_support: resolve.then(|| InlayHintResolveClientCapabilities {
                    properties: vec!["tooltip".to_string()],
                }),
            }),
            ..Default::default()
        }),
        workspace: Some(WorkspaceClientCapabilities {
            inlay_hint: Some(InlayHintWorkspaceClientCapabilities {
                refresh_support: Some(refresh),
            }),
            ..Default::default()
        }),
        ..Default::default()
    }
}

/// A whole-document range (lines 0..1000) — wide enough to capture every hint in the fixtures.
fn whole_doc() -> Range {
    Range {
        start: Position {
            line: 0,
            character: 0,
        },
        end: Position {
            line: 1000,
            character: 0,
        },
    }
}

fn request_hints(client: &Connection, id: i32, uri: &Uri, range: Range) -> Vec<InlayHint> {
    client
        .sender
        .send(request(
            id,
            "textDocument/inlayHint",
            serde_json::json!({
                "textDocument": TextDocumentIdentifier { uri: uri.clone() },
                "range": range,
            }),
        ))
        .unwrap();
    let resp = common::recv_response(client);
    assert!(resp.error.is_none(), "inlayHint errored: {:?}", resp.error);
    serde_json::from_value(resp.result.expect("inlayHint result")).unwrap()
}

/// The label text of a hint (the `String` form; panics on the parts form, which gdls never emits).
fn label_of(h: &InlayHint) -> String {
    match &h.label {
        lsp_types::InlayHintLabel::String(s) => s.clone(),
        lsp_types::InlayHintLabel::LabelParts(_) => panic!("gdls emits only String labels"),
    }
}

/// A base project (project.godot + api), no source files — tests write their own.
fn base_project() -> TempProject {
    let p = TempProject::new();
    p.write(
        "project.godot",
        "config_version=5\n\n[application]\nconfig/features=PackedStringArray(\"4.6\")\n",
    );
    p.write("extension_api.json", common::MINI_API);
    p
}

/// Criterion 1: the server advertises `inlayHintProvider` with `resolveProvider`.
#[test]
fn inlay_hint_provider_advertised() {
    let p = base_project();
    let (server, client) = Connection::memory();
    let server_thread = std::thread::spawn(move || gd_server::serve(server));
    let result = init_and_open_caps(
        &p,
        &client,
        &[("a.gd", "extends Node\n")],
        ClientCapabilities::default(),
    );
    let provider = result
        .capabilities
        .inlay_hint_provider
        .expect("inlayHintProvider must be advertised");
    // OneOf::Right(Options{ resolve_provider: Some(true) }).
    match provider {
        lsp_types::OneOf::Right(lsp_types::InlayHintServerCapabilities::Options(opts)) => {
            assert_eq!(
                opts.resolve_provider,
                Some(true),
                "resolveProvider must be advertised"
            );
        }
        other => panic!("expected InlayHintServerCapabilities::Options; got {other:?}"),
    }
    shutdown(&client, server_thread);
}

/// Criterion 2: `var speed := 5.0` yields a `: float` TYPE hint after the identifier; an inferred
/// `for` loop variable yields its element-type hint.
#[test]
fn type_hints_on_inferred_var_and_for() {
    let p = base_project();
    let (server, client) = Connection::memory();
    let server_thread = std::thread::spawn(move || gd_server::serve(server));
    // `for i in range(3):` — `range()` yields a typed element, so the loop var infers `int`. (An
    // untyped `[1, 2, 3]` literal would infer `Variant`, which gdls deliberately does NOT hint.)
    let src = "extends Node\n\nfunc run() -> void:\n\tvar speed := 5.0\n\tfor i in range(3):\n\t\tprint(i)\n";
    init_and_open_caps(&p, &client, &[("t.gd", src)], inlay_caps(false, false));
    let uri = file_uri(&p.root.join("t.gd"));
    let hints = request_hints(&client, 10, &uri, whole_doc());

    // `var speed := 5.0` → a TYPE hint `: float` positioned right after `speed`.
    let speed_byte_line = 3; // 0-based: line 3 is `\tvar speed := 5.0`
    let type_hints: Vec<&InlayHint> = hints
        .iter()
        .filter(|h| h.kind == Some(InlayHintKind::TYPE))
        .collect();
    assert!(
        type_hints
            .iter()
            .any(|h| label_of(h) == ": float" && h.position.line == speed_byte_line),
        "expected a `: float` TYPE hint on line {speed_byte_line}; got {hints:?}"
    );

    // The inferred `for i in range(3):` loop var → a TYPE hint `: int` on line 4.
    assert!(
        type_hints
            .iter()
            .any(|h| label_of(h) == ": int" && h.position.line == 4),
        "expected a `: int` TYPE hint on the inferred for-loop var (line 4); got {hints:?}"
    );

    shutdown(&client, server_thread);
}

/// An explicitly-annotated `var x: int = 1` and a plain `var y = 2` get NO type hint (the first is
/// already annotated; the second the user deliberately left untyped — only the `:=` walrus form
/// hints).
#[test]
fn no_type_hint_for_annotated_or_plain_var() {
    let p = base_project();
    let (server, client) = Connection::memory();
    let server_thread = std::thread::spawn(move || gd_server::serve(server));
    let src = "extends Node\n\nfunc run() -> void:\n\tvar x: int = 1\n\tvar y = 2\n\tvar z := 3\n";
    init_and_open_caps(&p, &client, &[("t.gd", src)], inlay_caps(false, false));
    let uri = file_uri(&p.root.join("t.gd"));
    let hints = request_hints(&client, 10, &uri, whole_doc());

    // Only the `:=` form (`var z := 3`, line 5) gets a TYPE hint; lines 3 and 4 get none.
    let type_lines: Vec<u32> = hints
        .iter()
        .filter(|h| h.kind == Some(InlayHintKind::TYPE))
        .map(|h| h.position.line)
        .collect();
    assert!(
        type_lines.contains(&5),
        "the `:=` var (line 5) must get a TYPE hint; got lines {type_lines:?}"
    );
    assert!(
        !type_lines.contains(&3) && !type_lines.contains(&4),
        "an annotated `var x: int` (line 3) and a plain `var y = 2` (line 4) must get NO TYPE hint; \
         got lines {type_lines:?}"
    );

    shutdown(&client, server_thread);
}

/// Regression (review B1): a `var := …` that infers an UNNAMED-script type (a `.gd` without a
/// `class_name`) gets the informational hint but NO `textEdit` — its display label is the file
/// basename (`a.gd`), which `: a.gd` would re-parse as `type a` member `gd` and CORRUPT the file.
/// The edit gate is kind-driven (only a `class_name`'d script is insertable), so the edit is omitted
/// while the label still shows.
#[test]
fn no_text_edit_for_unnamed_script_inferred_type() {
    let p = base_project();
    let (server, client) = Connection::memory();
    let server_thread = std::thread::spawn(move || gd_server::serve(server));
    // `a.gd` has NO class_name; `t.gd` infers `var x := A.new()` to that unnamed-script type.
    let a = "extends RefCounted\n\nfunc greet() -> void:\n\tpass\n";
    let t = "extends Node\n\nconst A = preload(\"res://a.gd\")\n\nfunc run() -> void:\n\tvar x := A.new()\n\tx.greet()\n";
    init_and_open_caps(
        &p,
        &client,
        &[("a.gd", a), ("t.gd", t)],
        inlay_caps(false, false),
    );
    let uri = file_uri(&p.root.join("t.gd"));
    let hints = request_hints(&client, 10, &uri, whole_doc());

    // The inferred `var x := A.new()` (line 5) gets a TYPE hint…
    let type_hint = hints
        .iter()
        .find(|h| h.kind == Some(InlayHintKind::TYPE) && h.position.line == 5)
        .expect("the inferred unnamed-script var must still get a TYPE hint (label)");
    // …but it must carry NO textEdit (the basename label is not a source-valid annotation).
    assert!(
        type_hint.text_edits.is_none(),
        "an unnamed-script inferred type must carry NO textEdit (would corrupt the file); got {:?}",
        type_hint.text_edits
    );

    shutdown(&client, server_thread);
}

/// A `var := …` that infers a typed CONTAINER (`Array[int]`) gets a `textEdit` inserting the full
/// parametrized form — the container annotation is derived by recursing on the element types (each
/// must itself be source-valid), so a clean `Array[int]` edit is produced.
#[test]
fn text_edit_for_typed_container_inferred_type() {
    let p = base_project();
    let (server, client) = Connection::memory();
    let server_thread = std::thread::spawn(move || gd_server::serve(server));
    let t =
        "extends Node\n\nfunc run() -> void:\n\tvar nums: Array[int] = []\n\tvar copy := nums\n";
    init_and_open_caps(&p, &client, &[("t.gd", t)], inlay_caps(false, false));
    let uri = file_uri(&p.root.join("t.gd"));
    let hints = request_hints(&client, 10, &uri, whole_doc());

    let type_hint = hints
        .iter()
        .find(|h| h.kind == Some(InlayHintKind::TYPE) && h.position.line == 4)
        .expect("the inferred Array[int] var must get a TYPE hint");
    assert_eq!(label_of(type_hint), ": Array[int]");
    let edit = type_hint
        .text_edits
        .as_ref()
        .and_then(|e| e.first())
        .expect("a typed container must carry a textEdit");
    assert_eq!(edit.new_text, ": Array[int] = ");

    shutdown(&client, server_thread);
}

/// A `var := []` infers a BARE (untyped) `Array` — the `annotation_type` container fall-through must
/// produce a clean `: Array` edit (the element-less branch), not a dropped or garbled one.
#[test]
fn text_edit_for_bare_untyped_array_inferred_type() {
    let p = base_project();
    let (server, client) = Connection::memory();
    let server_thread = std::thread::spawn(move || gd_server::serve(server));
    let t = "extends Node\n\nfunc run() -> void:\n\tvar items := []\n";
    init_and_open_caps(&p, &client, &[("t.gd", t)], inlay_caps(false, false));
    let uri = file_uri(&p.root.join("t.gd"));
    let hints = request_hints(&client, 10, &uri, whole_doc());

    let type_hint = hints
        .iter()
        .find(|h| h.kind == Some(InlayHintKind::TYPE) && h.position.line == 3)
        .expect("the inferred bare-Array var must get a TYPE hint");
    assert_eq!(label_of(type_hint), ": Array");
    let edit = type_hint
        .text_edits
        .as_ref()
        .and_then(|e| e.first())
        .expect("a bare Array type must carry a clean textEdit");
    assert_eq!(edit.new_text, ": Array = ");

    shutdown(&client, server_thread);
}

/// Regression (review B1, the CONTAINER-ELEMENT variant — the proven corruption path the original
/// `dt.to_string()` rendering let through): a `var := …` that infers a typed container whose ELEMENT
/// is a script-owned enum in an UNNAMED script (`Array[A.E]`, `E` declared in `a.gd` with no
/// `class_name`) gets the informational label `: Array[a.gd.E]` but carries **NO `textEdit`**.
///
/// Why this is the load-bearing test: `DataType`'s `Display` renders such an enum element as the file
/// BASENAME (`a.gd.E`), so a `textEdit` built from the display string would emit `: Array[a.gd.E] = `
/// — which, applied, re-parses `a.gd` as `type a` member `gd` and yields a NEW
/// `Could not find type "a"` diagnostic, silently corrupting the user's file. The fix is that
/// `annotation_type` recurses container element types through ITSELF (the kind-driven gate), where a
/// `DtKind::Enum` element hits the conservative `_ => None` arm and `?`-propagates `None` up the
/// container — so the whole container yields no edit while the label still shows. The scalar sibling
/// (`no_text_edit_for_unnamed_script_inferred_type`) never exercises this element-recursion path; the
/// `new.len() <= base.len()` count compare the older clean-apply test used would also have missed it.
#[test]
fn no_text_edit_for_unnamed_script_enum_container_element() {
    let p = base_project();
    let (server, client) = Connection::memory();
    let server_thread = std::thread::spawn(move || gd_server::serve(server));
    // `a.gd` is UNNAMED (no class_name) and declares `enum E`. `t.gd` types `arr: Array[A.E]` via a
    // preload, then `var x := arr` infers that container — element `A.E` is a script-owned enum. A
    // sibling `dict: Dictionary[A.E, int]` inferred by `var d := dict` exercises the SAME script-enum
    // element in a Dictionary KEY position (#73 mutating-surface review, area 3): the `annotation_type`
    // Dictionary arm recurses K and V through itself, so the enum K hits the same conservative
    // `_ => None` and `?`-propagates — no edit, like the Array case.
    let a = "extends RefCounted\n\nenum E { X }\n";
    let t = "extends Node\n\nconst A = preload(\"res://a.gd\")\n\nvar arr: Array[A.E] = []\nvar dict: Dictionary[A.E, int] = {}\n\nfunc run() -> void:\n\tvar x := arr\n\tvar d := dict\n";
    init_and_open_caps(
        &p,
        &client,
        &[("a.gd", a), ("t.gd", t)],
        inlay_caps(false, false),
    );
    let uri = file_uri(&p.root.join("t.gd"));
    let hints = request_hints(&client, 10, &uri, whole_doc());

    // The inferred `var x := arr` (line 8) gets a TYPE hint with the basename-rendered label…
    let type_hint = hints
        .iter()
        .find(|h| h.kind == Some(InlayHintKind::TYPE) && h.position.line == 8)
        .expect("the inferred Array[A.E] var must still get a TYPE hint (label)");
    assert_eq!(
        label_of(type_hint),
        ": Array[a.gd.E]",
        "the informational label renders the enum element as the file basename"
    );
    // …but it must carry NO textEdit: a `: Array[a.gd.E] = ` edit, applied, would re-parse the
    // basename as `type a` member `gd` and produce a NEW `Could not find type \"a\"` diagnostic —
    // the exact silent corruption the kind-driven element recursion exists to prevent.
    assert!(
        type_hint.text_edits.is_none(),
        "a script-enum container element must carry NO textEdit (would corrupt the file); got {:?}",
        type_hint.text_edits
    );

    // The same enum element in a Dictionary KEY (`var d := dict`, line 9): label shows, NO textEdit
    // (the Dictionary arm recurses K through `annotation_type`, the enum K → `None` → whole dict
    // drops the edit). Whatever the label, the edit must never be present.
    let dict_hint = hints
        .iter()
        .find(|h| h.kind == Some(InlayHintKind::TYPE) && h.position.line == 9)
        .expect("the inferred Dictionary[A.E, int] var must still get a TYPE hint (label)");
    assert!(
        dict_hint.text_edits.is_none(),
        "a script-enum Dictionary key must carry NO textEdit (would corrupt the file); got {:?}",
        dict_hint.text_edits
    );

    shutdown(&client, server_thread);
}

/// #115: a `var := …` that infers a container whose ELEMENT is a `class_name`'d script
/// (`Array[Hero]`) MUST get a `Array[Hero]` TYPE hint AND a correct auto-insert `textEdit`.
///
/// Inference is correct (hover renders `Array[<Script #N>]`); the bug was that the shared label
/// renderer `human_type_label` did not recurse name-substitution into container element types, so a
/// script element fell through to `Display`'s `<Script #N>` placeholder and the `<`-guard in
/// `hintable_type_label` dropped the whole hint. With the fix, the label is `Array[Hero]` (no `<`),
/// so the hint survives, and the edit path (`annotation_type`, which already recurses) emits
/// `: Array[Hero] = `. Verified by apply→reanalyze-by-identity (no new diagnostic on insert).
#[test]
fn named_script_container_element_emits_hint() {
    let p = base_project();
    let (server, client) = Connection::memory();
    let server_thread = std::thread::spawn(move || gd_server::serve(server));
    let hero = "class_name Hero\nextends RefCounted\n";
    let t = "extends Node\n\nvar arr: Array[Hero] = []\n\nfunc run() -> void:\n\tvar h := arr\n";
    init_and_open_caps(
        &p,
        &client,
        &[("hero.gd", hero), ("t.gd", t)],
        inlay_caps(false, false),
    );
    let uri = file_uri(&p.root.join("t.gd"));

    // Baseline diagnostics for the unedited source (by code+message identity).
    let base_diags = reopen_and_get_diags(&client, &uri, t, 100);
    let base_set: std::collections::HashSet<(Option<lsp_types::NumberOrString>, String)> =
        base_diags
            .diagnostics
            .iter()
            .map(|d| (d.code.clone(), d.message.clone()))
            .collect();

    let hints = request_hints(&client, 10, &uri, whole_doc());

    // The `var h := arr` line (5) must now carry a TYPE hint labelled `: Array[Hero] = `.
    let type_hint = hints
        .iter()
        .find(|h| h.kind == Some(InlayHintKind::TYPE) && h.position.line == 5)
        .expect("Array[Hero] must now yield a TYPE hint (label recurses name-substitution)");
    assert_eq!(
        label_of(type_hint),
        ": Array[Hero]",
        "the label must render the named-script element as `Hero`, not `<Script #N>`"
    );

    // …and a correct auto-insert textEdit. Verify by apply→reanalyze-by-identity, not a string-only
    // compare: applying the edit must introduce NO new diagnostic.
    let edit = type_hint
        .text_edits
        .as_ref()
        .and_then(|e| e.first())
        .expect("a named-script container element must carry a textEdit");
    assert_eq!(
        edit.new_text, ": Array[Hero] = ",
        "the edit must insert the parametrized container annotation; got {:?}",
        edit.new_text
    );

    let edited = apply_edits(t, std::slice::from_ref(edit));
    assert!(
        edited.contains("var h: Array[Hero] = arr"),
        "the edit must produce `var h: Array[Hero] = arr`; got:\n{edited}"
    );
    let new_diags = reopen_and_get_diags(&client, &uri, &edited, 101);
    for d in &new_diags.diagnostics {
        assert!(
            base_set.contains(&(d.code.clone(), d.message.clone())),
            "applying the Array[Hero] hint edit introduced a NEW diagnostic: {d:?}\n\
             baseline was {:?}\nedited source:\n{edited}",
            base_diags.diagnostics
        );
    }

    shutdown(&client, server_thread);
}

/// A `var := …` that infers a `class_name`'d script type DOES get a `textEdit` inserting the bare
/// class name (the positive companion to the unnamed-script case above).
#[test]
fn text_edit_for_named_script_inferred_type() {
    let p = base_project();
    let (server, client) = Connection::memory();
    let server_thread = std::thread::spawn(move || gd_server::serve(server));
    let hero = "class_name Hero\nextends RefCounted\n\nfunc greet() -> void:\n\tpass\n";
    let t = "extends Node\n\nfunc run() -> void:\n\tvar h := Hero.new()\n\th.greet()\n";
    init_and_open_caps(
        &p,
        &client,
        &[("hero.gd", hero), ("t.gd", t)],
        inlay_caps(false, false),
    );
    let uri = file_uri(&p.root.join("t.gd"));
    let hints = request_hints(&client, 10, &uri, whole_doc());

    let type_hint = hints
        .iter()
        .find(|h| h.kind == Some(InlayHintKind::TYPE) && h.position.line == 3)
        .expect("the inferred named-script var must get a TYPE hint");
    let edit = type_hint
        .text_edits
        .as_ref()
        .and_then(|e| e.first())
        .expect("a class_name'd script type must carry a textEdit");
    assert_eq!(
        edit.new_text, ": Hero = ",
        "the edit must insert the bare class_name; got {:?}",
        edit.new_text
    );

    shutdown(&client, server_thread);
}

/// A `var := …` that infers a script type whose `class_name` uses NON-ASCII (Unicode XID) identifier
/// characters DOES get a `textEdit` inserting the bare class name — the lexer accepts Unicode
/// identifiers (`unicode_ident::is_xid_start`/`is_xid_continue`), so the annotation gate must too. A
/// withheld edit here would be a silent under-emission for a perfectly source-valid type.
#[test]
fn text_edit_for_unicode_named_script_inferred_type() {
    let p = base_project();
    let (server, client) = Connection::memory();
    let server_thread = std::thread::spawn(move || gd_server::serve(server));
    let hero = "class_name Héro\nextends RefCounted\n\nfunc greet() -> void:\n\tpass\n";
    let t = "extends Node\n\nfunc run() -> void:\n\tvar h := Héro.new()\n\th.greet()\n";
    init_and_open_caps(
        &p,
        &client,
        &[("hero.gd", hero), ("t.gd", t)],
        inlay_caps(false, false),
    );
    let uri = file_uri(&p.root.join("t.gd"));
    let hints = request_hints(&client, 10, &uri, whole_doc());

    let type_hint = hints
        .iter()
        .find(|h| h.kind == Some(InlayHintKind::TYPE) && h.position.line == 3)
        .expect("the inferred Unicode-named-script var must get a TYPE hint");
    let edit = type_hint
        .text_edits
        .as_ref()
        .and_then(|e| e.first())
        .expect("a Unicode class_name'd script type must carry a textEdit");
    assert_eq!(
        edit.new_text, ": Héro = ",
        "the edit must insert the bare Unicode class_name; got {:?}",
        edit.new_text
    );

    shutdown(&client, server_thread);
}

/// A script whose `class_name` collides with a builtin type name (`Array`) must NOT get a
/// type-annotation `textEdit` — `: Array = ` would re-parse as the builtin `Array`, not the script,
/// silently mis-annotating the variable. Godot's analyzer rejects such a `class_name` ("hides a
/// built-in type", `gdscript_analyzer.cpp`), so this is a malformed-project-only path; under gdls's
/// "never crash, never lie" tolerance the index still surfaces the name (the registry does not
/// arbitrate), so the annotation gate must refuse it. The informational LABEL may still show; only
/// the corrupting auto-insert edit is withheld.
#[test]
fn no_text_edit_for_script_class_name_colliding_with_builtin() {
    let p = base_project();
    let (server, client) = Connection::memory();
    let server_thread = std::thread::spawn(move || gd_server::serve(server));
    // A sibling script declaring `class_name Array` (collides with the builtin `Array`). Godot would
    // reject this, but gdls's index surfaces it anyway. The var is typed via `preload(…).new()` (NOT
    // the bare `Array.new()`, which the analyzer resolves to the builtin) so the inferred type is the
    // SCRIPT — exercising the `DtKind::Script` annotation arm with a colliding `class_name`.
    let arr = "class_name Array\nextends RefCounted\n\nfunc greet() -> void:\n\tpass\n";
    let t = "extends Node\n\nfunc run() -> void:\n\tvar h := preload(\"res://arr.gd\").new()\n\th.greet()\n";
    init_and_open_caps(
        &p,
        &client,
        &[("arr.gd", arr), ("t.gd", t)],
        inlay_caps(false, false),
    );
    let uri = file_uri(&p.root.join("t.gd"));
    let hints = request_hints(&client, 10, &uri, whole_doc());

    if let Some(type_hint) = hints
        .iter()
        .find(|h| h.kind == Some(InlayHintKind::TYPE) && h.position.line == 3)
    {
        assert!(
            type_hint.text_edits.as_ref().is_none_or(|e| e.is_empty()),
            "a script `class_name` that collides with the builtin `Array` must carry NO textEdit \
             (`: Array = ` would re-parse as the builtin); got {:?}",
            type_hint.text_edits
        );
    }

    shutdown(&client, server_thread);
}

/// A script whose `class_name` collides with a native engine class name (`Node`) must NOT get a
/// type-annotation `textEdit` — `: Node = ` would re-parse as the native `Node`, not the script.
/// Same malformed-project-only rationale as the builtin-collision case (Godot rejects it: "hides a
/// native class", `gdscript_analyzer.cpp`).
#[test]
fn no_text_edit_for_script_class_name_colliding_with_native() {
    let p = base_project();
    let (server, client) = Connection::memory();
    let server_thread = std::thread::spawn(move || gd_server::serve(server));
    let node = "class_name Node\nextends RefCounted\n\nfunc greet() -> void:\n\tpass\n";
    let t = "extends Node\n\nfunc run() -> void:\n\tvar h := preload(\"res://node.gd\").new()\n\th.greet()\n";
    init_and_open_caps(
        &p,
        &client,
        &[("node.gd", node), ("t.gd", t)],
        inlay_caps(false, false),
    );
    let uri = file_uri(&p.root.join("t.gd"));
    let hints = request_hints(&client, 10, &uri, whole_doc());

    if let Some(type_hint) = hints
        .iter()
        .find(|h| h.kind == Some(InlayHintKind::TYPE) && h.position.line == 3)
    {
        assert!(
            type_hint.text_edits.as_ref().is_none_or(|e| e.is_empty()),
            "a script `class_name` that collides with the native `Node` must carry NO textEdit; \
             got {:?}",
            type_hint.text_edits
        );
    }

    shutdown(&client, server_thread);
}

/// Regression (review M1): an inner-class method that shares a name with a root-class method must
/// get the INNER method's parameter names, not the root's — the analyzer's `class_path` disambiguates
/// the callee, and the resolver must honor it (a wrong name is a "never lie" violation).
#[test]
fn parameter_hints_inner_class_method_not_confused_with_root() {
    let p = base_project();
    let (server, client) = Connection::memory();
    let server_thread = std::thread::spawn(move || gd_server::serve(server));
    // Root `combine(a, b)` and inner `Inner.combine(x, y)` — both 2-arg, same name. The call goes to
    // the INNER one, so the hints must be `x:`/`y:`, NEVER `a:`/`b:`.
    let src = "extends Node\n\nfunc combine(a: int, b: int) -> void:\n\tpass\n\nclass Inner:\n\tfunc combine(x: int, y: int) -> void:\n\t\tpass\n\nfunc run() -> void:\n\tvar inner := Inner.new()\n\tinner.combine(1, 2)\n";
    init_and_open_caps(&p, &client, &[("t.gd", src)], inlay_caps(false, false));
    let uri = file_uri(&p.root.join("t.gd"));
    let hints = request_hints(&client, 10, &uri, whole_doc());

    let param_labels: Vec<String> = hints
        .iter()
        .filter(|h| h.kind == Some(InlayHintKind::PARAMETER))
        .map(label_of)
        .collect();
    // If the inner method resolves, its names appear and the root names must NOT. (If gdls can't
    // resolve the inner call at all, it emits NO param hints — also acceptable: a miss, not a lie.)
    assert!(
        !param_labels.contains(&"a:".to_string()) && !param_labels.contains(&"b:".to_string()),
        "the root `combine(a, b)` names must NEVER label the inner `Inner.combine` call; got {param_labels:?}"
    );

    shutdown(&client, server_thread);
}

/// Criterion 3: a multi-argument call gets PARAMETER hints (the parameter names before each
/// argument); a single-argument call gets NO parameter hint by default.
#[test]
fn parameter_hints_multi_arg_but_not_single_arg() {
    let p = base_project();
    let (server, client) = Connection::memory();
    let server_thread = std::thread::spawn(move || gd_server::serve(server));
    // A same-file `move(x, y)` and a same-file `jump(height)`; `run` calls both.
    let src = "extends Node\n\nfunc move(x: int, y: int) -> void:\n\tpass\n\nfunc jump(height: int) -> void:\n\tpass\n\nfunc run() -> void:\n\tmove(10, 20)\n\tjump(5)\n";
    init_and_open_caps(&p, &client, &[("t.gd", src)], inlay_caps(false, false));
    let uri = file_uri(&p.root.join("t.gd"));
    let hints = request_hints(&client, 10, &uri, whole_doc());

    let param_hints: Vec<&InlayHint> = hints
        .iter()
        .filter(|h| h.kind == Some(InlayHintKind::PARAMETER))
        .collect();
    let labels: Vec<String> = param_hints.iter().map(|h| label_of(h)).collect();

    // `move(10, 20)` → `x:` and `y:` parameter hints.
    assert!(
        labels.contains(&"x:".to_string()) && labels.contains(&"y:".to_string()),
        "multi-arg `move(10, 20)` must get `x:`/`y:` parameter hints; got {labels:?}"
    );
    // The single-arg `jump(5)` must get NO parameter hint — so `height:` never appears.
    assert!(
        !labels.contains(&"height:".to_string()),
        "single-arg `jump(5)` must get NO parameter hint by default; got {labels:?}"
    );

    shutdown(&client, server_thread);
}

/// A native multi-argument method call gets PARAMETER hints from the DB. (Uses a method on the
/// extended native chain so the names are sourced from `extension_api.json`.)
#[test]
fn parameter_hints_for_native_method() {
    let p = TempProject::new();
    p.write(
        "project.godot",
        "config_version=5\n\n[application]\nconfig/features=PackedStringArray(\"4.6\")\n",
    );
    // A richer API: Node with a 2-arg method so the native param-name path is exercised.
    p.write(
        "extension_api.json",
        r#"{
            "header": {"version_major": 4, "version_minor": 6, "version_patch": 3},
            "classes": [
                {"name": "Object"},
                {"name": "Node", "inherits": "Object", "methods": [
                    {"name": "add_child", "is_const": false, "is_static": false, "is_vararg": false, "is_virtual": false,
                     "return_value": {"type": "void"},
                     "arguments": [
                        {"name": "node", "type": "Node"},
                        {"name": "force_readable_name", "type": "bool"}
                     ]}
                ]}
            ]
        }"#,
    );
    let (server, client) = Connection::memory();
    let server_thread = std::thread::spawn(move || gd_server::serve(server));
    let src = "extends Node\n\nfunc run() -> void:\n\tadd_child(self, true)\n";
    init_and_open_caps(&p, &client, &[("t.gd", src)], inlay_caps(false, false));
    let uri = file_uri(&p.root.join("t.gd"));
    let hints = request_hints(&client, 10, &uri, whole_doc());

    let labels: Vec<String> = hints
        .iter()
        .filter(|h| h.kind == Some(InlayHintKind::PARAMETER))
        .map(label_of)
        .collect();
    assert!(
        labels.contains(&"node:".to_string())
            && labels.contains(&"force_readable_name:".to_string()),
        "native `add_child(self, true)` must get `node:`/`force_readable_name:` hints; got {labels:?}"
    );

    shutdown(&client, server_thread);
}

/// Criterion 4: each kind is independently config-toggleable; disabling TYPE hints via
/// `didChangeConfiguration` emits `workspace/inlayHint/refresh` and the next request reflects the
/// change (parameter hints still present, type hints gone) — verified live over the same connection.
#[test]
fn config_toggle_emits_refresh_and_reflects_live() {
    let p = base_project();
    let (server, client) = Connection::memory();
    let server_thread = std::thread::spawn(move || gd_server::serve(server));
    // A fixture with BOTH a type hint (`var n := 1`) and a param hint (`move(10, 20)`).
    let src = "extends Node\n\nfunc move(x: int, y: int) -> void:\n\tpass\n\nfunc run() -> void:\n\tvar n := 1\n\tmove(10, 20)\n";
    // refreshSupport on so the server actually sends the refresh.
    init_and_open_caps(&p, &client, &[("t.gd", src)], inlay_caps(false, true));
    let uri = file_uri(&p.root.join("t.gd"));

    // Before: both kinds present.
    let before = request_hints(&client, 10, &uri, whole_doc());
    assert!(
        before.iter().any(|h| h.kind == Some(InlayHintKind::TYPE)),
        "type hint present before toggle; got {before:?}"
    );
    assert!(
        before
            .iter()
            .any(|h| h.kind == Some(InlayHintKind::PARAMETER)),
        "param hint present before toggle; got {before:?}"
    );

    // Toggle: disable TYPE hints, keep PARAMETER hints (payload path — no workspace.configuration).
    client
        .sender
        .send(notification(
            "workspace/didChangeConfiguration",
            serde_json::json!({
                "settings": { "gdls": { "inlayHint": { "typeHints": false, "parameterHints": true } } }
            }),
        ))
        .unwrap();

    // The server sends `workspace/inlayHint/refresh` (a server→client REQUEST). Observe it (and
    // reply, so the outbound entry is consumed).
    let refresh = loop {
        match recv(&client) {
            Message::Request(req) if req.method == "workspace/inlayHint/refresh" => break req,
            Message::Request(other) => panic!("unexpected server request {}", other.method),
            _ => {} // skip any stray notification
        }
    };
    client
        .sender
        .send(Message::Response(Response::new_ok(
            refresh.id,
            serde_json::Value::Null,
        )))
        .unwrap();

    // After: TYPE hints gone, PARAMETER hints still present — the live re-request reflects the toggle.
    let after = request_hints(&client, 11, &uri, whole_doc());
    assert!(
        !after.iter().any(|h| h.kind == Some(InlayHintKind::TYPE)),
        "type hints must be gone after disabling them; got {after:?}"
    );
    assert!(
        after
            .iter()
            .any(|h| h.kind == Some(InlayHintKind::PARAMETER)),
        "param hints must remain after the toggle; got {after:?}"
    );

    shutdown(&client, server_thread);
}

/// Criterion 5a: a `resolveSupport` client receives hints WITHOUT an embedded tooltip; the tooltip
/// is filled lazily by `inlayHint/resolve`.
#[test]
fn resolve_support_client_gets_lazy_tooltip() {
    let p = base_project();
    let (server, client) = Connection::memory();
    let server_thread = std::thread::spawn(move || gd_server::serve(server));
    let src = "extends Node\n\nfunc run() -> void:\n\tvar speed := 5.0\n";
    init_and_open_caps(&p, &client, &[("t.gd", src)], inlay_caps(true, false));
    let uri = file_uri(&p.root.join("t.gd"));
    let hints = request_hints(&client, 10, &uri, whole_doc());

    let type_hint = hints
        .iter()
        .find(|h| h.kind == Some(InlayHintKind::TYPE))
        .expect("a type hint must be present")
        .clone();
    // Lazy: no tooltip yet, but a `data` blob carrying what resolve needs.
    assert!(
        type_hint.tooltip.is_none(),
        "a resolveSupport client must NOT receive the tooltip eagerly; got {:?}",
        type_hint.tooltip
    );
    assert!(
        type_hint.data.is_some(),
        "a deferred hint must carry a `data` blob for resolve"
    );

    // Resolve it: the tooltip is filled.
    client
        .sender
        .send(request(20, "inlayHint/resolve", &type_hint))
        .unwrap();
    let resp = common::recv_response(&client);
    assert!(
        resp.error.is_none(),
        "inlayHint/resolve errored: {:?}",
        resp.error
    );
    let resolved: InlayHint = serde_json::from_value(resp.result.unwrap()).unwrap();
    assert!(
        resolved.tooltip.is_some(),
        "resolve must fill the tooltip; got {resolved:?}"
    );

    shutdown(&client, server_thread);
}

/// Criterion 5b: a non-resolve client receives the tooltip EAGERLY (no resolve round-trip needed).
#[test]
fn non_resolve_client_gets_eager_tooltip() {
    let p = base_project();
    let (server, client) = Connection::memory();
    let server_thread = std::thread::spawn(move || gd_server::serve(server));
    let src = "extends Node\n\nfunc run() -> void:\n\tvar speed := 5.0\n";
    init_and_open_caps(&p, &client, &[("t.gd", src)], inlay_caps(false, false));
    let uri = file_uri(&p.root.join("t.gd"));
    let hints = request_hints(&client, 10, &uri, whole_doc());

    let type_hint = hints
        .iter()
        .find(|h| h.kind == Some(InlayHintKind::TYPE))
        .expect("a type hint must be present");
    assert!(
        type_hint.tooltip.is_some(),
        "a non-resolve client must receive the tooltip eagerly; got {type_hint:?}"
    );
    assert!(
        type_hint.data.is_none(),
        "an eager hint needs no `data` blob; got {:?}",
        type_hint.data
    );

    shutdown(&client, server_thread);
}

/// Criterion 6: a TYPE hint's `textEdit`, applied to the source, makes the file re-analyze with ZERO
/// new diagnostics (the "the hint becomes part of the document" contract). Verifies both the `:=`
/// (gap-replace) edit and the `for` (insert) edit.
#[test]
fn type_hint_text_edit_applies_clean() {
    let p = base_project();
    let (server, client) = Connection::memory();
    let server_thread = std::thread::spawn(move || gd_server::serve(server));
    let src = "extends Node\n\nfunc run() -> void:\n\tvar speed := 5.0\n\tfor i in range(3):\n\t\tprint(i)\n";
    init_and_open_caps(&p, &client, &[("t.gd", src)], inlay_caps(false, false));
    let uri = file_uri(&p.root.join("t.gd"));

    // Baseline: re-open the original and capture its diagnostic set (keyed on code+message — an edit
    // adds no newlines, so ranges don't shift, but code+message is the stable identity we compare on).
    let base_diags = reopen_and_get_diags(&client, &uri, src, 100);
    let base_set: std::collections::HashSet<(Option<lsp_types::NumberOrString>, String)> =
        base_diags
            .diagnostics
            .iter()
            .map(|d| (d.code.clone(), d.message.clone()))
            .collect();

    let hints = request_hints(&client, 10, &uri, whole_doc());
    // Apply every TYPE hint's textEdit to the source (they're on disjoint lines, so order-independent
    // application is fine; we apply each independently and re-check below per-edit too).
    let edits: Vec<lsp_types::TextEdit> = hints
        .iter()
        .filter(|h| h.kind == Some(InlayHintKind::TYPE))
        .filter_map(|h| h.text_edits.clone())
        .flatten()
        .collect();
    assert!(
        edits.len() >= 2,
        "expected a textEdit on both the `:=` and the `for` type hint; got {edits:?}"
    );

    let edited = apply_edits(src, &edits);
    // The edited source must read as the annotated forms.
    assert!(
        edited.contains("var speed: float = 5.0"),
        "the `:=` edit must produce `var speed: float = 5.0`; got:\n{edited}"
    );
    assert!(
        edited.contains("for i: int in range(3):"),
        "the `for` edit must produce `for i: int in …`; got:\n{edited}"
    );

    // Re-analyze the edited source: ZERO *new* diagnostics versus the baseline. Identity-based (not
    // a count compare): the `:=` edit legitimately REMOVES the inferred-declaration diagnostic, so a
    // count could drop even while a brand-new error slipped in — assert every edited-source
    // diagnostic was already present in the baseline.
    let new_diags = reopen_and_get_diags(&client, &uri, &edited, 101);
    for d in &new_diags.diagnostics {
        assert!(
            base_set.contains(&(d.code.clone(), d.message.clone())),
            "applying the type-hint textEdits introduced a NEW diagnostic: {d:?}\n\
             baseline was {:?}\nedited source:\n{edited}",
            base_diags.diagnostics
        );
    }

    shutdown(&client, server_thread);
}

/// Regression (#73 mutating-surface review): a `var x := …` whose initializer is PARENTHESIZED
/// (`var z := (1 + 2)`) must NOT corrupt the file. Parentheses are transparent in the AST — the
/// initializer node begins INSIDE the parens (at `1`) — so a `:=` edit that replaced
/// `[ident_end, init_start)` swallowed the `(` and orphaned the `)`, turning a baseline-clean file
/// into `var z: int = 1 + 2)` with a NEW `Closing ")" doesn't have an opening counterpart` error.
/// The fix ends the replace span at the operator's trailing whitespace (the `(` as WRITTEN), not at
/// the node start. Covers single-line, MULTILINE, and DOUBLE parens — all by-identity (a clean
/// baseline must gain no new diagnostic), the only proof that catches a swallowed token.
#[test]
fn type_hint_text_edit_paren_initializer_no_corruption() {
    let p = base_project();
    let (server, client) = Connection::memory();
    let server_thread = std::thread::spawn(move || gd_server::serve(server));
    // `a` single-line `(1 + 2)`, `b` multiline `(\n 3 \n)`, `c` double `((4 + 5))` — all infer int.
    let src = "extends Node\n\nfunc run() -> void:\n\tvar a := (1 + 2)\n\tvar b := (\n\t\t3\n\t)\n\tvar c := ((4 + 5))\n\tprint(a)\n\tprint(b)\n\tprint(c)\n";
    init_and_open_caps(&p, &client, &[("t.gd", src)], inlay_caps(false, false));
    let uri = file_uri(&p.root.join("t.gd"));

    let base_diags = reopen_and_get_diags(&client, &uri, src, 100);
    let base_set: std::collections::HashSet<(Option<lsp_types::NumberOrString>, String)> =
        base_diags
            .diagnostics
            .iter()
            .map(|d| (d.code.clone(), d.message.clone()))
            .collect();

    let hints = request_hints(&client, 10, &uri, whole_doc());
    let edits: Vec<lsp_types::TextEdit> = hints
        .iter()
        .filter(|h| h.kind == Some(InlayHintKind::TYPE))
        .filter_map(|h| h.text_edits.clone())
        .flatten()
        .collect();
    assert!(
        edits.len() >= 3,
        "all three parenthesized `:=` vars must carry a textEdit; got {edits:?}"
    );

    let edited = apply_edits(src, &edits);
    // The parens must SURVIVE intact (no swallowed `(`, no orphaned `)`).
    assert!(
        edited.contains("var a: int = (1 + 2)"),
        "single-line paren initializer must survive; got:\n{edited}"
    );
    assert!(
        edited.contains("var b: int = (") && edited.contains("\t\t3\n\t)"),
        "multiline paren initializer must survive; got:\n{edited}"
    );
    assert!(
        edited.contains("var c: int = ((4 + 5))"),
        "double-paren initializer must survive; got:\n{edited}"
    );

    // By-identity: the edited source must introduce NO new diagnostic versus the (clean) baseline.
    let new_diags = reopen_and_get_diags(&client, &uri, &edited, 101);
    for d in &new_diags.diagnostics {
        assert!(
            base_set.contains(&(d.code.clone(), d.message.clone())),
            "applying a paren-initializer type-hint edit introduced a NEW diagnostic: {d:?}\n\
             baseline was {:?}\nedited source:\n{edited}",
            base_diags.diagnostics
        );
    }

    shutdown(&client, server_thread);
}

/// Criterion 7: hints respect the requested range — a range covering only the `for` loop returns the
/// for-var hint but not the earlier `var :=` hint.
#[test]
fn hints_respect_requested_range() {
    let p = base_project();
    let (server, client) = Connection::memory();
    let server_thread = std::thread::spawn(move || gd_server::serve(server));
    let src = "extends Node\n\nfunc run() -> void:\n\tvar speed := 5.0\n\tfor i in range(3):\n\t\tprint(i)\n";
    init_and_open_caps(&p, &client, &[("t.gd", src)], inlay_caps(false, false));
    let uri = file_uri(&p.root.join("t.gd"));

    // A range covering only line 4 (`\tfor i in …`) — excludes the `var speed :=` on line 3.
    let range = Range {
        start: Position {
            line: 4,
            character: 0,
        },
        end: Position {
            line: 5,
            character: 0,
        },
    };
    let hints = request_hints(&client, 10, &uri, range);
    assert!(
        hints
            .iter()
            .any(|h| h.kind == Some(InlayHintKind::TYPE) && h.position.line == 4),
        "the for-var hint (line 4) must be in range; got {hints:?}"
    );
    assert!(
        hints.iter().all(|h| h.position.line != 3),
        "the `var speed :=` hint (line 3) must be OUT of the requested range; got {hints:?}"
    );

    shutdown(&client, server_thread);
}

// ---------------------------------------------------------------------------
// Helpers for the textEdit-clean test.
// ---------------------------------------------------------------------------

/// Re-open `uri` with `text` at `version` and return the resulting `publishDiagnostics`.
fn reopen_and_get_diags(
    client: &Connection,
    uri: &Uri,
    text: &str,
    version: i32,
) -> PublishDiagnosticsParams {
    client
        .sender
        .send(notification(
            "textDocument/didOpen",
            DidOpenTextDocumentParams {
                text_document: TextDocumentItem {
                    uri: uri.clone(),
                    language_id: "gdscript".to_string(),
                    version,
                    text: text.to_string(),
                },
            },
        ))
        .unwrap();
    // Drain to the publish for this URI (skip any stray earlier message).
    loop {
        if let Message::Notification(note) = recv(client) {
            if note.method == "textDocument/publishDiagnostics" {
                let params: PublishDiagnosticsParams = serde_json::from_value(note.params).unwrap();
                if &params.uri == uri {
                    return params;
                }
            }
        }
    }
}

/// Apply a set of LSP `TextEdit`s to `src`. Edits are sorted by start position (descending) so
/// earlier offsets stay valid as later ones are spliced — the standard non-overlapping-edit apply.
fn apply_edits(src: &str, edits: &[lsp_types::TextEdit]) -> String {
    // Build a byte-offset table for (line, utf16-char) → byte. The fixtures are ASCII, so utf16
    // char == byte column; keep it simple but correct for ASCII.
    let line_starts: Vec<usize> = std::iter::once(0)
        .chain(src.match_indices('\n').map(|(i, _)| i + 1))
        .collect();
    let to_byte = |pos: Position| -> usize {
        let line_start = line_starts[pos.line as usize];
        line_start + pos.character as usize
    };
    let mut spans: Vec<(usize, usize, String)> = edits
        .iter()
        .map(|e| {
            (
                to_byte(e.range.start),
                to_byte(e.range.end),
                e.new_text.clone(),
            )
        })
        .collect();
    spans.sort_by_key(|(start, _, _)| std::cmp::Reverse(*start));
    let mut out = src.to_string();
    for (start, end, new_text) in spans {
        out.replace_range(start..end, &new_text);
    }
    out
}

/// A drained inlay-hint request that asserts an empty result when both toggles are off — kept as a
/// fast guard the no-op path returns `[]` rather than `null`.
#[test]
fn both_toggles_off_returns_empty() {
    let p = base_project();
    let (server, client) = Connection::memory();
    let server_thread = std::thread::spawn(move || gd_server::serve(server));
    let src = "extends Node\n\nfunc run() -> void:\n\tvar speed := 5.0\n";
    init_and_open_caps(&p, &client, &[("t.gd", src)], inlay_caps(false, false));
    let uri = file_uri(&p.root.join("t.gd"));

    // Turn both off.
    client
        .sender
        .send(notification(
            "workspace/didChangeConfiguration",
            serde_json::json!({
                "settings": { "gdls": { "inlayHint": { "typeHints": false, "parameterHints": false } } }
            }),
        ))
        .unwrap();
    // No refreshSupport advertised → no refresh request is sent; drain briefly.
    while try_recv(&client, Duration::from_millis(200)).is_some() {}

    let hints = request_hints(&client, 10, &uri, whole_doc());
    assert!(
        hints.is_empty(),
        "both toggles off must yield no hints; got {hints:?}"
    );

    shutdown(&client, server_thread);
}

/// A `const X := …` is a `:=` declaration and gets the same inferred-type hint and rewrite edit as
/// a `var` (#347). It is the case with the least other signal — the type appears nowhere on the
/// line — and it is where the rewrite edit is most useful, since a project tightening its typing is
/// exactly what adds `const` annotations.
#[test]
fn type_hints_on_an_inferred_const_at_both_scopes() {
    let p = base_project();
    let (server, client) = Connection::memory();
    let server_thread = std::thread::spawn(move || gd_server::serve(server));
    let src = "extends Node\n\nconst CLASS_C := 2.0\nconst ANNOTATED: int = 4\n\nfunc run() -> void:\n\tconst LOCAL_C := 4.0\n\tconst PARENED := (1 + 2)\n\tprint(CLASS_C, ANNOTATED, LOCAL_C, PARENED)\n";
    init_and_open_caps(&p, &client, &[("t.gd", src)], inlay_caps(false, false));
    let uri = file_uri(&p.root.join("t.gd"));
    let hints = request_hints(&client, 10, &uri, whole_doc());
    let type_hints: Vec<&InlayHint> = hints
        .iter()
        .filter(|h| h.kind == Some(InlayHintKind::TYPE))
        .collect();

    for (line, label) in [(2, ": float"), (6, ": float"), (7, ": int")] {
        assert!(
            type_hints
                .iter()
                .any(|h| label_of(h) == label && h.position.line == line),
            "expected a `{label}` TYPE hint on line {line}; got {type_hints:?}"
        );
    }
    // An annotated `const` already says its type — no hint.
    assert!(
        !type_hints.iter().any(|h| h.position.line == 3),
        "an annotated `const ANNOTATED: int` must get NO hint; got {type_hints:?}"
    );

    // The rewrite edit neutralizes `:=` and, for a PARENTHESIZED initializer, must stop at the
    // `(` — replacing up to the initializer NODE's start would eat the paren and orphan its match.
    let parened = type_hints
        .iter()
        .find(|h| h.position.line == 7)
        .expect("the parenthesized const's hint");
    let edits = parened
        .text_edits
        .as_ref()
        .expect("a rewrite edit on a builtin-typed const");
    assert_eq!(edits.len(), 1);
    assert_eq!(edits[0].new_text, ": int = ");
    // `\tconst PARENED := (1 + 2)`: tab(0) `const`(1-5) ` `(6) `PARENED`(7-13) ` := `(14-17) `(`(18).
    assert_eq!(edits[0].range.start.character, 14);
    assert_eq!(
        edits[0].range.end.character, 18,
        "the edit must end at the `(`, not inside it: {:?}",
        edits[0].range
    );

    shutdown(&client, server_thread);
}
