//! #298 / #301: an inner class or a named enum is a cross-file symbol.
//!
//! `Owner.Inner` and `Owner.SomeEnum` are ordinary references from any file that can name
//! `Owner`, so `definition` must jump to them and `references` / `rename` must collect every one.
//! Before this, both were treated as file-private: `definition` returned no result, and
//! `references` fanned out empty cross-file — which under `rename` silently rewrote the declaring
//! file only and left every caller pointing at a name that no longer existed.

mod common;

use common::{file_uri, notification, recv, request, shutdown, TempProject};
use lsp_server::Connection;
use lsp_types::{
    DidOpenTextDocumentParams, GotoDefinitionParams, InitializeParams, InitializedParams, Location,
    Position, ReferenceContext, ReferenceParams, RenameParams, TextDocumentIdentifier,
    TextDocumentItem, TextDocumentPositionParams, WorkDoneProgressParams, WorkspaceEdit,
};

/// `owner.gd`, the declaring file. Line numbers are pinned by the probes below:
///   0 `class_name Owner`   1 `extends Node`   3 `enum Slot { WEAPON, ARMOR }`
///   5 `class Entry:`       6 `\tvar count := 0`
///   8 `func pick(s: Slot) -> Entry:`   9 `\treturn Entry.new()`
const OWNER_GD: &str = "class_name Owner\nextends Node\n\nenum Slot { WEAPON, ARMOR }\n\nclass Entry:\n\tvar count := 0\n\nfunc pick(s: Slot) -> Entry:\n\treturn Entry.new()\n";

/// `user.gd` reaches both through the `class_name`, in expression AND type position, plus once
/// through a preload const — the four shapes a real project mixes.
///   0 `extends Node`   1 `const OwnerScript := preload("res://owner.gd")`
///   3 `var a: Owner.Entry = null`   4 `var b := OwnerScript.Entry.new()`
///   5 `var c: Owner.Slot = Owner.Slot.WEAPON`
///   7 `func go() -> void:`   8 `\tprint(OwnerScript.Slot.ARMOR)`
const USER_GD: &str = "extends Node\nconst OwnerScript := preload(\"res://owner.gd\")\n\nvar a: Owner.Entry = null\nvar b := OwnerScript.Entry.new()\nvar c: Owner.Slot = Owner.Slot.WEAPON\n\nfunc go() -> void:\n\tprint(OwnerScript.Slot.ARMOR)\n";

/// A decoy: its own local named `Entry` must never be collected by a rename of `Owner.Entry`.
const DECOY_GD: &str = "extends Node\n\nfunc go() -> void:\n\tvar Entry := 1\n\tprint(Entry)\n";

fn project() -> TempProject {
    let p = TempProject::new();
    p.write(
        "project.godot",
        "config_version=5\n\n[application]\nconfig/features=PackedStringArray(\"4.6\")\n",
    );
    p.write("extension_api.json", common::MINI_API);
    p.write("owner.gd", OWNER_GD);
    p.write("user.gd", USER_GD);
    p.write("decoy.gd", DECOY_GD);
    p
}

fn init_and_open(project: &TempProject, client: &Connection) {
    let init = InitializeParams {
        initialization_options: Some(serde_json::json!({
            "projectRoot": project.root.as_str(),
            "autoDumpExtensionApi": false,
            "extensionApiPath": project.root.join("extension_api.json").as_str(),
        })),
        ..Default::default()
    };
    client.sender.send(request(1, "initialize", init)).unwrap();
    let _ = recv(client);
    client
        .sender
        .send(notification("initialized", InitializedParams {}))
        .unwrap();
    for (i, rel) in ["owner.gd", "user.gd", "decoy.gd"].iter().enumerate() {
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
                        version: (i + 2) as i32,
                        text,
                    },
                },
            ))
            .unwrap();
    }
    while common::try_recv(client, std::time::Duration::from_millis(300)).is_some() {}
}

fn pos(line: u32, character: u32) -> Position {
    Position { line, character }
}

fn refs_at(client: &Connection, uri: &lsp_types::Uri, at: Position, id: i32) -> Vec<Location> {
    let params = ReferenceParams {
        text_document_position: TextDocumentPositionParams {
            text_document: TextDocumentIdentifier { uri: uri.clone() },
            position: at,
        },
        context: ReferenceContext {
            include_declaration: true,
        },
        work_done_progress_params: WorkDoneProgressParams::default(),
        partial_result_params: Default::default(),
    };
    client
        .sender
        .send(request(id, "textDocument/references", params))
        .unwrap();
    let resp = common::recv_response(client);
    assert!(resp.error.is_none(), "references errored: {:?}", resp.error);
    serde_json::from_value(resp.result.expect("references result")).unwrap_or_default()
}

fn def_at(client: &Connection, uri: &lsp_types::Uri, at: Position, id: i32) -> Option<Location> {
    let params = GotoDefinitionParams {
        text_document_position_params: TextDocumentPositionParams {
            text_document: TextDocumentIdentifier { uri: uri.clone() },
            position: at,
        },
        work_done_progress_params: WorkDoneProgressParams::default(),
        partial_result_params: Default::default(),
    };
    client
        .sender
        .send(request(id, "textDocument/definition", params))
        .unwrap();
    let resp = common::recv_response(client);
    assert!(resp.error.is_none(), "definition errored: {:?}", resp.error);
    let value = resp.result?;
    if value.is_null() {
        return None;
    }
    serde_json::from_value::<Location>(value.clone())
        .ok()
        .or_else(|| {
            serde_json::from_value::<Vec<Location>>(value)
                .ok()
                .and_then(|v| v.into_iter().next())
        })
}

fn rename_at(
    client: &Connection,
    uri: &lsp_types::Uri,
    at: Position,
    new_name: &str,
    id: i32,
) -> WorkspaceEdit {
    let params = RenameParams {
        text_document_position: TextDocumentPositionParams {
            text_document: TextDocumentIdentifier { uri: uri.clone() },
            position: at,
        },
        new_name: new_name.to_owned(),
        work_done_progress_params: WorkDoneProgressParams::default(),
    };
    client
        .sender
        .send(request(id, "textDocument/rename", params))
        .unwrap();
    let resp = common::recv_response(client);
    assert!(resp.error.is_none(), "rename refused: {:?}", resp.error);
    serde_json::from_value(resp.result.expect("rename result")).unwrap()
}

/// Every edit an edit set carries, as `(file basename, line, start col, end col)`.
fn edit_sites(edit: &WorkspaceEdit) -> Vec<(String, u32, u32, u32)> {
    let mut out = Vec::new();
    let mut push = |uri: &lsp_types::Uri, edits: &[lsp_types::TextEdit]| {
        let name = uri.as_str().rsplit('/').next().unwrap_or("").to_owned();
        for e in edits {
            out.push((
                name.clone(),
                e.range.start.line,
                e.range.start.character,
                e.range.end.character,
            ));
        }
    };
    if let Some(changes) = &edit.changes {
        for (uri, edits) in changes {
            push(uri, edits);
        }
    }
    if let Some(lsp_types::DocumentChanges::Edits(docs)) = &edit.document_changes {
        for d in docs {
            let edits: Vec<lsp_types::TextEdit> = d
                .edits
                .iter()
                .map(|e| match e {
                    lsp_types::OneOf::Left(t) => t.clone(),
                    lsp_types::OneOf::Right(a) => a.text_edit.clone(),
                })
                .collect();
            push(&d.text_document.uri, &edits);
        }
    }
    out.sort();
    out
}

#[test]
fn definition_jumps_to_a_cross_file_inner_class_and_enum() {
    let p = project();
    let (server, client) = Connection::memory();
    let server_thread = std::thread::spawn(move || gd_server::serve(server));
    init_and_open(&p, &client);
    let user = file_uri(&p.root.join("user.gd"));
    let owner = file_uri(&p.root.join("owner.gd"));

    // `class Entry:` is line 5, `enum Slot` is line 3.
    for (at, want_line, label) in [
        (pos(3, 13), 5u32, "`var a: Owner.Entry` — type position"),
        (pos(4, 22), 5, "`OwnerScript.Entry.new()` — preload const"),
        (pos(5, 13), 3, "`var c: Owner.Slot` — type position"),
        (pos(5, 26), 3, "`Owner.Slot.WEAPON` — expression position"),
    ] {
        let got = def_at(
            &client,
            &user,
            at,
            100 + want_line as i32 + at.character as i32,
        );
        let loc = got.unwrap_or_else(|| panic!("{label}: definition returned no result"));
        assert_eq!(loc.uri, owner, "{label}: jumped to the wrong file");
        assert_eq!(
            loc.range.start.line, want_line,
            "{label}: jumped to the wrong line"
        );
    }
    shutdown(&client, server_thread);
}

#[test]
fn definition_jumps_to_a_cross_file_enum_value() {
    let p = project();
    let (server, client) = Connection::memory();
    let server_thread = std::thread::spawn(move || gd_server::serve(server));
    init_and_open(&p, &client);
    let user = file_uri(&p.root.join("user.gd"));
    let owner = file_uri(&p.root.join("owner.gd"));

    // `enum Slot { WEAPON, ARMOR }` — `WEAPON` at cols 12..18, `ARMOR` at 20..25.
    // The declaring file is `owner.gd`, not the file the cursor is in: the step that resolves
    // this used to search the CURRENT file's tree, which is why it returned nothing.
    for (at, want_col, label) in [
        (pos(5, 32), 12u32, "`Owner.Slot.WEAPON`"),
        (pos(8, 27), 20, "`OwnerScript.Slot.ARMOR`"),
    ] {
        let loc = def_at(&client, &user, at, 200 + want_col as i32)
            .unwrap_or_else(|| panic!("{label}: definition returned no result"));
        assert_eq!(loc.uri, owner, "{label}: jumped to the wrong file");
        assert_eq!(loc.range.start.line, 3, "{label}: wrong line");
        assert_eq!(
            loc.range.start.character, want_col,
            "{label}: landed on the wrong value of the enum"
        );
    }
    shutdown(&client, server_thread);
}

#[test]
fn references_on_an_inner_class_reach_across_files() {
    let p = project();
    let (server, client) = Connection::memory();
    let server_thread = std::thread::spawn(move || gd_server::serve(server));
    init_and_open(&p, &client);
    let owner = file_uri(&p.root.join("owner.gd"));
    let user = file_uri(&p.root.join("user.gd"));
    let decoy = file_uri(&p.root.join("decoy.gd"));

    // Click the declaration `class Entry:` (line 5, col 6..11).
    let locs = refs_at(&client, &owner, pos(5, 8), 300);
    assert!(
        locs.iter().filter(|l| l.uri == user).count() >= 2,
        "both `Owner.Entry` and `OwnerScript.Entry` in user.gd must be collected; got {locs:?}"
    );
    assert!(
        !locs.iter().any(|l| l.uri == decoy),
        "decoy.gd's unrelated local named `Entry` must never be collected; got {locs:?}"
    );
    shutdown(&client, server_thread);
}

#[test]
fn renaming_an_inner_class_rewrites_every_caller() {
    let p = project();
    let (server, client) = Connection::memory();
    let server_thread = std::thread::spawn(move || gd_server::serve(server));
    init_and_open(&p, &client);
    let owner = file_uri(&p.root.join("owner.gd"));

    let sites = edit_sites(&rename_at(&client, &owner, pos(5, 8), "Slotted", 400));
    // owner.gd: the `class Entry:` decl, the `-> Entry` return type, and `Entry.new()`.
    // user.gd: `Owner.Entry` and `OwnerScript.Entry`.
    assert_eq!(
        sites,
        vec![
            ("owner.gd".to_owned(), 5, 6, 11),
            ("owner.gd".to_owned(), 8, 22, 27),
            ("owner.gd".to_owned(), 9, 8, 13),
            ("user.gd".to_owned(), 3, 13, 18),
            ("user.gd".to_owned(), 4, 21, 26),
        ],
        "renaming an inner class must rewrite every caller and nothing else"
    );
    shutdown(&client, server_thread);
}

#[test]
fn renaming_an_inner_class_is_click_site_independent() {
    let p = project();
    let (server, client) = Connection::memory();
    let server_thread = std::thread::spawn(move || gd_server::serve(server));
    init_and_open(&p, &client);
    let owner = file_uri(&p.root.join("owner.gd"));
    let user = file_uri(&p.root.join("user.gd"));

    // A rename started from a USE site must produce the same edit set as one started from the
    // declaration — otherwise the safe-looking half-rename comes back through the other door.
    let from_decl = edit_sites(&rename_at(&client, &owner, pos(5, 8), "Slotted", 500));
    let from_use = edit_sites(&rename_at(&client, &user, pos(3, 13), "Slotted", 501));
    assert_eq!(
        from_decl, from_use,
        "rename from a use site must match rename from the declaration"
    );
    shutdown(&client, server_thread);
}

#[test]
fn renaming_a_named_enum_rewrites_every_caller() {
    let p = project();
    let (server, client) = Connection::memory();
    let server_thread = std::thread::spawn(move || gd_server::serve(server));
    init_and_open(&p, &client);
    let owner = file_uri(&p.root.join("owner.gd"));

    // `enum Slot` decl at line 3 col 5..9.
    let sites = edit_sites(&rename_at(&client, &owner, pos(3, 6), "Bucket", 600));
    assert_eq!(
        sites,
        vec![
            ("owner.gd".to_owned(), 3, 5, 9),
            ("owner.gd".to_owned(), 8, 13, 17),
            ("user.gd".to_owned(), 5, 13, 17),
            ("user.gd".to_owned(), 5, 26, 30),
            ("user.gd".to_owned(), 8, 19, 23),
        ],
        "renaming a named enum must rewrite every caller and nothing else"
    );
    shutdown(&client, server_thread);
}
