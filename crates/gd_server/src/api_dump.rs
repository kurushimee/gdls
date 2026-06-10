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

/// Whether this `Workspace::load` call may spawn Godot for a stale/missing dump. Only the
/// session-startup path (`serve_inner`) passes `SpawnIfStale`; reloads mid-session, `gdls
/// diagnose`, and every direct test construction stay `NeverSpawn` — a `.gdextension` change
/// just marks the meta stale and the next startup re-dumps, so the single-threaded event loop
/// never blocks on a Godot boot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApiDumpPolicy {
    SpawnIfStale,
    NeverSpawn,
}

/// Bump independently of `gd_project::cache::CACHE_FORMAT_VERSION` when this file's shape changes.
const META_FORMAT_VERSION: u32 = 1;

/// Wall-clock budget for the dump. A cold Godot boot on a large project takes seconds; 60 s is
/// generous without letting a hung binary wedge startup forever.
const DUMP_TIMEOUT: Duration = Duration::from_secs(60);

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
/// fresh `.gdls` dump → auto-dump (policy + kill-switch + binary permitting) → stale `.gdls`
/// dump → `<root>/extension_api.json` (unmanaged user file) → empty (dynamic). One log line per
/// decision so an operator can always reconstruct which source won.
pub(crate) fn resolve_native_db(
    options: &InitializationOptions,
    project: &ProjectModel,
    root: &Utf8Path,
    policy: ApiDumpPolicy,
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

    // (2) Auto-dump — only for a real Godot project. A bare-`.gd` session whose root fell back
    // to some cwd has no project to dump against; booting Godot there is all cost and surprise.
    let is_godot_project = root.join("project.godot").as_std_path().exists();
    if options.auto_dump_extension_api && policy == ApiDumpPolicy::SpawnIfStale && !is_godot_project
    {
        log::debug!("native API: no project.godot at {root}; auto-dump skipped");
    }
    if options.auto_dump_extension_api && policy == ApiDumpPolicy::SpawnIfStale && is_godot_project
    {
        match &binary {
            Some(bin) => match run_dump(bin, root) {
                Ok(()) => {
                    if let Ok(db) = try_adopt_dump(root, project, bin) {
                        return db;
                    }
                }
                Err(e) => {
                    log::warn!("native API: auto-dump failed ({e}); falling back");
                }
            },
            None => {
                log::warn!(
                    "native API: no Godot binary found (godotBinaryPath unset, GDLS_GODOT unset, \
                     no godot4/godot on PATH); cannot auto-dump"
                );
            }
        }
    } else if !options.auto_dump_extension_api {
        log::debug!("native API: auto-dump disabled by autoDumpExtensionApi=false");
    }

    // (3) Stale managed dump — known provenance (made with project context) beats nothing.
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

    // (4) Unmanaged user file at the project root.
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

    // (5) Nothing — dynamic.
    log::warn!(
        "native API unavailable (no extensionApiPath, no cached dump, auto-dump {}); native \
         types degrade to dynamic — set godotBinaryPath or GDLS_GODOT",
        if options.auto_dump_extension_api {
            "found no source"
        } else {
            "disabled"
        }
    );
    NativeDb::empty()
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
/// user file at that path means NO dump (never clobber — resolution step 4 will use it).
fn run_dump(binary: &Utf8Path, root: &Utf8Path) -> Result<(), String> {
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

    // std::process has no timeout — poll, then kill + reap on the deadline.
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) => {
                if start.elapsed() > DUMP_TIMEOUT {
                    let _ = child.kill();
                    let _ = child.wait();
                    break None;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(e) => return Err(format!("wait failed: {e}")),
        }
    };
    // Drain piped output so the handles close (and keep the stderr tail for diagnostics).
    let output = child.wait_with_output().ok();
    let stderr_tail = output
        .as_ref()
        .map(|o| {
            let s = String::from_utf8_lossy(&o.stderr);
            s.chars()
                .rev()
                .take(400)
                .collect::<String>()
                .chars()
                .rev()
                .collect::<String>()
        })
        .unwrap_or_default();

    match status {
        None => Err(format!(
            "timed out after {}s; killed",
            DUMP_TIMEOUT.as_secs()
        )),
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
}
