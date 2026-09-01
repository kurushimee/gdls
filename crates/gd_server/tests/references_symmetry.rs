//! #544: `textDocument/references` answers the same set from every anchor.
//!
//! The answer used to depend on which site the user clicked. A bare call site classified through
//! the non-method path, which collects `Binding::Use` locations by identity and never projects
//! `Binding::Call` — so dotted call sites were missing, and so was the whole override group. From
//! the declaration or from any dotted site the same symbol returned the full set.
//!
//! `rename` already had the right shape: it canonicalizes the click to a declaration first, which
//! is why it was never wrong here. These pin that `references` now does too, and pin what must NOT
//! start rerouting — a bare native or utility call keeps its raw-identifier answer.

mod common;

use common::{file_uri, notification, recv, request, shutdown, TempProject};
use lsp_server::Connection;
use lsp_types::{
    DidOpenTextDocumentParams, DocumentHighlight, DocumentHighlightParams, InitializeParams,
    InitializedParams, Location, Position, ReferenceContext, ReferenceParams,
    TextDocumentIdentifier, TextDocumentItem, TextDocumentPositionParams, WorkDoneProgressParams,
};

const NODE_API: &str = r#"{
    "header": { "version_major": 4, "version_minor": 6, "version_patch": 3 },
    "classes": [
        {"name": "Object", "is_instantiable": true},
        {"name": "Node", "inherits": "Object", "is_instantiable": true,
         "methods": [{"name": "set_process", "is_const": false, "is_static": false,
                      "is_vararg": false, "is_virtual": false, "hash": 1,
                      "arguments": [{"name": "enable", "type": "bool"}]}]},
        {"name": "CanvasItem", "inherits": "Node", "is_instantiable": true},
        {"name": "Node2D", "inherits": "CanvasItem", "is_instantiable": true}
    ],
    "utility_functions": [
        {"name": "print", "return_type": "void", "is_vararg": true, "arguments": []}
    ]
}"#;

fn boot(
    project: &TempProject,
    files: &[&str],
) -> (Connection, std::thread::JoinHandle<anyhow::Result<()>>) {
    let (server, client) = Connection::memory();
    let handle = std::thread::spawn(move || gd_server::serve(server));
    let init = InitializeParams {
        initialization_options: Some(serde_json::json!({
            "projectRoot": project.root.as_str(),
            "autoDumpExtensionApi": false,
            "extensionApiPath": project.root.join("extension_api.json").as_str(),
        })),
        ..Default::default()
    };
    client.sender.send(request(1, "initialize", init)).unwrap();
    let _ = recv(&client);
    client
        .sender
        .send(notification("initialized", InitializedParams {}))
        .unwrap();
    for (i, rel) in files.iter().enumerate() {
        let abs = project.root.join(rel);
        let text = std::fs::read_to_string(abs.as_std_path()).expect("read fixture");
        client
            .sender
            .send(notification(
                "textDocument/didOpen",
                DidOpenTextDocumentParams {
                    text_document: TextDocumentItem {
                        uri: file_uri(&abs),
                        language_id: "gdscript".to_string(),
                        version: (i + 2) as i32,
                        text,
                    },
                },
            ))
            .unwrap();
    }
    while common::try_recv(&client, std::time::Duration::from_millis(300)).is_some() {}
    (client, handle)
}

fn pos(uri: &lsp_types::Uri, line: u32, character: u32) -> TextDocumentPositionParams {
    TextDocumentPositionParams {
        text_document: TextDocumentIdentifier { uri: uri.clone() },
        position: Position { line, character },
    }
}

/// Reference sites as `file:line`, sorted — a set a test can compare across anchors without
/// pinning columns.
fn ref_sites(client: &Connection, id: i32, p: TextDocumentPositionParams) -> Vec<String> {
    client
        .sender
        .send(request(
            id,
            "textDocument/references",
            ReferenceParams {
                text_document_position: p,
                context: ReferenceContext {
                    include_declaration: true,
                },
                work_done_progress_params: WorkDoneProgressParams::default(),
                partial_result_params: Default::default(),
            },
        ))
        .unwrap();
    let resp = common::recv_response(client);
    assert!(resp.error.is_none(), "references errored: {:?}", resp.error);
    let locs: Vec<Location> =
        serde_json::from_value(resp.result.expect("references result")).unwrap_or_default();
    let mut out: Vec<String> = locs
        .iter()
        .map(|l| {
            let s = l.uri.as_str();
            format!(
                "{}:{}",
                s.rsplit('/').next().unwrap_or(s),
                l.range.start.line
            )
        })
        .collect();
    out.sort();
    out
}

fn highlight_lines(client: &Connection, id: i32, p: TextDocumentPositionParams) -> Vec<u32> {
    client
        .sender
        .send(request(
            id,
            "textDocument/documentHighlight",
            DocumentHighlightParams {
                text_document_position_params: p,
                work_done_progress_params: WorkDoneProgressParams::default(),
                partial_result_params: Default::default(),
            },
        ))
        .unwrap();
    let resp = common::recv_response(client);
    let hls: Vec<DocumentHighlight> =
        serde_json::from_value(resp.result.expect("highlight result")).unwrap_or_default();
    let mut out: Vec<u32> = hls.iter().map(|h| h.range.start.line).collect();
    out.sort_unstable();
    out
}

fn project(files: &[(&str, &str)]) -> TempProject {
    let p = TempProject::new();
    p.write(
        "project.godot",
        "config_version=5\n\n[application]\n\nconfig/name=\"T\"\n",
    );
    p.write("extension_api.json", NODE_API);
    for (rel, src) in files {
        p.write(rel, src);
    }
    p
}

/// The issue's repro, one file. Line 2 declares, line 6 calls bare, line 9 calls through `self.`.
const SAME_FILE: &str = "\
extends Node

func g() -> void:
\tpass

func a() -> void:
\tg()

func b() -> void:
\tself.g()
";

#[test]
fn a_same_file_method_answers_the_same_from_every_anchor() {
    let p = project(&[("same.gd", SAME_FILE)]);
    let (client, handle) = boot(&p, &["same.gd"]);
    let uri = file_uri(&p.root.join("same.gd"));

    let want = vec![
        "same.gd:2".to_owned(),
        "same.gd:6".to_owned(),
        "same.gd:9".to_owned(),
    ];
    // The declaration, the bare call, and the `self.` call.
    assert_eq!(ref_sites(&client, 2, pos(&uri, 2, 5)), want, "declaration");
    assert_eq!(ref_sites(&client, 3, pos(&uri, 6, 1)), want, "bare call");
    assert_eq!(ref_sites(&client, 4, pos(&uri, 9, 6)), want, "self. call");
    shutdown(&client, handle);
}

/// documentHighlight is the in-file subset of `references`, so it takes the same reroute or the
/// two disagree inside one buffer.
#[test]
fn document_highlight_agrees_from_every_anchor() {
    let p = project(&[("same.gd", SAME_FILE)]);
    let (client, handle) = boot(&p, &["same.gd"]);
    let uri = file_uri(&p.root.join("same.gd"));

    let want = vec![2, 6, 9];
    assert_eq!(highlight_lines(&client, 2, pos(&uri, 2, 5)), want);
    assert_eq!(highlight_lines(&client, 3, pos(&uri, 6, 1)), want);
    assert_eq!(highlight_lines(&client, 4, pos(&uri, 9, 6)), want);
    shutdown(&client, handle);
}

const BASE_GD: &str = "\
class_name SymBase
extends Node2D

func describe() -> String:
\treturn \"base\"
";

const SUB_GD: &str = "\
class_name SymSub
extends SymBase

func h() -> void:
\tdescribe()
\tself.describe()
";

const USER_GD: &str = "\
extends Node

func f(s: SymSub) -> void:
\ts.describe()
";

/// Across files, with the declaration in a parent script: four anchors, one answer. The bare call
/// is the one that used to return a two-element subset.
#[test]
fn a_cross_file_inherited_method_answers_the_same_from_every_anchor() {
    let p = project(&[
        ("base.gd", BASE_GD),
        ("sub.gd", SUB_GD),
        ("user.gd", USER_GD),
    ]);
    let (client, handle) = boot(&p, &["base.gd", "sub.gd", "user.gd"]);
    let base_uri = file_uri(&p.root.join("base.gd"));
    let sub_uri = file_uri(&p.root.join("sub.gd"));
    let user_uri = file_uri(&p.root.join("user.gd"));

    let want = vec![
        "base.gd:3".to_owned(),
        "sub.gd:4".to_owned(),
        "sub.gd:5".to_owned(),
        "user.gd:3".to_owned(),
    ];
    assert_eq!(ref_sites(&client, 2, pos(&base_uri, 3, 6)), want, "decl");
    assert_eq!(ref_sites(&client, 3, pos(&sub_uri, 4, 1)), want, "bare");
    assert_eq!(ref_sites(&client, 4, pos(&sub_uri, 5, 7)), want, "self.");
    assert_eq!(ref_sites(&client, 5, pos(&user_uri, 3, 3)), want, "typed");
    shutdown(&client, handle);
}

/// The fail-closed half: a bare call whose callee is NATIVE or a utility resolves to no project
/// script, so it keeps the raw-identifier answer it has today and never reroutes.
#[test]
fn a_bare_native_or_utility_call_keeps_its_answer() {
    let src = "extends Node\n\nfunc f() -> void:\n\tprint(1)\n\tset_process(true)\n";
    let p = project(&[("native.gd", src)]);
    let (client, handle) = boot(&p, &["native.gd"]);
    let uri = file_uri(&p.root.join("native.gd"));

    assert_eq!(
        ref_sites(&client, 2, pos(&uri, 3, 1)),
        vec!["native.gd:3".to_owned()],
        "a utility call answers itself only"
    );
    assert_eq!(
        ref_sites(&client, 3, pos(&uri, 4, 1)),
        vec!["native.gd:4".to_owned()],
        "a native method call answers itself only"
    );
    shutdown(&client, handle);
}

/// A local is function-scoped and has no `Binding::Call` at all, so the reroute cannot reach it.
#[test]
fn a_local_variable_still_stays_in_its_function() {
    let src = "extends Node\n\nfunc f() -> void:\n\tvar g := 1\n\tprint(g)\n\nfunc other() -> void:\n\tvar g := 2\n\tprint(g)\n";
    let p = project(&[("local.gd", src)]);
    let (client, handle) = boot(&p, &["local.gd"]);
    let uri = file_uri(&p.root.join("local.gd"));

    assert_eq!(
        ref_sites(&client, 2, pos(&uri, 3, 5)),
        vec!["local.gd:3".to_owned(), "local.gd:4".to_owned()],
        "the other function's `g` must not join"
    );
    shutdown(&client, handle);
}
