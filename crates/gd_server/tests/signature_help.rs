//! M8 (#65) Phase 5: `textDocument/signatureHelp` over a real in-memory `Connection`. These are the
//! acceptance tests for the phase — they assert the wire contract a Godot-unaware client sees:
//!
//! - inside `foo(<cursor>)` the response is `foo`'s signature with `activeParameter` = the argument
//!   index under the cursor, and advancing past a comma advances it;
//! - nested `outer(inner(<cursor>))` resolves to the INNERMOST call;
//! - a `)` / `,` inside a string argument never mis-resolves the active call or parameter (#65);
//! - the overload sources resolve — a native method, a `@GlobalScope` utility, and a constructor
//!   (`ClassName.new(` → `_init`), plus a CROSS-FILE script method whose parameter names come from
//!   the declaring file's parse tree;
//! - label offsets are gated both ways (`labelOffsetSupport` → `[start,end)`, off → substring) and a
//!   per-signature `activeParameter` only appears behind `activeParameterSupport`;
//! - `null` when the cursor is in no call; a retrigger keeps `activeSignature` stable.

mod common;

use common::{file_uri, notification, recv_response, request, TempProject};
use lsp_server::Connection;
use lsp_types::{
    ClientCapabilities, DidOpenTextDocumentParams, InitializeParams, InitializedParams,
    ParameterInformation, ParameterLabel, Position, SignatureHelp, SignatureHelpClientCapabilities,
    SignatureInformationSettings, TextDocumentClientCapabilities, TextDocumentItem, Uri,
};

/// A signatureHelp-capable client capability bundle. `label_offsets` / `active_param` are the two
/// gates each test flips; an absent `signatureHelp` capability (both `None`) exercises the
/// all-default downgrade.
fn caps(label_offsets: bool, active_param: bool) -> ClientCapabilities {
    ClientCapabilities {
        text_document: Some(TextDocumentClientCapabilities {
            signature_help: Some(SignatureHelpClientCapabilities {
                signature_information: Some(SignatureInformationSettings {
                    documentation_format: None,
                    parameter_information: Some(lsp_types::ParameterInformationSettings {
                        label_offset_support: Some(label_offsets),
                    }),
                    active_parameter_support: Some(active_param),
                }),
                ..Default::default()
            }),
            ..Default::default()
        }),
        ..Default::default()
    }
}

/// A richer `extension_api.json`: a class with a method carrying TYPED params + a DEFAULT
/// (`Node.move(distance: float, relative: bool = true)`) and a VARARG utility (`print`), so the
/// MethodInfo-overload label format and the vararg slot are both exercised.
const SIG_API: &str = r#"{
    "header": {"version_major": 4, "version_minor": 6, "version_patch": 3},
    "utility_functions": [
        {"name": "print", "return_type": "void", "is_vararg": true, "arguments": []},
        {"name": "max", "return_type": "Variant", "is_vararg": false, "arguments": [
            {"name": "a", "type": "Variant"},
            {"name": "b", "type": "Variant"}
        ]}
    ],
    "classes": [
        {"name": "Object"},
        {"name": "Node", "inherits": "Object", "methods": [
            {"name": "move", "is_const": false, "is_static": false, "is_vararg": false,
             "is_virtual": false, "return_value": {"type": "void"}, "arguments": [
                {"name": "distance", "type": "float"},
                {"name": "relative", "type": "bool", "default_value": "true"}
            ]}
        ]},
        {"name": "CanvasItem", "inherits": "Node"},
        {"name": "Node2D", "inherits": "CanvasItem"}
    ],
    "builtin_classes": [
        {"name": "Vector2", "constructors": [
            {},
            {"arguments": [{"name": "from", "type": "Vector2"}]},
            {"arguments": [{"name": "from", "type": "Vector2i"}]},
            {"arguments": [{"name": "x", "type": "float"}, {"name": "y", "type": "float"}]}
        ]},
        {"name": "Color", "constructors": [
            {},
            {"arguments": [{"name": "from", "type": "Color"}]},
            {"arguments": [
                {"name": "r", "type": "float"},
                {"name": "g", "type": "float"},
                {"name": "b", "type": "float"}
            ]},
            {"arguments": [
                {"name": "r", "type": "float"},
                {"name": "g", "type": "float"},
                {"name": "b", "type": "float"},
                {"name": "a", "type": "float"}
            ]}
        ]}
    ]
}"#;

/// A throwaway project whose dump is [`SIG_API`], with a `class_name Hero` base that declares a
/// typed method + an `_init`, and a consumer file that extends a native class. Mirrors the
/// `common::sample_project` layout closely enough that the cross-file chain is exercised.
fn sig_project() -> TempProject {
    let p = TempProject::new();
    p.write(
        "project.godot",
        "config_version=5\n\n[application]\n\nconfig/name=\"T\"\n",
    );
    p.write("extension_api.json", SIG_API);
    // Hero: a project script class with a typed method (`greet`) carrying a default, and an `_init`
    // constructor. Both are resolved cross-file from a DIFFERENT consumer file, proving the
    // parameter names come from THIS declaring file's parse tree (not the index interface).
    p.write(
        "src/hero.gd",
        "class_name Hero\nextends Node2D\n\n\
         func _init(name: String, level: int = 1) -> void:\n\tpass\n\n\
         func greet(target: String, loud: bool = false) -> int:\n\treturn 0\n\n\
         ## Restore [param amount] hit points to the hero.\n\
         func heal(amount: int) -> void:\n\tpass\n",
    );
    p
}

/// Boot the server over an in-memory connection against `project`, handshaking with `client_caps`,
/// and open `(uri, text)`. Returns the connected client + the server thread join handle.
fn boot(
    project: &TempProject,
    client_caps: ClientCapabilities,
    uri: &Uri,
    text: &str,
) -> (Connection, std::thread::JoinHandle<anyhow::Result<()>>) {
    let options = serde_json::json!({
        "projectRoot": project.root.as_str(),
        "autoDumpExtensionApi": false,
        "extensionApiPath": project.root.join("extension_api.json").as_str(),
    });
    let (server, client) = Connection::memory();
    let server_thread = std::thread::spawn(move || gd_server::serve(server));

    let init = InitializeParams {
        capabilities: client_caps,
        initialization_options: Some(options),
        ..Default::default()
    };
    client.sender.send(request(1, "initialize", init)).unwrap();
    let resp = recv_response(&client);
    assert!(resp.error.is_none(), "initialize errored: {:?}", resp.error);

    client
        .sender
        .send(notification(
            "initialized",
            serde_json::to_value(InitializedParams {}).unwrap(),
        ))
        .unwrap();
    client
        .sender
        .send(notification(
            "textDocument/didOpen",
            DidOpenTextDocumentParams {
                text_document: TextDocumentItem {
                    uri: uri.clone(),
                    language_id: "gdscript".to_string(),
                    version: 1,
                    text: text.to_string(),
                },
            },
        ))
        .unwrap();

    (client, server_thread)
}

/// Send a `textDocument/signatureHelp` at `pos` in `uri` and return the RAW JSON result (so a test
/// can assert `null` directly, before any typed deserialization).
fn sig_raw(client: &Connection, id: i32, uri: &Uri, pos: Position) -> serde_json::Value {
    client
        .sender
        .send(request(
            id,
            "textDocument/signatureHelp",
            serde_json::json!({
                "textDocument": { "uri": uri.as_str() },
                "position": { "line": pos.line, "character": pos.character },
            }),
        ))
        .unwrap();
    let resp = recv_response(client);
    assert!(
        resp.error.is_none(),
        "signatureHelp errored: {:?}",
        resp.error
    );
    resp.result.expect("signatureHelp result")
}

/// Send a `textDocument/signatureHelp` at `pos` and deserialize the (non-null) [`SignatureHelp`].
fn sig(client: &Connection, id: i32, uri: &Uri, pos: Position) -> SignatureHelp {
    let raw = sig_raw(client, id, uri, pos);
    assert!(!raw.is_null(), "expected a SignatureHelp, got null");
    serde_json::from_value(raw).expect("SignatureHelp deserializes")
}

fn shutdown(client: &Connection, server_thread: std::thread::JoinHandle<anyhow::Result<()>>) {
    common::shutdown(client, server_thread);
}

/// The single active signature's label (asserting exactly one signature came back).
fn only_label(h: &SignatureHelp) -> &str {
    assert_eq!(h.signatures.len(), 1, "expected exactly one signature");
    &h.signatures[0].label
}

// ===================================================================================================
// Capability advertisement.
// ===================================================================================================

/// The server advertises `signatureHelpProvider` with `(`/`,` triggers and `)` retrigger.
#[test]
fn advertises_signature_help_provider_with_triggers() {
    let p = sig_project();
    let (server, client) = Connection::memory();
    let server_thread = std::thread::spawn(move || gd_server::serve(server));
    let init = InitializeParams {
        capabilities: caps(true, true),
        initialization_options: Some(serde_json::json!({ "projectRoot": p.root.as_str() })),
        ..Default::default()
    };
    client.sender.send(request(1, "initialize", init)).unwrap();
    let resp = recv_response(&client);
    let result: lsp_types::InitializeResult =
        serde_json::from_value(resp.result.expect("initialize result")).unwrap();
    let sp = result
        .capabilities
        .signature_help_provider
        .expect("signatureHelpProvider advertised");
    assert_eq!(
        sp.trigger_characters,
        Some(vec!["(".to_string(), ",".to_string()]),
        "trigger characters"
    );
    assert_eq!(
        sp.retrigger_characters,
        Some(vec![")".to_string()]),
        "retrigger characters"
    );

    client
        .sender
        .send(notification(
            "initialized",
            serde_json::to_value(InitializedParams {}).unwrap(),
        ))
        .unwrap();
    shutdown(&client, server_thread);
}

// ===================================================================================================
// activeParameter — the headline behavior.
// ===================================================================================================

/// Inside `print(<cursor>)` the signature is `print`'s, and `activeParameter` is 0. Advancing past a
/// comma to the second argument advances `activeParameter` to 1. (A vararg utility, so the index
/// keeps advancing without vanishing.)
#[test]
fn active_parameter_tracks_argument_index() {
    let p = sig_project();
    // A FIXED-arity callee (`max(a, b)`) so the clamped top-level `activeParameter` is a meaningful
    // 0→1 advancement (a vararg's single slot would clamp every index to 0 — covered separately).
    let src = "extends Node\n\nfunc f() -> void:\n\tmax(1, 2)\n";
    let uri = file_uri(&p.root.join("src/f.gd"));
    let (client, server_thread) = boot(&p, caps(true, true), &uri, src);

    // Cursor right after `max(` — line 3 (0-based), `\tmax(` = tab + 4 chars → column 5.
    let h0 = sig(&client, 10, &uri, Position::new(3, 5));
    assert!(
        only_label(&h0).starts_with("Variant max("),
        "label = {:?}",
        only_label(&h0)
    );
    assert_eq!(h0.active_parameter, Some(0), "first argument is index 0");

    // Cursor after the comma: `\tmax(1, ` → column 8 (past `1,` and the space).
    let h1 = sig(&client, 11, &uri, Position::new(3, 8));
    assert_eq!(
        h1.active_parameter,
        Some(1),
        "second argument is index 1 (advanced past the comma)"
    );

    shutdown(&client, server_thread);
}

/// A VARARG callee (`print(...)`) keeps the active parameter clamped to its single (vararg) slot for
/// every argument past the first: the cursor in argument 2 or 3 still reports `activeParameter` 0
/// (the `...args: Array` slot), rather than running off the end. Documents the vararg clamp on
/// purpose (Godot keeps the sentinel on the vararg slot for any over-supplied argument).
#[test]
fn vararg_active_parameter_clamps_to_vararg_slot() {
    let p = sig_project();
    let src = "extends Node\n\nfunc f() -> void:\n\tprint(1, 2, 3)\n";
    let uri = file_uri(&p.root.join("src/f.gd"));
    let (client, server_thread) = boot(&p, caps(true, true), &uri, src);

    // `print` has zero declared params + a vararg, so its only parameter slot is `...args: Array`
    // at index 0. The cursor in the THIRD argument (`\tprint(1, 2, ` → column 13) clamps to 0.
    let h = sig(&client, 10, &uri, Position::new(3, 13));
    assert!(
        only_label(&h).starts_with("void print("),
        "label = {:?}",
        only_label(&h)
    );
    assert_eq!(
        h.active_parameter,
        Some(0),
        "a vararg call clamps the active parameter to the vararg slot (index 0)"
    );

    shutdown(&client, server_thread);
}

// ===================================================================================================
// Nested calls — innermost wins.
// ===================================================================================================

/// `print(max(<cursor>))` resolves to the INNERMOST call (`max`), not the outer `print`.
#[test]
fn nested_call_resolves_innermost() {
    let p = sig_project();
    let src = "extends Node\n\nfunc f() -> void:\n\tprint(max(1, 2))\n";
    let uri = file_uri(&p.root.join("src/f.gd"));
    let (client, server_thread) = boot(&p, caps(true, true), &uri, src);

    // Cursor inside `max(` — `\tprint(max(` = tab + `print(` (6) + `max(` (4) → column 11.
    let h = sig(&client, 10, &uri, Position::new(3, 11));
    assert!(
        only_label(&h).contains("max("),
        "innermost call must be max, got {:?}",
        only_label(&h)
    );
    assert!(
        !only_label(&h).contains("print("),
        "must not resolve the outer print, got {:?}",
        only_label(&h)
    );

    shutdown(&client, server_thread);
}

// ===================================================================================================
// String robustness (#65) — a ) or , inside a string argument never mis-resolves.
// ===================================================================================================

/// A `)` and a `,` inside a STRING argument of `max("a, b)", 7)` do not close the call or advance the
/// parameter: the cursor after that string is still argument 1 of `max`, and the active call is
/// still `max` (the string is one token, so its brackets/commas are invisible to the scan). A
/// fixed-arity callee (`max`) is used so the clamped `activeParameter` is a meaningful `1`.
#[test]
fn string_literal_robustness() {
    let p = sig_project();
    // `max("a, b)", X)` — the cursor sits in the SECOND argument (after the string + comma). The
    // `,` and `)` inside the string must not have advanced/closed anything.
    let src = "extends Node\n\nfunc f() -> void:\n\tmax(\"a, b)\", 7)\n";
    let uri = file_uri(&p.root.join("src/f.gd"));
    let (client, server_thread) = boot(&p, caps(true, true), &uri, src);

    // Column: tab(1) + `max(`(4) + `"a, b)"`(7) + `,`(1) + ` `(1) = 14 → inside the 2nd arg.
    let h = sig(&client, 10, &uri, Position::new(3, 14));
    assert!(
        only_label(&h).starts_with("Variant max("),
        "active call must still be max, got {:?}",
        only_label(&h)
    );
    assert_eq!(
        h.active_parameter,
        Some(1),
        "the in-string ) and , must not have advanced/closed the call; expected arg index 1"
    );

    shutdown(&client, server_thread);
}

// ===================================================================================================
// Overload sources — native method, utility, constructor, cross-file script method.
// ===================================================================================================

/// A NATIVE method call `node.move(<cursor>)` resolves to `Node.move`'s signature, with the typed
/// parameters AND the default rendered: `void move(distance: float, relative: bool = true)`.
#[test]
fn native_method_signature() {
    let p = sig_project();
    let src = "extends Node\n\nfunc f(node: Node) -> void:\n\tnode.move(1.0)\n";
    let uri = file_uri(&p.root.join("src/f.gd"));
    let (client, server_thread) = boot(&p, caps(true, true), &uri, src);

    // Cursor inside `node.move(` — tab(1) + `node.move(`(10) = 11.
    let h = sig(&client, 10, &uri, Position::new(3, 11));
    assert_eq!(
        only_label(&h),
        "void move(distance: float, relative: bool = true)",
        "native method label with typed params + default"
    );

    shutdown(&client, server_thread);
}

/// A `@GlobalScope` UTILITY call (`max(<cursor>)`) resolves to the utility's signature.
#[test]
fn utility_signature() {
    let p = sig_project();
    let src = "extends Node\n\nfunc f() -> void:\n\tmax(1, 2)\n";
    let uri = file_uri(&p.root.join("src/f.gd"));
    let (client, server_thread) = boot(&p, caps(true, true), &uri, src);

    // Cursor inside `max(` — tab(1) + `max(`(4) = 5.
    let h = sig(&client, 10, &uri, Position::new(3, 5));
    assert_eq!(
        only_label(&h),
        "Variant max(a: Variant, b: Variant)",
        "utility label"
    );

    shutdown(&client, server_thread);
}

/// A CONSTRUCTOR call `Hero.new(<cursor>)` resolves to the class `_init`, whose parameter names come
/// from the declaring file's parse tree: `void _init(name: String, level: int = 1)`.
#[test]
fn constructor_new_resolves_to_init() {
    let p = sig_project();
    let src = "extends Node\n\nfunc f() -> void:\n\tHero.new(\"x\")\n";
    let uri = file_uri(&p.root.join("src/f.gd"));
    let (client, server_thread) = boot(&p, caps(true, true), &uri, src);

    // Cursor inside `Hero.new(` — tab(1) + `Hero.new(`(9) = 10.
    let h = sig(&client, 10, &uri, Position::new(3, 10));
    assert_eq!(
        only_label(&h),
        "void _init(name: String, level: int = 1)",
        "ClassName.new resolves to _init with declaring-file param names + default"
    );

    shutdown(&client, server_thread);
}

/// #257: a builtin-type CONSTRUCTOR call `Vector2(<cursor>)` answers with one signature per
/// overload, in dump order, labelled `Type Type(args)` — the `_make_arguments_hint` shape
/// `Variant::get_constructor_list` implies (it sets `mi.name = mi.return_val.type = type`).
///
/// This deliberately DIVERGES from Godot's own language server, which returns null here and puts
/// its constructor arghints on the completion surface instead (that surface, #194, is untouched).
/// Per #30 a generic client renders parameter hints from `signatureHelp` and nowhere else, and
/// these are among the most-typed calls in GDScript.
///
/// At argument index 0 the no-arg overload is already gone — Godot's own filter
/// (`gdscript_editor.cpp:3417`, `if (p_argidx >= E.arguments.size()) continue;`) drops any
/// overload the cursor's argument index overruns.
#[test]
fn builtin_constructor_offers_one_signature_per_overload() {
    let p = sig_project();
    let src = "extends Node\n\nfunc f() -> void:\n\tvar v = Vector2(\n";
    let uri = file_uri(&p.root.join("src/ctor.gd"));
    let (client, server_thread) = boot(&p, caps(true, true), &uri, src);

    // Cursor right after `Vector2(` — tab(1) + `var v = Vector2(`(16) = column 17.
    let h = sig(&client, 10, &uri, Position::new(3, 17));
    let labels: Vec<&str> = h.signatures.iter().map(|s| s.label.as_str()).collect();
    assert_eq!(
        labels,
        vec![
            "Vector2 Vector2(from: Vector2)",
            "Vector2 Vector2(from: Vector2i)",
            "Vector2 Vector2(x: float, y: float)",
        ],
        "one signature per surviving overload, in dump order; the no-arg overload is filtered out \
         at argument index 0"
    );
    assert_eq!(h.active_signature, Some(0));
    assert_eq!(h.active_parameter, Some(0));

    shutdown(&client, server_thread);
}

/// The overload filter tracks the cursor: past the first comma only overloads with a second
/// parameter survive, and `activeParameter` follows.
#[test]
fn builtin_constructor_overloads_narrow_as_arguments_are_typed() {
    let p = sig_project();
    let src = "extends Node\n\nfunc f() -> void:\n\tvar c = Color(0.5, 0.5, \n";
    let uri = file_uri(&p.root.join("src/ctor2.gd"));
    let (client, server_thread) = boot(&p, caps(true, true), &uri, src);

    // Cursor after the SECOND comma — argument index 2, so only the 3- and 4-arg overloads
    // survive. tab(1) + `var c = Color(0.5, 0.5, `(24) = column 25.
    let h = sig(&client, 10, &uri, Position::new(3, 25));
    let labels: Vec<&str> = h.signatures.iter().map(|s| s.label.as_str()).collect();
    assert_eq!(
        labels,
        vec![
            "Color Color(r: float, g: float, b: float)",
            "Color Color(r: float, g: float, b: float, a: float)",
        ],
        "the 0-arg and copy overloads can no longer be what is being typed"
    );
    assert_eq!(h.active_parameter, Some(2), "highlighting the third slot");

    shutdown(&client, server_thread);
}

/// Typing past EVERY overload's arity is an error state mid-edit. Godot's completion filter shows
/// nothing there; signature help keeps the popup alive instead, offering every overload widest
/// first so `activeSignature` points at the closest match — a hint that degrades rather than
/// vanishing under the user's cursor.
#[test]
fn builtin_constructor_past_every_arity_still_offers_the_widest() {
    let p = sig_project();
    let src = "extends Node\n\nfunc f() -> void:\n\tvar v = Vector2(1, 2, 3, \n";
    let uri = file_uri(&p.root.join("src/ctor3.gd"));
    let (client, server_thread) = boot(&p, caps(true, true), &uri, src);

    // Cursor after the third comma — argument index 3, past every Vector2 overload.
    // tab(1) + `var v = Vector2(1, 2, 3, `(25) = column 26.
    let h = sig(&client, 10, &uri, Position::new(3, 26));
    assert_eq!(
        h.signatures.first().map(|s| s.label.as_str()),
        Some("Vector2 Vector2(x: float, y: float)"),
        "widest overload first so activeSignature=0 is the closest match"
    );
    assert_eq!(
        h.signatures.len(),
        4,
        "every overload is offered as a floor"
    );

    shutdown(&client, server_thread);
}

/// A bare name that is NOT a builtin type still falls through to the self-method arm — the
/// constructor arm must not swallow ordinary calls.
#[test]
fn a_non_builtin_bare_name_still_resolves_as_a_self_method() {
    let p = sig_project();
    let src =
        "extends Node\n\nfunc helper(a: int) -> void:\n\tpass\n\nfunc f() -> void:\n\thelper(\n";
    let uri = file_uri(&p.root.join("src/ctor4.gd"));
    let (client, server_thread) = boot(&p, caps(true, true), &uri, src);

    let h = sig(&client, 10, &uri, Position::new(6, 8));
    assert_eq!(only_label(&h), "void helper(a: int)");

    shutdown(&client, server_thread);
}

/// THE cross-file proof: a script method called on a typed cross-file value
/// (`h.greet(<cursor>)` where `h: Hero`) resolves to `Hero.greet`'s signature with parameter NAMES
/// and the DEFAULT taken from `hero.gd`'s parse tree — `int greet(target: String, loud: bool =
/// false)` — even though the call is in a DIFFERENT file. This is what proves the names don't come
/// from the (name+type only, default-less) index interface.
#[test]
fn cross_file_script_method_param_names_from_declaring_tree() {
    let p = sig_project();
    let src = "extends Node\n\nfunc f(h: Hero) -> void:\n\th.greet(\"hi\")\n";
    let uri = file_uri(&p.root.join("src/consumer.gd"));
    let (client, server_thread) = boot(&p, caps(true, true), &uri, src);

    // Cursor inside `h.greet(` — tab(1) + `h.greet(`(8) = 9.
    let h = sig(&client, 10, &uri, Position::new(3, 9));
    assert_eq!(
        only_label(&h),
        "int greet(target: String, loud: bool = false)",
        "cross-file script method: names + default from the DECLARING file's tree"
    );

    shutdown(&client, server_thread);
}

/// A script method's `##` doc comment rides on its signature's `documentation` (#97). `Hero.heal`
/// carries a doc; its signatureHelp popup must surface the prose (rendered to the client's flavor),
/// while the `(params)` label is unchanged. Discriminates the new doc threading from the old
/// hardcoded `None`.
#[test]
fn cross_file_script_method_carries_doc_comment() {
    let p = sig_project();
    let src = "extends Node\n\nfunc f(h: Hero) -> void:\n\th.heal(1)\n";
    let uri = file_uri(&p.root.join("src/doc_consumer.gd"));
    let (client, server_thread) = boot(&p, caps(true, true), &uri, src);

    // Cursor inside `h.heal(` — tab(1) + `h.heal(`(7) = 8.
    let h = sig(&client, 10, &uri, Position::new(3, 8));
    let info = &h.signatures[0];
    assert_eq!(
        info.label, "void heal(amount: int)",
        "the label is unchanged by the doc threading"
    );
    let doc = info
        .documentation
        .as_ref()
        .expect("a script method with a `##` doc carries documentation");
    let text = match doc {
        lsp_types::Documentation::String(s) => s.clone(),
        lsp_types::Documentation::MarkupContent(mc) => mc.value.clone(),
    };
    assert!(
        text.contains("Restore") && text.contains("hit points"),
        "the popup surfaces the `##` prose; got {text:?}"
    );

    shutdown(&client, server_thread);
}

/// An INNER-class method's `##` doc rides on its signature too — the doc comes from the same
/// inner-walked `MemberDecl` whose `name_span` drives the (inner, not root) signature, so an inner
/// method that name-collides with a root method shows the inner doc, not the root's.
#[test]
fn inner_class_method_carries_its_own_doc() {
    let p = sig_project();
    let uri = file_uri(&p.root.join("src/inner_doc.gd"));
    let src = "class_name InnerDocHolder\nextends Node2D\n\n\
               ## The ROOT poke.\n\
               func poke(a: int) -> void:\n\tpass\n\n\
               class Inner:\n\t## The INNER poke.\n\tfunc poke(a: int) -> void:\n\t\tpass\n\n\
               func use_inner() -> void:\n\tvar x := Inner.new()\n\tx.poke(1)\n";
    let (client, server_thread) = boot(&p, caps(true, true), &uri, src);

    // `\tx.poke(1)` is line 14 (0-based); cursor in arg 0 = tab(1) + "x.poke(".len()(7) = 8.
    let h = sig(&client, 31, &uri, Position::new(14, 8));
    let doc = h.signatures[0]
        .documentation
        .as_ref()
        .expect("the inner method carries documentation");
    let text = match doc {
        lsp_types::Documentation::String(s) => s.clone(),
        lsp_types::Documentation::MarkupContent(mc) => mc.value.clone(),
    };
    assert!(
        text.contains("INNER") && !text.contains("ROOT"),
        "the inner method's own doc is shown, not the root's; got {text:?}"
    );

    shutdown(&client, server_thread);
}

/// A script method with NO doc comment carries no `documentation` (the empty-description filter): an
/// honest absence, never an empty popup. `Hero.greet` has no `##`.
#[test]
fn cross_file_script_method_without_doc_has_no_documentation() {
    let p = sig_project();
    let src = "extends Node\n\nfunc f(h: Hero) -> void:\n\th.greet(\"hi\")\n";
    let uri = file_uri(&p.root.join("src/nodoc_consumer.gd"));
    let (client, server_thread) = boot(&p, caps(true, true), &uri, src);

    let h = sig(&client, 10, &uri, Position::new(3, 9));
    assert!(
        h.signatures[0].documentation.is_none(),
        "an undocumented script method must carry no documentation popup"
    );

    shutdown(&client, server_thread);
}

// ===================================================================================================
// Label-offset gating — both ways.
// ===================================================================================================

/// With `labelOffsetSupport: true`, each parameter carries `[start, end)` offsets into the label
/// (NOT a substring), and the offsets actually index the parameter's slice of the label.
#[test]
fn label_offsets_when_supported() {
    let p = sig_project();
    let src = "extends Node\n\nfunc f(node: Node) -> void:\n\tnode.move(1.0)\n";
    let uri = file_uri(&p.root.join("src/f.gd"));
    let (client, server_thread) = boot(&p, caps(true, true), &uri, src);

    let h = sig(&client, 10, &uri, Position::new(3, 11));
    let info = &h.signatures[0];
    let label = &info.label;
    let params = info.parameters.as_ref().expect("parameters present");
    assert_eq!(params.len(), 2, "two parameters");
    // Each label is an OFFSET pair; the slice it points to is the parameter fragment.
    let slice = |pi: &ParameterInformation| -> String {
        match &pi.label {
            ParameterLabel::LabelOffsets([s, e]) => label[(*s as usize)..(*e as usize)].to_string(),
            ParameterLabel::Simple(s) => panic!("expected offsets, got substring {s:?}"),
        }
    };
    assert_eq!(slice(&params[0]), "distance: float");
    assert_eq!(slice(&params[1]), "relative: bool = true");

    shutdown(&client, server_thread);
}

/// With `labelOffsetSupport: false`, each parameter carries a SUBSTRING label (which must be a
/// literal substring of the signature label), never offsets.
#[test]
fn substring_labels_when_offsets_unsupported() {
    let p = sig_project();
    let src = "extends Node\n\nfunc f(node: Node) -> void:\n\tnode.move(1.0)\n";
    let uri = file_uri(&p.root.join("src/f.gd"));
    // label_offsets = false.
    let (client, server_thread) = boot(&p, caps(false, true), &uri, src);

    let h = sig(&client, 10, &uri, Position::new(3, 11));
    let info = &h.signatures[0];
    let params = info.parameters.as_ref().expect("parameters present");
    let as_str = |pi: &ParameterInformation| -> String {
        match &pi.label {
            ParameterLabel::Simple(s) => s.clone(),
            ParameterLabel::LabelOffsets(o) => panic!("expected substring, got offsets {o:?}"),
        }
    };
    assert_eq!(as_str(&params[0]), "distance: float");
    assert_eq!(as_str(&params[1]), "relative: bool = true");
    // And each substring really is a substring of the label.
    assert!(info.label.contains(&as_str(&params[0])));
    assert!(info.label.contains(&as_str(&params[1])));

    shutdown(&client, server_thread);
}

/// A per-signature `activeParameter` is present only behind `activeParameterSupport`. With it ON, a
/// signature carries its own `activeParameter`; with it OFF, the per-signature field is absent
/// (the top-level `SignatureHelp.activeParameter` carries it instead — always present).
#[test]
fn per_signature_active_parameter_gated() {
    let p = sig_project();
    let src = "extends Node\n\nfunc f() -> void:\n\tmax(1, 2)\n";
    let uri = file_uri(&p.root.join("src/f.gd"));

    // active_param ON → per-signature activeParameter set.
    {
        let (client, st) = boot(&p, caps(true, true), &uri, src);
        let h = sig(&client, 10, &uri, Position::new(3, 5));
        assert_eq!(
            h.signatures[0].active_parameter,
            Some(0),
            "per-signature activeParameter present when supported"
        );
        assert_eq!(h.active_parameter, Some(0), "top-level activeParameter too");
        shutdown(&client, st);
    }
    // active_param OFF → per-signature activeParameter absent, top-level still present.
    {
        let (client, st) = boot(&p, caps(true, false), &uri, src);
        let h = sig(&client, 20, &uri, Position::new(3, 5));
        assert_eq!(
            h.signatures[0].active_parameter, None,
            "per-signature activeParameter absent when unsupported"
        );
        assert_eq!(
            h.active_parameter,
            Some(0),
            "top-level activeParameter is always present"
        );
        shutdown(&client, st);
    }
}

// ===================================================================================================
// null when not in a call + retrigger.
// ===================================================================================================

/// Outside any call, signatureHelp returns `null` (the cursor is in no argument list).
#[test]
fn null_when_not_in_a_call() {
    let p = sig_project();
    let src = "extends Node\n\nfunc f() -> void:\n\tvar x = 1\n";
    let uri = file_uri(&p.root.join("src/f.gd"));
    let (client, server_thread) = boot(&p, caps(true, true), &uri, src);

    // Cursor at end of `\tvar x = 1` — no enclosing call.
    let raw = sig_raw(&client, 10, &uri, Position::new(3, 9));
    assert!(raw.is_null(), "expected null outside a call, got {raw:?}");

    shutdown(&client, server_thread);
}

/// A retrigger that echoes the prior `SignatureHelp` (with the user having navigated to a given
/// `activeSignature`) keeps that `activeSignature` when it is still in range. With a single
/// signature, `activeSignature` is 0 both before and after — asserted via the request `context`.
#[test]
fn retrigger_keeps_active_signature_stable() {
    let p = sig_project();
    let src = "extends Node\n\nfunc f() -> void:\n\tmax(1, 2)\n";
    let uri = file_uri(&p.root.join("src/f.gd"));
    let (client, server_thread) = boot(&p, caps(true, true), &uri, src);

    // First request (INVOKED): the initial hint.
    let first = sig(&client, 10, &uri, Position::new(3, 5));
    assert_eq!(first.active_signature, Some(0));

    // Retrigger on `)` with the prior help echoed in the context — activeSignature stays 0.
    client
        .sender
        .send(request(
            11,
            "textDocument/signatureHelp",
            serde_json::json!({
                "textDocument": { "uri": uri.as_str() },
                "position": { "line": 3, "character": 8 },
                "context": {
                    "triggerKind": 2,
                    "triggerCharacter": ",",
                    "isRetrigger": true,
                    "activeSignatureHelp": first,
                }
            }),
        ))
        .unwrap();
    let resp = recv_response(&client);
    assert!(resp.error.is_none(), "retrigger errored: {:?}", resp.error);
    let raw = resp.result.expect("retrigger result");
    assert!(!raw.is_null(), "retrigger should still be in the call");
    let second: SignatureHelp = serde_json::from_value(raw).unwrap();
    assert_eq!(
        second.active_signature,
        Some(0),
        "activeSignature stays stable across the retrigger"
    );

    shutdown(&client, server_thread);
}

/// Smoke check the all-default downgrade (no `signatureHelp` capability at all): the response is
/// still a well-formed `SignatureHelp` with substring parameter labels (offsets default off) and no
/// per-signature activeParameter (default off).
#[test]
fn minimal_client_gets_well_formed_downgrade() {
    let p = sig_project();
    let src = "extends Node\n\nfunc f() -> void:\n\tmax(1, 2)\n";
    let uri = file_uri(&p.root.join("src/f.gd"));
    // No text_document.signatureHelp capability at all.
    let (client, server_thread) = boot(&p, ClientCapabilities::default(), &uri, src);

    let h = sig(&client, 10, &uri, Position::new(3, 5));
    let info = &h.signatures[0];
    assert_eq!(
        info.active_parameter, None,
        "no per-signature AP by default"
    );
    let params = info.parameters.as_ref().expect("parameters");
    assert!(
        matches!(params[0].label, ParameterLabel::Simple(_)),
        "substring labels by default"
    );

    shutdown(&client, server_thread);
}

// ===================================================================================================
// #113 — inner-class method signature (class_path walk, not root-only lookup).
// ===================================================================================================

/// #113: a call to an INNER-class method that name-collides with a root-class method must render the
/// INNER signature, not the root one. `script_method_sig` previously did a root-only member lookup;
/// it now resolves the callee through the analyzer's `CalleeTarget` (the call binding's inner-class
/// `class_path`, as inlayHint does) and walks that chain — the base value's `ScriptRef` does not
/// carry it. Reproduce-first: before the fix, `x.process(1, 2)` (x an `Inner` instance) showed the
/// root `process(a)`.
#[test]
fn inner_class_method_signature_uses_inner_not_root() {
    let p = sig_project();
    let uri = file_uri(&p.root.join("src/holder.gd"));
    let src = "class_name Holder\nextends Node2D\n\n\
               func process(a: int) -> void:\n\tpass\n\n\
               class Inner:\n\tfunc process(a: int, extra: int) -> void:\n\t\tpass\n\n\
               func use_inner() -> void:\n\tvar x := Inner.new()\n\tx.process(1, 2)\n";
    let (client, server_thread) = boot(&p, caps(true, true), &uri, src);

    // `\tx.process(1, 2)` is line 12 (0-based); cursor in arg 0 = tab(1) + "x.process(".len()(10) = 11.
    let h = sig(&client, 30, &uri, Position::new(12, 11));
    let info = &h.signatures[0];
    let params = info.parameters.as_ref().expect("inner method parameters");
    assert_eq!(
        params.len(),
        2,
        "inner Inner.process(a, extra) has 2 params; got label {:?}",
        info.label
    );
    assert!(
        info.label.contains("extra"),
        "the inner signature (param `extra`) must be shown, not the root process(a); got {:?}",
        info.label
    );

    shutdown(&client, server_thread);
}

/// Regression guard: a ROOT-class method call still resolves to the root signature (empty
/// `class_path` → root member lookup, unchanged) — the inner walk must not pull the inner method.
#[test]
fn root_class_method_signature_unaffected_by_inner_walk() {
    let p = sig_project();
    let uri = file_uri(&p.root.join("src/holder2.gd"));
    let src = "class_name Holder2\nextends Node2D\n\n\
               func process(a: int) -> void:\n\tpass\n\n\
               class Inner:\n\tfunc process(a: int, extra: int) -> void:\n\t\tpass\n\n\
               func use_root() -> void:\n\tvar r: Holder2 = Holder2.new()\n\tr.process(\n";
    let (client, server_thread) = boot(&p, caps(true, true), &uri, src);

    // `\tr.process(` is line 12 (0-based); column = tab(1) + "r.process(".len()(10) = 11.
    let h = sig(&client, 31, &uri, Position::new(12, 11));
    let info = &h.signatures[0];
    let params = info.parameters.as_ref().expect("root method parameters");
    assert_eq!(
        params.len(),
        1,
        "root process(a) has 1 param; got label {:?}",
        info.label
    );
    assert!(
        !info.label.contains("extra"),
        "the root signature must not pull the inner method's `extra` param; got {:?}",
        info.label
    );

    shutdown(&client, server_thread);
}

// ===================================================================================================
// #193 — lambda `.call` signatures.
// ===================================================================================================

/// #193: `f.call(<cursor>)` where `f` holds a lambda shows the LAMBDA's parameters — names, types
/// and defaults from the lambda literal — with the cursor's argument index mapped onto them. Without
/// this the only answer is the native `Callable.call(...)` vararg shape, which says nothing about
/// what the call takes.
#[test]
fn lambda_call_shows_the_lambdas_parameters() {
    let p = sig_project();
    let uri = file_uri(&p.root.join("src/lambda_call.gd"));
    let src = "extends Node\n\n\
               func run() -> void:\n\
               \tvar f := func(a: int, b: String = \"hi\") -> void:\n\t\tpass\n\
               \tf.call(1, \"x\")\n";
    let (client, server_thread) = boot(&p, caps(true, true), &uri, src);

    // `\tf.call(1, "x")` is line 5 (0-based); arg 0 = tab(1) + "f.call(".len()(7) = 8.
    let h = sig(&client, 40, &uri, Position::new(5, 8));
    assert_eq!(
        only_label(&h),
        "void call(a: int, b: String = \"hi\")",
        "the lambda's own parameter list, under the `call` callee name"
    );
    assert_eq!(h.signatures[0].active_parameter, Some(0));

    // Advance past the comma into arg 1: column 11 is just after `1, `.
    let h2 = sig(&client, 41, &uri, Position::new(5, 11));
    assert_eq!(h2.signatures[0].active_parameter, Some(1));

    shutdown(&client, server_thread);
}

/// An UNANNOTATED lambda return renders as `Variant`, not `void`: `Callable.call` yields whatever
/// the lambda returns, so `void` would misreport the call's value. `call_deferred` resolves the same
/// way (it forwards the same argument list).
#[test]
fn lambda_call_deferred_renders_unannotated_return_as_variant() {
    let p = sig_project();
    let uri = file_uri(&p.root.join("src/lambda_deferred.gd"));
    let src = "extends Node\n\n\
               func run() -> void:\n\
               \tvar doubler := func(x):\n\t\treturn x * 2\n\
               \tdoubler.call_deferred(2)\n";
    let (client, server_thread) = boot(&p, caps(true, true), &uri, src);

    // `\tdoubler.call_deferred(2)` is line 5; arg 0 = tab(1) + "doubler.call_deferred(".len()(22).
    let h = sig(&client, 42, &uri, Position::new(5, 23));
    assert_eq!(only_label(&h), "Variant call_deferred(x: Variant)");

    shutdown(&client, server_thread);
}

/// A CLASS-LEVEL `var` holding a lambda resolves from inside any method (the visibility rule only
/// constrains a `var` declared inside a function).
#[test]
fn class_level_lambda_var_resolves_from_a_method() {
    let p = sig_project();
    let uri = file_uri(&p.root.join("src/lambda_member.gd"));
    let src = "extends Node\n\n\
               var handler := func(damage: int) -> bool:\n\treturn damage > 0\n\n\
               func run() -> void:\n\thandler.call(3)\n";
    let (client, server_thread) = boot(&p, caps(true, true), &uri, src);

    // `\thandler.call(3)` is line 6; arg 0 = tab(1) + "handler.call(".len()(13) = 14.
    let h = sig(&client, 43, &uri, Position::new(6, 14));
    assert_eq!(only_label(&h), "bool call(damage: int)");

    shutdown(&client, server_thread);
}

/// Fail-closed: a name REBOUND by a later assignment can hold a different callable than the lambda
/// it was declared with, and gdls does not track which assignment reaches the cursor — so no lambda
/// signature is offered. (This project's dump carries no `Callable` builtin, so the honest fallback
/// is `null` rather than a wrong parameter list.)
#[test]
fn reassigned_lambda_var_offers_no_lambda_signature() {
    let p = sig_project();
    let uri = file_uri(&p.root.join("src/lambda_reassigned.gd"));
    let src = "extends Node\n\n\
               func run() -> void:\n\
               \tvar f := func(a: int) -> void:\n\t\tpass\n\
               \tf = func(other: String, extra: int) -> void:\n\t\tpass\n\
               \tf.call(1)\n";
    let (client, server_thread) = boot(&p, caps(true, true), &uri, src);

    // `\tf.call(1)` is line 7; arg 0 = tab(1) + "f.call(".len()(7) = 8.
    let raw = sig_raw(&client, 44, &uri, Position::new(7, 8));
    assert!(
        raw.is_null(),
        "a rebound lambda name must not claim a lambda signature; got {raw}"
    );

    shutdown(&client, server_thread);
}

/// Two functions each declaring their own `var f := func …` each get THEIR OWN lambda: the base is
/// resolved through the tree's scope-correct binding resolver (the primitive the rename firewall
/// trusts), not a file-wide name match — so a same-named lambda in a sibling scope can never supply
/// the parameter list.
#[test]
fn same_named_lambda_vars_in_two_functions_resolve_per_scope() {
    let p = sig_project();
    let uri = file_uri(&p.root.join("src/lambda_scoped.gd"));
    let src = "extends Node\n\n\
               func one() -> void:\n\
               \tvar f := func(a: int) -> void:\n\t\tpass\n\
               \tf.call(1)\n\n\
               func two() -> void:\n\
               \tvar f := func(other: String) -> void:\n\t\tpass\n\
               \tf.call(\"x\")\n";
    let (client, server_thread) = boot(&p, caps(true, true), &uri, src);

    // `\tf.call(1)` is line 5; arg 0 = tab(1) + "f.call(".len()(7) = 8.
    let h1 = sig(&client, 45, &uri, Position::new(5, 8));
    assert_eq!(only_label(&h1), "void call(a: int)");

    // `\tf.call("x")` is line 10 — the OTHER function's `f`.
    let h2 = sig(&client, 46, &uri, Position::new(10, 8));
    assert_eq!(only_label(&h2), "void call(other: String)");

    shutdown(&client, server_thread);
}

/// `bind` keeps the native `Callable.bind` shape: it appends its arguments to the END of the
/// lambda's parameter list, so which parameter the cursor sits in depends on how many arguments the
/// user will eventually bind — unknowable mid-typing, and a wrong `activeParameter` is a lie.
#[test]
fn lambda_bind_is_not_mapped_onto_the_lambdas_parameters() {
    let p = sig_project();
    let uri = file_uri(&p.root.join("src/lambda_bind.gd"));
    let src = "extends Node\n\n\
               func run() -> void:\n\
               \tvar f := func(a: int, b: int) -> void:\n\t\tpass\n\
               \tf.bind(2)\n";
    let (client, server_thread) = boot(&p, caps(true, true), &uri, src);

    // `\tf.bind(2)` is line 5; arg 0 = tab(1) + "f.bind(".len()(7) = 8.
    let raw = sig_raw(&client, 46, &uri, Position::new(5, 8));
    assert!(
        raw.is_null(),
        "bind must not borrow the lambda's parameter list; got {raw}"
    );

    shutdown(&client, server_thread);
}

/// Fail-closed for a class-level holder too: a member `var` that any method rebinds (`self.handler =
/// …`) may hold a different callable at the call site than the lambda it was declared with, so no
/// lambda signature is offered.
#[test]
fn reassigned_class_level_lambda_var_refuses() {
    let p = sig_project();
    let uri = file_uri(&p.root.join("src/lambda_member_reassigned.gd"));
    let src = "extends Node\n\n\
               var handler := func(damage: int) -> bool:\n\treturn damage > 0\n\n\
               func swap() -> void:\n\
               \tself.handler = func(other: String) -> bool:\n\t\treturn true\n\n\
               func run() -> void:\n\thandler.call(3)\n";
    let (client, server_thread) = boot(&p, caps(true, true), &uri, src);

    // `\thandler.call(3)` is line 10; arg 0 = tab(1) + "handler.call(".len()(13) = 14.
    let raw = sig_raw(&client, 47, &uri, Position::new(10, 14));
    assert!(
        raw.is_null(),
        "a rebound member must not claim a lambda signature; got {raw}"
    );

    shutdown(&client, server_thread);
}
