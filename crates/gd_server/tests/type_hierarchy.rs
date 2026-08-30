//! M9 (#69) gate: `textDocument/prepareTypeHierarchy` + `typeHierarchy/supertypes` +
//! `typeHierarchy/subtypes` over an in-memory connection.
//!
//! The feature is a class-tree navigator over the `class_name` registry + `extends` graph (the same
//! structures `implementation` walks) crossing the project→native boundary. The project here is a
//! three-level chain `Base ← Mid ← Leaf`, all rooted at the native `Node` (Object←Node in the dump),
//! so a supertypes walk leaves the project (`Mid → Base → Node`) and lands on a stub-anchored native
//! item.
//!
//! Two non-obvious shapes the tests pin:
//!   - **`typeHierarchyProvider` is injected post-serialization** (lsp-types 0.97.0 has no
//!     `ServerCapabilities::type_hierarchy_provider` field, so the typed `InitializeResult` would
//!     drop the key). The advertisement test therefore reads the RAW initialize-response JSON.
//!   - **subtypes is ONE level (direct children), not the transitive closure `implementation`
//!     returns.** `implementation(Base)` = {Mid, Leaf} (transitive), but `subtypes(Base)` = {Mid}.
//!     The "matches implementation" criterion is asserted at `Mid`, where direct == transitive ==
//!     {Leaf} (Mid has no grandchildren).

use std::time::Duration;

use lsp_server::{Connection, Message, Notification, Request, RequestId};
use lsp_types::{
    ClientCapabilities, GeneralClientCapabilities, GotoDefinitionParams, GotoDefinitionResponse,
    InitializeParams, InitializedParams, PartialResultParams, Position, PositionEncodingKind,
    TextDocumentIdentifier, TextDocumentItem, TextDocumentPositionParams, TypeHierarchyItem, Uri,
    WorkDoneProgressParams,
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
/// (`Object←Node`, plus `RefCounted←Object`) makes the analyzer/index resolve native bases so the
/// supertypes walk crosses into a real `Node` stub rather than the empty-DB permissive path.
struct NativeProject {
    root: camino::Utf8PathBuf,
    _dir: tempfile::TempDir,
}

impl NativeProject {
    fn new(files: &[(&str, &str)]) -> Self {
        let dir = tempfile::Builder::new()
            .prefix("gdls_typehier_")
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
                    {"name": "RefCounted", "inherits": "Object"},
                    {"name": "Node", "inherits": "Object"}
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

/// Boot a server over an in-memory connection, UTF-8 negotiated (LSP characters == bytes for ASCII
/// docs). Returns the connection, the thread handle, and the RAW initialize-response JSON — raw,
/// because `typeHierarchyProvider` is injected past lsp-types' typed `ServerCapabilities` (which has
/// no field for it) and a typed deserialize would silently drop the key.
fn boot(project: &NativeProject) -> (Connection, std::thread::JoinHandle<()>, serde_json::Value) {
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
    let raw_result = resp.result.expect("initialize result");
    client
        .sender
        .send(notification(
            "initialized",
            serde_json::to_value(InitializedParams {}).unwrap(),
        ))
        .unwrap();
    (client, handle, raw_result)
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

/// `textDocument/prepareTypeHierarchy` at a position → the returned items (or `None` for `null`).
fn prepare_at(
    client: &Connection,
    uri: &Uri,
    position: Position,
) -> Option<Vec<TypeHierarchyItem>> {
    let params = serde_json::json!({
        "textDocument": { "uri": uri.as_str() },
        "position": { "line": position.line, "character": position.character },
    });
    client
        .sender
        .send(request(10, "textDocument/prepareTypeHierarchy", params))
        .unwrap();
    let resp = recv_response(client);
    serde_json::from_value(
        resp.result
            .expect("prepareTypeHierarchy result is always present"),
    )
    .expect("valid Option<Vec<TypeHierarchyItem>>")
}

/// `typeHierarchy/supertypes` for an item → the returned items (or `None` for `null`).
fn supertypes_of(client: &Connection, item: &TypeHierarchyItem) -> Option<Vec<TypeHierarchyItem>> {
    let params = serde_json::json!({ "item": item });
    client
        .sender
        .send(request(11, "typeHierarchy/supertypes", params))
        .unwrap();
    let resp = recv_response(client);
    serde_json::from_value(resp.result.expect("supertypes result is always present"))
        .expect("valid Option<Vec<TypeHierarchyItem>>")
}

/// `typeHierarchy/subtypes` for an item → the returned items (or `None` for `null`).
fn subtypes_of(client: &Connection, item: &TypeHierarchyItem) -> Option<Vec<TypeHierarchyItem>> {
    let params = serde_json::json!({ "item": item });
    client
        .sender
        .send(request(12, "typeHierarchy/subtypes", params))
        .unwrap();
    let resp = recv_response(client);
    serde_json::from_value(resp.result.expect("subtypes result is always present"))
        .expect("valid Option<Vec<TypeHierarchyItem>>")
}

fn implementation_at(
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
            13,
            "textDocument/implementation",
            serde_json::to_value(params).unwrap(),
        ))
        .unwrap();
    let resp = recv_response(client);
    serde_json::from_value(
        resp.result
            .expect("implementation result is always present"),
    )
    .expect("valid Option<GotoDefinitionResponse>")
}

/// The sorted target URIs `textDocument/implementation` reports at `position` (its transitive
/// subclass set). Panics if the result isn't the expected `Location[]`.
fn implementation_uris(client: &Connection, uri: &Uri, position: Position) -> Vec<String> {
    let resp = implementation_at(client, uri, position)
        .unwrap_or_else(|| panic!("implementation returned null at {position:?}"));
    let GotoDefinitionResponse::Array(locs) = resp else {
        panic!("implementation returns an array, got {resp:?}");
    };
    let mut uris: Vec<String> = locs.iter().map(|l| l.uri.as_str().to_string()).collect();
    uris.sort();
    uris
}

/// The single item `prepareTypeHierarchy` is expected to return at `position`.
fn prepare_one(client: &Connection, uri: &Uri, position: Position) -> TypeHierarchyItem {
    let items = prepare_at(client, uri, position)
        .unwrap_or_else(|| panic!("prepareTypeHierarchy returned null at {position:?}"));
    assert_eq!(
        items.len(),
        1,
        "prepareTypeHierarchy must return exactly one item, got {items:?}"
    );
    items.into_iter().next().unwrap()
}

const BASE: &str = "class_name Base\nextends Node\n";
const MID: &str = "class_name Mid\nextends Base\n";
const LEAF: &str = "class_name Leaf\nextends Mid\n";

fn three_level_project() -> NativeProject {
    NativeProject::new(&[("base.gd", BASE), ("mid.gd", MID), ("leaf.gd", LEAF)])
}

#[test]
fn type_hierarchy_provider_is_advertised() {
    // lsp-types 0.97.0 has no `ServerCapabilities::type_hierarchy_provider`, so the server injects
    // the standard `typeHierarchyProvider` key into the raw capabilities JSON. Assert on the raw
    // value — a typed deserialize would drop the unknown key.
    let project = three_level_project();
    let (client, handle, raw_init) = boot(&project);

    assert_eq!(
        raw_init
            .get("capabilities")
            .and_then(|c| c.get("typeHierarchyProvider")),
        Some(&serde_json::Value::Bool(true)),
        "typeHierarchyProvider must be advertised as `true`; got init result {raw_init}"
    );

    shutdown(&client, handle);
}

#[test]
fn prepare_on_project_class_returns_name_token_and_data() {
    // `prepareTypeHierarchy` on the `Mid` class name → one CLASS item whose selectionRange slices
    // exactly the `Mid` identifier (the #48 name-token lesson), with a non-empty `data` blob (the
    // #50 lesson — expansion must survive without a cursor).
    let project = three_level_project();
    let (client, handle, _) = boot(&project);
    let uri = project.uri("mid.gd");
    did_open(&client, &uri, MID);

    // `class_name Mid` → identifier `Mid` on line 0, cols 11..14.
    let item = prepare_one(&client, &uri, Position::new(0, 11));
    assert_eq!(item.name, "Mid");
    assert_eq!(item.kind, lsp_types::SymbolKind::CLASS);
    assert_eq!(item.uri, uri);
    // selectionRange == the `Mid` identifier token (cols 11..14 on line 0).
    assert_eq!(item.selection_range.start, Position::new(0, 11));
    assert_eq!(item.selection_range.end, Position::new(0, 14));
    // range must contain selectionRange (LSP invariant) — here they coincide.
    assert_eq!(item.range, item.selection_range);
    // data is non-empty and re-encodes the type identity (a project script → `fid`).
    let data = item.data.as_ref().expect("item carries a data blob");
    assert!(
        data.get("fid")
            .and_then(serde_json::Value::as_u64)
            .is_some(),
        "data blob should carry a project FileId, got {data}"
    );

    shutdown(&client, handle);
}

#[test]
fn supertypes_walk_crosses_into_native_base_multi_level() {
    // supertypes(Mid) → [Base] (project), then supertypes(Base) → [Node] (native, stub-anchored) —
    // multi-level, crossing the project→native boundary, re-resolving each step from `data` ALONE
    // (the returned item is fed straight back; no second prepare). This is also a depth>2 proof for
    // the supertypes direction: Mid (1) → Base (2) → Node (3).
    let project = three_level_project();
    let (client, handle, _) = boot(&project);
    let mid_uri = project.uri("mid.gd");
    did_open(&client, &mid_uri, MID);

    let mid = prepare_one(&client, &mid_uri, Position::new(0, 11));

    // Level 1: Mid's supertype is the project class Base.
    let supers =
        supertypes_of(&client, &mid).expect("supertypes is never null for a resolved item");
    assert_eq!(
        supers.len(),
        1,
        "Mid has exactly one supertype, got {supers:?}"
    );
    let base = &supers[0];
    assert_eq!(base.name, "Base");
    assert_eq!(base.uri, project.uri("base.gd"));
    assert!(
        base.data.as_ref().and_then(|d| d.get("fid")).is_some(),
        "Base (a project class) carries an fid data blob"
    );

    // Level 2 (depth>2): Base's supertype is the NATIVE class Node — resolved from Base's OWN data
    // blob, no cursor involved. Anchored at the Node stub header.
    let supers2 =
        supertypes_of(&client, base).expect("supertypes is never null for a resolved item");
    assert_eq!(
        supers2.len(),
        1,
        "Base has exactly one supertype (Node), got {supers2:?}"
    );
    let node = &supers2[0];
    assert_eq!(node.name, "Node");
    assert!(
        node.uri.as_str().ends_with("/Node.gd"),
        "the native base should anchor at the Node stub, got {}",
        node.uri.as_str()
    );
    // Node stub header: `class_name Node` → identifier at col 11..15 on line 0.
    assert_eq!(node.selection_range.start, Position::new(0, 11));
    assert_eq!(node.selection_range.end, Position::new(0, 15));
    assert_eq!(
        node.data.as_ref().and_then(|d| d.get("native")),
        Some(&serde_json::Value::String("Node".to_string())),
        "Node carries a native data blob"
    );

    // Level 3: Node's supertype is Object (native `inherits`), still from data alone — proving the
    // native chain also walks past the boundary it just crossed.
    let supers3 =
        supertypes_of(&client, node).expect("supertypes is never null for a resolved item");
    assert_eq!(supers3.len(), 1, "Node inherits Object, got {supers3:?}");
    assert_eq!(supers3[0].name, "Object");
    // Object is the top of the chain (no `inherits`) → empty supertypes (not null).
    let top =
        supertypes_of(&client, &supers3[0]).expect("supertypes never null for a resolved item");
    assert!(top.is_empty(), "Object has no supertype, got {top:?}");

    shutdown(&client, handle);
}

#[test]
fn subtypes_returns_direct_children_and_matches_implementation_at_a_leaf_parent() {
    // subtypes is ONE level (direct children): subtypes(Base) = {Mid} (NOT {Mid, Leaf} — that is
    // `implementation`'s transitive closure). The "matches implementation" criterion is asserted at
    // `Mid`, where the direct child set == the transitive set == {Leaf}.
    let project = three_level_project();
    let (client, handle, _) = boot(&project);
    let base_uri = project.uri("base.gd");
    let mid_uri = project.uri("mid.gd");
    did_open(&client, &base_uri, BASE);
    did_open(&client, &mid_uri, MID);

    // subtypes(Base) → exactly Mid (direct child), NOT Leaf (a grandchild).
    let base = prepare_one(&client, &base_uri, Position::new(0, 11));
    let base_subs =
        subtypes_of(&client, &base).expect("subtypes is never null for a resolved item");
    let base_sub_names: Vec<&str> = base_subs.iter().map(|i| i.name.as_str()).collect();
    assert_eq!(
        base_sub_names,
        vec!["Mid"],
        "subtypes(Base) must be the direct child Mid only (Leaf is transitive), got {base_subs:?}"
    );
    assert_eq!(base_subs[0].uri, mid_uri);

    // subtypes(Mid) → Leaf. At Mid the direct child set == the transitive set, so it must match
    // exactly what `implementation` reports for Mid.
    let mid = prepare_one(&client, &mid_uri, Position::new(0, 11));
    let mid_subs = subtypes_of(&client, &mid).expect("subtypes is never null for a resolved item");
    let mut sub_uris: Vec<String> = mid_subs
        .iter()
        .map(|i| i.uri.as_str().to_string())
        .collect();
    sub_uris.sort();

    let impl_uris_mid = implementation_uris(&client, &mid_uri, Position::new(0, 11));
    assert_eq!(
        sub_uris, impl_uris_mid,
        "subtypes(Mid) must match implementation(Mid) (both = {{leaf.gd}})"
    );
    assert_eq!(sub_uris, vec![project.uri("leaf.gd").as_str().to_string()]);

    // Criterion 4 stated against Base directly: `implementation(Base)` is the TRANSITIVE set
    // {mid, leaf}; the one-level subtypes walk reproduces it exactly when expanded recursively
    // (`subtypes(Base)` ∪ `subtypes(Mid)`). This is the only sense in which a one-level result
    // "matches what implementation reports for Base", and it re-confirms the one-level semantics.
    let impl_uris_base = implementation_uris(&client, &base_uri, Position::new(0, 11));
    assert_eq!(
        impl_uris_base,
        vec![
            project.uri("leaf.gd").as_str().to_string(),
            project.uri("mid.gd").as_str().to_string(),
        ],
        "implementation(Base) is the transitive closure {{mid, leaf}}"
    );
    // Expand subtypes recursively from Base and collect the closure.
    let mut closure: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    let mut frontier = vec![base.clone()];
    while let Some(item) = frontier.pop() {
        for child in subtypes_of(&client, &item).expect("subtypes never null for a resolved item") {
            if closure.insert(child.uri.as_str().to_string()) {
                frontier.push(child);
            }
        }
    }
    let closure: Vec<String> = closure.into_iter().collect();
    assert_eq!(
        closure, impl_uris_base,
        "recursive subtypes(Base) closure must reproduce implementation(Base) exactly"
    );

    shutdown(&client, handle);
}

#[test]
fn subtypes_expansion_survives_past_depth_2() {
    // depth>2 for the subtypes direction: subtypes(Base) → [Mid], then subtypes(Mid) → [Leaf],
    // the second step driven by the FIRST result's `data` blob alone (no re-prepare). Proves
    // downward expansion doesn't die at depth 2.
    let project = three_level_project();
    let (client, handle, _) = boot(&project);
    let base_uri = project.uri("base.gd");
    did_open(&client, &base_uri, BASE);

    let base = prepare_one(&client, &base_uri, Position::new(0, 11));
    let level1 = subtypes_of(&client, &base).expect("subtypes never null for a resolved item");
    assert_eq!(level1.len(), 1, "subtypes(Base) = [Mid], got {level1:?}");
    let mid = &level1[0];
    assert_eq!(mid.name, "Mid");

    // Expand the RETURNED item (Mid) using only its data — the depth>2 hop.
    let level2 = subtypes_of(&client, mid).expect("subtypes never null for a resolved item");
    assert_eq!(level2.len(), 1, "subtypes(Mid) = [Leaf], got {level2:?}");
    assert_eq!(level2[0].name, "Leaf");
    assert_eq!(level2[0].uri, project.uri("leaf.gd"));

    // And Leaf is a leaf — no further subtypes (empty, not null).
    let leaf = &level2[0];
    let level3 = subtypes_of(&client, leaf).expect("subtypes never null for a resolved item");
    assert!(level3.is_empty(), "Leaf has no subtypes, got {level3:?}");

    shutdown(&client, handle);
}

#[test]
fn prepare_off_an_identifier_returns_null() {
    // The cursor must land on a resolvable class. On whitespace / a non-class token, the handler
    // returns `null` (never crash, never guess).
    let project = three_level_project();
    let (client, handle, _) = boot(&project);
    let uri = project.uri("mid.gd");
    did_open(&client, &uri, MID);

    // Line 0, col 0 is the `c` of `class_name` (a keyword, not an identifier node here) — and the
    // unnamed-fallback gate only fires on the root class header for an UNNAMED script; this file is
    // named, so a click on the keyword (not the `Mid` identifier) resolves to no item.
    assert!(
        prepare_at(&client, &uri, Position::new(1, 0)).is_none(),
        "prepareTypeHierarchy on the `extends` keyword (col 0, line 1) should be null"
    );

    shutdown(&client, handle);
}

#[test]
fn prepare_on_native_class_name_anchors_at_stub() {
    // `prepareTypeHierarchy` on a native class name (`Node` in `extends Node`) → a stub-anchored
    // native item with a `{"native": "Node"}` data blob, whose supertypes walk one level up the
    // native `inherits` chain.
    let project = three_level_project();
    let (client, handle, _) = boot(&project);
    let uri = project.uri("base.gd");
    did_open(&client, &uri, BASE);

    // `extends Node` on line 1 → `Node` identifier at cols 8..12.
    let item = prepare_one(&client, &uri, Position::new(1, 8));
    assert_eq!(item.name, "Node");
    assert!(
        item.uri.as_str().ends_with("/Node.gd"),
        "native item anchors at the Node stub, got {}",
        item.uri.as_str()
    );
    assert_eq!(
        item.data.as_ref().and_then(|d| d.get("native")),
        Some(&serde_json::Value::String("Node".to_string()))
    );

    // Its supertype is Object (one hop up the native chain).
    let supers = supertypes_of(&client, &item).expect("supertypes never null for a resolved item");
    assert_eq!(supers.len(), 1);
    assert_eq!(supers[0].name, "Object");

    shutdown(&client, handle);
}

#[test]
fn supertypes_and_subtypes_on_a_malformed_data_blob_return_null_not_crash() {
    // The `data` blob is client-controlled — a garbage / empty / absent blob must degrade to the
    // LSP `null` response, never panic (never crash, never lie). Forge an item whose data carries
    // neither `fid` nor `native`.
    let project = three_level_project();
    let (client, handle, _) = boot(&project);

    let bogus = TypeHierarchyItem {
        name: "Whatever".to_string(),
        kind: lsp_types::SymbolKind::CLASS,
        tags: None,
        detail: None,
        uri: project.uri("mid.gd"),
        range: lsp_types::Range::new(Position::new(0, 0), Position::new(0, 0)),
        selection_range: lsp_types::Range::new(Position::new(0, 0), Position::new(0, 0)),
        data: Some(serde_json::json!({ "unrelated": 1 })),
    };

    assert!(
        supertypes_of(&client, &bogus).is_none(),
        "supertypes on a blob with no fid/native must be null"
    );
    assert!(
        subtypes_of(&client, &bogus).is_none(),
        "subtypes on a blob with no fid/native must be null"
    );

    // A `fid: 0` is also rejected (the index never mints 0; `FileId::new(0)` would panic) — guarded
    // back to null rather than trusting the wire.
    let zero_fid = TypeHierarchyItem {
        data: Some(serde_json::json!({ "fid": 0 })),
        ..bogus.clone()
    };
    assert!(
        supertypes_of(&client, &zero_fid).is_none(),
        "supertypes on a fid:0 blob must be null (not a panic)"
    );

    shutdown(&client, handle);
}

#[test]
fn prepare_on_unnamed_script_header_returns_the_file_item() {
    // The unnamed-script fallback (resolution step 3): an unnamed `.gd` clicked on an UNRESOLVED
    // identifier in its `extends` header — here `extends Unknown`, `Unknown` being neither a
    // project `class_name` nor a native class — still produces a hierarchy item for the file
    // itself, so an unnamed script is navigable. (A resolvable base like `Node` would take step 2
    // and anchor on Node instead; an identifier deep in the body would resolve to null — only the
    // header gates the fallback.) The item names the file stem and carries the file's `fid` blob.
    let unnamed = "extends Unknown\n";
    let project = NativeProject::new(&[("loose.gd", unnamed)]);
    let (client, handle, _) = boot(&project);
    let uri = project.uri("loose.gd");
    did_open(&client, &uri, unnamed);

    // `extends Unknown` on line 0 → the `Unknown` identifier at cols 8..15.
    let item = prepare_one(&client, &uri, Position::new(0, 8));
    assert_eq!(
        item.name, "loose",
        "an unnamed script falls back to its file stem"
    );
    assert_eq!(item.uri, uri);
    assert_eq!(item.kind, lsp_types::SymbolKind::CLASS);
    assert!(
        item.data.as_ref().and_then(|d| d.get("fid")).is_some(),
        "the file item carries a project fid blob, got {:?}",
        item.data
    );

    // And the data blob re-resolves: subtypes is empty (nothing extends this loose script) but
    // never null — the item resolved to a real type.
    assert_eq!(
        subtypes_of(&client, &item).expect("subtypes never null for a resolved item"),
        vec![],
        "no project file extends the loose script"
    );

    shutdown(&client, handle);
}

#[test]
fn refcounted_supertype_subtype_roundtrip_is_symmetric() {
    // A bare `class_name Plain` with no `extends` implicitly extends `RefCounted`. The
    // supertypes↔subtypes round-trip must be symmetric: walking UP from Plain reaches RefCounted,
    // and expanding RefCounted's subtypes (from its data blob alone) must list Plain AGAIN.
    // Regression for the PR #103 review: `extends_matches` previously returned false for
    // `Extends::None`, so a no-`extends` script vanished when expanding `RefCounted`'s subtypes.
    let project = NativeProject::new(&[("plain.gd", "class_name Plain\n")]);
    let (client, handle, _) = boot(&project);
    let uri = project.uri("plain.gd");
    did_open(&client, &uri, "class_name Plain\n");

    // `class_name Plain` → identifier `Plain` at cols 11..16.
    let plain = prepare_one(&client, &uri, Position::new(0, 11));
    assert_eq!(plain.name, "Plain");

    // supertypes(Plain) → [RefCounted] (the implied native base).
    let supers = supertypes_of(&client, &plain).expect("supertypes never null for a resolved item");
    assert_eq!(
        supers.len(),
        1,
        "Plain's implied supertype is RefCounted, got {supers:?}"
    );
    let refcounted = &supers[0];
    assert_eq!(refcounted.name, "RefCounted");
    assert_eq!(
        refcounted.data.as_ref().and_then(|d| d.get("native")),
        Some(&serde_json::Value::String("RefCounted".to_string())),
    );

    // Round-trip: subtypes(RefCounted) — driven by its data blob alone — must include Plain again.
    let subs = subtypes_of(&client, refcounted).expect("subtypes never null for a resolved item");
    assert!(
        subs.iter().any(|i| i.name == "Plain" && i.uri == uri),
        "subtypes(RefCounted) must include the no-`extends` Plain (symmetric round-trip), got {subs:?}"
    );

    shutdown(&client, handle);
}

// =============================================================================================
// #359 — a dotted `extends Outer.Inner` names ONE class, and it is not whatever top-level
// `class_name` shares the last segment.
// =============================================================================================

/// The #359 project: an `Outer` holding an inner `Inner`, a completely unrelated top-level
/// `class_name Inner`, and one subclass of each. The two `Inner`s share nothing but their text, so
/// every answer about one that mentions the other's file is wrong.
const OUTER: &str = "class_name Outer\nextends Node\n\n## The nested worker Outer owns.\nclass Inner:\n\textends Node\n\tfunc tick() -> void:\n\t\tpass\n";
const UNRELATED: &str =
    "## A top-level class that has nothing to do with Outer.\nclass_name Inner\nextends Node\n";
const CHILD: &str = "extends Outer.Inner\n";
const SUB_UNRELATED: &str = "extends Inner\n";

fn inner_class_project() -> NativeProject {
    NativeProject::new(&[
        ("outer.gd", OUTER),
        ("unrelated.gd", UNRELATED),
        ("child.gd", CHILD),
        ("sub_unrelated.gd", SUB_UNRELATED),
    ])
}

/// `textDocument/definition` for the request the handler answers under test.
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
            14,
            "textDocument/definition",
            serde_json::to_value(params).unwrap(),
        ))
        .unwrap();
    let resp = recv_response(client);
    serde_json::from_value(resp.result.expect("definition result is always present"))
        .expect("valid Option<GotoDefinitionResponse>")
}

/// `definition` on the SUFFIX segment of `extends Outer.Inner` lands on `Outer`'s inner class, not
/// on the unrelated top-level `class_name Inner` that shares the segment's text.
#[test]
fn definition_on_an_extends_suffix_segment_lands_on_the_inner_class() {
    let project = inner_class_project();
    let (client, handle, _) = boot(&project);
    let child_uri = project.uri("child.gd");
    did_open(&client, &child_uri, CHILD);

    // `extends Outer.Inner` — the `Inner` segment starts at column 14.
    let resp = definition_at(&client, &child_uri, Position::new(0, 14))
        .expect("definition resolves the suffix segment");
    let GotoDefinitionResponse::Scalar(loc) = resp else {
        panic!("definition returns a single location, got {resp:?}");
    };
    assert_eq!(loc.uri, project.uri("outer.gd"));
    // `class Inner:` is line 4; the identifier spans columns 6..11.
    assert_eq!(loc.range.start, Position::new(4, 6));
    assert_eq!(loc.range.end, Position::new(4, 11));

    shutdown(&client, handle);
}

/// `prepareTypeHierarchy` on that same segment yields the inner class as an item, with a `data`
/// blob that carries the inner-class path — the identity a bare name cannot express.
#[test]
fn prepare_on_an_extends_suffix_segment_yields_the_inner_class() {
    let project = inner_class_project();
    let (client, handle, _) = boot(&project);
    let child_uri = project.uri("child.gd");
    did_open(&client, &child_uri, CHILD);

    let item = prepare_one(&client, &child_uri, Position::new(0, 14));
    assert_eq!(item.name, "Inner");
    assert_eq!(item.uri, project.uri("outer.gd"));
    assert_eq!(item.selection_range.start, Position::new(4, 6));
    let data = item.data.as_ref().expect("the item carries a data blob");
    assert_eq!(
        data.get("inner").and_then(serde_json::Value::as_array),
        Some(&vec![serde_json::json!("Inner")]),
        "the blob addresses the INNER class, not the file's head class: {data}"
    );

    shutdown(&client, handle);
}

/// The other side of the same confusion: the unrelated top-level `Inner` must not claim `child.gd`
/// as a subtype, on either the type-hierarchy or the `implementation` surface.
#[test]
fn an_unrelated_global_of_the_same_name_claims_no_subtypes() {
    let project = inner_class_project();
    let (client, handle, _) = boot(&project);
    let unrelated_uri = project.uri("unrelated.gd");
    did_open(&client, &unrelated_uri, UNRELATED);

    // `class_name Inner` — line 1, the identifier starting at column 11.
    let item = prepare_one(&client, &unrelated_uri, Position::new(1, 11));
    assert_eq!(item.uri, unrelated_uri);
    let subs = subtypes_of(&client, &item).expect("subtypes are an array, never null");
    let sub_uris: Vec<&str> = subs.iter().map(|i| i.uri.as_str()).collect();
    assert_eq!(
        sub_uris,
        vec![project.uri("sub_unrelated.gd").as_str()],
        "`child.gd` extends Outer.Inner, a different class entirely"
    );

    let impls = implementation_uris(&client, &unrelated_uri, Position::new(1, 11));
    assert_eq!(
        impls,
        vec![project.uri("sub_unrelated.gd").as_str().to_string()]
    );

    shutdown(&client, handle);
}

/// From the declaring side: clicking the inner class's own `class Inner:` identifier addresses THAT
/// class, so the round trip down to `child.gd` and up to the native root both work.
#[test]
fn prepare_on_an_inner_class_declaration_round_trips() {
    let project = inner_class_project();
    let (client, handle, _) = boot(&project);
    let outer_uri = project.uri("outer.gd");
    did_open(&client, &outer_uri, OUTER);

    let item = prepare_one(&client, &outer_uri, Position::new(4, 6));
    assert_eq!(item.name, "Inner");
    assert_eq!(item.uri, outer_uri, "the inner class lives in `outer.gd`");

    let subs = subtypes_of(&client, &item).expect("subtypes are an array, never null");
    let sub_uris: Vec<&str> = subs.iter().map(|i| i.uri.as_str()).collect();
    assert_eq!(sub_uris, vec![project.uri("child.gd").as_str()]);

    let supers = supertypes_of(&client, &item).expect("supertypes are an array, never null");
    let super_names: Vec<&str> = supers.iter().map(|i| i.name.as_str()).collect();
    assert_eq!(
        super_names,
        vec!["Node"],
        "the inner class's own `extends Node`, not `Outer`'s"
    );

    shutdown(&client, handle);
}

/// A hand-forged `data` blob whose `inner` key is not an array of strings names a class the server
/// cannot identify. It decodes to nothing, and the request answers `null` — never a guess at the
/// file's head class.
#[test]
fn a_malformed_inner_path_in_the_blob_answers_null() {
    let project = inner_class_project();
    let (client, handle, _) = boot(&project);
    let outer_uri = project.uri("outer.gd");
    did_open(&client, &outer_uri, OUTER);

    let mut item = prepare_one(&client, &outer_uri, Position::new(4, 6));
    for bad in [
        serde_json::json!({ "fid": 1, "inner": "Inner" }),
        serde_json::json!({ "fid": 1, "inner": 42 }),
        serde_json::json!({ "fid": 1, "inner": [1] }),
    ] {
        item.data = Some(bad.clone());
        assert_eq!(supertypes_of(&client, &item), None, "blob {bad}");
        assert_eq!(subtypes_of(&client, &item), None, "blob {bad}");
    }

    shutdown(&client, handle);
}

/// The head segment of an `extends` chain follows Godot's own order in `resolve_class_inheritance`
/// (`gdscript_analyzer.cpp:469-543`): the global `class_name` registry first, the script's own
/// classes last. So `class B extends GA:` beside `class GA:` binds the GLOBAL `GA` while one is
/// registered. Godot separately reports the inner `GA` as hiding it, so a legal project cannot hold
/// this collision — but when one does, gdls resolves it the way Godot does rather than inventing a
/// friendlier answer.
#[test]
fn an_extends_head_binds_the_global_class_before_a_same_named_inner_one() {
    const GLOBAL: &str = "class_name GA\nextends Node\n";
    const COLLIDE: &str =
        "extends Node\n\nclass GA:\n\textends Node\n\nclass B extends GA:\n\tpass\n";
    let project = NativeProject::new(&[("ga.gd", GLOBAL), ("collide.gd", COLLIDE)]);
    let (client, handle, _) = boot(&project);
    let collide_uri = project.uri("collide.gd");
    did_open(&client, &collide_uri, COLLIDE);

    // `class B extends GA:` is line 5; `B`'s identifier starts at column 6.
    let item = prepare_one(&client, &collide_uri, Position::new(5, 6));
    assert_eq!(item.name, "B");

    let supers = supertypes_of(&client, &item).expect("supertypes are an array, never null");
    let parents: Vec<(&str, &str)> = supers
        .iter()
        .map(|i| (i.name.as_str(), i.uri.as_str()))
        .collect();
    assert_eq!(parents, vec![("GA", project.uri("ga.gd").as_str())]);

    shutdown(&client, handle);
}

/// The `implementation` seed is where the confusion used to be worst: it looked the cursor's bare
/// name up in the global registry, so an inner-class cursor seeded on the unrelated top-level
/// `Inner` and reported ITS subclasses. Both inner-class cursors now answer with `Outer.Inner`'s
/// own subclass, and neither borrows the top-level class's.
#[test]
fn implementation_on_an_inner_class_reports_its_own_subclasses() {
    let project = inner_class_project();
    let (client, handle, _) = boot(&project);
    let outer_uri = project.uri("outer.gd");
    let child_uri = project.uri("child.gd");
    did_open(&client, &outer_uri, OUTER);
    did_open(&client, &child_uri, CHILD);

    for (label, uri, pos) in [
        ("the declaration", &outer_uri, Position::new(4, 6)),
        ("the extends suffix", &child_uri, Position::new(0, 14)),
    ] {
        assert_eq!(
            implementation_uris(&client, uri, pos),
            vec![project.uri("child.gd").as_str().to_owned()],
            "{label} must report `child.gd` and never `sub_unrelated.gd`, \
             which subclasses a different `Inner`"
        );
    }

    shutdown(&client, handle);
}

/// The other direction of the same identity: the top-level `Inner` keeps `sub_unrelated.gd` and
/// does not pick up `Outer.Inner`'s child.
#[test]
fn implementation_on_the_top_level_class_keeps_its_own_subclasses() {
    let project = inner_class_project();
    let (client, handle, _) = boot(&project);
    let unrelated_uri = project.uri("unrelated.gd");
    did_open(&client, &unrelated_uri, UNRELATED);

    // `class_name Inner` is line 1; the identifier starts at column 11.
    assert_eq!(
        implementation_uris(&client, &unrelated_uri, Position::new(1, 11)),
        vec![project.uri("sub_unrelated.gd").as_str().to_owned()]
    );

    shutdown(&client, handle);
}

/// `typeHierarchy/subtypes` reads the same graph. It used to enumerate `iter_interfaces`, so only a
/// file's HEAD class was ever a candidate child; an inner class extending the cursor's class was
/// invisible no matter how it was reached (#368).
#[test]
fn subtypes_include_an_inner_class_child() {
    const BASE_GD: &str = "class_name TBase
extends Node
";
    const HOLDER: &str = "extends Node

class TSub extends TBase:
	pass
";
    let project = NativeProject::new(&[("tbase.gd", BASE_GD), ("holder.gd", HOLDER)]);
    let (client, handle, _) = boot(&project);
    let base_uri = project.uri("tbase.gd");
    did_open(&client, &base_uri, BASE_GD);

    // `class_name TBase` is line 0; the identifier starts at column 11.
    let item = prepare_one(&client, &base_uri, Position::new(0, 11));
    let subs = subtypes_of(&client, &item).expect("subtypes are an array, never null");
    let names: Vec<(&str, &str)> = subs
        .iter()
        .map(|i| (i.name.as_str(), i.uri.as_str()))
        .collect();
    assert_eq!(names, vec![("TSub", project.uri("holder.gd").as_str())]);

    shutdown(&client, handle);
}

/// A method declared on an inner class is a real override target on both ends: the cursor may sit
/// on the inner class's own `func`, and the reported override may itself live in an inner class.
#[test]
fn method_overrides_cross_the_inner_class_boundary() {
    const BASE_GD: &str = "class_name MBase
extends Node

class Job:
	extends Node
	func run() -> void:
		pass
";
    const HOLDER: &str = "extends Node

class Worker extends MBase.Job:
	func run() -> void:
		pass
";
    let project = NativeProject::new(&[("mbase.gd", BASE_GD), ("holder.gd", HOLDER)]);
    let (client, handle, _) = boot(&project);
    let base_uri = project.uri("mbase.gd");
    did_open(&client, &base_uri, BASE_GD);

    // `\tfunc run() -> void:` is line 5; `run` starts at column 6.
    let resp = implementation_at(&client, &base_uri, Position::new(5, 6))
        .expect("implementation returns an array, never null");
    let GotoDefinitionResponse::Array(locs) = resp else {
        panic!("implementation returns an array");
    };
    let hits: Vec<(String, Position)> = locs
        .iter()
        .map(|l| (l.uri.as_str().to_owned(), l.range.start))
        .collect();
    // `\tfunc run() -> void:` in holder.gd is line 3; `run` starts at column 6.
    assert_eq!(
        hits,
        vec![(
            project.uri("holder.gd").as_str().to_owned(),
            Position::new(3, 6)
        )]
    );

    shutdown(&client, handle);
}

/// `hover` reads the same identity. It used to render the unrelated top-level class's `##` doc for
/// the suffix segment, which is the wrong answer wearing the right name.
#[test]
fn hover_on_an_extends_suffix_shows_the_inner_class_doc() {
    let project = inner_class_project();
    let (client, handle, _) = boot(&project);
    let child_uri = project.uri("child.gd");
    did_open(&client, &child_uri, CHILD);

    let params = serde_json::json!({
        "textDocument": { "uri": child_uri.as_str() },
        "position": { "line": 0, "character": 14 },
    });
    client
        .sender
        .send(request(15, "textDocument/hover", params))
        .unwrap();
    let resp = recv_response(&client);
    let hover: Option<lsp_types::Hover> =
        serde_json::from_value(resp.result.expect("hover result is always present"))
            .expect("valid Option<Hover>");
    let lsp_types::HoverContents::Markup(markup) = hover.expect("hover resolves").contents else {
        panic!("hover renders markup");
    };
    assert!(
        markup.value.contains("The nested worker Outer owns."),
        "hover must document `Outer.Inner`, got {:?}",
        markup.value
    );
    assert!(
        !markup.value.contains("nothing to do with Outer"),
        "hover must not document the unrelated top-level class, got {:?}",
        markup.value
    );

    shutdown(&client, handle);
}

/// Walking UP from a subclass reaches the inner class, and the blob that item carries expands
/// again — the depth>2 guarantee, now with an inner-class identity riding through the round trip.
#[test]
fn supertypes_cross_the_file_boundary_into_an_inner_class() {
    const GRANDCHILD: &str = "class_name GrandChild\nextends Outer.Inner\n";
    let project = NativeProject::new(&[
        ("outer.gd", OUTER),
        ("unrelated.gd", UNRELATED),
        ("grandchild.gd", GRANDCHILD),
    ]);
    let (client, handle, _) = boot(&project);
    let uri = project.uri("grandchild.gd");
    did_open(&client, &uri, GRANDCHILD);

    // `class_name GrandChild` — the identifier starts at column 11.
    let item = prepare_one(&client, &uri, Position::new(0, 11));
    let parents = supertypes_of(&client, &item).expect("supertypes are an array");
    assert_eq!(parents.len(), 1);
    assert_eq!(parents[0].name, "Inner");
    assert_eq!(parents[0].uri, project.uri("outer.gd"));

    // Expand again from the returned item alone — no cursor involved.
    let grandparents = supertypes_of(&client, &parents[0]).expect("supertypes are an array");
    let names: Vec<&str> = grandparents.iter().map(|i| i.name.as_str()).collect();
    assert_eq!(names, vec!["Node"]);

    shutdown(&client, handle);
}

/// An `extends` suffix that names nothing must not fall back to the bare-name lookup — that is
/// exactly what produced the wrong file. `definition` answers nothing at all; `prepare` keeps its
/// unnamed-script fallback, which names THIS file rather than a base. The one thing neither may do
/// is mention the same-named class in `unrelated.gd`.
#[test]
fn an_unresolvable_extends_suffix_never_reaches_the_global_registry() {
    const BAD_CHILD: &str = "extends Outer.Inner.Missing\n";
    let project = NativeProject::new(&[
        ("outer.gd", OUTER),
        ("unrelated.gd", UNRELATED),
        ("bad_child.gd", BAD_CHILD),
    ]);
    let (client, handle, _) = boot(&project);
    let uri = project.uri("bad_child.gd");
    did_open(&client, &uri, BAD_CHILD);

    // `extends Outer.Inner.Missing` — the `Missing` segment starts at column 20.
    assert_eq!(definition_at(&client, &uri, Position::new(0, 20)), None);
    let item = prepare_one(&client, &uri, Position::new(0, 20));
    assert_eq!(
        item.uri, uri,
        "the unnamed-script fallback names this file, never `unrelated.gd`"
    );

    // The `Inner` segment before it still resolves — a broken tail does not poison the prefix.
    let mid = prepare_one(&client, &uri, Position::new(0, 14));
    assert_eq!(mid.uri, project.uri("outer.gd"));

    shutdown(&client, handle);
}

/// #388: under `extends "res://x.gd".Inner` the parser stores `Inner` at chain position 0, so a
/// cursor on it used to read as a chain HEAD and resolve through the global registry — answering a
/// same-named top-level `class_name Inner` outright rather than the inner class the clause names.
#[test]
fn a_path_extends_segment_names_the_inner_class_not_a_global_of_the_same_name() {
    let project = NativeProject::new(&[
        (
            "pathbase.gd",
            "extends Node\nclass Inner:\n\textends Node\n",
        ),
        ("pathchild.gd", "extends \"res://pathbase.gd\".Inner\n"),
        ("unrelated.gd", "class_name Inner\nextends Node\n"),
    ]);
    let (client, handle, _) = boot(&project);
    let child_uri = project.uri("pathchild.gd");
    let child_src = "extends \"res://pathbase.gd\".Inner\n";
    did_open(&client, &child_uri, child_src);

    // `Inner` starts at column 28: `extends ` is 8, the quoted path runs 8..=26, then the dot.
    let item = prepare_one(&client, &child_uri, Position::new(0, 29));
    assert_eq!(item.name, "Inner");
    assert_eq!(
        item.uri,
        project.uri("pathbase.gd"),
        "the segment names pathbase.gd's inner class, never the unrelated global"
    );
    assert_eq!(
        item.data
            .as_ref()
            .and_then(|d| d.get("inner").cloned())
            .unwrap_or(serde_json::Value::Null),
        serde_json::json!(["Inner"]),
        "the identity is the inner class, not the file's head: {:?}",
        item.data
    );

    shutdown(&client, handle);
}
