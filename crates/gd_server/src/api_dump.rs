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
use gd_syntax::Dialect;
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
    dialect: Dialect,
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
        if let Some(db) = embedded_stock_db(dialect) {
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
            let outcome = run_dump_ladder(&binary, &root, &project);
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

/// The stock asset for one release. One `include_bytes!` arm per [`Dialect`] variant, so adding a
/// release is a compile error here until its asset is vendored — not a silent fallback onto the
/// wrong engine surface.
fn embedded_stock_bytes(dialect: Dialect) -> &'static [u8] {
    match dialect {
        Dialect::Godot4_6 => include_bytes!("../assets/extension_api_4.6.3_stock.min.json.gz"),
        Dialect::Godot4_7 => include_bytes!("../assets/extension_api_4.7.2_stock.min.json.gz"),
    }
}

/// The bundled stock-Godot class surface, gunzipped + ingested on demand.
///
/// It carries the DOCUMENTATION fields (#259): this is the first-run path for a user who installs
/// gdls with no Godot on `PATH`, and a docs-free asset gave them correct signatures with no prose
/// anywhere in the engine surface — silently, with nothing saying why.
///
/// There is one asset per supported feature release, picked by `dialect`: a 4.6 project asking a
/// 4.7 surface about its engine classes gets wrong signatures, wrong enums, and classes that do
/// not exist for it yet.
///
/// Regenerate with `scripts/regen-stock-api.py`, from a STOCK binary of the matching release
/// (`godot --headless --dump-extension-api-with-docs`, run outside any project so no GDExtension
/// gets baked in). That script keeps exactly the fields `gd_types::api` reads and drops the
/// GDExtension ABI sections gdls never touches, which pays for much of the prose, and it names the
/// output from the dump's own header so it cannot overwrite another release's asset.
///
/// `None` only if the embedded bytes fail to decompress/parse — corrupt vendored asset, caught
/// by `embedded_stock_db_loads` in CI — so callers degrade rather than unwrap.
pub(crate) fn embedded_stock_db(dialect: Dialect) -> Option<NativeDb> {
    use std::io::Read;

    let embedded_gz: &[u8] = embedded_stock_bytes(dialect);
    let start = Instant::now();
    let mut text = String::new();
    if let Err(e) = flate2::read::GzDecoder::new(embedded_gz).read_to_string(&mut text) {
        log::error!("native API: embedded stock dump failed to decompress: {e}");
        return None;
    }
    match NativeDb::from_json(&text) {
        Ok(mut db) => {
            db.set_provenance(gd_types::ApiProvenance::Generic);
            log::info!(
                "native API: embedded stock {dialect} fallback ingested ({} classes, {} ms)",
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

/// Windows-only Godot loader detail: before loading a GDExtension library, the engine copies it
/// to a sibling with a `~`-prefixed name and loads the COPY, so the original stays replaceable
/// while running. The copy name is fixed, so while one Godot process has an extension loaded,
/// a second one (our dump child) cannot create/replace the same `~` path — its load of that
/// extension fails, and the dump silently comes out without the extension's classes.
///
/// This walks each extension's addon directory for `~` loader copies and probes writability:
/// a copy a running editor still has mapped fails a write-mode open. Those are exactly the
/// extensions a dump taken right now will miss, which is worth naming BEFORE the silent miss.
///
/// `~RF*.TMP` siblings are Windows restart-manager leftovers from earlier failed replaces, not
/// live load copies — skipped without probing (there can be hundreds). Unix makes no `~` copies
/// and the loader there locks the original instead; the probe is a Windows-shaped no-op via the
/// always-false `is_locked`, so callers run unchanged on every platform.
fn locked_extension_copies(
    root: &Utf8Path,
    extensions: &[gd_project::gdextension::GdExtension],
) -> Vec<Utf8PathBuf> {
    #[cfg(windows)]
    {
        collect_locked_extension_copies(root, extensions, probe_write_locked)
    }
    #[cfg(not(windows))]
    {
        collect_locked_extension_copies(root, extensions, |_| false)
    }
}

/// The platform-independent core: enumerate candidate `~` loader copies and keep the ones the
/// injected probe reports as locked. Split from the platform probe for tests.
fn collect_locked_extension_copies(
    root: &Utf8Path,
    extensions: &[gd_project::gdextension::GdExtension],
    is_locked: impl Fn(&Utf8Path) -> bool,
) -> Vec<Utf8PathBuf> {
    let mut locked = Vec::new();
    for ext in extensions {
        let addon_dir = if ext.addon_dir.is_absolute() {
            ext.addon_dir.clone()
        } else {
            root.join(&ext.addon_dir)
        };
        if !addon_dir.as_std_path().is_dir() {
            continue;
        }
        // Loader copies sit next to the library they were made from — a couple of directory
        // levels down (e.g. `bin/`, `libs/`, `bin/windows/`) — never deeper.
        for entry in walkdir::WalkDir::new(addon_dir.as_std_path())
            .max_depth(3)
            .follow_links(false)
            .into_iter()
            .filter_map(Result::ok)
        {
            if !entry.file_type().is_file() {
                continue;
            }
            let Some(name) = entry.file_name().to_str() else {
                continue;
            };
            if !name.starts_with('~') || name.contains("~RF") {
                continue;
            }
            let Ok(path) = Utf8PathBuf::from_path_buf(entry.into_path()) else {
                continue;
            };
            if is_locked(&path) {
                locked.push(path);
            }
        }
    }
    locked.sort();
    locked
}

/// Write-mode open as the lock probe. A dll another Godot process has mapped was opened without
/// share-write, so a write access fails with a sharing violation; a copy nobody holds opens and
/// closes harmlessly. A NotFound race means the copy vanished — not a lock.
#[cfg(windows)]
fn probe_write_locked(path: &Utf8Path) -> bool {
    use std::fs::OpenOptions;
    match OpenOptions::new()
        .read(true)
        .write(true)
        .open(path.as_std_path())
    {
        Ok(_) => false,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => false,
        Err(_) => true,
    }
}

/// GDExtensions whose hinted classes are ALL absent from an adopted dump: their load failed in
/// the dump process, so the dump is partial for them. Empty means the dump captured every
/// declared extension (or the project declares none with class hints).
fn failing_extensions(
    db: &NativeDb,
    extensions: &[gd_project::gdextension::GdExtension],
) -> Vec<String> {
    let mut failed = Vec::new();
    for ext in extensions {
        if ext.class_hints.is_empty() {
            continue;
        }
        if !ext.class_hints.iter().any(|h| db.class_named(h).is_some()) {
            failed.push(ext.config.to_string());
        }
    }
    failed
}

/// The staged shadow tree for a locked-dump retry. Lives under `.gdls/` so nothing the dump
/// touches ever appears in the user's tree; a fixed path means a crashed run's leftovers are
/// simply replaced at the next staging.
fn shadow_root(root: &Utf8Path) -> Utf8PathBuf {
    root.join(".gdls").join("dump_shadow")
}

/// Stage a minimal project that reaches the SAME GDExtensions without touching any file a
/// running editor holds. The trick is where Godot's Windows loader puts its `~`-prefixed load
/// copy: a string sibling of the library path. The staged addon directories are REAL
/// directories (hardlinks to the original files where the filesystem allows, byte copies
/// otherwise), so the dump child's `~` copies land inside the shadow — fresh lock real estate —
/// instead of colliding with the editor's. Hardlinks make staging free for multi-MB SDK dlls;
/// the byte-copy fallback covers exFAT/network volumes where links are unavailable.
///
/// `project.godot` is minimal but carries the REAL `config/features` line verbatim: a
/// `.gdextension`'s library table keys on feature tags, and a shadow claiming the wrong feature
/// set loads the wrong (or no) library build. `.godot/extension_list.cfg` is GENERATED from the
/// hint-bearing extensions only: the real list's hint-less addons (native SDKs, renderer
/// helpers) can crash headless dump mode before it writes, and without class hints they have
/// nothing to contribute to the dump anyway.
fn stage_shadow_dump(root: &Utf8Path, project: &ProjectModel) -> Result<Utf8PathBuf, String> {
    let shadow = shadow_root(root);
    let _ = std::fs::remove_dir_all(shadow.as_std_path());
    std::fs::create_dir_all(shadow.join(".godot").as_std_path())
        .map_err(|e| format!("mkdir shadow/.godot: {e}"))?;

    let real = std::fs::read_to_string(root.join("project.godot").as_std_path())
        .map_err(|e| format!("read project.godot: {e}"))?;
    let features = real.lines().find_map(|line| {
        let t = line.trim();
        t.starts_with("config/features=").then(|| t.to_owned())
    });
    let mut pg =
        String::from("config_version=5\n\n[application]\n\nconfig/name=\"gdls dump shadow\"\n");
    if let Some(f) = features {
        pg.push_str(&f);
        pg.push('\n');
    }
    std::fs::write(shadow.join("project.godot").as_std_path(), pg)
        .map_err(|e| format!("write shadow project.godot: {e}"))?;

    // extension_list.cfg: GENERATED, listing only the hint-bearing extensions. The real list
    // would also load hint-less addons, and those are exactly the ones a dump neither needs nor
    // tolerates — native-SDK addons (Steam, EOS) and renderer helpers can crash headless dump
    // mode before it writes anything, and without class hints they have nothing to contribute.
    let hinting: Vec<&gd_project::gdextension::GdExtension> = project
        .gdextensions
        .iter()
        .filter(|e| !e.class_hints.is_empty())
        .collect();
    let mut cfg_text = String::new();
    for ext in &hinting {
        if let Some(res) = gd_project::paths::path_to_res(root, &ext.config) {
            cfg_text.push_str(&res);
            cfg_text.push('\n');
        }
    }
    std::fs::write(
        shadow
            .join(".godot")
            .join("extension_list.cfg")
            .as_std_path(),
        cfg_text,
    )
    .map_err(|e| format!("write shadow extension_list.cfg: {e}"))?;

    // One mirror per distinct hint-bearing addon directory — two extensions can share one.
    let mut seen: Vec<std::path::PathBuf> = Vec::new();
    for ext in &hinting {
        let addon = if ext.addon_dir.is_absolute() {
            ext.addon_dir.clone()
        } else {
            root.join(&ext.addon_dir)
        };
        let Ok(canon) = std::fs::canonicalize(addon.as_std_path()) else {
            continue; // addon dir gone since enumeration — its extension can't load anyway
        };
        if seen.contains(&canon) {
            continue;
        }
        seen.push(canon);
        mirror_addon_dir(root, &addon, &shadow)?;
    }
    Ok(shadow)
}

/// Mirror one addon directory tree into the shadow: hardlink every regular file, byte-copy when
/// linking fails. Restart-manager litter (`~…~RF*.TMP`) and VCS metadata are skipped; other
/// loader copies (`~`-prefixed) are skipped too, but every mirrored dynamic library gets a FRESH
/// `~` twin next to it: on Windows the engine loads the `~` copy when one exists, and CRASHES
/// (0xC0000005, observed on 4.7.2 headless dump runs) when it has to create the copy itself
/// inside the shadow. Pre-creating it is what makes the staged dump load the extension at all.
fn mirror_addon_dir(
    root: &Utf8Path,
    real_dir: &Utf8Path,
    shadow_base: &Utf8Path,
) -> Result<(), String> {
    for entry in walkdir::WalkDir::new(real_dir.as_std_path())
        .follow_links(false)
        .into_iter()
        .filter_map(Result::ok)
    {
        if !entry.file_type().is_file() {
            continue;
        }
        let Ok(src) = Utf8PathBuf::from_path_buf(entry.path().to_path_buf()) else {
            continue;
        };
        let name = src.file_name().unwrap_or_default();
        if name.starts_with('~') || name.starts_with(".git") {
            continue;
        }
        let Ok(rel) = src.strip_prefix(root) else {
            continue; // not under the project root — nothing sane to mirror it to
        };
        let dst = shadow_base.join(rel);
        if let Some(parent) = dst.parent() {
            std::fs::create_dir_all(parent.as_std_path())
                .map_err(|e| format!("mkdir {}: {e}", dst.parent().unwrap_or(&dst)))?;
        }
        if std::fs::hard_link(src.as_std_path(), dst.as_std_path()).is_err() {
            std::fs::copy(src.as_std_path(), dst.as_std_path())
                .map_err(|e| format!("stage {src}: {e}"))?;
        }
        if is_dynamic_library(name) {
            let twin = dst.with_file_name(format!("~{name}"));
            if !twin.as_std_path().exists()
                && std::fs::hard_link(dst.as_std_path(), twin.as_std_path()).is_err()
            {
                let _ = std::fs::copy(dst.as_std_path(), twin.as_std_path());
            }
        }
    }
    Ok(())
}

/// The library extensions a GDExtension's platform table can resolve to on this machine. The
/// loader's `~`-copy treatment applies to these, so only these get pre-created twins.
fn is_dynamic_library(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    lower.ends_with(".dll") || lower.ends_with(".so") || lower.ends_with(".dylib")
}

/// The dump ladder. Rung 1: the direct dump — unchanged behavior whenever no loader copy is
/// locked and the result captures every declared extension. Rung 2: the staged shadow dump,
/// taken whenever the direct path was (or would be) partial. Rung 3: fall back to whatever the
/// direct run produced anyway — a stock-surface dump with precise per-extension notices still
/// beats no dump, and the stale meta makes the next start retry the ladder.
fn run_dump_ladder(binary: &Utf8Path, root: &Utf8Path, project: &ProjectModel) -> DumpOutcome {
    let locked = locked_extension_copies(root, &project.gdextensions);
    if !locked.is_empty() {
        log::warn!(
            "native API: {} Godot loader cop{} under the addon directories {} locked by another \
             Godot process (an open editor has the project's extensions mapped): {} — a direct \
             dump would miss those extensions",
            locked.len(),
            if locked.len() == 1 { "y" } else { "ies" },
            if locked.len() == 1 { "is" } else { "are" },
            list_up_to(&locked, 3),
        );
    }

    let adopt = |produced: &Utf8Path, locked: &[Utf8PathBuf]| {
        try_adopt_dump(produced, root, project, binary, locked)
    };

    // Rung 1 — direct.
    let mut best: Option<NativeDb> = None;
    if run_dump(binary, root).is_ok() {
        match adopt(&root.join("extension_api.json"), &locked) {
            Ok(db) => {
                if failing_extensions(&db, &project.gdextensions).is_empty() {
                    return adopted_outcome(&db);
                }
                best = Some(db); // partial — keep it as the fallback while trying the shadow
            }
            Err(()) => {}
        }
    }

    // Rung 2 — staged shadow (booting one is only worth it when some extension carries class
    // hints — a hint-less project has nothing to capture from a shadow). Even an unlocked
    // direct dump lands here when its result is partial (e.g. a never-imported project: the
    // shadow generates the extension list the real project lacks).
    let shadow_worthwhile = project
        .gdextensions
        .iter()
        .any(|e| !e.class_hints.is_empty());
    if shadow_worthwhile {
        match stage_shadow_dump(root, project) {
            Ok(shadow) => {
                let produced = shadow.join("extension_api.json");
                let run = run_dump(binary, &shadow);
                let shadow_adopt = if run.is_ok() && produced.as_std_path().exists() {
                    adopt(&produced, &locked)
                } else {
                    match &run {
                        Err(e) => log::warn!("native API: shadow dump failed: {e}"),
                        Ok(()) => log::warn!("native API: shadow dump produced no artifact"),
                    }
                    Err(())
                };
                let _ = std::fs::remove_dir_all(shadow.as_std_path());
                match shadow_adopt {
                    Ok(db) => {
                        if failing_extensions(&db, &project.gdextensions).is_empty() {
                            return adopted_outcome(&db);
                        }
                        best = best.or(Some(db));
                    }
                    Err(()) => {
                        log::warn!("native API: shadow dump not adoptable (quarantined)")
                    }
                }
            }
            Err(e) => log::warn!("native API: shadow staging failed: {e}"),
        }
    }

    // Rung 3 — the best dump we have, partial or not (notices name the gaps).
    if let Some(db) = best {
        return adopted_outcome(&db);
    }
    DumpOutcome::Failed("no adoptable dump (direct and shadow both failed)".to_owned())
}

fn adopted_outcome(db: &NativeDb) -> DumpOutcome {
    DumpOutcome::Adopted {
        classes: db.class_count(),
        version: version_label(db).to_owned(),
    }
}

/// Bounded path list for log lines.
fn list_up_to(paths: &[Utf8PathBuf], max: usize) -> String {
    let shown: Vec<&str> = paths.iter().take(max).map(|p| p.as_str()).collect();
    let mut s = shown.join(", ");
    if paths.len() > max {
        s.push_str(&format!(", and {} more", paths.len() - max));
    }
    s
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

    // ETXTBSY retry: exec transiently fails with "Text file busy" while the binary is open
    // for write anywhere — a Godot build mid-link/mid-copy on the user's side, or another
    // thread's fork briefly holding a just-written file's fd until its own exec (CLOEXEC
    // closes at exec, not at fork). The condition clears in milliseconds; retry briefly
    // before treating it as fatal.
    let mut child = loop {
        match cmd.spawn() {
            Ok(child) => break child,
            Err(e)
                if e.kind() == std::io::ErrorKind::ExecutableFileBusy
                    && start.elapsed() < Duration::from_millis(500) =>
            {
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(e) => return Err(format!("failed to spawn {binary}: {e}")),
        }
    };

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

/// Move the fresh dump into `.gdls/`, parse it, write the meta, and name any extension whose
/// classes the dump missed. `produced` is the dump artifact wherever the ladder staged it (the
/// project root for the direct run, the shadow for a staged one); `root` is always the REAL
/// project (meta keys and notices reference it). Any failure quarantines/cleans and reports
/// `Err` so the ladder falls through.
fn try_adopt_dump(
    produced: &Utf8Path,
    root: &Utf8Path,
    project: &ProjectModel,
    binary: &Utf8Path,
    locked: &[Utf8PathBuf],
) -> Result<NativeDb, ()> {
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

    // Per-extension post-check: an extension whose hinted classes are ALL absent from the dump
    // had its load fail in the dump process — name it, and name the likely cause so the fix is
    // a decision, not a hunt.
    for ext in &project.gdextensions {
        if ext.class_hints.is_empty() {
            continue;
        }
        if ext.class_hints.iter().any(|h| db.class_named(h).is_some()) {
            continue;
        }
        let cause = if locked.is_empty() {
            "the project may never have been imported — open it once in the Godot editor (this \
             generates .godot/extension_list.cfg) and restart gdls to capture them"
        } else {
            "Godot loader copies under the addon directories were locked by another Godot \
             process at dump time — close it and delete .gdls/extension_api.json to re-dump \
             with the extension's classes"
        };
        log::warn!(
            "native API: GDExtension {}: none of its {} hinted class(es) made it into the dump \
             — {cause}",
            ext.config,
            ext.class_hints.len()
        );
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
    /// `Generic` provenance, contain the everyday classes whose absence caused the v1.0.1
    /// first-run false-positive storm (issue #24), ingest the full Variant utility set so the
    /// canonical registry and the dump can never silently drift apart, and CARRY DOCUMENTATION
    /// (#259) — a dump regenerated with `--dump-extension-api` instead of
    /// `--dump-extension-api-with-docs` parses perfectly and silently empties every hover on the
    /// first-run path, which is exactly the failure this pins.
    #[test]
    fn embedded_stock_db_loads() {
        for dialect in [Dialect::Godot4_6, Dialect::Godot4_7] {
            embedded_stock_db_loads_for(dialect);
        }
    }

    fn embedded_stock_db_loads_for(dialect: Dialect) {
        let db = embedded_stock_db(dialect).expect("embedded stock dump must ingest");
        let (major, minor) = dialect.version();
        assert_eq!(
            (db.header().version_major, db.header().version_minor),
            (major, minor),
            "the {dialect} asset is not a {dialect} dump"
        );
        assert_eq!(db.provenance(), gd_types::ApiProvenance::Generic);
        for class in ["Node", "Timer", "Marker3D", "CollisionObject3D", "Object"] {
            assert!(
                db.class_named(class).is_some(),
                "embedded stock dump is missing {class}"
            );
        }
        // #259: prose at every level hover reads it from — the class itself, one of its methods,
        // and one of its properties.
        let node = db.class_named("Node").expect("Node is present");
        assert!(
            !node.brief_description.is_empty() && !node.description.is_empty(),
            "the embedded dump must carry class documentation — regenerate it with \
             `--dump-extension-api-with-docs` via scripts/regen-stock-api.py"
        );
        assert!(
            node.methods
                .iter()
                .any(|m| db.name_of(m.name) == "add_child" && !m.description.is_empty()),
            "the embedded dump must carry per-method documentation"
        );
        assert!(
            node.properties.iter().any(|p| !p.description.is_empty()),
            "the embedded dump must carry per-property documentation"
        );
        // Every canonical Variant utility must survive the ingest path
        // (extension_api `utility_functions` → `NativeDb::utility`)...
        for name in gd_types::VARIANT_UTILITY_FUNCTIONS {
            assert!(
                db.utility(name).is_some(),
                "embedded stock dump did not ingest Variant utility {name}"
            );
        }
        // ...and the dump must carry no more than the registry names (count parity ⇒ the two sets
        // are equal, so a drift in either direction trips this guard). The dump's
        // `utility_functions` section is the Variant family only — GDScript-only utilities live in
        // the analyzer's hard-coded table, not the dump — so this count covers exactly the registry.
        assert_eq!(
            db.utility_count(),
            gd_types::VARIANT_UTILITY_FUNCTIONS.len(),
            "embedded dump Variant-utility count drifted from the canonical registry"
        );
    }

    /// The dialect actually picks the surface, and the two surfaces are not interchangeable: 4.7
    /// added engine classes 4.6 has no idea about. Without this, a wrong-release asset would fail
    /// only as mysterious diagnostics in a user's project.
    #[test]
    fn each_dialect_gets_its_own_engine_surface() {
        let old = embedded_stock_db(Dialect::Godot4_6).expect("4.6 stock dump must ingest");
        let new = embedded_stock_db(Dialect::Godot4_7).expect("4.7 stock dump must ingest");
        for class in ["AccessibilityServer", "AreaLight3D", "AwaitTweener"] {
            assert!(
                new.class_named(class).is_some(),
                "{class} must be in the 4.7 surface"
            );
            assert!(
                old.class_named(class).is_none(),
                "{class} does not exist at 4.6 — the 4.6 asset is the wrong dump"
            );
        }
        // Everything 4.6 knows, 4.7 still knows: the surface only grew.
        assert!(new.class_count() > old.class_count());
    }

    /// The RAW, un-seeded embedded stock dump (parsed straight into [`gd_types::api::ExtensionApi`],
    /// NOT via `embedded_stock_db()` — `NativeDb::from_json` runs the seed *during* ingest, so the
    /// seeded names would already be present and the subset check below would be vacuous).
    fn raw_stock_extension_api(dialect: Dialect) -> gd_types::api::ExtensionApi {
        use std::io::Read;
        let mut text = String::new();
        flate2::read::GzDecoder::new(embedded_stock_bytes(dialect))
            .read_to_string(&mut text)
            .expect("embedded stock dump must decompress");
        serde_json::from_str(&text).expect("embedded stock dump must parse as ExtensionApi")
    }

    /// #172: cross-dump drift/corruption tripwire for `DUMP_OMITTED_NATIVE_METHODS`. The table is
    /// regenerated against a Godot binary by `scripts/regen-dump-omitted-methods.sh` (binary-only,
    /// so not a CI step); these invariants run binary-free against the REAL embedded stock dump
    /// (only reachable from `gd_server`) so a bump of the vendored dump that leaves the table stale
    /// fails CI. The complementary structural invariants live in `gd_types::native_db` tests.
    #[test]
    fn dump_omitted_methods_are_the_strict_omitted_set_of_the_stock_dump() {
        // The table is one shared list across releases, so it has to hold against every vendored
        // dump: a name ClassDB resolves but the dump omits is an editor-side omission, not a
        // per-release API change.
        for dialect in [Dialect::Godot4_6, Dialect::Godot4_7] {
            dump_omitted_methods_hold_for(dialect);
        }
    }

    fn dump_omitted_methods_hold_for(dialect: Dialect) {
        use std::collections::{HashMap, HashSet};

        let api = raw_stock_extension_api(dialect);

        // The table must be regenerated FOR the versions the vendored dumps ship — a bump that
        // does not regenerate the table is exactly the silent drift #172 guards against.
        let (major, minor) = dialect.version();
        assert_eq!(
            (api.header.version_major, api.header.version_minor),
            (major, minor),
            "embedded stock dump version != the version DUMP_OMITTED_NATIVE_METHODS was generated \
             for — regenerate the table with scripts/regen-dump-omitted-methods.sh"
        );

        // Raw per-class OWN-method name sets, straight from the un-seeded dump.
        let raw_own: HashMap<&str, HashSet<&str>> = api
            .classes
            .iter()
            .map(|c| {
                (
                    c.name.as_str(),
                    c.methods.iter().map(|m| m.name.as_str()).collect(),
                )
            })
            .collect();

        for &(class, method, _) in gd_types::DUMP_OMITTED_NATIVE_METHODS {
            // (a) every table class is present in the stock dump (a seed for an absent class is dead).
            let own = raw_own.get(class).unwrap_or_else(|| {
                panic!(
                    "DUMP_OMITTED_NATIVE_METHODS class {class:?} is absent from the {dialect} \
                     stock dump"
                )
            });
            // (b) no table row is ALREADY an own-method of the dump — the table is strictly the
            //     OMITTED set; a row the dump now carries means the dump moved and the table is stale.
            assert!(
                !own.contains(method),
                "{class}::{method} is already an own-method of the {dialect} stock dump — it is \
                 no longer omitted; regenerate DUMP_OMITTED_NATIVE_METHODS"
            );
        }

        // The seed END-TO-END: known omitted methods the RAW dump lacks resolve on the SEEDED DB.
        // `Object::free` (the lone non-`_` omission) and `CanvasItem::_edit_get_rect` (a per-class
        // editor method) both miss the raw dump but must resolve post-seed.
        let seeded = embedded_stock_db(dialect).expect("embedded stock dump must ingest");
        for (class, method) in [("Object", "free"), ("CanvasItem", "_edit_get_rect")] {
            assert!(
                !raw_own[class].contains(method),
                "{class}::{method} unexpectedly present in the raw dump — pick another seed probe"
            );
            assert!(
                seeded.lookup_member(class, method).is_some(),
                "{class}::{method} must resolve on the seeded embedded DB"
            );
        }
    }

    /// Two-extension fixture: real `project.godot` with a features line, a real
    /// `extension_list.cfg`, and two addon dirs carrying libraries, a loader copy, restart-
    /// manager litter, and VCS metadata.
    fn ext_fixture() -> (tempfile::TempDir, Utf8PathBuf) {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("utf8 tempdir");
        std::fs::write(
            root.join("project.godot").as_std_path(),
            "config_version=5\n\n[application]\n\nconfig/name=\"T\"\nconfig/features=PackedStringArray(\"4.7\", \"Forward Plus\")\n",
        )
        .expect("project.godot");
        std::fs::create_dir_all(root.join(".godot").as_std_path()).expect(".godot");
        std::fs::write(
            root.join(".godot").join("extension_list.cfg").as_std_path(),
            "res://addons/foo/foo.gdextension\nres://addons/bar/bar.gdextension\n",
        )
        .expect("extension_list.cfg");
        std::fs::create_dir_all(root.join("addons/foo/bin").as_std_path()).expect("foo/bin");
        std::fs::write(
            root.join("addons/foo/foo.gdextension").as_std_path(),
            "[configuration]\nentry_symbol=\"foo_lib_init\"\n\n[icons]\nFoo=\"res://addons/foo/icon.svg\"\n",
        )
        .expect("foo.gdextension");
        std::fs::write(root.join("addons/foo/bin/lib.dll").as_std_path(), b"lib").expect("lib");
        std::fs::write(root.join("addons/foo/bin/~lib.dll").as_std_path(), b"copy").expect("~copy");
        std::fs::write(
            root.join("addons/foo/bin/~lib.dll~RF1.TMP").as_std_path(),
            b"litter",
        )
        .expect("rf litter");
        std::fs::write(root.join("addons/foo/.gitignore").as_std_path(), b"*\n")
            .expect(".gitignore");
        std::fs::create_dir_all(root.join("addons/bar/win64").as_std_path()).expect("bar/win64");
        std::fs::write(
            root.join("addons/bar/bar.gdextension").as_std_path(),
            "[configuration]\nentry_symbol=\"bar_lib_init\"\n\n[icons]\nBar=\"res://addons/bar/icon.svg\"\n",
        )
        .expect("bar.gdextension");
        std::fs::write(root.join("addons/bar/win64/lib.dll").as_std_path(), b"lib").expect("lib");
        (dir, root)
    }

    /// A live loader copy a running editor still holds mapped is reported; restart-manager
    /// litter (never a live copy) is skipped without probing.
    #[test]
    fn locked_probe_skips_rf_litter_and_reports_real_copies() {
        let (_dir, root) = ext_fixture();
        let exts = ext_fixture_extensions(&root);
        let locked = collect_locked_extension_copies(&root, &exts, |p| {
            p.file_name().is_some_and(|n| n == "~lib.dll")
        });
        assert_eq!(locked, vec![root.join("addons/foo/bin/~lib.dll")]);
    }

    /// The shadow mirrors every addon dir (hardlink or copy — indistinguishable to a reader),
    /// The shadow mirrors every hint-bearing addon dir (hardlink or copy — indistinguishable to
    /// a reader), carries the real features line verbatim, generates the extension list from the
    /// hints, and never stages loader copies or VCS metadata.
    #[test]
    fn shadow_staging_mirrors_addons_and_carries_the_real_config() {
        let (_dir, root) = ext_fixture();
        let project = ProjectModel::load(&root);
        let shadow = stage_shadow_dump(&root, &project).expect("staging succeeds");
        let pg = std::fs::read_to_string(shadow.join("project.godot").as_std_path())
            .expect("shadow project.godot");
        assert!(
            pg.contains("config/features=PackedStringArray(\"4.7\", \"Forward Plus\")"),
            "the features line must be copied verbatim: {pg}"
        );
        let cfg = std::fs::read_to_string(
            shadow
                .join(".godot")
                .join("extension_list.cfg")
                .as_std_path(),
        )
        .expect("shadow extension_list.cfg");
        assert!(
            cfg.contains("res://addons/foo/foo.gdextension")
                && cfg.contains("res://addons/bar/bar.gdextension"),
            "generated list covers every hint-bearing extension: {cfg}"
        );
        assert!(shadow.join("addons/foo/bin/lib.dll").as_std_path().exists());
        assert!(
            shadow
                .join("addons/foo/bin/~lib.dll")
                .as_std_path()
                .exists(),
            "a fresh loader-copy twin must be pre-created for every mirrored library"
        );
        assert!(shadow
            .join("addons/bar/win64/lib.dll")
            .as_std_path()
            .exists());
        assert_eq!(
            std::fs::read(shadow.join("addons/foo/bin/lib.dll").as_std_path()).expect("read"),
            b"lib",
            "staged content parity (hardlink or copy)"
        );
        assert!(
            shadow
                .join("addons/foo/bin/~lib.dll")
                .as_std_path()
                .exists(),
            "a fresh loader-copy twin must be pre-created for every mirrored library"
        );
        assert!(
            !shadow
                .join("addons/foo/bin/~lib.dll~RF1.TMP")
                .as_std_path()
                .exists(),
            "restart-manager litter must never be staged"
        );
        assert!(
            !shadow.join("addons/foo/.gitignore").as_std_path().exists(),
            "VCS metadata must never be staged"
        );
    }

    /// A hint-less extension (no `[icons]` — e.g. a native-SDK addon) is excluded from the
    /// shadow entirely: not listed in the extension list, not mirrored. Loading it buys the
    /// dump nothing and native-SDK addons are the ones that crash dump mode.
    #[test]
    fn shadow_staging_skips_hintless_extensions_entirely() {
        let (_dir, root) = ext_fixture();
        std::fs::write(
            root.join("addons/bar/bar.gdextension").as_std_path(),
            "[configuration]\nentry_symbol=\"bar_lib_init\"\n",
        )
        .expect("strip bar icons");
        let project = ProjectModel::load(&root);
        let shadow = stage_shadow_dump(&root, &project).expect("staging succeeds");
        let cfg = std::fs::read_to_string(
            shadow
                .join(".godot")
                .join("extension_list.cfg")
                .as_std_path(),
        )
        .expect("generated cfg");
        assert!(
            cfg.contains("res://addons/foo/foo.gdextension"),
            "the hint-bearing extension stays: {cfg}"
        );
        assert!(
            !cfg.contains("bar.gdextension"),
            "the hint-less extension must not be listed: {cfg}"
        );
        assert!(
            !shadow.join("addons/bar").as_std_path().exists(),
            "the hint-less addon must not be mirrored"
        );
    }

    /// The validator names an extension only when EVERY hint is absent: a fully-absent extension
    /// means its load failed; a partially-covered one is per-name territory (#480), not a dump
    /// failure; no hints means nothing to check.
    #[test]
    fn failing_extensions_names_only_the_fully_absent_ones() {
        let db = NativeDb::from_json(
            r#"{"header":{"version_major":4,"version_minor":6,"version_patch":3,"version_full_name":"Godot Engine v4.6.3.fake"},"classes":[{"name":"Foo"}]}"#,
        )
        .expect("mini db");
        let exts = vec![
            gd_project::gdextension::GdExtension {
                config: Utf8PathBuf::from("res://a/a.gdextension"),
                addon_dir: Utf8PathBuf::from("res://addons/a"),
                class_hints: vec!["Foo".to_owned()],
            },
            gd_project::gdextension::GdExtension {
                config: Utf8PathBuf::from("res://b/b.gdextension"),
                addon_dir: Utf8PathBuf::from("res://addons/b"),
                class_hints: vec!["Ghost".to_owned()],
            },
            gd_project::gdextension::GdExtension {
                config: Utf8PathBuf::from("res://c/c.gdextension"),
                addon_dir: Utf8PathBuf::from("res://addons/c"),
                class_hints: vec!["Foo".to_owned(), "Ghost".to_owned()],
            },
            gd_project::gdextension::GdExtension {
                config: Utf8PathBuf::from("res://d/d.gdextension"),
                addon_dir: Utf8PathBuf::from("res://addons/d"),
                class_hints: vec![],
            },
        ];
        assert_eq!(
            failing_extensions(&db, &exts),
            vec!["res://b/b.gdextension".to_owned()],
            "only the fully-absent extension is a dump failure"
        );
    }

    /// Helper: the two extensions the fixture's project.godot/extension_list.cfg describe.
    fn ext_fixture_extensions(root: &Utf8Path) -> Vec<gd_project::gdextension::GdExtension> {
        vec![
            gd_project::gdextension::GdExtension {
                config: Utf8PathBuf::from("res://addons/foo/foo.gdextension"),
                addon_dir: root.join("addons/foo"),
                class_hints: vec!["Foo".to_owned()],
            },
            gd_project::gdextension::GdExtension {
                config: Utf8PathBuf::from("res://addons/bar/bar.gdextension"),
                addon_dir: root.join("addons/bar"),
                class_hints: vec!["Bar".to_owned()],
            },
        ]
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
            run_dump_with_timeout(&bin, &root, Duration::from_secs(10))
                .expect("artifact present => Ok regardless of exit status");
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
            run_dump_with_timeout(&bin, &root, Duration::from_secs(1))
                .expect("complete artifact at the deadline kill => Ok");
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
            run_dump_with_timeout(&bin, &root, Duration::from_secs(20))
                .expect("a chatty child must still dump successfully");
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
            let db = resolve_native_db(&options, &project, &root, Dialect::NEWEST);
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
