//! M10 (#74): `textDocument/documentColor` + `textDocument/colorPresentation` for GDScript `Color`
//! literals.
//!
//! Protocol-shape tests over the in-memory `Connection` (the same rig as `folding_range.rs` /
//! `document_highlight.rs`). They cover the three literal forms the feature must recognize, the
//! colorPresentation round-trip, the no-false-swatch discrimination, and — airtight — that the
//! named-constant RGBA flows from `extension_api.json` (the fixture below gives one constant a
//! deliberately non-standard value; the handler must echo *that*, which a hard-coded color table
//! never would).
//!
//! Acceptance criteria exercised:
//!   1. `colorProvider` advertised in `InitializeResult`.
//!   2. `Color(0.2, 0.4, 0.6)` and `Color(1, 0, 0, 0.5)` → correct normalized RGBA, range over the
//!      whole call.
//!   3. `Color.RED` / `Color.CORNFLOWER_BLUE` → exact RGBA from the native DB (NOT a hard-coded
//!      table — proven by the doctored fixture).
//!   4. `Color("#ff8800")`, `Color("#ff8800cc")`, `Color("red")` → correct color; a malformed string
//!      → NO swatch.
//!   5. colorPresentation round-trips losslessly (returned edit re-parses through documentColor to
//!      the identical color); named constant offered only on exact match; edit replaces the whole
//!      literal.
//!   6. A user variable/type named `Color` (non-constructor) → NO swatch.

mod common;

use common::{file_uri, notification, request, shutdown, TempProject};
use lsp_server::Connection;
use lsp_types::{
    ClientCapabilities, Color, ColorInformation, ColorPresentation, ColorPresentationParams,
    DidOpenTextDocumentParams, DocumentColorParams, InitializeParams, InitializeResult,
    InitializedParams, PartialResultParams, Range, TextDocumentIdentifier, TextDocumentItem, Uri,
    WorkDoneProgressParams,
};

/// A dump whose `Color` builtin carries named-color constants as `value` strings (exactly the shape
/// `extension_api.json` writes). `CORNFLOWER_BLUE`/`RED`/`WHITE` use their real RGBA; `RED` and the
/// alias pair are realistic. **`AZURE` is deliberately doctored to a non-standard value** so a test
/// can prove the swatch RGBA is sourced from this dump, not a built-in table (the real AZURE is
/// `(0.94, 1, 1, 1)`).
const COLOR_API: &str = r#"{
    "header": {"version_major": 4, "version_minor": 6, "version_patch": 3},
    "classes": [
        {"name": "Object"},
        {"name": "Node", "inherits": "Object"}
    ],
    "builtin_classes": [
        {"name": "Color",
         "constants": [
            {"name": "RED", "type": "Color", "value": "Color(1, 0, 0, 1)"},
            {"name": "CORNFLOWER_BLUE", "type": "Color",
             "value": "Color(0.39215687, 0.58431375, 0.92941177, 1)"},
            {"name": "WHITE", "type": "Color", "value": "Color(1, 1, 1, 1)"},
            {"name": "AQUA", "type": "Color", "value": "Color(0, 1, 1, 1)"},
            {"name": "CYAN", "type": "Color", "value": "Color(0, 1, 1, 1)"},
            {"name": "AZURE", "type": "Color", "value": "Color(0.123, 0.456, 0.789, 1)"}
         ]}
    ]
}"#;

/// Initialize against `project` with the COLOR_API dump, send `initialized`, open `files`, drain
/// diagnostics. Returns the parsed `InitializeResult`.
fn init_and_open(
    project: &TempProject,
    client: &Connection,
    files: &[(&str, &str)],
) -> InitializeResult {
    let init = InitializeParams {
        initialization_options: Some(serde_json::json!({
            "projectRoot": project.root.as_str(),
            "autoDumpExtensionApi": false,
            "extensionApiPath": project.root.join("extension_api.json").as_str(),
        })),
        capabilities: ClientCapabilities::default(),
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

/// A base project: project.godot + the COLOR_API dump, no source files (tests write their own).
fn base_project() -> TempProject {
    let p = TempProject::new();
    p.write("project.godot", "config_version=5\n");
    p.write("extension_api.json", COLOR_API);
    p
}

fn document_color(client: &Connection, id: i32, uri: &Uri) -> Vec<ColorInformation> {
    client
        .sender
        .send(request(
            id,
            "textDocument/documentColor",
            DocumentColorParams {
                text_document: TextDocumentIdentifier { uri: uri.clone() },
                work_done_progress_params: WorkDoneProgressParams::default(),
                partial_result_params: PartialResultParams::default(),
            },
        ))
        .unwrap();
    let resp = common::recv_response(client);
    assert!(
        resp.error.is_none(),
        "documentColor errored: {:?}",
        resp.error
    );
    serde_json::from_value(resp.result.expect("documentColor result")).unwrap()
}

fn color_presentation(
    client: &Connection,
    id: i32,
    uri: &Uri,
    color: Color,
    range: Range,
) -> Vec<ColorPresentation> {
    client
        .sender
        .send(request(
            id,
            "textDocument/colorPresentation",
            ColorPresentationParams {
                text_document: TextDocumentIdentifier { uri: uri.clone() },
                color,
                range,
                work_done_progress_params: WorkDoneProgressParams::default(),
                partial_result_params: PartialResultParams::default(),
            },
        ))
        .unwrap();
    let resp = common::recv_response(client);
    assert!(
        resp.error.is_none(),
        "colorPresentation errored: {:?}",
        resp.error
    );
    serde_json::from_value(resp.result.expect("colorPresentation result")).unwrap()
}

/// Bitwise-exact f32 comparison (the lossless contract; mirrors the handler's `to_bits` match).
fn color_eq(a: Color, b: Color) -> bool {
    a.red.to_bits() == b.red.to_bits()
        && a.green.to_bits() == b.green.to_bits()
        && a.blue.to_bits() == b.blue.to_bits()
        && a.alpha.to_bits() == b.alpha.to_bits()
}

fn approx(a: f32, b: f32) -> bool {
    (a - b).abs() < 1e-6
}

/// Criterion 1: the server advertises `colorProvider`.
#[test]
fn color_provider_advertised() {
    let p = base_project();
    let (server, client) = Connection::memory();
    let server_thread = std::thread::spawn(move || gd_server::serve(server));
    let result = init_and_open(&p, &client, &[("a.gd", "extends Node\n")]);
    assert!(
        result.capabilities.color_provider.is_some(),
        "colorProvider must be advertised"
    );
    shutdown(&client, server_thread);
}

/// Criterion 2: numeric `Color(r, g, b)` / `Color(r, g, b, a)` → normalized RGBA, range over the
/// whole call. Components are 0–1 floats (NOT 0–255): `Color(1, 0, 0, 0.5)` is full red at half
/// alpha.
#[test]
fn numeric_constructor_rgba_and_range() {
    let p = base_project();
    let (server, client) = Connection::memory();
    let server_thread = std::thread::spawn(move || gd_server::serve(server));
    // Line 0: extends; line 2 and line 3 carry one literal each.
    let src = "extends Node\n\nvar a := Color(0.2, 0.4, 0.6)\nvar b := Color(1, 0, 0, 0.5)\n";
    init_and_open(&p, &client, &[("c.gd", src)]);
    let uri = file_uri(&p.root.join("c.gd"));
    let colors = document_color(&client, 10, &uri);
    assert_eq!(colors.len(), 2, "two numeric literals; got {colors:?}");

    // First literal: Color(0.2, 0.4, 0.6) → (0.2, 0.4, 0.6, 1).
    let first = colors.iter().find(|c| c.range.start.line == 2).unwrap();
    assert!(
        approx(first.color.red, 0.2)
            && approx(first.color.green, 0.4)
            && approx(first.color.blue, 0.6)
            && approx(first.color.alpha, 1.0),
        "Color(0.2,0.4,0.6) → (0.2,0.4,0.6,1); got {:?}",
        first.color
    );
    // Range spans exactly `Color(0.2, 0.4, 0.6)` (20 chars): from col 9 (after `var a := `) to
    // col 29 (the position just past the closing paren).
    assert_eq!(
        first.range.start.character, 9,
        "range starts at the `Color` token"
    );
    assert_eq!(
        first.range.end.character, 29,
        "range ends just past the closing paren; got {:?}",
        first.range
    );

    // Second literal: Color(1, 0, 0, 0.5) → full red, half alpha — NOT 1/255.
    let second = colors.iter().find(|c| c.range.start.line == 3).unwrap();
    assert!(
        approx(second.color.red, 1.0)
            && approx(second.color.green, 0.0)
            && approx(second.color.blue, 0.0)
            && approx(second.color.alpha, 0.5),
        "Color(1,0,0,0.5) → full red at 0.5 alpha (0–1 floats, not 0–255); got {:?}",
        second.color
    );
    shutdown(&client, server_thread);
}

/// Criterion 3: `Color.RED` / `Color.CORNFLOWER_BLUE` resolve to the exact native-DB RGBA, and the
/// **doctored** `Color.AZURE` proves the value comes from the dump (a hard-coded table would return
/// the real azure, not the fixture's `(0.123, 0.456, 0.789, 1)`).
#[test]
fn named_constant_rgba_from_native_db_not_hardcoded() {
    let p = base_project();
    let (server, client) = Connection::memory();
    let server_thread = std::thread::spawn(move || gd_server::serve(server));
    let src = "extends Node\n\nvar a := Color.RED\nvar b := Color.CORNFLOWER_BLUE\nvar c := Color.AZURE\n";
    init_and_open(&p, &client, &[("c.gd", src)]);
    let uri = file_uri(&p.root.join("c.gd"));
    let colors = document_color(&client, 10, &uri);
    assert_eq!(colors.len(), 3, "three named constants; got {colors:?}");

    let red = colors.iter().find(|c| c.range.start.line == 2).unwrap();
    assert!(
        color_eq(
            red.color,
            Color {
                red: 1.0,
                green: 0.0,
                blue: 0.0,
                alpha: 1.0
            }
        ),
        "Color.RED → (1,0,0,1); got {:?}",
        red.color
    );
    // Range spans exactly `Color.RED` (col 9..18).
    assert_eq!(red.range.start.character, 9);
    assert_eq!(
        red.range.end.character, 18,
        "range ends at the constant name"
    );

    let cb = colors.iter().find(|c| c.range.start.line == 3).unwrap();
    assert!(
        color_eq(
            cb.color,
            Color {
                red: 0.39215687,
                green: 0.58431375,
                blue: 0.92941177,
                alpha: 1.0
            }
        ),
        "Color.CORNFLOWER_BLUE → exact fractional RGBA; got {:?}",
        cb.color
    );

    // The airtight proof: AZURE echoes the fixture's doctored value.
    let az = colors.iter().find(|c| c.range.start.line == 4).unwrap();
    assert!(
        color_eq(
            az.color,
            Color { red: 0.123, green: 0.456, blue: 0.789, alpha: 1.0 }
        ),
        "Color.AZURE → the DOCTORED dump value (proves DB sourcing, not a hard-coded table); got {:?}",
        az.color
    );
    shutdown(&client, server_thread);
}

/// Criterion 4: hex and named string forms parse; a malformed string yields NO swatch.
#[test]
fn string_forms_hex_and_name_and_malformed() {
    let p = base_project();
    let (server, client) = Connection::memory();
    let server_thread = std::thread::spawn(move || gd_server::serve(server));
    // line 2: "#ff8800" (6-digit), line 3: "#ff8800cc" (8-digit), line 4: "red" (name),
    // line 5: "not-a-color" (malformed → no swatch).
    let src = "extends Node\n\nvar a := Color(\"#ff8800\")\nvar b := Color(\"#ff8800cc\")\nvar c := Color(\"red\")\nvar d := Color(\"not-a-color\")\n";
    init_and_open(&p, &client, &[("c.gd", src)]);
    let uri = file_uri(&p.root.join("c.gd"));
    let colors = document_color(&client, 10, &uri);

    // Exactly three swatches — the malformed string on line 5 must NOT produce one.
    assert_eq!(
        colors.len(),
        3,
        "3 valid string literals (the malformed one yields no swatch); got {colors:?}"
    );
    assert!(
        !colors.iter().any(|c| c.range.start.line == 5),
        "the malformed Color(\"not-a-color\") must produce NO swatch; got {colors:?}"
    );

    // #ff8800 → (1, 136/255, 0, 1).
    let hex6 = colors.iter().find(|c| c.range.start.line == 2).unwrap();
    assert!(
        approx(hex6.color.red, 1.0)
            && approx(hex6.color.green, 136.0 / 255.0)
            && approx(hex6.color.blue, 0.0)
            && approx(hex6.color.alpha, 1.0),
        "Color(\"#ff8800\") → (1, 0.533, 0, 1); got {:?}",
        hex6.color
    );

    // #ff8800cc → adds alpha 204/255.
    let hex8 = colors.iter().find(|c| c.range.start.line == 3).unwrap();
    assert!(
        approx(hex8.color.alpha, 204.0 / 255.0),
        "Color(\"#ff8800cc\") → alpha 204/255; got {:?}",
        hex8.color
    );

    // "red" → the DB's RED constant (1,0,0,1), via the named-color normalization path.
    let named = colors.iter().find(|c| c.range.start.line == 4).unwrap();
    assert!(
        color_eq(
            named.color,
            Color {
                red: 1.0,
                green: 0.0,
                blue: 0.0,
                alpha: 1.0
            }
        ),
        "Color(\"red\") → the named RED color from the DB; got {:?}",
        named.color
    );
    shutdown(&client, server_thread);
}

/// The named-color normalization is faithful to Godot's `find_named_color`: spaces/case/separators
/// are ignored. `Color("cornflower blue")` resolves the same constant as `Color.CORNFLOWER_BLUE`.
#[test]
fn named_string_normalization_ignores_separators_and_case() {
    let p = base_project();
    let (server, client) = Connection::memory();
    let server_thread = std::thread::spawn(move || gd_server::serve(server));
    let src =
        "extends Node\n\nvar a := Color(\"cornflower blue\")\nvar b := Color(\"CornflowerBlue\")\n";
    init_and_open(&p, &client, &[("c.gd", src)]);
    let uri = file_uri(&p.root.join("c.gd"));
    let colors = document_color(&client, 10, &uri);
    assert_eq!(
        colors.len(),
        2,
        "both name spellings resolve; got {colors:?}"
    );
    let want = Color {
        red: 0.39215687,
        green: 0.58431375,
        blue: 0.92941177,
        alpha: 1.0,
    };
    for c in &colors {
        assert!(
            color_eq(c.color, want),
            "normalized name → CORNFLOWER_BLUE RGBA; got {:?}",
            c.color
        );
    }
    shutdown(&client, server_thread);
}

/// Criterion 6: a user variable / type *use* named `Color` (not a constructor, not a `.constant`)
/// produces NO swatch.
#[test]
fn user_color_identifier_is_not_a_swatch() {
    let p = base_project();
    let (server, client) = Connection::memory();
    let server_thread = std::thread::spawn(move || gd_server::serve(server));
    // `Color` appears as a bare identifier (assignment target/use) and as a member access
    // (`self.Color`), never as a constructor or `Color.<constant>` — so: no swatches at all.
    let src = "extends Node\n\nvar Color := 5\nfunc f():\n\tvar x = Color\n\tprint(self.Color)\n";
    init_and_open(&p, &client, &[("c.gd", src)]);
    let uri = file_uri(&p.root.join("c.gd"));
    let colors = document_color(&client, 10, &uri);
    assert!(
        colors.is_empty(),
        "a bare/member `Color` (non-constructor) must yield no swatch; got {colors:?}"
    );
    shutdown(&client, server_thread);
}

/// A non-constant constructor argument (an identifier, a nested call) bails — no false swatch.
#[test]
fn non_constant_arguments_yield_no_swatch() {
    let p = base_project();
    let (server, client) = Connection::memory();
    let server_thread = std::thread::spawn(move || gd_server::serve(server));
    let src = "extends Node\n\nvar r := 1.0\nvar a := Color(r, 0.0, 0.0)\nvar b := Color(get_r(), 0, 0)\n";
    init_and_open(&p, &client, &[("c.gd", src)]);
    let uri = file_uri(&p.root.join("c.gd"));
    let colors = document_color(&client, 10, &uri);
    assert!(
        colors.is_empty(),
        "constructors with identifier/call args must yield no swatch; got {colors:?}"
    );
    shutdown(&client, server_thread);
}

/// Criterion 5: colorPresentation round-trips losslessly, the edit replaces the whole literal, and
/// the named-constant form is offered only on an exact match.
#[test]
fn color_presentation_round_trips_and_replaces_whole_literal() {
    let p = base_project();
    let (server, client) = Connection::memory();
    let server_thread = std::thread::spawn(move || gd_server::serve(server));
    // One numeric literal we'll "edit" via colorPresentation.
    let src = "extends Node\n\nvar a := Color(0.2, 0.4, 0.6)\n";
    init_and_open(&p, &client, &[("c.gd", src)]);
    let uri = file_uri(&p.root.join("c.gd"));

    // Locate the literal via documentColor (gives us its range).
    let colors = document_color(&client, 10, &uri);
    let info = &colors[0];
    let orig_range = info.range;

    // The user picks a new color with a non-1 alpha (forces the 4-arg form) that is NOT a named
    // constant — so only the float presentation should come back.
    let picked = Color {
        red: 0.5,
        green: 0.25,
        blue: 0.125,
        alpha: 0.5,
    };
    let pres = color_presentation(&client, 11, &uri, picked, orig_range);
    assert!(!pres.is_empty(), "at least the float presentation");
    // No named form for an arbitrary color.
    assert!(
        pres.iter().all(|p| !p.label.starts_with("Color.")),
        "no named-constant form for a non-matching color; got {:?}",
        pres.iter().map(|p| &p.label).collect::<Vec<_>>()
    );
    let float_pres = pres.iter().find(|p| p.label.starts_with("Color(")).unwrap();
    let edit = float_pres.text_edit.as_ref().expect("a textEdit");
    // The edit replaces the WHOLE original literal range.
    assert_eq!(
        edit.range, orig_range,
        "the textEdit must replace the whole literal"
    );

    // Round-trip: apply the edit to the buffer and re-run documentColor through the SAME handler;
    // the re-parsed color must equal what was picked, bitwise.
    let edited = "extends Node\n\nvar a := ".to_string() + &edit.new_text + "\n";
    let abs = p.root.join("c2.gd");
    p.write("c2.gd", &edited);
    let uri2 = file_uri(&abs);
    client
        .sender
        .send(notification(
            "textDocument/didOpen",
            DidOpenTextDocumentParams {
                text_document: TextDocumentItem {
                    uri: uri2.clone(),
                    language_id: "gdscript".to_string(),
                    version: 99,
                    text: edited.clone(),
                },
            },
        ))
        .unwrap();
    while common::try_recv(&client, std::time::Duration::from_millis(300)).is_some() {}
    let reparsed = document_color(&client, 12, &uri2);
    assert_eq!(reparsed.len(), 1, "the edited buffer has one literal");
    assert!(
        color_eq(reparsed[0].color, picked),
        "round-trip must be lossless: picked {picked:?}, re-parsed {:?}",
        reparsed[0].color
    );
    shutdown(&client, server_thread);
}

/// colorPresentation offers the `Color.NAME` form when the picked color exactly matches a DB
/// constant (and lists it before the float form so a picker prefers the readable name).
#[test]
fn color_presentation_offers_named_form_on_exact_match() {
    let p = base_project();
    let (server, client) = Connection::memory();
    let server_thread = std::thread::spawn(move || gd_server::serve(server));
    let src = "extends Node\n\nvar a := Color(1, 0, 0)\n";
    init_and_open(&p, &client, &[("c.gd", src)]);
    let uri = file_uri(&p.root.join("c.gd"));
    let range = document_color(&client, 10, &uri)[0].range;

    // Pick exactly RED.
    let red = Color {
        red: 1.0,
        green: 0.0,
        blue: 0.0,
        alpha: 1.0,
    };
    let pres = color_presentation(&client, 11, &uri, red, range);
    assert!(
        pres.iter().any(|p| p.label == "Color.RED"),
        "an exact match must offer the named form; got {:?}",
        pres.iter().map(|p| &p.label).collect::<Vec<_>>()
    );
    // Named form is listed first.
    assert_eq!(pres[0].label, "Color.RED", "named form is offered first");
    // The named edit also replaces the whole literal and round-trips.
    let named_edit = pres[0].text_edit.as_ref().unwrap();
    assert_eq!(named_edit.range, range);
    assert_eq!(named_edit.new_text, "Color.RED");

    // Every returned presentation re-parses to the picked color (the lossless invariant).
    for pr in &pres {
        let edited = "extends Node\n\nvar a := ".to_string()
            + &pr.text_edit.as_ref().unwrap().new_text
            + "\n";
        let rel = format!("rt_{}.gd", pr.label.replace(['(', ')', ',', ' ', '.'], "_"));
        p.write(&rel, &edited);
        let u = file_uri(&p.root.join(&rel));
        client
            .sender
            .send(notification(
                "textDocument/didOpen",
                DidOpenTextDocumentParams {
                    text_document: TextDocumentItem {
                        uri: u.clone(),
                        language_id: "gdscript".to_string(),
                        version: 50,
                        text: edited.clone(),
                    },
                },
            ))
            .unwrap();
        while common::try_recv(&client, std::time::Duration::from_millis(200)).is_some() {}
        let rp = document_color(&client, 20, &u);
        assert_eq!(
            rp.len(),
            1,
            "presentation `{}` re-parses to one literal",
            pr.label
        );
        assert!(
            color_eq(rp[0].color, red),
            "presentation `{}` must round-trip to RED; got {:?}",
            pr.label,
            rp[0].color
        );
    }
    shutdown(&client, server_thread);
}

/// documentColor is a `Some(vec)` even for a buffer with no color literals (never `null`/error), and
/// a syntactically broken buffer never panics — it returns whatever the tokenizer recovered.
#[test]
fn empty_and_malformed_buffers_never_panic() {
    let p = base_project();
    let (server, client) = Connection::memory();
    let server_thread = std::thread::spawn(move || gd_server::serve(server));
    let broken = "extends Node\n\nfunc busted(:\n\tvar a := Color(0.2, 0.4, 0.6)\n\tprint(\n";
    init_and_open(
        &p,
        &client,
        &[
            ("none.gd", "extends Node\nvar x := 1\n"),
            ("broken.gd", broken),
        ],
    );

    let none_uri = file_uri(&p.root.join("none.gd"));
    let none = document_color(&client, 10, &none_uri);
    assert!(
        none.is_empty(),
        "no literals → empty list (not null); got {none:?}"
    );

    // The broken buffer must still recover the well-formed Color literal and never panic.
    let broken_uri = file_uri(&p.root.join("broken.gd"));
    let recovered = document_color(&client, 11, &broken_uri);
    assert!(
        recovered
            .iter()
            .any(|c| approx(c.color.red, 0.2) && approx(c.color.green, 0.4)),
        "the well-formed Color literal in a broken buffer still yields a swatch; got {recovered:?}"
    );
    shutdown(&client, server_thread);
}
