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

use std::cell::RefCell;
use std::rc::Rc;
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
    let Some(p) = crate::uri::uri_to_path(uri) else {
        return false;
    };
    if p.starts_with(&base) {
        return true;
    }
    // Windows filesystems are case-insensitive and clients re-derive URIs with their own
    // casing (VS Code lowercases the drive letter), while `base` comes from the environment —
    // a literal component compare can miss and a stub page would self-diagnose. Retry folded.
    #[cfg(windows)]
    {
        let mut pc = p.components();
        base.components().all(|b| {
            pc.next()
                .is_some_and(|c| c.as_str().eq_ignore_ascii_case(b.as_str()))
        })
    }
    #[cfg(not(windows))]
    {
        false
    }
}

/// Per-session cache of rendered pages: a class's page is a pure function of the dump (keyed
/// by its content hash), so each (dump, class) pair renders at most once per session — repeat
/// definition requests into a large class skip the O(members) rebuild. Interior-mutable so
/// shared-`&` request paths can fill it. The disk probe in [`ensure_class_stub`] still runs
/// per call: a foreign session's GC may collect the file while this cache is warm, and the
/// probe re-materializes it.
#[derive(Default)]
pub(crate) struct StubCache(RefCell<FxHashMap<String, (u64, Rc<RenderedStub>)>>);

/// One member's anchor within a rendered page: its 0-based line plus the byte extent of the
/// name token within that line. Every declaration line opens with a fixed ASCII prefix
/// (`"func "`, `"var "`, …) and engine member names are ASCII identifiers, so the byte columns
/// are valid character offsets under ANY negotiated position encoding — no mapper needed.
#[derive(Clone, Copy)]
pub(crate) struct MemberAnchor {
    pub line: u32,
    pub name_col: u32,
    pub name_len: u32,
}

/// A rendered API page: the text, the 0-based line of the class header (plus the byte column of
/// the class name on it), and every member's [`MemberAnchor`] keyed by name (enum values
/// included) — the definition anchors.
pub(crate) struct RenderedStub {
    pub text: String,
    pub class_line: u32,
    /// Byte column of the class name on [`Self::class_line`] (after `"class_name "`).
    pub class_name_col: u32,
    pub member_lines: FxHashMap<String, MemberAnchor>,
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
    // Each declaration line opens with a fixed ASCII prefix, so the name token's byte extent is
    // `prefix.len()..prefix.len()+name.len()`. Asserted at every insert so a renderer format
    // change (say, a future `static func`) fails tests here instead of silently mis-anchoring
    // definition jumps into the page.
    let anchor = |line: u32, decl: &str, prefix: &str, name: &str| {
        debug_assert_eq!(
            decl.get(prefix.len()..prefix.len() + name.len()),
            Some(name),
            "stub anchor drift: {decl:?} does not open with {prefix:?} + {name:?}"
        );
        MemberAnchor {
            line,
            name_col: prefix.len() as u32,
            name_len: name.len() as u32,
        }
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
        let name = db.name_of(k.name).to_owned();
        let decl = native_render::member_decl(db, &class_name, &NativeMember::Constant(k));
        member_lines.insert(name.clone(), anchor(line, &decl, "const ", &name));
        push(&mut text, &mut line, &decl);
    }
    for e in &class.enums {
        section(&mut text, &mut line);
        let name = db.name_of(e.name).to_owned();
        let decl = format!("enum {name} {{");
        member_lines.insert(name.clone(), anchor(line, &decl, "enum ", &name));
        push(&mut text, &mut line, &decl);
        for v in &e.values {
            let vname = db.name_of(v.name).to_owned();
            push_doc(&mut text, &mut line, &v.description);
            let decl = format!("\t{vname} = {},", v.value);
            member_lines.insert(vname.clone(), anchor(line, &decl, "\t", &vname));
            push(&mut text, &mut line, &decl);
        }
        push(&mut text, &mut line, "}");
    }
    for p in &class.properties {
        section(&mut text, &mut line);
        push_doc(&mut text, &mut line, &p.description);
        let name = db.name_of(p.name).to_owned();
        let decl = native_render::member_decl(db, &class_name, &NativeMember::Property(p));
        member_lines.insert(name.clone(), anchor(line, &decl, "var ", &name));
        push(&mut text, &mut line, &decl);
    }
    for s in &class.signals {
        section(&mut text, &mut line);
        push_doc(&mut text, &mut line, &s.description);
        let name = db.name_of(s.name).to_owned();
        let decl = native_render::member_decl(db, &class_name, &NativeMember::Signal(s));
        member_lines.insert(name.clone(), anchor(line, &decl, "signal ", &name));
        push(&mut text, &mut line, &decl);
    }
    for m in &class.methods {
        section(&mut text, &mut line);
        push_doc(&mut text, &mut line, &m.description);
        let name = db.name_of(m.name).to_owned();
        let decl = native_render::member_decl(db, &class_name, &NativeMember::Method(m));
        member_lines.insert(name.clone(), anchor(line, &decl, "func ", &name));
        push(&mut text, &mut line, &decl);
    }

    RenderedStub {
        text,
        class_line,
        class_name_col: "class_name ".len() as u32,
        member_lines,
    }
}

/// The [`render`] twin for a Variant type (`Vector2`, `Array`, `String`, …). Same page shape
/// minus the `extends` line, which a builtin has no equivalent of. #370: builtin types are what
/// most GDScript touches most often, and they had no page at all, so `definition` on
/// `arr.append` answered null while `node.add_child` answered a stub location.
pub(crate) fn render_builtin(db: &NativeDb, bt: &gd_types::BuiltinType) -> RenderedStub {
    let mut text = String::new();
    let mut member_lines = FxHashMap::default();
    let mut line: u32 = 0;
    let push = |text: &mut String, line: &mut u32, s: &str| {
        text.push_str(s);
        text.push('\n');
        *line += 1;
    };
    let anchor = |line: u32, decl: &str, prefix: &str, name: &str| {
        debug_assert_eq!(
            decl.get(prefix.len()..prefix.len() + name.len()),
            Some(name),
            "stub anchor drift: {decl:?} does not open with {prefix:?} + {name:?}"
        );
        MemberAnchor {
            line,
            name_col: prefix.len() as u32,
            name_len: name.len() as u32,
        }
    };

    let type_name = db.name_of(bt.name).to_owned();
    push_doc(&mut text, &mut line, &bt.brief_description);
    if !bt.description.is_empty() && bt.description != bt.brief_description {
        if !bt.brief_description.is_empty() {
            push(&mut text, &mut line, "##");
        }
        push_doc(&mut text, &mut line, &bt.description);
    }
    let class_line = line;
    push(&mut text, &mut line, &format!("class_name {type_name}"));

    let section = |text: &mut String, line: &mut u32| push(text, line, "");

    for k in &bt.constants {
        section(&mut text, &mut line);
        push_doc(&mut text, &mut line, &k.description);
        let name = db.name_of(k.name).to_owned();
        let decl = native_render::member_decl(db, &type_name, &NativeMember::Constant(k));
        member_lines.insert(name.clone(), anchor(line, &decl, "const ", &name));
        push(&mut text, &mut line, &decl);
    }
    for e in &bt.enums {
        section(&mut text, &mut line);
        let name = db.name_of(e.name).to_owned();
        let decl = format!("enum {name} {{");
        member_lines.insert(name.clone(), anchor(line, &decl, "enum ", &name));
        push(&mut text, &mut line, &decl);
        for v in &e.values {
            let vname = db.name_of(v.name).to_owned();
            push_doc(&mut text, &mut line, &v.description);
            let decl = format!("\t{vname} = {},", v.value);
            member_lines.insert(vname.clone(), anchor(line, &decl, "\t", &vname));
            push(&mut text, &mut line, &decl);
        }
        push(&mut text, &mut line, "}");
    }
    for p in &bt.members {
        section(&mut text, &mut line);
        push_doc(&mut text, &mut line, &p.description);
        let name = db.name_of(p.name).to_owned();
        let decl = native_render::member_decl(db, &type_name, &NativeMember::Property(p));
        member_lines.insert(name.clone(), anchor(line, &decl, "var ", &name));
        push(&mut text, &mut line, &decl);
    }
    for m in &bt.methods {
        section(&mut text, &mut line);
        push_doc(&mut text, &mut line, &m.description);
        let name = db.name_of(m.name).to_owned();
        let decl = native_render::member_decl(db, &type_name, &NativeMember::Method(m));
        member_lines.insert(name.clone(), anchor(line, &decl, "func ", &name));
        push(&mut text, &mut line, &decl);
    }

    RenderedStub {
        text,
        class_line,
        class_name_col: "class_name ".len() as u32,
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

/// Resolve `class_name`'s rendered stub (from `cache`, rendering on first sight of this dump's
/// hash) and write it to disk if absent (atomic temp + rename; identical bytes per key make a
/// concurrent double-write benign). Returns the stub path and the rendered line map; `None` on
/// any IO failure — the caller degrades to "no definition".
pub(crate) fn ensure_class_stub(
    cache: &StubCache,
    db: &NativeDb,
    class_name: &str,
    override_root: Option<&str>,
) -> Option<(Utf8PathBuf, Rc<RenderedStub>)> {
    // The name becomes a path component below, and the dump is project-supplied data (a
    // project-root `extension_api.json` is auto-adopted): refuse anything that isn't
    // identifier-shaped rather than let a crafted class name (`../…`) write outside the stub
    // directory. Engine and GDExtension class names are always ASCII identifiers.
    if !is_identifier_shaped(class_name) {
        return None;
    }
    // Engine classes and Variant types share one page namespace and one directory; Godot keeps
    // the two name sets disjoint, so a name resolves to at most one of them (#370).
    if db.class_named(class_name).is_none() && db.builtin_named(class_name).is_none() {
        return None;
    }
    let dir = stub_dir(db, override_root)?;
    std::fs::create_dir_all(dir.as_std_path()).ok()?;
    if let Some(base) = stubs_base_dir(override_root) {
        freshen_and_gc(&base, &dir);
    }
    let hash = db.content_hash();
    let stub = {
        let mut map = cache.0.borrow_mut();
        match map.get(class_name) {
            Some((h, stub)) if *h == hash => Rc::clone(stub),
            _ => {
                // A mid-session dump adoption changes the hash: sweep the dead
                // generation out instead of accumulating two dumps' pages.
                map.retain(|_, (h, _)| *h == hash);
                let rendered = match db.class_named(class_name) {
                    Some(class) => render(db, class),
                    None => render_builtin(db, db.builtin_named(class_name)?),
                };
                let stub = Rc::new(rendered);
                map.insert(class_name.to_owned(), (hash, Rc::clone(&stub)));
                stub
            }
        }
    };
    let path = dir.join(format!("{class_name}.gd"));
    if !path.as_std_path().exists() {
        let tmp = dir.join(format!(".{class_name}.gd.tmp"));
        std::fs::write(tmp.as_std_path(), &stub.text).ok()?;
        if std::fs::rename(tmp.as_std_path(), path.as_std_path()).is_err() {
            // Windows refuses to rename onto an existing target, so losing a double-write
            // race to a concurrent session lands here with that session's identical bytes
            // already at `path` — serve them. Only a rename failure with NO file at the
            // target is a real IO failure.
            let _ = std::fs::remove_file(tmp.as_std_path());
            if !path.as_std_path().exists() {
                return None;
            }
        }
    }
    Some((path, stub))
}

/// `[A-Za-z_][A-Za-z0-9_]*` — the only class-name shape allowed to become a stub filename.
fn is_identifier_shaped(name: &str) -> bool {
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Refresh the current directory's freshness sentinel (every call) and collect stale stub
/// directories (once per server session — the scan is per-process idempotent, so repeating
/// IT per request would be IO churn for nothing). All IO errors ignored — stubs are
/// regenerable cache.
fn freshen_and_gc(base: &Utf8Path, current: &Utf8Path) {
    // Rewriting `.touch` updates the FILE's mtime, the freshness signal `gc_stale_stubs`
    // reads — a directory's own mtime only moves on entry creation/removal, so it goes
    // stale once every page is materialized no matter how recently a session read them.
    // One tiny write per native-definition request keeps even a months-long session's
    // directory alive against a foreign session's 30-day collection.
    let _ = std::fs::write(current.join(".touch").as_std_path(), b"");
    static GC_DONE: AtomicBool = AtomicBool::new(false);
    if GC_DONE.swap(true, Ordering::Relaxed) {
        return;
    }
    gc_stale_stubs(base, current);
}

/// The ungated GC body (separated from [`freshen_and_gc`]'s process-global once-flag so tests
/// can drive it directly): remove sibling stub directories with an older `STUB_FORMAT_VERSION`
/// unconditionally, and same-version foreign-hash directories only when untouched for 30+ days
/// — another live project's session may legitimately own a different hash. A directory's age
/// is the newest evidence of life: its `.touch` sentinel's mtime (rewritten by every ensure)
/// or the directory's own mtime (last entry change), whichever is fresher.
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
        let dir_mtime = e.metadata().and_then(|m| m.modified()).ok();
        let touch_mtime = std::fs::metadata(p.join(".touch"))
            .and_then(|m| m.modified())
            .ok();
        let freshness = match (dir_mtime, touch_mtime) {
            (Some(d), Some(t)) => Some(d.max(t)),
            (d, t) => d.or(t),
        };
        let stale_by_age = freshness
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
                                     "description": "Pixel width."},
                                    {"name": "v", "type": "int",
                                     "setter": "set_v", "getter": "get_v"}],
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
        assert_eq!(
            &lines[a.class_line as usize][a.class_name_col as usize..],
            "Widget",
            "class_name_col anchors the class name token"
        );
        let at = |name: &str| lines[a.member_lines[name].line as usize];
        assert_eq!(at("MAX_DEPTH"), "const MAX_DEPTH = 8");
        assert_eq!(at("Mode"), "enum Mode {");
        assert_eq!(at("MODE_B"), "\tMODE_B = 1,");
        assert_eq!(at("width"), "var width: int");
        assert!(
            lines[a.member_lines["width"].line as usize - 1].contains("## Pixel width."),
            "member doc precedes the declaration"
        );
        assert_eq!(at("resized"), "signal resized()");
        assert_eq!(at("grow"), "func grow(by: int = 1) -> void");
        // The column extents slice exactly the name token out of each declaration line — the
        // anchor definition jumps land on. The one-letter property `v` pins the fixed-prefix
        // computation: a naive `line.find(name)` would match the `v` inside `"var "`.
        for name in [
            "MAX_DEPTH",
            "Mode",
            "MODE_B",
            "width",
            "v",
            "resized",
            "grow",
        ] {
            let m = &a.member_lines[name];
            assert_eq!(
                &lines[m.line as usize][m.name_col as usize..(m.name_col + m.name_len) as usize],
                name,
                "anchor of `{name}` must slice exactly its name token"
            );
        }
        assert_eq!(at("v"), "var v: int");
        assert_eq!(a.member_lines["v"].name_col, 4);
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
        let cache = StubCache::default();
        let (path, stub) =
            ensure_class_stub(&cache, &db, "Widget", Some(&root)).expect("stub written");
        assert!(path.as_std_path().exists());
        assert!(
            path.parent().unwrap().join(".touch").as_std_path().exists(),
            "every ensure refreshes the freshness sentinel"
        );
        let first = std::fs::metadata(path.as_std_path())
            .unwrap()
            .modified()
            .unwrap();
        let (path2, stub2) =
            ensure_class_stub(&cache, &db, "Widget", Some(&root)).expect("stub reused");
        assert_eq!(path, path2);
        assert!(
            Rc::ptr_eq(&stub, &stub2),
            "second ensure reuses the session-cached render"
        );
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
    fn ensure_serves_a_file_another_session_already_wrote() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().to_string_lossy().into_owned();
        let db = db();
        // Materialize via one "session", then start a fresh cache (a second session) over
        // the same root: the page is already on disk, so the second session reuses it (no
        // rewrite) while still returning its own in-memory render of the same bytes.
        let first_session = StubCache::default();
        let (path, _) =
            ensure_class_stub(&first_session, &db, "Widget", Some(&root)).expect("stub written");
        let before = std::fs::metadata(path.as_std_path())
            .unwrap()
            .modified()
            .unwrap();
        let second_session = StubCache::default();
        let (path2, stub) = ensure_class_stub(&second_session, &db, "Widget", Some(&root))
            .expect("a page already on disk is served, not an IO failure");
        assert_eq!(path, path2);
        let after = std::fs::metadata(path2.as_std_path())
            .unwrap()
            .modified()
            .unwrap();
        assert_eq!(before, after, "an existing on-disk page is never rewritten");
        assert_eq!(
            std::fs::read_to_string(path.as_std_path()).unwrap(),
            stub.text
        );
    }

    #[test]
    fn hostile_class_name_never_becomes_a_path() {
        // A crafted dump (a project-root extension_api.json is auto-adopted from any opened
        // repo) must not turn a class name into a path traversal: `../escape` would land the
        // stub at `<root>/stubs/escape.gd`, outside its version-hash directory.
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().to_string_lossy().into_owned();
        let db = NativeDb::from_json(
            r#"{
                "header": {"version_major": 4, "version_minor": 6, "version_patch": 3},
                "classes": [{"name": "../escape", "is_instantiable": true}]
            }"#,
        )
        .expect("ingest stores names verbatim");
        let cache = StubCache::default();
        assert!(
            ensure_class_stub(&cache, &db, "../escape", Some(&root)).is_none(),
            "non-identifier class names are refused"
        );
        assert!(
            !tmp.path().join("stubs").join("escape.gd").exists(),
            "nothing may be written outside the version-hash directory"
        );
    }

    /// The >30-day-session scenario: a directory whose ENTRIES stopped changing long ago
    /// (stale dir mtime — on POSIX materializing the last page is the last mtime bump) but
    /// whose `.touch` sentinel a still-live session keeps rewriting must survive a foreign
    /// GC; one stale by both signals is collected. Unix-only: faking a directory's mtime
    /// needs `File::open` on a directory, which Windows refuses.
    #[cfg(unix)]
    #[test]
    fn gc_freshness_prefers_the_touch_sentinel_over_dir_mtime() {
        let tmp = tempfile::tempdir().unwrap();
        let base = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).unwrap();
        let current = base.join(format!("v{STUB_FORMAT_VERSION}-{:016x}", 1u64));
        let live_long_session = base.join(format!("v{STUB_FORMAT_VERSION}-{:016x}", 2u64));
        let dead = base.join(format!("v{STUB_FORMAT_VERSION}-{:016x}", 3u64));
        for d in [&current, &live_long_session, &dead] {
            std::fs::create_dir_all(d.as_std_path()).unwrap();
        }
        let old = std::time::SystemTime::now() - std::time::Duration::from_secs(40 * 24 * 3600);
        let set_mtime = |p: &Utf8PathBuf| {
            std::fs::File::open(p.as_std_path())
                .unwrap()
                .set_modified(old)
                .unwrap();
        };
        std::fs::write(live_long_session.join(".touch").as_std_path(), b"").unwrap();
        set_mtime(&live_long_session); // dir mtime stale, sentinel fresh
        let dead_touch = dead.join(".touch");
        std::fs::write(dead_touch.as_std_path(), b"").unwrap();
        set_mtime(&dead_touch);
        set_mtime(&dead); // both signals stale
        gc_stale_stubs(&base, &current);
        assert!(
            live_long_session.as_std_path().exists(),
            "a fresh sentinel keeps the directory alive past a stale dir mtime"
        );
        assert!(
            !dead.as_std_path().exists(),
            "stale by both signals is collected"
        );
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
