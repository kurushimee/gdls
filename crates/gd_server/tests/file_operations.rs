//! M11 (#79) gate: `workspace/willRenameFiles` (the MUTATING reference-rewrite) + the `did*`
//! file-operation index nudges, over the in-memory Connection rig (`tests/rename.rs` style).
//!
//! The discipline under test is the rename-saga bar carried to file operations: **rewrite ONLY a
//! `res://` string literal that is positively a `preload`/`load` ARGUMENT AND resolves to the
//! renamed file; leave everything else (dynamic paths, unrelated literals, same-basename-different-
//! dir, prefix neighbours, AND a `res://` string in NON-load position — a guard/value/display
//! string) untouched; never emit a span that swallows a quote or a new path that breaks the
//! literal.** Coverage:
//!
//!   1. capability gating — advertised iff (per-operation) the client offers `fileOperations.*`.
//!   2. multi-file `.gd` rename — every `preload("res://a.gd")` rewritten; an unrelated
//!      `preload("res://other.gd")` and a dynamic `load(var)` UNTOUCHED. Plus an unopened
//!      (disk-text, `version: None`) referrer + a self-reference, the exact inner-quote span for
//!      both quote styles, and a batch renaming a `.gd` AND a `.tscn` in one request.
//!   3. a `.tscn` move rewrites `.gd` `preload`/`load("res://….tscn")` targeting it.
//!   4. scene-attached `.gd` move → a `window/showMessage(Warning)` naming the scene(s).
//!   5. a `did*` notification nudges the index (no double-processing / no panic; correct end state).
//!   6. fail-closed adversarial — a prefix neighbour (`res://a.gd` vs `res://ab.gd`), a
//!      same-basename-different-dir file, an old==new rename, an unresolvable literal, a new path
//!      with a quote / a `..` segment (write-side corruption guards), and a DIRTY open buffer
//!      (rewritten from buffer text + live version, never stale disk text).
//!   7. write-set scope — a RESOLVING `res://` literal in NON-load position (a `==` guard, a `const`
//!      value, a dict key/value, a bare expression statement) is NOT rewritten (rewriting it would
//!      change behavior); a `ResourceLoader.load(…)` arg IS rewritten while an unrelated
//!      `obj.load(…)` method arg is NOT; and the same literal text as a preload arg vs a const in
//!      one file rewrites ONLY the preload arg (position, not string, is the gate).

mod common;

use std::time::Duration;

use common::{file_uri, notification, recv_response, request, try_recv, TempProject, MINI_API};
use lsp_server::{Connection, Message};
use lsp_types::{
    CreateFilesParams, DeleteFilesParams, DidOpenTextDocumentParams, DocumentChanges, FileCreate,
    FileDelete, FileRename, InitializeParams, InitializeResult, InitializedParams, Range,
    RenameFilesParams, TextDocumentItem, WorkspaceEdit,
};

// ---------------------------------------------------------------------------------------------
// Rig
// ---------------------------------------------------------------------------------------------

fn boot() -> (Connection, std::thread::JoinHandle<anyhow::Result<()>>) {
    let (server, client) = Connection::memory();
    let handle = std::thread::spawn(move || gd_server::serve(server));
    (client, handle)
}

/// `initialize` + `initialized` with the given client caps, then open every requested file.
/// Returns the parsed `InitializeResult` so a test can assert the advertised file-operation caps.
fn init_open(
    project: &TempProject,
    client: &Connection,
    caps: serde_json::Value,
    files: &[&str],
) -> InitializeResult {
    let init = InitializeParams {
        initialization_options: Some(serde_json::json!({
            "projectRoot": project.root.as_str(),
            "autoDumpExtensionApi": false,
            "extensionApiPath": project.root.join("extension_api.json").as_str(),
        })),
        capabilities: serde_json::from_value(caps).expect("client caps"),
        ..Default::default()
    };
    client.sender.send(request(1, "initialize", init)).unwrap();
    let init_resp = recv_response(client);
    let result: InitializeResult =
        serde_json::from_value(init_resp.result.expect("initialize result")).unwrap();
    client
        .sender
        .send(notification("initialized", InitializedParams {}))
        .unwrap();

    for (i, rel) in files.iter().enumerate() {
        let abs = project.root.join(rel);
        let text = std::fs::read_to_string(abs.as_std_path()).expect("read file");
        let uri = file_uri(&abs);
        client
            .sender
            .send(notification(
                "textDocument/didOpen",
                DidOpenTextDocumentParams {
                    text_document: TextDocumentItem {
                        uri,
                        language_id: "gdscript".to_string(),
                        version: 1 + i as i32,
                        text,
                    },
                },
            ))
            .unwrap();
    }
    // Drain the publishDiagnostics pushes the opens trigger.
    while try_recv(client, Duration::from_millis(300)).is_some() {}
    result
}

fn shutdown(client: &Connection, thread: std::thread::JoinHandle<anyhow::Result<()>>) {
    client
        .sender
        .send(request(99, "shutdown", serde_json::Value::Null))
        .unwrap();
    let _ = recv_response(client);
    client
        .sender
        .send(notification("exit", serde_json::Value::Null))
        .unwrap();
    thread
        .join()
        .expect("server thread panicked")
        .expect("serve() returned an error");
}

/// Caps advertising willRename + the three did* + documentChanges (the rich path).
fn caps_full() -> serde_json::Value {
    serde_json::json!({
        "workspace": {
            "fileOperations": {
                "willRename": true,
                "didRename": true,
                "didCreate": true,
                "didDelete": true,
            },
            "workspaceEdit": { "documentChanges": true },
        }
    })
}

/// A minimal project: `project.godot` + a native dump. No scripts (each test writes its own).
fn bare_project() -> TempProject {
    let p = TempProject::new();
    p.write(
        "project.godot",
        "config_version=5\n\n[application]\n\nconfig/name=\"Test\"\nconfig/features=PackedStringArray(\"4.6\")\n",
    );
    p.write("extension_api.json", MINI_API);
    p
}

/// Send `workspace/willRenameFiles` for one `old → new` move and return the response result.
fn will_rename(
    client: &Connection,
    id: i32,
    project: &TempProject,
    old_rel: &str,
    new_rel: &str,
) -> serde_json::Value {
    will_rename_batch(client, id, project, &[(old_rel, new_rel)])
}

/// Send `workspace/willRenameFiles` for a BATCH of `(old, new)` moves in one request.
fn will_rename_batch(
    client: &Connection,
    id: i32,
    project: &TempProject,
    moves: &[(&str, &str)],
) -> serde_json::Value {
    let params = RenameFilesParams {
        files: moves
            .iter()
            .map(|(old_rel, new_rel)| FileRename {
                old_uri: file_uri(&project.root.join(old_rel)).as_str().to_string(),
                new_uri: file_uri(&project.root.join(new_rel)).as_str().to_string(),
            })
            .collect(),
    };
    client
        .sender
        .send(request(id, "workspace/willRenameFiles", params))
        .unwrap();
    let resp = loop {
        let r = recv_response(client);
        if r.id == lsp_server::RequestId::from(id) {
            break r;
        }
    };
    assert!(resp.error.is_none(), "willRenameFiles errored: {resp:?}");
    resp.result.unwrap_or(serde_json::Value::Null)
}

fn cmp_range(a: &Range, b: &Range) -> std::cmp::Ordering {
    (a.start.line, a.start.character, a.end.line, a.end.character).cmp(&(
        b.start.line,
        b.start.character,
        b.end.line,
        b.end.character,
    ))
}

/// Flatten a `WorkspaceEdit` (either shape) into sorted (uri, range, new_text) triples + per-uri
/// versions (documentChanges shape only).
struct EditView {
    edits: Vec<(String, Range, String)>,
    versions: Vec<(String, Option<i32>)>,
}

fn flatten_edit(value: serde_json::Value) -> EditView {
    let edit: WorkspaceEdit = serde_json::from_value(value).expect("a WorkspaceEdit");
    let mut edits: Vec<(String, Range, String)> = Vec::new();
    let mut versions: Vec<(String, Option<i32>)> = Vec::new();
    match (&edit.document_changes, &edit.changes) {
        (Some(DocumentChanges::Edits(tde)), None) => {
            for e in tde {
                versions.push((
                    e.text_document.uri.as_str().to_string(),
                    e.text_document.version,
                ));
                for oneof in &e.edits {
                    if let lsp_types::OneOf::Left(te) = oneof {
                        edits.push((
                            e.text_document.uri.as_str().to_string(),
                            te.range,
                            te.new_text.clone(),
                        ));
                    } else {
                        panic!("annotated edit not expected");
                    }
                }
            }
        }
        (None, Some(changes)) => {
            for (uri, tes) in changes {
                for te in tes {
                    edits.push((uri.as_str().to_string(), te.range, te.new_text.clone()));
                }
            }
        }
        other => panic!("exactly one WorkspaceEdit field must be populated, got {other:?}"),
    }
    edits.sort_by(|a, b| a.0.cmp(&b.0).then(cmp_range(&a.1, &b.1)));
    EditView { edits, versions }
}

/// Apply a single-file `TextEdit` (LSP ranges, UTF-16 default — fine for ASCII test fixtures) to a
/// source string and return the result. Proves an emitted edit produces valid post-rename source.
fn apply_edit_to(source: &str, range: &Range, new_text: &str) -> String {
    let lines: Vec<&str> = source.split('\n').collect();
    let mut out = String::new();
    let start_off = byte_off(&lines, range.start.line, range.start.character);
    let end_off = byte_off(&lines, range.end.line, range.end.character);
    out.push_str(&source[..start_off]);
    out.push_str(new_text);
    out.push_str(&source[end_off..]);
    out
}

fn byte_off(lines: &[&str], line: u32, character: u32) -> usize {
    let mut off = 0usize;
    for l in lines.iter().take(line as usize) {
        off += l.len() + 1; // +1 for the '\n' that split removed
    }
    off + character as usize
}

// ---------------------------------------------------------------------------------------------
// 1. Capability gating
// ---------------------------------------------------------------------------------------------

/// `workspace.fileOperations.willRename` is advertised iff the client offered it; the per-operation
/// flags map 1:1. A client offering nothing gets no `fileOperations` block at all.
#[test]
fn capability_advertised_only_when_offered() {
    // Offered → advertised, with `**/*.gd` + `**/*.tscn` filters.
    let p = bare_project();
    let (client, thread) = boot();
    let result = init_open(&p, &client, caps_full(), &[]);
    let fo = result
        .capabilities
        .workspace
        .as_ref()
        .and_then(|w| w.file_operations.as_ref())
        .expect("fileOperations advertised when offered");
    let will = fo
        .will_rename
        .as_ref()
        .expect("willRename advertised when offered");
    let globs: Vec<&str> = will
        .filters
        .iter()
        .map(|f| f.pattern.glob.as_str())
        .collect();
    assert_eq!(globs, vec!["**/*.gd", "**/*.tscn"]);
    assert!(fo.did_rename.is_some());
    assert!(fo.did_create.is_some());
    assert!(fo.did_delete.is_some());
    // We never advertise willCreate / willDelete (no edit to contribute).
    assert!(fo.will_create.is_none());
    assert!(fo.will_delete.is_none());
    shutdown(&client, thread);

    // Not offered → no fileOperations block at all (and so the handler is dead, by construction).
    let p2 = bare_project();
    let (client2, thread2) = boot();
    let result2 = init_open(&p2, &client2, serde_json::json!({}), &[]);
    assert!(
        result2
            .capabilities
            .workspace
            .as_ref()
            .and_then(|w| w.file_operations.as_ref())
            .is_none(),
        "no fileOperations advertised when the client offered none"
    );
    shutdown(&client2, thread2);

    // Offered ONLY didCreate → only that op advertised; willRename absent.
    let p3 = bare_project();
    let (client3, thread3) = boot();
    let result3 = init_open(
        &p3,
        &client3,
        serde_json::json!({ "workspace": { "fileOperations": { "didCreate": true } } }),
        &[],
    );
    let fo3 = result3
        .capabilities
        .workspace
        .as_ref()
        .and_then(|w| w.file_operations.as_ref())
        .expect("fileOperations advertised (didCreate offered)");
    assert!(fo3.did_create.is_some());
    assert!(
        fo3.will_rename.is_none(),
        "willRename not offered → not advertised"
    );
    shutdown(&client3, thread3);
}

// ---------------------------------------------------------------------------------------------
// 2. Multi-file rename: every resolving literal rewritten; unrelated + dynamic untouched
// ---------------------------------------------------------------------------------------------

/// Renaming `a.gd` (preloaded by b.gd AND c.gd) rewrites EVERY `preload("res://a.gd")` literal to
/// the new path. An unrelated `preload("res://other.gd")` and a dynamic `load(var)` are UNTOUCHED.
/// Each emitted edit, applied to its source, yields exactly the new `res://` path inside the quotes.
#[test]
fn multi_file_rewrite_only_resolving_literals() {
    let p = bare_project();
    p.write("a.gd", "extends Node\n");
    p.write("other.gd", "extends Node\n");
    // b.gd preloads a.gd (target) AND other.gd (unrelated).
    let b_src =
        "extends Node\nconst A = preload(\"res://a.gd\")\nconst O = preload(\"res://other.gd\")\n";
    p.write("b.gd", b_src);
    // c.gd preloads a.gd via `load`, and has a DYNAMIC load that must never be touched.
    let c_src = "extends Node\nvar n := \"a.gd\"\nfunc f():\n\tvar x = load(\"res://a.gd\")\n\tvar y = load(\"res://\" + n)\n";
    p.write("c.gd", c_src);

    let (client, thread) = boot();
    init_open(
        &p,
        &client,
        caps_full(),
        &["a.gd", "other.gd", "b.gd", "c.gd"],
    );

    let result = will_rename(&client, 10, &p, "a.gd", "renamed/a2.gd");
    let view = flatten_edit(result);

    let b_uri = file_uri(&p.root.join("b.gd")).as_str().to_string();
    let c_uri = file_uri(&p.root.join("c.gd")).as_str().to_string();

    // Exactly two edits: the a.gd literal in b.gd and the a.gd literal in c.gd. NOT other.gd, NOT
    // the dynamic load.
    assert_eq!(
        view.edits.len(),
        2,
        "exactly the two literals resolving to a.gd are rewritten; got {:?}",
        view.edits
    );
    let uris: Vec<&str> = view.edits.iter().map(|(u, _, _)| u.as_str()).collect();
    assert!(uris.contains(&b_uri.as_str()));
    assert!(uris.contains(&c_uri.as_str()));
    // Every rewrite targets the new res path.
    for (_, _, nt) in &view.edits {
        assert_eq!(nt, "res://renamed/a2.gd");
    }

    // Apply each edit and confirm: only the path inside the quotes changed; the quotes survive; the
    // unrelated literal + dynamic load are byte-identical.
    let b_edit = view.edits.iter().find(|(u, _, _)| u == &b_uri).unwrap();
    let b_after = apply_edit_to(b_src, &b_edit.1, &b_edit.2);
    assert!(b_after.contains("preload(\"res://renamed/a2.gd\")"));
    assert!(
        b_after.contains("preload(\"res://other.gd\")"),
        "unrelated literal must be byte-identical: {b_after}"
    );

    let c_edit = view.edits.iter().find(|(u, _, _)| u == &c_uri).unwrap();
    let c_after = apply_edit_to(c_src, &c_edit.1, &c_edit.2);
    assert!(c_after.contains("load(\"res://renamed/a2.gd\")"));
    assert!(
        c_after.contains("load(\"res://\" + n)"),
        "dynamic load must be byte-identical: {c_after}"
    );

    // documentChanges shape carries the open buffers' live versions.
    assert!(view
        .versions
        .iter()
        .any(|(u, v)| u == &b_uri && v.is_some()));
    assert!(view
        .versions
        .iter()
        .any(|(u, v)| u == &c_uri && v.is_some()));

    shutdown(&client, thread);
}

/// #132 (const indirection): `const P := "res://a.gd"` consumed by `load(P)` in the same file is a
/// load reference one hop away — the const's own literal is rewritten. The proof is positive: the
/// name is declared exactly once in the file AND a load consumes it. A `res://` const that NO load
/// consumes stays untouched (the module's "a value is never rewritten" rule).
#[test]
fn const_indirection_rewritten_only_when_a_load_consumes_it() {
    let p = bare_project();
    p.write("a.gd", "extends Node\n");
    // b.gd: the const IS consumed by `load` → rewritten. The unconsumed const of the same path is
    // in another file so the two cases can't share a literal.
    let b_src =
        "extends Node\nconst P := \"res://a.gd\"\nfunc f():\n\tvar x = load(P)\n\tprint(x)\n";
    p.write("b.gd", b_src);
    // c.gd: a `res://a.gd` const used only as a VALUE (compared, printed) → never rewritten.
    let c_src =
        "extends Node\nconst Q := \"res://a.gd\"\nfunc f(s: String) -> bool:\n\treturn s == Q\n";
    p.write("c.gd", c_src);

    let (client, thread) = boot();
    init_open(&p, &client, caps_full(), &["a.gd", "b.gd", "c.gd"]);
    let view = flatten_edit(will_rename(&client, 10, &p, "a.gd", "renamed/a2.gd"));

    let b_uri = file_uri(&p.root.join("b.gd")).as_str().to_string();
    let c_uri = file_uri(&p.root.join("c.gd")).as_str().to_string();
    assert!(
        view.edits.iter().all(|(u, _, _)| u != &c_uri),
        "a `res://` const no load consumes is a VALUE and must never be rewritten; got {:?}",
        view.edits
    );
    let b_edit = view
        .edits
        .iter()
        .find(|(u, _, _)| u == &b_uri)
        .unwrap_or_else(|| {
            panic!(
                "the load-consumed const must be rewritten; got {:?}",
                view.edits
            )
        });
    let b_after = apply_edit_to(b_src, &b_edit.1, &b_edit.2);
    assert!(
        b_after.contains("const P := \"res://renamed/a2.gd\""),
        "the const literal follows the move: {b_after}"
    );
    shutdown(&client, thread);
}

/// #132 (const indirection, fail-closed): a const whose name is declared MORE THAN ONCE in the file
/// (a same-named local/parameter shadows it somewhere) cannot be trusted to mean the same thing at
/// the load site, so it is NOT rewritten — a missed rewrite, never a wrong one.
#[test]
fn const_indirection_refused_when_the_name_is_declared_twice() {
    let p = bare_project();
    p.write("a.gd", "extends Node\n");
    let b_src = "extends Node\nconst P := \"res://a.gd\"\nfunc f():\n\tvar P := \"res://other.gd\"\n\tvar x = load(P)\n\tprint(x)\n";
    p.write("b.gd", b_src);

    let (client, thread) = boot();
    init_open(&p, &client, caps_full(), &["a.gd", "b.gd"]);
    let result = will_rename(&client, 10, &p, "a.gd", "renamed/a2.gd");
    assert!(
        result.is_null(),
        "a shadowed const name must not be rewritten (fail-closed) — nothing else references \
         a.gd, so the whole edit is null; got {result:?}"
    );
    shutdown(&client, thread);
}

/// #132 (`load` shadow): a file that declares its own `func load` is not calling the `@GlobalScope`
/// utility, so its bare `load("res://…")` argument must NOT be rewritten — that literal belongs to a
/// user method whose meaning gdls cannot know. `preload` (a dedicated AST node, unshadowable) in the
/// same file is still rewritten.
#[test]
fn bare_load_skipped_in_a_file_that_declares_its_own_load() {
    let p = bare_project();
    p.write("a.gd", "extends Node\n");
    let b_src = "extends Node\nconst A = preload(\"res://a.gd\")\nfunc load(path: String) -> String:\n\treturn path\nfunc f():\n\tprint(load(\"res://a.gd\"))\n";
    p.write("b.gd", b_src);

    let (client, thread) = boot();
    init_open(&p, &client, caps_full(), &["a.gd", "b.gd"]);
    let view = flatten_edit(will_rename(&client, 10, &p, "a.gd", "renamed/a2.gd"));

    assert_eq!(
        view.edits.len(),
        1,
        "only the `preload` literal may be rewritten while `load` is shadowed; got {:?}",
        view.edits
    );
    let after = apply_edit_to(b_src, &view.edits[0].1, &view.edits[0].2);
    assert!(
        after.contains("preload(\"res://renamed/a2.gd\")"),
        "the preload literal follows the move: {after}"
    );
    assert!(
        after.contains("print(load(\"res://a.gd\"))"),
        "the shadowed `load` call's argument must be byte-identical: {after}"
    );
    shutdown(&client, thread);
}

/// #132 (other `ResourceLoader` forms): `ResourceLoader["load"](…)` (the same call by string index)
/// and `ResourceLoader.load_threaded_request(…)` take their path in the same first-argument position
/// and are rewritten. `other_obj.load(…)` still is not — the base name is matched precisely.
#[test]
fn resource_loader_index_and_threaded_forms_are_rewritten() {
    let p = bare_project();
    p.write("a.gd", "extends Node\n");
    let b_src = "extends Node\nvar other_obj\nfunc f():\n\tResourceLoader[\"load\"](\"res://a.gd\")\n\tResourceLoader.load_threaded_request(\"res://a.gd\")\n\tother_obj.load(\"res://a.gd\")\n";
    p.write("b.gd", b_src);

    let (client, thread) = boot();
    init_open(&p, &client, caps_full(), &["a.gd", "b.gd"]);
    let view = flatten_edit(will_rename(&client, 10, &p, "a.gd", "renamed/a2.gd"));

    assert_eq!(
        view.edits.len(),
        2,
        "the index form and the threaded request are rewritten, the foreign `.load` is not; got {:?}",
        view.edits
    );
    // Apply from the LAST range backwards so earlier ranges stay valid.
    let mut edits = view.edits.clone();
    edits.sort_by(|a, b| cmp_range(&b.1, &a.1));
    let mut after = b_src.to_string();
    for (_, range, new_text) in &edits {
        after = apply_edit_to(&after, range, new_text);
    }
    assert!(
        after.contains("ResourceLoader[\"load\"](\"res://renamed/a2.gd\")"),
        "the string-index form follows the move: {after}"
    );
    assert!(
        after.contains("load_threaded_request(\"res://renamed/a2.gd\")"),
        "the threaded request follows the move: {after}"
    );
    assert!(
        after.contains("other_obj.load(\"res://a.gd\")"),
        "a foreign object's `.load` must be byte-identical: {after}"
    );
    shutdown(&client, thread);
}

/// A referencing file that is NOT open is rewritten from its DISK text, and its `TextDocumentEdit`
/// carries version `None` (the "content on disk is master" case) — the span and the `None` version
/// describe the same disk bytes. Also covers a SELF-reference: `a.gd` preloading itself is rewritten
/// to the new path (after the move, its own `preload` must point at the new location).
#[test]
fn unopened_file_uses_disk_text_and_null_version() {
    let p = bare_project();
    // a.gd self-references — it must be rewritten too (self-preload follows the move).
    let a_src = "extends Node\nconst SELF = preload(\"res://a.gd\")\n";
    p.write("a.gd", a_src);
    // b.gd references a.gd but is NEVER opened — the disk branch.
    let b_src = "extends Node\nconst A = preload(\"res://a.gd\")\n";
    p.write("b.gd", b_src);

    let (client, thread) = boot();
    // Open ONLY a.gd; b.gd stays on disk.
    init_open(&p, &client, caps_full(), &["a.gd"]);

    let result = will_rename(&client, 10, &p, "a.gd", "a2.gd");
    let view = flatten_edit(result);

    let a_uri = file_uri(&p.root.join("a.gd")).as_str().to_string();
    let b_uri = file_uri(&p.root.join("b.gd")).as_str().to_string();

    // Both the open self-reference and the unopened b.gd reference are rewritten.
    assert_eq!(
        view.edits.len(),
        2,
        "self-ref + unopened ref; got {:?}",
        view.edits
    );
    // a.gd is open → its edit carries a concrete version; b.gd is unopened → version None.
    let a_ver = view.versions.iter().find(|(u, _)| u == &a_uri).unwrap().1;
    let b_ver = view.versions.iter().find(|(u, _)| u == &b_uri).unwrap().1;
    assert!(a_ver.is_some(), "open buffer carries its live version");
    assert!(
        b_ver.is_none(),
        "unopened file carries version None (disk is master)"
    );

    // The disk-text edit, applied to b.gd's disk bytes, is clean.
    let b_edit = view.edits.iter().find(|(u, _, _)| u == &b_uri).unwrap();
    let b_after = apply_edit_to(b_src, &b_edit.1, &b_edit.2);
    assert!(b_after.contains("preload(\"res://a2.gd\")"));
    // The self-reference edit, applied to a.gd, is clean.
    let a_edit = view.edits.iter().find(|(u, _, _)| u == &a_uri).unwrap();
    let a_after = apply_edit_to(a_src, &a_edit.1, &a_edit.2);
    assert!(a_after.contains("preload(\"res://a2.gd\")"));

    shutdown(&client, thread);
}

/// The edit range covers EXACTLY the path between the quotes — never the quotes themselves — for
/// both double- and single-quoted literals. This is the corruption-prone detail: an off-by-one that
/// swallowed a quote would emit `res://a2.gd"` (broken) or strip the opening quote.
#[test]
fn edit_span_covers_path_inside_quotes_only() {
    let p = bare_project();
    p.write("a.gd", "extends Node\n");
    // Double-quoted on line 1, single-quoted on line 2 (GDScript accepts both).
    let src = "extends Node\nconst D = preload(\"res://a.gd\")\nconst S = preload('res://a.gd')\n";
    p.write("user.gd", src);

    let (client, thread) = boot();
    init_open(&p, &client, caps_full(), &["a.gd", "user.gd"]);

    let result = will_rename(&client, 10, &p, "a.gd", "a2.gd");
    let view = flatten_edit(result);
    assert_eq!(
        view.edits.len(),
        2,
        "both quoting styles rewritten; got {:?}",
        view.edits
    );

    // `const D = preload("res://a.gd")` — the opening quote is at col 18, so the inner path starts at
    // col 19; `res://a.gd` is 10 chars → ends at col 29 (the closing quote is at col 29, excluded).
    let line1 = view
        .edits
        .iter()
        .find(|(_, r, _)| r.start.line == 1)
        .expect("an edit on line 1");
    assert_eq!(
        line1.1.start.character, 19,
        "starts after the opening quote"
    );
    assert_eq!(line1.1.end.character, 29, "ends before the closing quote");

    // Same column geometry for the single-quoted line 2.
    let line2 = view
        .edits
        .iter()
        .find(|(_, r, _)| r.start.line == 2)
        .expect("an edit on line 2");
    assert_eq!(line2.1.start.character, 19);
    assert_eq!(line2.1.end.character, 29);

    // Apply both and confirm the quotes survive on both lines.
    let mut after = src.to_string();
    let mut sorted = view.edits.clone();
    sorted.sort_by(|a, b| cmp_range(&b.1, &a.1));
    for (_, range, nt) in &sorted {
        after = apply_edit_to(&after, range, nt);
    }
    assert!(
        after.contains("preload(\"res://a2.gd\")"),
        "double quotes survive: {after}"
    );
    assert!(
        after.contains("preload('res://a2.gd')"),
        "single quotes survive: {after}"
    );

    shutdown(&client, thread);
}

// ---------------------------------------------------------------------------------------------
// 3. .tscn move rewrites .gd preload/load of it
// ---------------------------------------------------------------------------------------------

/// Moving a `.tscn` rewrites a `.gd`'s `preload`/`load("res://….tscn")` literal that targets it
/// (same mechanism — the literal resolves to the moved on-disk resource).
#[test]
fn tscn_move_rewrites_gd_literal() {
    let p = bare_project();
    p.write(
        "main.tscn",
        "[gd_scene format=3]\n[node name=\"Root\" type=\"Node\"]\n",
    );
    let user_src = "extends Node\nconst S = preload(\"res://main.tscn\")\nfunc f():\n\tvar d = load(\"res://main.tscn\")\n";
    p.write("user.gd", user_src);

    let (client, thread) = boot();
    init_open(&p, &client, caps_full(), &["user.gd"]);

    let result = will_rename(&client, 10, &p, "main.tscn", "scenes/main.tscn");
    let view = flatten_edit(result);

    assert_eq!(
        view.edits.len(),
        2,
        "both the preload and the load of main.tscn are rewritten; got {:?}",
        view.edits
    );
    for (_, _, nt) in &view.edits {
        assert_eq!(nt, "res://scenes/main.tscn");
    }
    // Apply both edits (right-to-left so earlier offsets stay valid) and confirm a clean result.
    let mut after = user_src.to_string();
    let mut sorted = view.edits.clone();
    sorted.sort_by(|a, b| cmp_range(&b.1, &a.1)); // descending
    for (_, range, nt) in &sorted {
        after = apply_edit_to(&after, range, nt);
    }
    assert!(after.contains("preload(\"res://scenes/main.tscn\")"));
    assert!(after.contains("load(\"res://scenes/main.tscn\")"));
    assert!(!after.contains("\"res://main.tscn\""));

    shutdown(&client, thread);
}

/// A BATCH renaming a `.gd` AND a `.tscn` in ONE request rewrites both sets of references; each
/// literal is repointed to its own new path (the targets don't cross-contaminate).
#[test]
fn batch_rename_gd_and_tscn_together() {
    let p = bare_project();
    p.write("a.gd", "extends Node\n");
    p.write(
        "s.tscn",
        "[gd_scene format=3]\n[node name=\"R\" type=\"Node\"]\n",
    );
    let user_src =
        "extends Node\nconst A = preload(\"res://a.gd\")\nconst S = preload(\"res://s.tscn\")\n";
    p.write("user.gd", user_src);

    let (client, thread) = boot();
    init_open(&p, &client, caps_full(), &["a.gd", "user.gd"]);

    // One request, two moves.
    let result = will_rename_batch(
        &client,
        10,
        &p,
        &[("a.gd", "dir/a2.gd"), ("s.tscn", "scenes/s2.tscn")],
    );
    let view = flatten_edit(result);
    assert_eq!(
        view.edits.len(),
        2,
        "both refs rewritten; got {:?}",
        view.edits
    );

    let new_texts: Vec<&str> = view.edits.iter().map(|(_, _, t)| t.as_str()).collect();
    assert!(
        new_texts.contains(&"res://dir/a2.gd"),
        "the .gd ref → its new path"
    );
    assert!(
        new_texts.contains(&"res://scenes/s2.tscn"),
        "the .tscn ref → its new path"
    );

    // Apply both (descending offset order) → clean source, both literals intact.
    let mut after = user_src.to_string();
    let mut sorted = view.edits.clone();
    sorted.sort_by(|a, b| cmp_range(&b.1, &a.1));
    for (_, range, nt) in &sorted {
        after = apply_edit_to(&after, range, nt);
    }
    assert!(after.contains("preload(\"res://dir/a2.gd\")"));
    assert!(after.contains("preload(\"res://scenes/s2.tscn\")"));

    shutdown(&client, thread);
}

// ---------------------------------------------------------------------------------------------
// 4. Scene-attached .gd move → .tscn rewritten, NO dangling warning (#131)
// ---------------------------------------------------------------------------------------------

/// Moving a `.gd` that a scene attaches as its `ext_resource` Script to a SAFE new path now rewrites
/// the scene's `ext_resource` (the second mutating surface, #131) instead of warning — so NO dangling
/// `window/showMessage(Warning)` for that scene fires (it is no longer dangling). The rewrite content
/// itself is asserted by `scene_attached_script_move_rewrites_tscn_ext_resource`; this pins the
/// warning-suppression half: a successfully-rewritten scene must not also be reported as dangling.
#[test]
fn scene_attached_script_safe_move_rewrites_without_dangling_warning() {
    let p = bare_project();
    p.write("player.gd", "extends Node\n");
    p.write(
        "player.tscn",
        "[gd_scene format=3]\n[ext_resource type=\"Script\" path=\"res://player.gd\" id=\"1\"]\n[node name=\"Player\" type=\"Node\"]\nscript = ExtResource(\"1\")\n",
    );

    let (client, thread) = boot();
    init_open(&p, &client, caps_full(), &[]);

    // Drain any stray messages first, then trigger the move.
    while try_recv(&client, Duration::from_millis(200)).is_some() {}
    let params = RenameFilesParams {
        files: vec![FileRename {
            old_uri: file_uri(&p.root.join("player.gd")).as_str().to_string(),
            new_uri: file_uri(&p.root.join("nodes/player.gd"))
                .as_str()
                .to_string(),
        }],
    };
    client
        .sender
        .send(request(10, "workspace/willRenameFiles", params))
        .unwrap();

    // No dangling `window/showMessage(Warning)` for player.tscn may arrive (it was rewritten), and
    // the response must carry a .tscn edit.
    let mut dangling_warned = false;
    let mut got_tscn_edit = false;
    let tscn_uri = file_uri(&p.root.join("player.tscn")).as_str().to_string();
    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    while std::time::Instant::now() < deadline {
        match try_recv(&client, Duration::from_millis(200)) {
            Some(Message::Notification(n)) if n.method == "window/showMessage" => {
                let msg = n.params["message"].as_str().unwrap_or("");
                if msg.contains("res://player.tscn") && msg.contains("dangling") {
                    dangling_warned = true;
                }
            }
            Some(Message::Response(r)) if r.id == lsp_server::RequestId::from(10) => {
                let view = flatten_edit(r.result.unwrap_or(serde_json::Value::Null));
                got_tscn_edit = view.edits.iter().any(|(u, _, _)| u == &tscn_uri);
                break;
            }
            _ => continue,
        }
    }
    assert!(
        got_tscn_edit,
        "a safe scene-attached .gd move must rewrite the scene's ext_resource"
    );
    assert!(
        !dangling_warned,
        "a successfully-rewritten scene must NOT also be reported as dangling"
    );

    shutdown(&client, thread);
}

// ---------------------------------------------------------------------------------------------
// 5. did* nudges the index (no double-processing / no panic; correct end state)
// ---------------------------------------------------------------------------------------------

/// A `didCreateFiles` for a new `class_name` resolves it project-wide; a `didDeleteFiles` removes
/// it. Delivering the same create twice does not panic / corrupt — the second is a no-op (the
/// content-fingerprint gate). The observable end state is correct after each.
#[test]
fn did_file_ops_nudge_index() {
    let p = bare_project();
    let (client, thread) = boot();
    init_open(&p, &client, caps_full(), &[]);

    // Open a probe that extends a not-yet-existing class — its diagnostics are the signal.
    let probe_uri = file_uri(&p.root.join("probe.gd"));
    p.write("probe.gd", "extends Fresh\n");
    client
        .sender
        .send(notification(
            "textDocument/didOpen",
            DidOpenTextDocumentParams {
                text_document: TextDocumentItem {
                    uri: probe_uri.clone(),
                    language_id: "gdscript".to_string(),
                    version: 1,
                    text: "extends Fresh\n".to_string(),
                },
            },
        ))
        .unwrap();
    let first = next_diagnostics(&client);
    assert!(serde_json::to_string(&first.params)
        .unwrap()
        .contains("Fresh"));

    // didCreateFiles: write the class, nudge via the client op.
    p.write("fresh.gd", "class_name Fresh\nextends Node\n");
    let fresh_uri = file_uri(&p.root.join("fresh.gd")).as_str().to_string();
    client
        .sender
        .send(notification(
            "workspace/didCreateFiles",
            CreateFilesParams {
                files: vec![FileCreate {
                    uri: fresh_uri.clone(),
                }],
            },
        ))
        .unwrap();
    // The probe republishes with its inheritance error cleared.
    loop {
        let n = next_diagnostics(&client);
        if n.params["diagnostics"].as_array().unwrap().is_empty() {
            break;
        }
    }

    // DUPLICATE didCreateFiles for the same file — must NOT double-process (no extra republish, no
    // panic). The content-fingerprint gate swallows it.
    client
        .sender
        .send(notification(
            "workspace/didCreateFiles",
            CreateFilesParams {
                files: vec![FileCreate { uri: fresh_uri }],
            },
        ))
        .unwrap();
    let stray = try_recv(&client, Duration::from_millis(400));
    assert!(
        !matches!(&stray, Some(Message::Notification(n)) if n.method == "textDocument/publishDiagnostics"),
        "duplicate didCreateFiles must not double-process; got {stray:?}"
    );

    // didDeleteFiles: remove the class, nudge — the probe re-breaks.
    p.remove("fresh.gd");
    client
        .sender
        .send(notification(
            "workspace/didDeleteFiles",
            DeleteFilesParams {
                files: vec![FileDelete {
                    uri: file_uri(&p.root.join("fresh.gd")).as_str().to_string(),
                }],
            },
        ))
        .unwrap();
    let rebroken = next_diagnostics(&client);
    assert!(
        !rebroken.params["diagnostics"]
            .as_array()
            .unwrap()
            .is_empty(),
        "deleting Fresh via a client op must re-break the probe"
    );

    shutdown(&client, thread);
}

/// Receive messages until a `textDocument/publishDiagnostics` notification arrives, skipping any
/// other messages in between (the did* test interleaves diagnostics with other traffic).
fn next_diagnostics(client: &Connection) -> lsp_server::Notification {
    loop {
        if let Message::Notification(n) = common::recv(client) {
            if n.method == "textDocument/publishDiagnostics" {
                return n;
            }
        }
    }
}

// ---------------------------------------------------------------------------------------------
// 6. Fail-closed adversarial
// ---------------------------------------------------------------------------------------------

/// A prefix neighbour (`res://a.gd` vs `res://ab.gd`) and a same-basename-different-dir file are
/// NOT wrongly rewritten when `a.gd` is renamed: identity matching, not string matching.
#[test]
fn fail_closed_prefix_and_basename_collisions() {
    let p = bare_project();
    p.write("a.gd", "extends Node\n");
    p.write("ab.gd", "extends Node\n"); // prefix neighbour — must NOT match a.gd
    p.write("sub/a.gd", "extends Node\n"); // same basename, different dir — must NOT match
                                           // user.gd references all three by their distinct res paths.
    let user_src = "extends Node\nconst A = preload(\"res://a.gd\")\nconst AB = preload(\"res://ab.gd\")\nconst SUB = preload(\"res://sub/a.gd\")\n";
    p.write("user.gd", user_src);

    let (client, thread) = boot();
    init_open(
        &p,
        &client,
        caps_full(),
        &["a.gd", "ab.gd", "sub/a.gd", "user.gd"],
    );

    // Rename the top-level a.gd. ONLY `res://a.gd` must be rewritten — not ab.gd, not sub/a.gd.
    let result = will_rename(&client, 10, &p, "a.gd", "a_renamed.gd");
    let view = flatten_edit(result);

    assert_eq!(
        view.edits.len(),
        1,
        "only the exact res://a.gd literal is rewritten (not ab.gd / sub/a.gd); got {:?}",
        view.edits
    );
    assert_eq!(view.edits[0].2, "res://a_renamed.gd");
    // Apply it and confirm the neighbours survived verbatim.
    let after = apply_edit_to(user_src, &view.edits[0].1, &view.edits[0].2);
    assert!(after.contains("preload(\"res://a_renamed.gd\")"));
    assert!(
        after.contains("preload(\"res://ab.gd\")"),
        "prefix neighbour untouched: {after}"
    );
    assert!(
        after.contains("preload(\"res://sub/a.gd\")"),
        "same-basename-different-dir untouched: {after}"
    );

    shutdown(&client, thread);
}

/// An old==new rename (a no-op the client shouldn't send, but be defensive) produces no edit
/// (`null`), and a literal that resolves to NOTHING (a missing `res://` target) is never rewritten.
#[test]
fn fail_closed_noop_and_unresolvable() {
    let p = bare_project();
    p.write("a.gd", "extends Node\n");
    // user.gd references a.gd (resolvable) AND a missing target (unresolvable — must stay untouched).
    let user_src = "extends Node\nconst A = preload(\"res://a.gd\")\nconst M = preload(\"res://missing.gd\")\n";
    p.write("user.gd", user_src);

    let (client, thread) = boot();
    init_open(&p, &client, caps_full(), &["a.gd", "user.gd"]);

    // old == new → null result, no edits.
    let noop = will_rename(&client, 10, &p, "a.gd", "a.gd");
    assert!(
        noop.is_null(),
        "an old==new rename must yield null (no edits); got {noop:?}"
    );

    // Renaming `missing.gd` (which doesn't exist) → no literal resolves to it → null.
    let missing = will_rename(&client, 11, &p, "missing.gd", "elsewhere.gd");
    assert!(
        missing.is_null(),
        "renaming a non-existent file rewrites nothing; got {missing:?}"
    );

    // Renaming a.gd → only res://a.gd rewritten; the unresolvable res://missing.gd untouched.
    let real = will_rename(&client, 12, &p, "a.gd", "a2.gd");
    let view = flatten_edit(real);
    assert_eq!(
        view.edits.len(),
        1,
        "only the resolvable literal; got {:?}",
        view.edits
    );
    let after = apply_edit_to(user_src, &view.edits[0].1, &view.edits[0].2);
    assert!(after.contains("preload(\"res://a2.gd\")"));
    assert!(
        after.contains("preload(\"res://missing.gd\")"),
        "unresolvable literal untouched: {after}"
    );

    shutdown(&client, thread);
}

/// The legacy `changes`-map shape (a client WITHOUT `workspace.workspaceEdit.documentChanges`) is
/// emitted when documentChanges isn't advertised — the same fail-closed write-set, different shape.
#[test]
fn legacy_changes_shape_without_document_changes() {
    let p = bare_project();
    p.write("a.gd", "extends Node\n");
    let b_src = "extends Node\nconst A = preload(\"res://a.gd\")\n";
    p.write("b.gd", b_src);

    let (client, thread) = boot();
    // willRename offered, but documentChanges NOT.
    init_open(
        &p,
        &client,
        serde_json::json!({ "workspace": { "fileOperations": { "willRename": true } } }),
        &["a.gd", "b.gd"],
    );

    let result = will_rename(&client, 10, &p, "a.gd", "a2.gd");
    let edit: WorkspaceEdit = serde_json::from_value(result).expect("a WorkspaceEdit");
    assert!(
        edit.changes.is_some() && edit.document_changes.is_none(),
        "a client without documentChanges gets the legacy changes map"
    );
    let view = flatten_edit(serde_json::to_value(&edit).unwrap());
    assert_eq!(view.edits.len(), 1);
    assert_eq!(view.edits[0].2, "res://a2.gd");

    shutdown(&client, thread);
}

// ---------------------------------------------------------------------------------------------
// 7. Write-side fail-closed (adversarial review fixes): unsafe NEW path, dirty open buffer
// ---------------------------------------------------------------------------------------------

/// Renaming a file to a name whose `res://` text would BREAK the literal it's injected into (a quote,
/// a backslash, a control char) REFUSES the rewrite — the reference stays untouched rather than
/// producing corrupt source like `preload("res://weird"name.gd")`.
#[test]
fn fail_closed_unsafe_new_path_quote() {
    let p = bare_project();
    p.write("a.gd", "extends Node\n");
    let user_src = "extends Node\nconst A = preload(\"res://a.gd\")\n";
    p.write("user.gd", user_src);

    let (client, thread) = boot();
    init_open(&p, &client, caps_full(), &["a.gd", "user.gd"]);

    // Rename a.gd → a name containing a double quote. The new res path would be `res://we"rd.gd`,
    // which injected between the literal's quotes would close the string early. Must REFUSE → null.
    let result = will_rename(&client, 10, &p, "a.gd", "we\"rd.gd");
    assert!(
        result.is_null(),
        "a quote in the new filename must refuse the rewrite (no corrupt edit); got {result:?}"
    );

    shutdown(&client, thread);
}

/// Renaming a file to a name containing an invisible bidi text-direction control character (which
/// the GDScript tokenizer DIAGNOSES inside a string — `char::is_control` does not catch it) REFUSES
/// the rewrite. Injecting it would turn a clean `preload` into a literal Godot/gdls flag as invalid.
#[test]
fn fail_closed_unsafe_new_path_bidi_control() {
    let p = bare_project();
    p.write("a.gd", "extends Node\n");
    let user_src = "extends Node\nconst A = preload(\"res://a.gd\")\n";
    p.write("user.gd", user_src);

    let (client, thread) = boot();
    init_open(&p, &client, caps_full(), &["a.gd", "user.gd"]);

    // U+202E (RIGHT-TO-LEFT OVERRIDE) in the new filename — round-trips through the URI verbatim.
    let result = will_rename(&client, 10, &p, "a.gd", "ev\u{202E}il.gd");
    assert!(
        result.is_null(),
        "a bidi control char in the new filename must refuse the rewrite; got {result:?}"
    );

    shutdown(&client, thread);
}

/// A new path with a `..` segment under the root (a non-normalized client `newUri`) REFUSES the
/// rewrite — `path_to_res` would emit a malformed `res://../…` that `res_to_path` itself rejects.
#[test]
fn fail_closed_unsafe_new_path_traversal() {
    let p = bare_project();
    p.write("a.gd", "extends Node\n");
    let user_src = "extends Node\nconst A = preload(\"res://a.gd\")\n";
    p.write("user.gd", user_src);

    let (client, thread) = boot();
    init_open(&p, &client, caps_full(), &["a.gd", "user.gd"]);

    // Construct an explicit `..`-bearing newUri (the rel-path helper would normalize it away, so
    // build the params by hand). old = a.gd, new = a `..`-spelled in-root sibling.
    let params = RenameFilesParams {
        files: vec![FileRename {
            old_uri: file_uri(&p.root.join("a.gd")).as_str().to_string(),
            new_uri: file_uri(&p.root.join("sub/../a2.gd")).as_str().to_string(),
        }],
    };
    client
        .sender
        .send(request(10, "workspace/willRenameFiles", params))
        .unwrap();
    let resp = loop {
        let r = recv_response(&client);
        if r.id == lsp_server::RequestId::from(10) {
            break r;
        }
    };
    assert!(resp.error.is_none());
    let result = resp.result.unwrap_or(serde_json::Value::Null);
    // Either the URI normalized the `..` away client-side (then it's a clean a2.gd rewrite) OR it
    // survived and we refused. A MALFORMED `res://../…` edit must NEVER appear.
    if !result.is_null() {
        let view = flatten_edit(result);
        for (_, _, nt) in &view.edits {
            assert!(
                !nt.contains(".."),
                "must never emit a malformed res://../ path; got {nt:?}"
            );
        }
    }

    shutdown(&client, thread);
}

/// A DIRTY open buffer (edited via `didChange` after `didOpen`, shifting offsets) is rewritten from
/// its CURRENT buffer text and stamped with its CURRENT version — never disk text / `version: None`.
/// This is the coherence invariant: the span and the version describe the same (buffer) bytes.
#[test]
fn dirty_open_buffer_uses_buffer_text_and_live_version() {
    let p = bare_project();
    p.write("a.gd", "extends Node\n");
    // On-disk b.gd has the literal on line 1.
    let b_disk = "extends Node\nconst A = preload(\"res://a.gd\")\n";
    p.write("b.gd", b_disk);

    let (client, thread) = boot();
    init_open(&p, &client, caps_full(), &["a.gd", "b.gd"]);

    // Make b.gd DIRTY: prepend two comment lines via a full-document didChange (version 5). The
    // literal now lives on line 3, not line 1 — so a disk-text span (line 1) would be WRONG.
    let b_uri = file_uri(&p.root.join("b.gd"));
    let b_dirty = "# edited\n# twice\nextends Node\nconst A = preload(\"res://a.gd\")\n";
    client
        .sender
        .send(notification(
            "textDocument/didChange",
            serde_json::json!({
                "textDocument": { "uri": b_uri.as_str(), "version": 5 },
                "contentChanges": [ { "text": b_dirty } ],
            }),
        ))
        .unwrap();
    while try_recv(&client, Duration::from_millis(300)).is_some() {}

    let result = will_rename(&client, 10, &p, "a.gd", "a2.gd");
    let view = flatten_edit(result);

    let b_uri_s = b_uri.as_str().to_string();
    let b_edit = view
        .edits
        .iter()
        .find(|(u, _, _)| u == &b_uri_s)
        .expect("an edit for the dirty b.gd");
    // The span must be on line 3 (the dirty position), proving buffer text — not disk line 1.
    assert_eq!(
        b_edit.1.start.line, 3,
        "the edit must target the literal's DIRTY-buffer line (3), not the disk line (1)"
    );
    // Applied to the DIRTY buffer text, it produces clean source.
    let after = apply_edit_to(b_dirty, &b_edit.1, &b_edit.2);
    assert!(after.contains("preload(\"res://a2.gd\")"));
    // The stamped version must be the live buffer version (5), never None.
    let ver = view.versions.iter().find(|(u, _)| u == &b_uri_s).unwrap().1;
    assert_eq!(ver, Some(5), "must stamp the live buffer version, not None");

    shutdown(&client, thread);
}

// ---------------------------------------------------------------------------------------------
// 7. Write-set scope: ONLY a preload/load ARGUMENT literal is rewritten (positive identification)
// ---------------------------------------------------------------------------------------------

/// THE blocker-closing test. A `res://` string literal that RESOLVES to the renamed file but is in a
/// NON-load position — a `==` guard comparison, a `const` display value, a dict key, a dict value, a
/// bare expression statement — must NOT be rewritten: it is a VALUE, not a load reference, so
/// repointing it would silently change program behavior (flip the guard, alter the value). Every one
/// of these literals resolves through the index exactly like a `preload` arg would, so the old
/// scan-all-`res://`-literals write-set rewrote them — a corrupting edit. The narrowed write-set
/// (positive `preload`/`load` argument identification) must leave them all untouched → `null` edit.
#[test]
fn non_load_position_literals_are_never_rewritten() {
    let p = bare_project();
    p.write("a.gd", "extends Node\n");
    // Every `"res://a.gd"` below resolves to a.gd, but NONE is a preload/load argument: a guard
    // comparison, a const value, a dict key, a dict value, and a bare expression-statement string.
    let user_src = "extends Node\n\
        const DISPLAY := \"res://a.gd\"\n\
        const TABLE := {\"res://a.gd\": 1, \"k\": \"res://a.gd\"}\n\
        func is_self(p: String) -> bool:\n\
        \t\"res://a.gd\"\n\
        \treturn p == \"res://a.gd\"\n";
    p.write("user.gd", user_src);

    let (client, thread) = boot();
    init_open(&p, &client, caps_full(), &["a.gd", "user.gd"]);

    let result = will_rename(&client, 10, &p, "a.gd", "renamed/a2.gd");
    // NOTHING to rewrite: every literal is a value, not a load reference → LSP `null`.
    assert_eq!(
        result,
        serde_json::Value::Null,
        "no edit may be emitted for `res://` literals in non-load positions; got {result:?}"
    );

    shutdown(&client, thread);
}

/// Discriminator: the EXACT same literal text `"res://a.gd"` appears twice in one file — once as a
/// `preload` argument (a load reference) and once as a `const` value (NOT a load) — when a.gd is
/// renamed. Exactly ONE edit is emitted, over the preload argument; the const value is byte-identical
/// afterward. This proves the gate is the literal's POSITION (a load argument), not its STRING value
/// (which is identical for both) — a string-keyed write-set would wrongly rewrite both.
#[test]
fn same_literal_text_rewrites_only_the_load_argument() {
    let p = bare_project();
    p.write("a.gd", "extends Node\n");
    let user_src = "extends Node\n\
        const LOADED = preload(\"res://a.gd\")\n\
        const LABEL := \"res://a.gd\"\n";
    p.write("user.gd", user_src);

    let (client, thread) = boot();
    init_open(&p, &client, caps_full(), &["a.gd", "user.gd"]);

    let result = will_rename(&client, 10, &p, "a.gd", "renamed/a2.gd");
    let view = flatten_edit(result);

    // Exactly one edit — the preload argument, NOT the const value (same text, different position).
    assert_eq!(
        view.edits.len(),
        1,
        "only the preload-argument literal is rewritten, not the identical-text const value; got {:?}",
        view.edits
    );
    assert_eq!(view.edits[0].2, "res://renamed/a2.gd");

    // Applied: the preload is repointed; the LABEL const value is byte-identical (a value, untouched).
    let after = apply_edit_to(user_src, &view.edits[0].1, &view.edits[0].2);
    assert!(
        after.contains("preload(\"res://renamed/a2.gd\")"),
        "the load argument is repointed: {after}"
    );
    assert!(
        after.contains("const LABEL := \"res://a.gd\""),
        "the non-load const value must be byte-identical: {after}"
    );

    shutdown(&client, thread);
}

/// `ResourceLoader.load("res://a.gd")` (the singleton method form) IS a load reference and is
/// rewritten; an arbitrary `obj.load("res://a.gd")` USER method of the same name is NOT (it is some
/// other object's `.load`, never the resource loader) — matching any `.load` would re-introduce the
/// over-capture this narrowing removes. Proves the `ResourceLoader.load` arm is base-name precise.
#[test]
fn resource_loader_load_rewritten_but_unrelated_method_not() {
    let p = bare_project();
    p.write("a.gd", "extends Node\n");
    // `loader.load(...)` calls a *user* object's `load` method, NOT `ResourceLoader.load` — its
    // `res://` argument must NOT be rewritten. `ResourceLoader.load(...)` must be.
    let user_src = "extends Node\n\
        var loader = Loader.new()\n\
        func f():\n\
        \tvar a = ResourceLoader.load(\"res://a.gd\")\n\
        \tvar b = loader.load(\"res://a.gd\")\n";
    p.write("user.gd", user_src);

    let (client, thread) = boot();
    init_open(&p, &client, caps_full(), &["a.gd", "user.gd"]);

    let result = will_rename(&client, 10, &p, "a.gd", "renamed/a2.gd");
    let view = flatten_edit(result);

    // Exactly one edit: the ResourceLoader.load argument. The user `loader.load` arg is untouched.
    assert_eq!(
        view.edits.len(),
        1,
        "only the ResourceLoader.load argument is rewritten, not an unrelated obj.load; got {:?}",
        view.edits
    );
    assert_eq!(view.edits[0].2, "res://renamed/a2.gd");

    let after = apply_edit_to(user_src, &view.edits[0].1, &view.edits[0].2);
    assert!(
        after.contains("ResourceLoader.load(\"res://renamed/a2.gd\")"),
        "ResourceLoader.load is repointed: {after}"
    );
    assert!(
        after.contains("loader.load(\"res://a.gd\")"),
        "an unrelated obj.load argument must be byte-identical: {after}"
    );

    shutdown(&client, thread);
}

// ---------------------------------------------------------------------------------------------
// 8. .tscn ext_resource rewrite (#131): the SECOND mutating surface
// ---------------------------------------------------------------------------------------------

/// Read a `.tscn` file relative to the project root.
fn read_tscn(p: &TempProject, rel: &str) -> String {
    std::fs::read_to_string(p.root.join(rel).as_std_path()).expect("read .tscn")
}

/// Moving a scene-attached `.gd` rewrites the scene's `ext_resource path="res://old.gd"` entry to
/// the new path — the SECOND mutating surface (#131). The emitted edit, applied to the `.tscn` text,
/// re-resolves the ext_resource to the new identity; the warning for THAT scene no longer fires
/// (it is no longer dangling).
#[test]
fn scene_attached_script_move_rewrites_tscn_ext_resource() {
    let p = bare_project();
    p.write("player.gd", "extends Node\n");
    let tscn_src = "[gd_scene format=3]\n[ext_resource type=\"Script\" path=\"res://player.gd\" id=\"1\"]\n[node name=\"Player\" type=\"Node\"]\nscript = ExtResource(\"1\")\n";
    p.write("player.tscn", tscn_src);

    let (client, thread) = boot();
    init_open(&p, &client, caps_full(), &[]);
    while try_recv(&client, Duration::from_millis(200)).is_some() {}

    let result = will_rename(&client, 10, &p, "player.gd", "nodes/player.gd");
    let view = flatten_edit(result);

    let tscn_uri = file_uri(&p.root.join("player.tscn")).as_str().to_string();
    let tscn_edit = view
        .edits
        .iter()
        .find(|(u, _, _)| u == &tscn_uri)
        .unwrap_or_else(|| panic!("expected a TextEdit over player.tscn; got {:?}", view.edits));
    assert_eq!(
        tscn_edit.2, "res://nodes/player.gd",
        "the ext_resource path is rewritten to the new res:// path"
    );

    // Apply the edit and reparse — the ext_resource must now resolve to the NEW path.
    let after = apply_edit_to(tscn_src, &tscn_edit.1, &tscn_edit.2);
    let scene = gd_project::scene::parse_scene(&after);
    let attached: Vec<&str> = scene.attached_scripts().collect();
    assert_eq!(
        attached,
        vec!["res://nodes/player.gd"],
        "after the edit, the scene attaches the NEW path: {after}"
    );
    assert!(
        !after.contains("res://player.gd"),
        "the old path must be gone: {after}"
    );

    shutdown(&client, thread);
}

/// Moving a `.tscn` that is INSTANCED as a sub-scene by another `.tscn` rewrites the parent's
/// `ext_resource path="res://old.tscn"` (PackedScene) entry to the new path. (#131 — instanced-scene
/// ext_resource.)
#[test]
fn instanced_subscene_move_rewrites_parent_tscn_ext_resource() {
    let p = bare_project();
    let child_src = "[gd_scene format=3]\n[node name=\"ChildRoot\" type=\"Node\"]\n";
    p.write("child.tscn", child_src);
    let parent_src = "[gd_scene format=3]\n[ext_resource type=\"PackedScene\" path=\"res://child.tscn\" id=\"1\"]\n[node name=\"Root\" type=\"Node\"]\n[node name=\"Sub\" parent=\".\" instance=ExtResource(\"1\")]\n";
    p.write("parent.tscn", parent_src);

    let (client, thread) = boot();
    init_open(&p, &client, caps_full(), &[]);
    while try_recv(&client, Duration::from_millis(200)).is_some() {}

    let result = will_rename(&client, 10, &p, "child.tscn", "scenes/child.tscn");
    let view = flatten_edit(result);

    let parent_uri = file_uri(&p.root.join("parent.tscn")).as_str().to_string();
    let parent_edit = view
        .edits
        .iter()
        .find(|(u, _, _)| u == &parent_uri)
        .unwrap_or_else(|| panic!("expected a TextEdit over parent.tscn; got {:?}", view.edits));
    assert_eq!(parent_edit.2, "res://scenes/child.tscn");

    let after = apply_edit_to(parent_src, &parent_edit.1, &parent_edit.2);
    let scene = gd_project::scene::parse_scene(&after);
    let instanced: Vec<&str> = scene.instanced_scenes().collect();
    assert_eq!(
        instanced,
        vec!["res://scenes/child.tscn"],
        "after the edit, the parent instances the NEW sub-scene path: {after}"
    );

    shutdown(&client, thread);
}

/// #229 never-lie backstop: when a renamed `.tscn` is INSTANCED as a sub-scene by another scene and
/// gdls REFUSES to rewrite that reference (here: the new path contains a quote, so the `ext_resource`
/// path text can't be injected), the parent scene is left dangling — so a sub-scene-appropriate
/// `window/showMessage(Warning)` must fire (mirroring the script-attachment dangle path). Pre-#229
/// the refused sub-scene move emitted neither a rewrite nor a warning.
#[test]
fn fail_closed_unsafe_subscene_rewrite_warns_dangling() {
    let p = bare_project();
    let child_src = "[gd_scene format=3]\n[node name=\"ChildRoot\" type=\"Node\"]\n";
    p.write("child.tscn", child_src);
    let parent_src = "[gd_scene format=3]\n[ext_resource type=\"PackedScene\" path=\"res://child.tscn\" id=\"1\"]\n[node name=\"Root\" type=\"Node\"]\n[node name=\"Sub\" parent=\".\" instance=ExtResource(\"1\")]\n";
    p.write("parent.tscn", parent_src);

    let (client, thread) = boot();
    init_open(&p, &client, caps_full(), &[]);
    while try_recv(&client, Duration::from_millis(200)).is_some() {}

    // Rename child.tscn → a name with a double quote: the new res path can't be injected into the
    // parent's `ext_resource path="…"` without breaking it. Refuse the .tscn rewrite, but warn.
    let params = RenameFilesParams {
        files: vec![FileRename {
            old_uri: file_uri(&p.root.join("child.tscn")).as_str().to_string(),
            new_uri: file_uri(&p.root.join("ch\"ild.tscn")).as_str().to_string(),
        }],
    };
    client
        .sender
        .send(request(10, "workspace/willRenameFiles", params))
        .unwrap();

    let mut warned = false;
    let mut got_edit = false;
    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    while std::time::Instant::now() < deadline {
        match try_recv(&client, Duration::from_millis(200)) {
            Some(Message::Notification(n)) if n.method == "window/showMessage" => {
                let msg = n.params["message"].as_str().unwrap_or("");
                // The sub-scene-appropriate message naming the dangling parent scene.
                if msg.contains("sub-scene") && msg.contains("res://parent.tscn") {
                    warned = true;
                }
            }
            Some(Message::Response(r)) if r.id == lsp_server::RequestId::from(10) => {
                got_edit = !r.result.unwrap_or(serde_json::Value::Null).is_null();
                break;
            }
            _ => continue,
        }
    }
    assert!(
        !got_edit,
        "an unsafe new path must refuse the sub-scene .tscn rewrite (no edit)"
    );
    assert!(
        warned,
        "a refused instanced sub-scene move must emit a sub-scene dangling warning naming the parent"
    );
    // The parent on disk must still reference the OLD sub-scene path (we never touched it).
    let parent_now = read_tscn(&p, "parent.tscn");
    assert!(parent_now.contains("res://child.tscn"));

    shutdown(&client, thread);
}

/// Fail-closed: renaming a scene-attached `.gd` to a name whose `res://` text would BREAK the
/// `ext_resource` path (a quote) REFUSES the `.tscn` rewrite — the scene stays untouched AND the
/// dangling warning still fires (the scene is genuinely left dangling, so the user must be told).
#[test]
fn fail_closed_unsafe_tscn_rewrite_keeps_warning() {
    let p = bare_project();
    p.write("player.gd", "extends Node\n");
    let tscn_src = "[gd_scene format=3]\n[ext_resource type=\"Script\" path=\"res://player.gd\" id=\"1\"]\n[node name=\"Player\" type=\"Node\"]\nscript = ExtResource(\"1\")\n";
    p.write("player.tscn", tscn_src);

    let (client, thread) = boot();
    init_open(&p, &client, caps_full(), &[]);
    while try_recv(&client, Duration::from_millis(200)).is_some() {}

    // Rename player.gd → a name with a double quote: the new res path can't be injected into the
    // ext_resource `path="…"` without breaking it. Refuse → no .tscn edit, but warn.
    let params = RenameFilesParams {
        files: vec![FileRename {
            old_uri: file_uri(&p.root.join("player.gd")).as_str().to_string(),
            new_uri: file_uri(&p.root.join("we\"rd.gd")).as_str().to_string(),
        }],
    };
    client
        .sender
        .send(request(10, "workspace/willRenameFiles", params))
        .unwrap();

    // The dangling warning must still fire (the scene is genuinely left dangling).
    let mut warned = false;
    let mut got_edit = false;
    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    while std::time::Instant::now() < deadline {
        match try_recv(&client, Duration::from_millis(200)) {
            Some(Message::Notification(n)) if n.method == "window/showMessage" => {
                let msg = n.params["message"].as_str().unwrap_or("");
                if msg.contains("res://player.tscn") {
                    warned = true;
                }
            }
            Some(Message::Response(r)) if r.id == lsp_server::RequestId::from(10) => {
                got_edit = !r.result.unwrap_or(serde_json::Value::Null).is_null();
                break;
            }
            _ => continue,
        }
    }
    assert!(
        !got_edit,
        "an unsafe new path must refuse the .tscn rewrite (no edit)"
    );
    assert!(
        warned,
        "a refused (still-dangling) scene must keep its dangling warning"
    );
    // The scene on disk must still reference the OLD path (we never touched it).
    let tscn_now = read_tscn(&p, "player.tscn");
    assert!(tscn_now.contains("res://player.gd"));

    shutdown(&client, thread);
}

// ---------------------------------------------------------------------------------------------
// 8. Autoload entries a move leaves dangling get a warning (#309)
// ---------------------------------------------------------------------------------------------

/// Moving a script registered as an autoload leaves `project.godot`'s `[autoload]` entry pointing
/// at a dead path. gdls deliberately does not edit `project.godot` (`docs/09` §6.7 scopes the edit
/// to `preload`/`load` argument literals), so the empty edit set is right — but returning nothing
/// at all told the user nothing, which is the degradation the sibling scene warning already covers.
#[test]
fn autoload_script_move_warns_that_project_godot_will_dangle() {
    let p = TempProject::new();
    p.write(
        "project.godot",
        "config_version=5\n\n[application]\n\nconfig/name=\"Test\"\nconfig/features=PackedStringArray(\"4.6\")\n\n[autoload]\n\nGameState=\"*res://autoload/game_state.gd\"\n",
    );
    p.write("extension_api.json", MINI_API);
    p.write("autoload/game_state.gd", "extends Node\n\nvar score := 0\n");

    let (client, thread) = boot();
    init_open(&p, &client, caps_full(), &["autoload/game_state.gd"]);
    while try_recv(&client, Duration::from_millis(200)).is_some() {}

    let params = RenameFilesParams {
        files: vec![FileRename {
            old_uri: file_uri(&p.root.join("autoload/game_state.gd"))
                .as_str()
                .to_string(),
            new_uri: file_uri(&p.root.join("autoload/state.gd"))
                .as_str()
                .to_string(),
        }],
    };
    client
        .sender
        .send(request(10, "workspace/willRenameFiles", params))
        .unwrap();

    let mut warned = false;
    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    while std::time::Instant::now() < deadline && !warned {
        match try_recv(&client, Duration::from_millis(200)) {
            Some(Message::Notification(n)) if n.method == "window/showMessage" => {
                let msg = n.params["message"].as_str().unwrap_or("");
                warned = msg.contains("GameState") && msg.contains("autoload");
            }
            Some(_) => continue,
            None => continue,
        }
    }
    assert!(
        warned,
        "moving an autoload's script must warn that `project.godot` is left pointing at a dead path"
    );
    shutdown(&client, thread);
}

/// The warning is keyed on the autoload's RESOLVED target, so an unrelated move is silent — the
/// message must not become noise on every rename.
#[test]
fn unrelated_move_does_not_warn_about_autoloads() {
    let p = TempProject::new();
    p.write(
        "project.godot",
        "config_version=5\n\n[application]\n\nconfig/name=\"Test\"\nconfig/features=PackedStringArray(\"4.6\")\n\n[autoload]\n\nGameState=\"*res://autoload/game_state.gd\"\n",
    );
    p.write("extension_api.json", MINI_API);
    p.write("autoload/game_state.gd", "extends Node\n\nvar score := 0\n");
    p.write("other.gd", "extends Node\n");

    let (client, thread) = boot();
    init_open(&p, &client, caps_full(), &["other.gd"]);
    while try_recv(&client, Duration::from_millis(200)).is_some() {}

    let params = RenameFilesParams {
        files: vec![FileRename {
            old_uri: file_uri(&p.root.join("other.gd")).as_str().to_string(),
            new_uri: file_uri(&p.root.join("moved.gd")).as_str().to_string(),
        }],
    };
    client
        .sender
        .send(request(11, "workspace/willRenameFiles", params))
        .unwrap();

    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    while std::time::Instant::now() < deadline {
        if let Some(Message::Notification(n)) = try_recv(&client, Duration::from_millis(200)) {
            if n.method == "window/showMessage" {
                let msg = n.params["message"].as_str().unwrap_or("");
                assert!(
                    !msg.contains("autoload"),
                    "an unrelated move must not warn about autoloads; got: {msg}"
                );
            }
        }
    }
    shutdown(&client, thread);
}
