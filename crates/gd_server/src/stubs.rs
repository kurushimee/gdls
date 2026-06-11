//! Materialized native-class API stubs (#34): `textDocument/definition` on a native symbol
//! returns a standard `file://` Location into a real, read-only pseudo-GDScript page rendering
//! the class's full API — LSP ≤ 3.17 has no virtual-document mechanism, and the generic-LSP
//! principle (#30) rules out custom URI schemes, so gdls follows the TypeScript/Pyright shape:
//! builtins resolve into bundled declaration files on disk.
//!
//! Stubs live under the **user-level** gdls cache — outside any workspace root, so the project
//! indexer can never ingest them as project scripts — keyed by renderer version + the dump's
//! content hash: `<cache>/gdls/stubs/v{N}-{hash:016x}/<Class>.gd`. A dump swap (the background
//! auto-dump adopting mid-session) or a renderer change lands in a fresh directory; stale
//! directories are garbage-collected best-effort, once per session (older renderer versions
//! unconditionally, same-version foreign hashes only after 30 untouched days — another live
//! project's session may legitimately own them).
//!
//! Rendering is a deterministic function of the `NativeClass`: identical bytes per key, so a
//! concurrent double-write between two gdls instances is benign. Stub buffers never
//! self-diagnose (`server.rs` publishes empty diagnostics for URIs under the stubs base): an
//! API page need not be analyzable GDScript, only readable as it.

use std::sync::atomic::{AtomicBool, Ordering};

use camino::{Utf8Path, Utf8PathBuf};
use gd_types::{NativeClass, NativeDb, NativeMember};
use rustc_hash::FxHashMap;

use crate::native_render;

/// Bump when [`render`]'s output shape changes: the version keys the stub directory, so a gdls
/// upgrade with a new renderer regenerates pages instead of serving stale ones for an
/// unchanged dump.
const STUB_FORMAT_VERSION: u32 = 1;

/// The user-level gdls cache root — `%LOCALAPPDATA%\gdls` on Windows, `$XDG_CACHE_HOME/gdls`
/// (else `~/.cache/gdls`) elsewhere. `None` when the environment defines no home; every
/// consumer degrades (definition on natives returns null, the pre-#34 behavior).
fn user_cache_root() -> Option<Utf8PathBuf> {
    #[cfg(windows)]
    {
        let base = std::env::var("LOCALAPPDATA")
            .ok()
            .filter(|s| !s.is_empty())?;
        Some(Utf8PathBuf::from(base).join("gdls"))
    }
    #[cfg(not(windows))]
    {
        let base = match std::env::var("XDG_CACHE_HOME") {
            Ok(x) if !x.is_empty() => Utf8PathBuf::from(x),
            _ => Utf8PathBuf::from(std::env::var("HOME").ok().filter(|s| !s.is_empty())?)
                .join(".cache"),
        };
        Some(base.join("gdls"))
    }
}

/// The stubs base directory (`<cache root>/stubs`). The diagnostics gate matches on THIS — any
/// version/hash — because an old-hash stub buffer can stay open across a mid-session dump swap.
pub(crate) fn stubs_base_dir(override_root: Option<&str>) -> Option<Utf8PathBuf> {
    let root = match override_root {
        Some(p) => Utf8PathBuf::from(p),
        None => user_cache_root()?,
    };
    Some(root.join("stubs"))
}

/// The per-dump stub directory — computed per request from the CURRENT db (`content_hash`), so
/// a background dump adoption mid-session transparently switches directories.
fn stub_dir(db: &NativeDb, override_root: Option<&str>) -> Option<Utf8PathBuf> {
    Some(
        stubs_base_dir(override_root)?
            .join(format!("v{STUB_FORMAT_VERSION}-{:016x}", db.content_hash())),
    )
}

/// `true` when `uri` points under the stubs base — the diagnostics-suppression predicate.
pub(crate) fn is_stub_uri(uri: &lsp_types::Uri, override_root: Option<&str>) -> bool {
    let Some(base) = stubs_base_dir(override_root) else {
        return false;
    };
    crate::uri::uri_to_path(uri).is_some_and(|p| p.starts_with(&base))
}

/// A rendered API page: the text, the 0-based line of the class header, and the 0-based line of
/// every member keyed by name (enum values included) — the definition anchors.
pub(crate) struct RenderedStub {
    pub text: String,
    pub class_line: u32,
    pub member_lines: FxHashMap<String, u32>,
}

/// Render `class`'s API page. Deterministic: header docs as `##` comments, `class_name` +
/// `extends`, then constants, enums, properties, signals, methods — each preceded by its
/// docstring when the dump carries one. Member declaration lines come from
/// [`native_render::member_decl`], so a stub line reads byte-for-byte like the member's hover.
pub(crate) fn render(db: &NativeDb, class: &NativeClass) -> RenderedStub {
    let mut text = String::new();
    let mut member_lines = FxHashMap::default();
    let mut line: u32 = 0;
    let push = |text: &mut String, line: &mut u32, s: &str| {
        text.push_str(s);
        text.push('\n');
        *line += 1;
    };

    let class_name = db.name_of(class.name).to_owned();
    push_doc(&mut text, &mut line, &class.brief_description);
    if !class.description.is_empty() && class.description != class.brief_description {
        if !class.brief_description.is_empty() {
            push(&mut text, &mut line, "##");
        }
        push_doc(&mut text, &mut line, &class.description);
    }
    let class_line = line;
    push(&mut text, &mut line, &format!("class_name {class_name}"));
    if let Some(parent) = class.inherits {
        push(
            &mut text,
            &mut line,
            &format!("extends {}", db.name_of(parent)),
        );
    }

    let section = |text: &mut String, line: &mut u32| push(text, line, "");

    for k in &class.constants {
        section(&mut text, &mut line);
        member_lines.insert(db.name_of(k.name).to_owned(), line);
        let decl = native_render::member_decl(db, &class_name, &NativeMember::Constant(k));
        push(&mut text, &mut line, &decl);
    }
    for e in &class.enums {
        section(&mut text, &mut line);
        member_lines.insert(db.name_of(e.name).to_owned(), line);
        push(
            &mut text,
            &mut line,
            &format!("enum {} {{", db.name_of(e.name)),
        );
        for (name, value) in &e.values {
            member_lines.insert(db.name_of(*name).to_owned(), line);
            push(
                &mut text,
                &mut line,
                &format!("\t{} = {value},", db.name_of(*name)),
            );
        }
        push(&mut text, &mut line, "}");
    }
    for p in &class.properties {
        section(&mut text, &mut line);
        push_doc(&mut text, &mut line, &p.description);
        member_lines.insert(db.name_of(p.name).to_owned(), line);
        let decl = native_render::member_decl(db, &class_name, &NativeMember::Property(p));
        push(&mut text, &mut line, &decl);
    }
    for s in &class.signals {
        section(&mut text, &mut line);
        push_doc(&mut text, &mut line, &s.description);
        member_lines.insert(db.name_of(s.name).to_owned(), line);
        let decl = native_render::member_decl(db, &class_name, &NativeMember::Signal(s));
        push(&mut text, &mut line, &decl);
    }
    for m in &class.methods {
        section(&mut text, &mut line);
        push_doc(&mut text, &mut line, &m.description);
        member_lines.insert(db.name_of(m.name).to_owned(), line);
        let decl = native_render::member_decl(db, &class_name, &NativeMember::Method(m));
        push(&mut text, &mut line, &decl);
    }

    RenderedStub {
        text,
        class_line,
        member_lines,
    }
}

/// Append a (possibly multi-line) docstring as `## ` comment lines. No-op when empty.
fn push_doc(text: &mut String, line: &mut u32, doc: &str) {
    if doc.is_empty() {
        return;
    }
    for l in doc.lines() {
        if l.is_empty() {
            text.push_str("##\n");
        } else {
            text.push_str("## ");
            text.push_str(l);
            text.push('\n');
        }
        *line += 1;
    }
}

/// Render `class_name`'s stub in memory and write it to disk if absent (atomic temp + rename;
/// identical bytes per key make a concurrent double-write benign). Returns the stub path and
/// the rendered line map; `None` on any IO failure — the caller degrades to "no definition".
pub(crate) fn ensure_class_stub(
    db: &NativeDb,
    class_name: &str,
    override_root: Option<&str>,
) -> Option<(Utf8PathBuf, RenderedStub)> {
    let class = db.class_named(class_name)?;
    let dir = stub_dir(db, override_root)?;
    std::fs::create_dir_all(dir.as_std_path()).ok()?;
    if let Some(base) = stubs_base_dir(override_root) {
        session_gc(&base, &dir);
    }
    let stub = render(db, class);
    let path = dir.join(format!("{class_name}.gd"));
    if !path.as_std_path().exists() {
        let tmp = dir.join(format!(".{class_name}.gd.tmp"));
        std::fs::write(tmp.as_std_path(), &stub.text).ok()?;
        std::fs::rename(tmp.as_std_path(), path.as_std_path()).ok()?;
    }
    Some((path, stub))
}

/// Best-effort stale-stub collection + a freshness touch on the current directory, once per
/// server session (the work is per-process idempotent; repeating it per request would be IO
/// churn for nothing). All IO errors ignored — stubs are regenerable cache.
fn session_gc(base: &Utf8Path, current: &Utf8Path) {
    static GC_DONE: AtomicBool = AtomicBool::new(false);
    if GC_DONE.swap(true, Ordering::Relaxed) {
        return;
    }
    // Creating/truncating a file inside `current` bumps the directory's mtime, which is the
    // age signal `gc_stale_stubs` reads — a long-lived dump stays fresh as long as some
    // session keeps using it.
    let _ = std::fs::write(current.join(".touch").as_std_path(), b"");
    gc_stale_stubs(base, current);
}

/// The ungated GC body (separated from [`session_gc`]'s process-global once-flag so tests can
/// drive it directly): remove sibling stub directories with an older `STUB_FORMAT_VERSION`
/// unconditionally, and same-version foreign-hash directories only when untouched for 30+ days
/// — another live project's session may legitimately own a different hash.
pub(crate) fn gc_stale_stubs(base: &Utf8Path, current: &Utf8Path) {
    const STALE_AFTER: std::time::Duration = std::time::Duration::from_secs(30 * 24 * 3600);
    let Ok(entries) = std::fs::read_dir(base.as_std_path()) else {
        return;
    };
    for e in entries.flatten() {
        let p = e.path();
        if !p.is_dir() || p == current.as_std_path() {
            continue;
        }
        let name = e.file_name().to_string_lossy().into_owned();
        let old_version = name
            .strip_prefix('v')
            .and_then(|rest| rest.split_once('-'))
            .and_then(|(v, _)| v.parse::<u32>().ok())
            .is_some_and(|v| v < STUB_FORMAT_VERSION);
        let stale_by_age = e
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.elapsed().ok())
            .is_some_and(|age| age > STALE_AFTER);
        if old_version || stale_by_age {
            let _ = std::fs::remove_dir_all(&p);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn db() -> NativeDb {
        NativeDb::from_json(
            r#"{
                "header": {"version_major": 4, "version_minor": 6, "version_patch": 3},
                "classes": [
                    {"name": "Object", "is_instantiable": true},
                    {"name": "Widget", "inherits": "Object", "is_instantiable": true,
                     "brief_description": "A clickable thing.",
                     "constants": [{"name": "MAX_DEPTH", "value": 8}],
                     "enums": [{"name": "Mode", "is_bitfield": false,
                                "values": [{"name": "MODE_A", "value": 0},
                                            {"name": "MODE_B", "value": 1}]}],
                     "properties": [{"name": "width", "type": "int",
                                     "setter": "set_width", "getter": "get_width",
                                     "description": "Pixel width."}],
                     "signals": [{"name": "resized"}],
                     "methods": [{"name": "grow", "is_const": false, "is_static": false,
                                  "is_vararg": false, "is_virtual": false, "hash": 1,
                                  "arguments": [{"name": "by", "type": "int",
                                                  "default_value": "1"}]}]}
                ]
            }"#,
        )
        .expect("stub-test dump")
    }

    #[test]
    fn render_is_deterministic_and_lines_point_at_their_declarations() {
        let db = db();
        let class = db.class_named("Widget").unwrap();
        let a = render(&db, class);
        let b = render(&db, class);
        assert_eq!(a.text, b.text, "two renders are byte-identical");

        let lines: Vec<&str> = a.text.lines().collect();
        assert_eq!(lines[a.class_line as usize], "class_name Widget");
        assert_eq!(lines[a.class_line as usize + 1], "extends Object");
        let at = |name: &str| lines[a.member_lines[name] as usize];
        assert_eq!(at("MAX_DEPTH"), "const MAX_DEPTH = 8");
        assert_eq!(at("Mode"), "enum Mode {");
        assert_eq!(at("MODE_B"), "\tMODE_B = 1,");
        assert_eq!(at("width"), "var width: int");
        assert!(
            lines[a.member_lines["width"] as usize - 1].contains("## Pixel width."),
            "member doc precedes the declaration"
        );
        assert_eq!(at("resized"), "signal resized()");
        assert_eq!(at("grow"), "func grow(by: int = 1) -> void");
        assert!(
            a.text.starts_with("## A clickable thing.\n"),
            "class docs head the page: {:?}",
            &a.text[..40]
        );
    }

    #[test]
    fn ensure_writes_once_and_reuses_the_existing_file() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().to_string_lossy().into_owned();
        let db = db();
        let (path, _) = ensure_class_stub(&db, "Widget", Some(&root)).expect("stub written");
        assert!(path.as_std_path().exists());
        let first = std::fs::metadata(path.as_std_path())
            .unwrap()
            .modified()
            .unwrap();
        let (path2, stub2) = ensure_class_stub(&db, "Widget", Some(&root)).expect("stub reused");
        assert_eq!(path, path2);
        let second = std::fs::metadata(path2.as_std_path())
            .unwrap()
            .modified()
            .unwrap();
        assert_eq!(first, second, "second ensure must not rewrite the file");
        assert_eq!(
            std::fs::read_to_string(path.as_std_path()).unwrap(),
            stub2.text,
            "on-disk bytes equal the in-memory render"
        );
        assert!(path.as_str().contains(&format!("v{STUB_FORMAT_VERSION}-")));
    }

    #[test]
    fn gc_removes_old_versions_keeps_fresh_foreign_hashes() {
        let tmp = tempfile::tempdir().unwrap();
        let base = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).unwrap();
        let current = base.join(format!("v{STUB_FORMAT_VERSION}-{:016x}", 1u64));
        let old_version = base.join(format!("v{}-{:016x}", STUB_FORMAT_VERSION - 1, 2u64));
        let foreign_fresh = base.join(format!("v{STUB_FORMAT_VERSION}-{:016x}", 3u64));
        for d in [&current, &old_version, &foreign_fresh] {
            std::fs::create_dir_all(d.as_std_path()).unwrap();
        }
        gc_stale_stubs(&base, &current);
        assert!(current.as_std_path().exists(), "current dir survives");
        assert!(
            !old_version.as_std_path().exists(),
            "older renderer version is collected unconditionally"
        );
        assert!(
            foreign_fresh.as_std_path().exists(),
            "fresh same-version foreign hash survives (may belong to a live session)"
        );
    }
}
