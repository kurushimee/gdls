//! Auto-managed `extension_api.json` (v1.0.1, issue #20): discover the user's Godot binary, run
//! `--dump-extension-api-with-docs` WITH project context (the only way GDExtension classes are
//! captured), and keep the result + staleness metadata under `.gdls/` — so native typing needs
//! zero configuration.
//!
//! Operational facts this module is built around (verified against Godot 4.6.3):
//! - `godot --headless --path <root> --dump-extension-api-with-docs` writes
//!   `<root>/extension_api.json` — into the PROJECT ROOT, regardless of the child's cwd. The
//!   dump is therefore guarded on a pre-existing user file at that path (never clobber) and
//!   renamed into `.gdls/` immediately after.
//! - Godot may abort/core-dump on exit AFTER writing a complete dump (observed on 4.6.3 with
//!   `--path`). The exit status is logged but never trusted over the artifact: if the dump file
//!   appears and parses, it is used.
//! - A never-imported project (no `.godot/extension_list.cfg`) may dump without GDExtension
//!   classes; a heuristic post-check logs the remediation.
//!
//! Hard rules: stdout is the LSP wire, so the child's stdio is null/piped — never inherited; and
//! every failure degrades to the next resolution step with one log line — the server always
//! starts.

use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use camino::{Utf8Path, Utf8PathBuf};
use gd_project::cache::FileStat;
use gd_project::ProjectModel;
use gd_types::NativeDb;
use serde::{Deserialize, Serialize};

use crate::config::InitializationOptions;

/// What the background auto-dump thread reports back to the event loop (issue #25). On
/// `Adopted`, the dump has already been parsed, moved into `.gdls/`, and its meta written —
/// the receiving loop only reloads the native DB and republishes open buffers.
#[derive(Debug)]
pub(crate) enum DumpOutcome {
    Adopted { classes: usize, version: String },
    Failed(String),
}

/// Bump independently of `gd_project::cache::CACHE_FORMAT_VERSION` when this file's shape changes.
const META_FORMAT_VERSION: u32 = 1;

/// Wall-clock budget for the dump. Generous on purpose: the dump runs on a background thread
/// (never on the startup path), so a long wait costs only a lingering child process — while a
/// tight deadline kills legitimate slow first boots (cold import caches, AV-scanned binaries,
/// huge projects). A deadline kill still adopts a completed artifact ("the artifact decides").
const DUMP_TIMEOUT: Duration = Duration::from_secs(300);

/// `.gdls/extension_api.meta.json` — everything that decides whether the cached dump is fresh.
/// Written only after the dump PARSED, so a torn dump can never look fresh.
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ApiDumpMeta {
    meta_format_version: u32,
    /// The dumping binary's identity: path + (size, mtime). One stat per startup; an
    /// upgrade-in-place changes both. (A `--version` probe would cost a full process launch.)
    binary: FileStat,
    /// From the dump's own header — diagnostics/logging only, never a freshness key.
    godot_version_full_name: String,
    /// The project's `.gdextension` config files (path, size, mtime), sorted by path — the
    /// precise extension surface; `project.godot` itself is deliberately NOT keyed (it changes
    /// on every settings tweak and cannot alter the engine-class surface).
    gdextensions: Vec<FileStat>,
}

/// The managed dump location.
pub(crate) fn dump_path(root: &Utf8Path) -> Utf8PathBuf {
    root.join(".gdls").join("extension_api.json")
}

fn meta_path(root: &Utf8Path) -> Utf8PathBuf {
    root.join(".gdls").join("extension_api.meta.json")
}

/// Resolve the native DB when no explicit `extensionApiPath` is configured. The order:
/// fresh `.gdls` dump → stale `.gdls` dump → `<root>/extension_api.json` (unmanaged user file) →
/// embedded stock fallback → empty (dynamic). One log line per decision so an operator can
/// always reconstruct which source won.
///
/// v1.0.2 (issue #25): resolution itself NEVER spawns Godot. The auto-dump runs on a background
/// thread ([`spawn_background_dump`], session startup only) and is adopted mid-session through
/// the event loop, so the first request never queues behind a Godot boot.
pub(crate) fn resolve_native_db(
    options: &InitializationOptions,
    project: &ProjectModel,
    root: &Utf8Path,
) -> NativeDb {
    let managed = dump_path(root);
    let binary = discover_binary(options);

    // (1) Fresh managed dump.
    let staleness = staleness_reason(root, project, binary.as_deref());
    if managed.as_std_path().exists() {
        match staleness {
            None => match NativeDb::load(managed.as_str()) {
                Ok(db) => {
                    log::info!(
                        "native API: using cached auto-dump ({} classes, {})",
                        db.class_count(),
                        version_label(&db),
                    );
                    return db;
                }
                Err(e) => {
                    quarantine(&managed, &format!("{e}"));
                }
            },
            Some(ref reason) => {
                log::info!("native API: cached dump is stale ({reason})");
            }
        }
    }

    // (2) Stale managed dump — known provenance (made with project context) beats nothing.
    if managed.as_std_path().exists() {
        if let Ok(db) = NativeDb::load(managed.as_str()) {
            log::warn!(
                "native API: using STALE cached dump ({}); reason: {}",
                version_label(&db),
                staleness_reason(root, project, binary.as_deref())
                    .unwrap_or_else(|| "unknown".to_owned()),
            );
            return db;
        }
    }

    // (3) Unmanaged user file at the project root.
    let root_file = root.join("extension_api.json");
    if root_file.as_std_path().exists() {
        if let Ok(db) = NativeDb::load(root_file.as_str()) {
            log::info!(
                "native API: using project-root extension_api.json (unmanaged; set \
                 extensionApiPath to pin it, or remove it to let gdls manage the dump)"
            );
            return db;
        }
    }

    // (4) Embedded stock fallback — builtins always resolve, even on a fresh install with no
    // Godot binary anywhere. `Generic` provenance: the analyzer will not turn ITS misses into
    // "Could not find type" errors (a custom engine build legitimately has classes this stock
    // dump doesn't).
    if options.embedded_api_fallback {
        if let Some(db) = embedded_stock_db() {
            log::warn!(
                "native API: no project-derived source (no extensionApiPath, no cached dump, \
                 auto-dump {}); using the embedded stock {} surface — project-specific native \
                 classes degrade to dynamic. Set godotBinaryPath or GDLS_GODOT for an exact dump.",
                if options.auto_dump_extension_api {
                    "found no source"
                } else {
                    "disabled"
                },
                version_label(&db),
            );
            return db;
        }
    }

    // (5) Nothing — dynamic.
    log::warn!(
        "native API unavailable (no extensionApiPath, no cached dump, auto-dump {}, embedded \
         fallback {}); native types degrade to dynamic — set godotBinaryPath or GDLS_GODOT",
        if options.auto_dump_extension_api {
            "found no source"
        } else {
            "disabled"
        },
        if options.embedded_api_fallback {
            "failed"
        } else {
            "disabled"
        },
    );
    NativeDb::empty()
}

/// Decide whether this session should auto-dump, and if so run it on a BACKGROUND thread:
/// the dump (a full Godot boot, seconds — or a 5 min timeout when the binary wedges) must never
/// sit between `initialize` and the first served request (issue #25). Returns the receiver the
/// event loop selects on, or `None` when no dump is warranted (fresh cache, kill switch, a
/// pinned `extensionApiPath`, no binary, no project, or a user-managed root file).
///
/// The thread does the whole job — spawn, drain, parse, move into `.gdls/`, write meta — and
/// reports a [`DumpOutcome`]; the loop's only duty on `Adopted` is `reload_native` +
/// republish. Mid-write watcher echoes of `<root>/extension_api.json` are harmless: the
/// reload path never downgrades a populated DB on a torn read, and the content hash dedupes
/// the post-adoption echo.
pub(crate) fn spawn_background_dump(
    options: &InitializationOptions,
    project: &ProjectModel,
    root: &Utf8Path,
) -> Option<crossbeam_channel::Receiver<DumpOutcome>> {
    if !options.auto_dump_extension_api {
        log::debug!("native API: auto-dump disabled by autoDumpExtensionApi=false");
        return None;
    }
    // A pinned explicit path makes the managed dump unservable — `load_native` resolves the
    // `Some(extensionApiPath)` arm without ever consulting the `.gdls/` ladder — so the boot
    // would be pure waste (adoption re-resolves the pinned path and dedupes to a no-op).
    if options.extension_api_path.is_some() {
        log::debug!("native API: extensionApiPath is pinned; auto-dump skipped");
        return None;
    }
    // Only for a real Godot project. A bare-`.gd` session whose root fell back to some cwd has
    // no project to dump against; booting Godot there is all cost and surprise.
    if !root.join("project.godot").as_std_path().exists() {
        log::debug!("native API: no project.godot at {root}; auto-dump skipped");
        return None;
    }
    // A pre-existing user file at the dump's output path means NO dump (never clobber) — the
    // resolution ladder already serves it as the unmanaged root-file source.
    if root.join("extension_api.json").as_std_path().exists() {
        log::debug!(
            "native API: project-root extension_api.json is user-managed; auto-dump skipped"
        );
        return None;
    }
    let Some(binary) = discover_binary(options) else {
        log::warn!(
            "native API: no Godot binary found (godotBinaryPath unset, GDLS_GODOT unset, \
             no godot4/godot on PATH); cannot auto-dump"
        );
        return None;
    };
    // Fresh managed dump ⇒ resolution step (1) already served it; nothing to do.
    let stale_reason = staleness_reason(root, project, Some(&binary))?;

    let (tx, rx) = crossbeam_channel::bounded::<DumpOutcome>(1);
    let root = root.to_path_buf();
    let project = project.clone();
    let spawned = std::thread::Builder::new()
        .name("gdls-api-dump".to_owned())
        .spawn(move || {
            let outcome = match run_dump(&binary, &root) {
                Ok(()) => match try_adopt_dump(&root, &project, &binary) {
                    Ok(db) => DumpOutcome::Adopted {
                        classes: db.class_count(),
                        version: version_label(&db).to_owned(),
                    },
                    Err(()) => DumpOutcome::Failed(
                        "dump produced but not adoptable (unparseable — quarantined)".to_owned(),
                    ),
                },
                Err(e) => DumpOutcome::Failed(e),
            };
            // A send error means the event loop dropped the receiver (session over) — fine.
            let _ = tx.send(outcome);
        });
    match spawned {
        Ok(_handle) => {
            log::info!("native API: auto-dump started in the background ({stale_reason})");
            Some(rx)
        }
        Err(e) => {
            log::warn!("native API: could not spawn the dump thread: {e}");
            None
        }
    }
}

/// The bundled stock-Godot class surface, gunzipped + ingested on demand. Regenerate the asset
/// from a stock binary of the pinned reference version:
/// `godot --headless --dump-extension-api` (no docs — types only, hover descriptions stay
/// empty under the fallback), then minify + `gzip -9` to
/// `assets/extension_api_4.6.3_stock.min.json.gz`.
///
/// `None` only if the embedded bytes fail to decompress/parse — corrupt vendored asset, caught
/// by `embedded_stock_db_loads` in CI — so callers degrade rather than unwrap.
pub(crate) fn embedded_stock_db() -> Option<NativeDb> {
    use std::io::Read;

    const EMBEDDED_GZ: &[u8] = include_bytes!("../assets/extension_api_4.6.3_stock.min.json.gz");
    let start = Instant::now();
    let mut text = String::new();
    if let Err(e) = flate2::read::GzDecoder::new(EMBEDDED_GZ).read_to_string(&mut text) {
        log::error!("native API: embedded stock dump failed to decompress: {e}");
        return None;
    }
    match NativeDb::from_json(&text) {
        Ok(mut db) => {
            db.set_provenance(gd_types::ApiProvenance::Generic);
            log::info!(
                "native API: embedded stock fallback ingested ({} classes, {} ms)",
                db.class_count(),
                start.elapsed().as_millis()
            );
            Some(db)
        }
        Err(e) => {
            log::error!("native API: embedded stock dump failed to parse: {e}");
            None
        }
    }
}

/// Discovery order: explicit `godotBinaryPath` → `GDLS_GODOT` env (empty or `off` hard-disables)
/// → `godot4` → `godot` on PATH (`which` handles Windows PATHEXT). The pure core is split out
/// for tests.
fn discover_binary(options: &InitializationOptions) -> Option<Utf8PathBuf> {
    discover_binary_with(
        options.godot_binary_path.as_deref(),
        std::env::var("GDLS_GODOT").ok().as_deref(),
        |name| which::which(name).ok(),
    )
}

fn discover_binary_with(
    explicit: Option<&str>,
    env_val: Option<&str>,
    path_lookup: impl Fn(&str) -> Option<PathBuf>,
) -> Option<Utf8PathBuf> {
    if let Some(p) = explicit {
        return Some(Utf8PathBuf::from(p));
    }
    if let Some(v) = env_val {
        let v = v.trim();
        if v.is_empty() || v.eq_ignore_ascii_case("off") {
            return None; // explicit disable
        }
        return Some(Utf8PathBuf::from(v));
    }
    for candidate in ["godot4", "godot"] {
        if let Some(p) = path_lookup(candidate) {
            if let Ok(u) = Utf8PathBuf::from_path_buf(p) {
                return Some(u);
            }
        }
    }
    None
}

/// `None` = fresh; `Some(reason)` = regenerate. Missing meta/dump, a different/changed binary,
/// or a changed `.gdextension` file set all invalidate.
fn staleness_reason(
    root: &Utf8Path,
    project: &ProjectModel,
    binary: Option<&Utf8Path>,
) -> Option<String> {
    if !dump_path(root).as_std_path().exists() {
        return Some("no cached dump".to_owned());
    }
    let meta: ApiDumpMeta = match std::fs::read_to_string(meta_path(root))
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
    {
        Some(m) => m,
        None => return Some("meta missing or unreadable".to_owned()),
    };
    if meta.meta_format_version != META_FORMAT_VERSION {
        return Some("meta format version changed".to_owned());
    }
    match binary {
        None => { /* no binary to compare — the cached dump is the best we have; treat as fresh */
        }
        Some(bin) => match file_stat(bin) {
            Some(cur) => {
                if cur.path != meta.binary.path
                    || cur.size != meta.binary.size
                    || cur.mtime_ns != meta.binary.mtime_ns
                {
                    return Some("godot binary changed".to_owned());
                }
            }
            None => return Some("godot binary unreadable".to_owned()),
        },
    }
    let current = gdextension_stats(root, project);
    if current != meta.gdextensions {
        return Some(".gdextension set changed".to_owned());
    }
    None
}

fn file_stat(path: &Utf8Path) -> Option<FileStat> {
    let meta = std::fs::metadata(path.as_std_path()).ok()?;
    Some(gd_project::cache::stat_from_metadata(
        path.to_path_buf(),
        &meta,
    ))
}

fn gdextension_stats(root: &Utf8Path, project: &ProjectModel) -> Vec<FileStat> {
    let mut stats: Vec<FileStat> = project
        .gdextensions
        .iter()
        .filter_map(|e| {
            let abs = if e.config.is_absolute() {
                e.config.clone()
            } else {
                root.join(&e.config)
            };
            file_stat(&abs)
        })
        .collect();
    stats.sort_by(|a, b| a.path.cmp(&b.path));
    stats
}

/// Spawn the dump. The child's cwd is the project root and `--path` names it explicitly; the
/// output lands at `<root>/extension_api.json` (Godot's fixed behavior). Guarded: a pre-existing
/// user file at that path means NO dump (never clobber — resolution step 3 will use it).
fn run_dump(binary: &Utf8Path, root: &Utf8Path) -> Result<(), String> {
    run_dump_with_timeout(binary, root, DUMP_TIMEOUT)
}

/// [`run_dump`] with an injectable deadline (production uses [`DUMP_TIMEOUT`]; tests shrink it
/// to exercise the kill path without the full 5 min wait).
fn run_dump_with_timeout(
    binary: &Utf8Path,
    root: &Utf8Path,
    timeout: Duration,
) -> Result<(), String> {
    let out_file = root.join("extension_api.json");
    if out_file.as_std_path().exists() {
        return Err(format!(
            "{out_file} already exists (user-managed); refusing to overwrite — remove it or set \
             extensionApiPath to use it explicitly"
        ));
    }

    let start = Instant::now();
    let mut cmd = Command::new(binary.as_std_path());
    cmd.args([
        "--headless",
        "--path",
        root.as_str(),
        "--dump-extension-api-with-docs",
    ])
    .current_dir(root.as_std_path())
    .stdin(Stdio::null())
    // stdout is OUR LSP wire — the child must never inherit it.
    .stdout(Stdio::piped())
    .stderr(Stdio::piped());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // CREATE_NO_WINDOW: no console flash when gdls is hosted by a GUI editor.
        cmd.creation_flags(0x0800_0000);
    }

    let mut child = cmd
        .spawn()
        .map_err(|e| format!("failed to spawn {binary}: {e}"))?;

    // Drain stdout/stderr CONCURRENTLY (issue #25): a chatty engine boot (warnings scale with
    // project size) can otherwise fill the 64 KB pipe buffer, block the child mid-dump, and
    // ride the whole thing into the timeout. Each drainer keeps only a bounded tail for the
    // failure log, reported over a channel — NOT a join handle, because a grandchild that
    // inherited the pipe (an orphaned helper process after our deadline kill) holds the write
    // end open past the child's death, and joining would hang on it.
    let stdout_pipe = child.stdout.take();
    let stderr_pipe = child.stderr.take();
    let (tail_tx, tail_rx) = crossbeam_channel::bounded::<String>(1);
    std::thread::spawn(move || {
        // stdout tail is uninteresting (Godot's banner) — drain it purely for flow control.
        drain_tail(stdout_pipe);
    });
    std::thread::spawn(move || {
        let _ = tail_tx.send(drain_tail(stderr_pipe));
    });

    // std::process has no timeout — poll, then kill + reap on the deadline.
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) => {
                if start.elapsed() > timeout {
                    let _ = child.kill();
                    let _ = child.wait();
                    break None;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(e) => return Err(format!("wait failed: {e}")),
        }
    };
    // Collect the stderr tail, briefly: after a clean exit it arrives immediately (pipe EOF);
    // after a kill, an orphaned grandchild may hold the pipe open indefinitely — give up after
    // a short grace and let the detached drainer die with the pipe.
    let stderr_tail = tail_rx
        .recv_timeout(Duration::from_millis(500))
        .unwrap_or_default();

    match status {
        None => {
            // Killed on the deadline. "The artifact decides, not the exit status" applies here
            // too (issue #25): a binary that wrote a complete dump and then wedged on shutdown
            // (Windows Error Reporting hold, audio-device teardown, …) still produced exactly
            // what we need — adoption parses it, so a torn file can't slip through.
            if out_file.as_std_path().exists() {
                log::warn!(
                    "native API: dump timed out after {}s and was killed, but a dump artifact \
                     exists — adopting it (a torn file fails the parse and is quarantined)",
                    timeout.as_secs()
                );
                Ok(())
            } else {
                Err(format!("timed out after {}s; killed", timeout.as_secs()))
            }
        }
        Some(st) => {
            // Godot 4.6.3 has been observed to abort on exit AFTER writing a complete dump —
            // the artifact decides, not the exit status.
            if !st.success() {
                log::debug!(
                    "native API: godot exited with {st} (dump may still be complete); stderr \
                     tail: {stderr_tail}"
                );
            }
            if out_file.as_std_path().exists() {
                log::info!(
                    "native API: dumped via {binary} in {} ms",
                    start.elapsed().as_millis()
                );
                Ok(())
            } else {
                Err(format!(
                    "godot exited ({st}) without producing {out_file}; stderr tail: {stderr_tail}"
                ))
            }
        }
    }
}

/// Read a child pipe to EOF, retaining only the last ~4 KB (failure-log material). Never
/// errors: a broken pipe mid-read just ends the drain with whatever tail was collected.
fn drain_tail<R: std::io::Read>(pipe: Option<R>) -> String {
    const TAIL_BYTES: usize = 4096;
    let Some(mut pipe) = pipe else {
        return String::new();
    };
    let mut tail: Vec<u8> = Vec::with_capacity(2 * TAIL_BYTES);
    let mut buf = [0u8; 8192];
    loop {
        match pipe.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                tail.extend_from_slice(&buf[..n]);
                if tail.len() > 2 * TAIL_BYTES {
                    let cut = tail.len() - TAIL_BYTES;
                    tail.drain(..cut);
                }
            }
        }
    }
    String::from_utf8_lossy(&tail).into_owned()
}

/// Move the fresh root dump into `.gdls/`, parse it, write the meta, run the GDExtension
/// post-check. Any failure quarantines/cleans and reports `Err` so resolution falls through.
fn try_adopt_dump(
    root: &Utf8Path,
    project: &ProjectModel,
    binary: &Utf8Path,
) -> Result<NativeDb, ()> {
    let produced = root.join("extension_api.json");
    let managed = dump_path(root);
    if let Err(e) = std::fs::create_dir_all(managed.parent().expect("managed path has parent")) {
        log::warn!("native API: mkdir .gdls failed: {e}");
        let _ = std::fs::remove_file(produced.as_std_path());
        return Err(());
    }
    if let Err(e) = std::fs::rename(produced.as_std_path(), managed.as_std_path()) {
        log::warn!("native API: moving dump into .gdls failed: {e}");
        let _ = std::fs::remove_file(produced.as_std_path());
        return Err(());
    }
    let db = match NativeDb::load(managed.as_str()) {
        Ok(db) => db,
        Err(e) => {
            quarantine(&managed, &format!("{e}"));
            return Err(());
        }
    };
    let version = version_label(&db).to_owned();
    log::info!(
        "native API: {} classes ({version}) -> {managed}",
        db.class_count()
    );

    // Meta write failure is non-fatal: the dump is used this session and simply re-runs next
    // start (costs one spawn, never serves wrong data).
    if let Some(binary_stat) = file_stat(binary) {
        let meta = ApiDumpMeta {
            meta_format_version: META_FORMAT_VERSION,
            binary: binary_stat,
            godot_version_full_name: version,
            gdextensions: gdextension_stats(root, project),
        };
        match serde_json::to_string_pretty(&meta) {
            Ok(json) => {
                if let Err(e) = std::fs::write(meta_path(root).as_std_path(), json) {
                    log::warn!("native API: meta write failed ({e}); will re-dump next start");
                }
            }
            Err(e) => log::warn!("native API: meta serialize failed: {e}"),
        }
    }

    // `.godot/extension_list.cfg` caveat: a never-imported project loads no extensions, so the
    // dump silently misses their classes. Detect the symptom and name the remediation.
    if !project.gdextensions.is_empty() {
        let any_hint_resolves = project
            .gdextensions
            .iter()
            .flat_map(|e| e.class_hints.iter())
            .any(|h| db.class_named(h).is_some());
        if !any_hint_resolves {
            log::info!(
                "native API: the project declares GDExtensions but none of their classes are in \
                 the dump — open the project once in the Godot editor (this generates \
                 .godot/extension_list.cfg) and restart gdls to capture them"
            );
        }
    }

    Ok(db)
}

/// The dump header's full version name, for logs; empty headers render as "unknown version".
fn version_label(db: &NativeDb) -> &str {
    let v = db.header().version_full_name.as_str();
    if v.is_empty() {
        "unknown version"
    } else {
        v
    }
}

/// Move an unparseable dump aside (mirrors `gd_project::cache`'s quarantine) so the next start
/// regenerates instead of re-failing on the same bytes.
fn quarantine(path: &Utf8Path, why: &str) {
    let to = Utf8PathBuf::from(format!("{path}.corrupt"));
    let moved = std::fs::rename(path.as_std_path(), to.as_std_path()).is_ok();
    log::warn!(
        "native API: {path} unparseable ({why}); {}",
        if moved {
            "quarantined to *.corrupt"
        } else {
            "quarantine rename failed"
        }
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discovery_order_explicit_env_path() {
        let lookup_hits = |name: &str| Some(PathBuf::from(format!("/fake/bin/{name}")));
        let lookup_misses = |_: &str| None;

        // Explicit beats everything.
        assert_eq!(
            discover_binary_with(Some("/opt/godot"), Some("/env/godot"), lookup_hits),
            Some(Utf8PathBuf::from("/opt/godot"))
        );
        // Env beats PATH.
        assert_eq!(
            discover_binary_with(None, Some("/env/godot"), lookup_hits),
            Some(Utf8PathBuf::from("/env/godot"))
        );
        // Empty / "off" env = hard disable, even with PATH hits available.
        assert_eq!(discover_binary_with(None, Some(""), lookup_hits), None);
        assert_eq!(discover_binary_with(None, Some("off"), lookup_hits), None);
        // PATH: godot4 preferred over godot.
        assert_eq!(
            discover_binary_with(None, None, lookup_hits),
            Some(Utf8PathBuf::from("/fake/bin/godot4"))
        );
        // Nothing anywhere.
        assert_eq!(discover_binary_with(None, None, lookup_misses), None);
    }

    /// CI guard on the vendored asset: the embedded stock dump must decompress, parse, carry
    /// `Generic` provenance, and contain the everyday classes whose absence caused the v1.0.1
    /// first-run false-positive storm (issue #24).
    #[test]
    fn embedded_stock_db_loads() {
        let db = embedded_stock_db().expect("embedded stock dump must ingest");
        assert_eq!(db.provenance(), gd_types::ApiProvenance::Generic);
        for class in ["Node", "Timer", "Marker3D", "CollisionObject3D", "Object"] {
            assert!(
                db.class_named(class).is_some(),
                "embedded stock dump is missing {class}"
            );
        }
    }

    /// `run_dump` behavior against fake "godot" binaries (issue #25). Shell-script fixtures, so
    /// unix-only — the logic under test (drain threads, deadline kill, artifact-decides) is
    /// platform-independent; Windows runs the embedded/discovery tests above.
    #[cfg(unix)]
    mod fake_binary {
        use super::*;

        const MINI_DUMP: &str = r#"{"header":{"version_major":4,"version_minor":6,"version_patch":3,"version_full_name":"Godot Engine v4.6.3.fake"},"classes":[{"name":"Object"},{"name":"Node","inherits":"Object"}]}"#;

        /// A project root and a fake godot whose behavior is the given shell script body.
        /// The script sees the dump target as `$root/extension_api.json` (run_dump sets the
        /// child's cwd to the root).
        fn fixture(script_body: &str) -> (tempfile::TempDir, Utf8PathBuf, Utf8PathBuf) {
            use std::os::unix::fs::PermissionsExt;
            let dir = tempfile::tempdir().expect("tempdir");
            let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("utf8 tempdir");
            std::fs::write(
                root.join("project.godot").as_std_path(),
                "config_version=5\n",
            )
            .expect("project.godot");
            let bin = root.join("fake-godot.sh");
            std::fs::write(bin.as_std_path(), format!("#!/bin/sh\n{script_body}\n"))
                .expect("fake binary");
            std::fs::set_permissions(bin.as_std_path(), std::fs::Permissions::from_mode(0o755))
                .expect("chmod");
            (dir, root, bin)
        }

        /// Crash-after-write (observed in the wild: a custom build aborts on audio-device
        /// teardown AFTER dumping): the artifact decides, not the exit status.
        #[test]
        fn nonzero_exit_with_artifact_is_ok() {
            let (_dir, root, bin) = fixture(&format!(
                "cat > extension_api.json <<'EOF'\n{MINI_DUMP}\nEOF\nexit 5"
            ));
            assert!(run_dump_with_timeout(&bin, &root, Duration::from_secs(10)).is_ok());
            assert!(root.join("extension_api.json").as_std_path().exists());
        }

        /// Write-then-wedge (Windows Error Reporting hold, device teardown hang): the deadline
        /// kill must still adopt the complete artifact instead of throwing it away (issue #25).
        #[test]
        fn timeout_with_artifact_is_ok() {
            let (_dir, root, bin) = fixture(&format!(
                "cat > extension_api.json <<'EOF'\n{MINI_DUMP}\nEOF\nsleep 60"
            ));
            let start = Instant::now();
            assert!(run_dump_with_timeout(&bin, &root, Duration::from_secs(1)).is_ok());
            assert!(
                start.elapsed() < Duration::from_secs(30),
                "must kill at the deadline, not wait for the child"
            );
            assert!(root.join("extension_api.json").as_std_path().exists());
        }

        /// A chatty child (engine warnings scale with project size) must not pipe-deadlock:
        /// without the concurrent drain threads this test wedges until the deadline and fails.
        #[test]
        fn chatty_child_does_not_deadlock() {
            // ~1 MB of stderr noise — far past the ~64 KB pipe buffer — then a valid dump.
            let (_dir, root, bin) = fixture(&format!(
                "i=0\nwhile [ $i -lt 16384 ]; do\n  printf 'WARNING: noisy engine boot line with some padding to make it long\\n' >&2\n  i=$((i+1))\ndone\ncat > extension_api.json <<'EOF'\n{MINI_DUMP}\nEOF\nexit 0"
            ));
            let start = Instant::now();
            assert!(run_dump_with_timeout(&bin, &root, Duration::from_secs(20)).is_ok());
            assert!(
                start.elapsed() < Duration::from_secs(15),
                "drain must keep the child flowing (took {:?})",
                start.elapsed()
            );
        }

        /// No artifact + nonzero exit = a real failure.
        #[test]
        fn no_artifact_is_err() {
            let (_dir, root, bin) = fixture("exit 1");
            assert!(run_dump_with_timeout(&bin, &root, Duration::from_secs(10)).is_err());
        }

        /// End-to-end: spawn_background_dump decision + thread + adoption. The fake binary
        /// writes a parseable mini dump; the outcome must be `Adopted` with the dump moved
        /// into `.gdls/` + meta written, and a follow-up resolution must serve it as the
        /// fresh managed source.
        #[test]
        fn background_dump_adopts_end_to_end() {
            let (_dir, root, bin) = fixture(&format!(
                "cat > extension_api.json <<'EOF'\n{MINI_DUMP}\nEOF\nexit 0"
            ));
            let options = InitializationOptions {
                godot_binary_path: Some(bin.to_string()),
                ..Default::default()
            };
            let project = ProjectModel::load(&root);
            let rx = spawn_background_dump(&options, &project, &root)
                .expect("stale/missing cache + binary => dump spawns");
            match rx.recv_timeout(Duration::from_secs(30)) {
                Ok(DumpOutcome::Adopted { classes, .. }) => assert_eq!(classes, 2),
                other => panic!("expected Adopted, got {other:?}"),
            }
            assert!(dump_path(&root).as_std_path().exists(), "managed dump");
            assert!(meta_path(&root).as_std_path().exists(), "meta");
            assert!(
                !root.join("extension_api.json").as_std_path().exists(),
                "root artifact moved into .gdls/"
            );
            // The next resolution serves the adopted dump as step (1) — and a fresh
            // spawn_background_dump declines (nothing stale).
            let db = resolve_native_db(&options, &project, &root);
            assert_eq!(db.provenance(), gd_types::ApiProvenance::Exact);
            assert_eq!(db.class_count(), 2);
            assert!(spawn_background_dump(&options, &project, &root).is_none());
        }

        /// A pinned extensionApiPath makes the managed dump unservable (`load_native` never
        /// consults the `.gdls/` ladder when the explicit path is set), so no background boot
        /// fires even when everything else — binary, project, missing cache — warrants one.
        #[test]
        fn pinned_extension_api_path_skips_dump() {
            let (_dir, root, bin) = fixture(&format!(
                "cat > extension_api.json <<'EOF'\n{MINI_DUMP}\nEOF\nexit 0"
            ));
            let options = InitializationOptions {
                godot_binary_path: Some(bin.to_string()),
                extension_api_path: Some(root.join("pinned.json").to_string()),
                ..Default::default()
            };
            let project = ProjectModel::load(&root);
            assert!(spawn_background_dump(&options, &project, &root).is_none());
        }
    }
}
