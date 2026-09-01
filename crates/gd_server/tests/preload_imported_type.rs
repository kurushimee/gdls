//! Integration tests for imported-asset `preload` typing (#444).
//!
//! `preload("res://tex.svg")` must type as the class the IMPORTER produced — read from the
//! asset's `.import` sidecar `[remap] type=` line, never guessed from the extension. Verified
//! against Godot 4.7.2 (`--check-only`): assigning the result to an `int` fires BOTH engine
//! errors, each naming `CompressedTexture2D`. A missing sidecar, or one naming a class this
//! dump doesn't know, degrades to Variant with no diagnostic — a missed precise type is a
//! known limitation; a wrong one is a defect.
//!
//! `.gd`/`.tscn`/`.gdshader`/`.tres` preloads keep their existing extension handling (not
//! imported assets, no sidecar exists) and are covered by the cross-file suites.

mod common;

use common::{file_uri, notification, recv, request, shutdown, TempProject};
use lsp_server::{Connection, Message};
use lsp_types::{
    DidOpenTextDocumentParams, InitializeParams, InitializedParams, PositionEncodingKind,
    PublishDiagnosticsParams, TextDocumentItem,
};

/// The `Node` hierarchy plus `Resource` → `Texture2D` → `CompressedTexture2D`: `Exact`
/// provenance so typed-assignment errors fire, and `CompressedTexture2D` present so the
/// sidecar's `type=` survives the known-class gate.
const TEXTURE_API: &str = r#"{
    "header": {"version_major": 4, "version_minor": 6, "version_patch": 3},
    "classes": [
        {"name": "Object"},
        {"name": "Node", "inherits": "Object"},
        {"name": "Resource", "inherits": "Object"},
        {"name": "Texture2D", "inherits": "Resource"},
        {"name": "CompressedTexture2D", "inherits": "Texture2D"}
    ]
}"#;

fn setup_project(script_rel: &str, script: &str) -> TempProject {
    let p = TempProject::new();
    p.write(
        "project.godot",
        "config_version=5\n\n[application]\n\nconfig/name=\"Imp\"\nconfig/features=PackedStringArray(\"4.6\")\n",
    );
    p.write("extension_api.json", TEXTURE_API);
    p.write(script_rel, script);
    p
}

fn boot(project: &TempProject) -> (Connection, std::thread::JoinHandle<anyhow::Result<()>>) {
    let (server, client) = Connection::memory();
    let handle = std::thread::spawn(move || gd_server::serve(server));
    let init = InitializeParams {
        initialization_options: Some(serde_json::json!({
            "projectRoot": project.root.as_str(),
            "autoDumpExtensionApi": false,
            "extensionApiPath": project.root.join("extension_api.json").as_str().to_owned(),
        })),
        capabilities: lsp_types::ClientCapabilities {
            general: Some(lsp_types::GeneralClientCapabilities {
                position_encodings: Some(vec![PositionEncodingKind::UTF8]),
                ..Default::default()
            }),
            ..Default::default()
        },
        ..Default::default()
    };
    client.sender.send(request(1, "initialize", init)).unwrap();
    let _ = recv(&client);
    client
        .sender
        .send(notification("initialized", InitializedParams {}))
        .unwrap();
    (client, handle)
}

/// Receive until the `publishDiagnostics` push arrives, skipping anything else the server sends
/// unprompted — a conforming client tolerates server notifications in any order.
fn recv_publish(client: &Connection) -> PublishDiagnosticsParams {
    loop {
        let msg = recv(client);
        let Message::Notification(notif) = msg else {
            panic!("expected a publishDiagnostics notification, got {msg:?}");
        };
        if notif.method == "textDocument/publishDiagnostics" {
            return serde_json::from_value(notif.params).expect("valid PublishDiagnosticsParams");
        }
    }
}

fn open_and_collect(
    client: &Connection,
    project: &TempProject,
    rel: &str,
) -> PublishDiagnosticsParams {
    let abs = project.root.join(rel);
    let text = std::fs::read_to_string(abs.as_std_path()).expect("read file");
    client
        .sender
        .send(notification(
            "textDocument/didOpen",
            DidOpenTextDocumentParams {
                text_document: TextDocumentItem {
                    uri: file_uri(&abs),
                    language_id: "gdscript".to_string(),
                    version: 1,
                    text,
                },
            },
        ))
        .unwrap();
    recv_publish(client)
}

#[test]
fn imported_asset_preload_types_from_the_sidecar() {
    let p = setup_project(
        "src/tex.gd",
        "extends Node\n\nconst TEX := preload(\"res://assets/tex.svg\")\n\nfunc f() -> void:\n\tvar n: int = TEX\n\tn = n + 0\n",
    );
    p.write(
        "assets/tex.svg.import",
        "[remap]\n\nimporter=\"texture\"\ntype=\"CompressedTexture2D\"\n",
    );
    let (client, handle) = boot(&p);
    let diags = open_and_collect(&client, &p, "src/tex.gd").diagnostics;
    assert_eq!(
        diags.len(),
        2,
        "Godot 4.7.2 emits both halves (as-type and to-variable): {diags:?}"
    );
    for d in &diags {
        assert!(
            d.message.contains("CompressedTexture2D"),
            "typed as the importer's product, not Variant: {d:?}"
        );
    }
    shutdown(&client, handle);
}

#[test]
fn imported_asset_without_a_sidecar_stays_variant_clean() {
    // No `assets/tex.svg.import` exists: no guessing from the extension — degrade to Variant
    // exactly as before #444, and the "never false-positive" rule holds.
    let p = setup_project(
        "src/tex.gd",
        "extends Node\n\nconst TEX := preload(\"res://assets/tex.svg\")\n\nfunc f() -> void:\n\tvar n: int = TEX\n\tn = n + 0\n",
    );
    let (client, handle) = boot(&p);
    let diags = open_and_collect(&client, &p, "src/tex.gd").diagnostics;
    assert!(
        diags.is_empty(),
        "a missing sidecar must stay silent: {diags:?}"
    );
    shutdown(&client, handle);
}

#[test]
fn sidecar_naming_a_class_unknown_to_the_dump_stays_variant_clean() {
    let p = setup_project(
        "src/tex.gd",
        "extends Node\n\nconst TEX := preload(\"res://assets/tex.svg\")\n\nfunc f() -> void:\n\tvar n: int = TEX\n\tn = n + 0\n",
    );
    p.write(
        "assets/tex.svg.import",
        "[remap]\n\nimporter=\"texture\"\ntype=\"NotARealClass\"\n",
    );
    let (client, handle) = boot(&p);
    let diags = open_and_collect(&client, &p, "src/tex.gd").diagnostics;
    assert!(
        diags.is_empty(),
        "an unknown sidecar class must degrade to Variant, never lie: {diags:?}"
    );
    shutdown(&client, handle);
}

#[test]
fn relative_preload_resolves_against_the_scripts_directory() {
    // `preload("tex2.svg")` joins the referring script's directory (analyzer.cpp:437's
    // relativization) — the sidecar found there types it just like the res:// form.
    let p = setup_project(
        "src/tex.gd",
        "extends Node\n\nconst TEX := preload(\"tex2.svg\")\n\nfunc f() -> void:\n\tvar n: int = TEX\n\tn = n + 0\n",
    );
    p.write(
        "src/tex2.svg.import",
        "[remap]\n\ntype=\"CompressedTexture2D\"\n",
    );
    let (client, handle) = boot(&p);
    let diags = open_and_collect(&client, &p, "src/tex.gd").diagnostics;
    assert_eq!(
        diags.len(),
        2,
        "the relative form types through its sibling sidecar: {diags:?}"
    );
    for d in &diags {
        assert!(
            d.message.contains("CompressedTexture2D"),
            "typed as the importer's product: {d:?}"
        );
    }
    shutdown(&client, handle);
}
