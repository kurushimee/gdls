//! Filesystem watcher: drives index freshness when files change outside open buffers.
//!
//! [`FileWatcher`] wraps [`notify_debouncer_full::Debouncer`] with a fixed 250 ms quiet-time so
//! atomic-write bursts (write `.tmp`, rename over) collapse into single events, and exposes the
//! debounced stream as a [`crossbeam_channel::Receiver`] that the main loop's
//! [`crossbeam_channel::select!`] can read alongside `lsp_server::Connection::receiver`. The
//! debouncer runs its own internal thread; the LSP main loop is the sole [`crate::Workspace`]
//! mutator (see `docs/03 §6.1`).
//!
//! Path classification + the exclusion filter live in [`classify`] / [`is_excluded`] (WP-W2),
//! consumed by `server::handle_watcher` (WP-W3). The dispatch never panics on a malformed event;
//! everything off the .gd / project-file path is dropped via [`Reaction::Other`].
//!
//! Lifecycle: a [`FileWatcher`] is constructed in `serve()` *after* [`crate::Workspace::load`]
//! finishes the cold scan — and after the `initialize` *response* has already been sent, so the
//! heavy scan never stalls the handshake. (The `initialized` notification is a client→server
//! message the server *receives*, not one it sends, so the watcher's construction is not ordered
//! against it.) The companion [`crate::Workspace::reconcile`] pass diffs the live tree against the
//! index to catch any events `notify` dropped during the heavy startup window (a documented
//! behavior on every supported platform; the `need_rescan` flag on a [`DebouncedEvent`] is the
//! live-stream analog).
//!
//! Source for the debouncer behaviour:
//! <https://docs.rs/notify-debouncer-full/0.6.0/notify_debouncer_full/>.

use std::time::Duration;

use anyhow::Context;
use camino::{Utf8Path, Utf8PathBuf};
use crossbeam_channel::{unbounded, Receiver};
use notify::{RecommendedWatcher, RecursiveMode};
use notify_debouncer_full::{
    new_debouncer_opt, DebounceEventResult, DebouncedEvent, Debouncer, NoCache,
};

/// Quiet-time the debouncer waits before emitting a coalesced event set. 250 ms round-trips
/// atomic-write bursts from editors that create `.tmp` + rename, matches the budget in
/// `docs/03 §6.1`, and stays well under the 1 s bulk-event budget asserted in WP-T1.
pub const DEBOUNCE_QUIET_TIME: Duration = Duration::from_millis(250);

/// What kind of project file an event concerns. M4's `server::handle_watcher` matches on this.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum Reaction {
    /// A `.gd` source file changed.
    ///
    /// `path` is the canonical *destination* path: for `Created` / `Modified` / `Deleted`
    /// it is the file the change happened to; for `Renamed { from, to }` it equals
    /// `change.to` (intentional ergonomic redundancy — handlers that only need "what file is
    /// this now?" read `path`; handlers that need both ends read `change`). The redundancy
    /// is enforced at construction in `classify_event`.
    GdSource {
        path: Utf8PathBuf,
        change: FileChange,
    },
    /// `project.godot` was modified — rebuild [`gd_project::ProjectModel`] + warn policy.
    ProjectGodot,
    /// `extension_api.json` (or the configured native dump path) was modified — re-ingest [`gd_types::NativeDb`].
    ExtensionApiJson,
    /// A `.gdextension` file was added/removed/modified — re-enumerate.
    Gdextension {
        path: Utf8PathBuf,
        change: FileChange,
    },
    /// M11 (#76): a `.tscn` scene file changed — re-index it in the [`gd_project::SceneIndex`]
    /// (or drop it on delete). `path` is the canonical destination path, same convention as
    /// [`Self::GdSource`]. Phase 1 keeps the scene index live; it does NOT yet re-diagnose the
    /// scene's attached scripts (the analyzer doesn't consume scenes until Phase 2).
    Scene {
        path: Utf8PathBuf,
        change: FileChange,
    },
    /// A doc-classes XML changed under an addon directory — re-merge into [`gd_types::NativeDb`].
    DocClassesXml { path: Utf8PathBuf },
    /// Anything else — dropped without action. WP-RD7 split the former catch-all `Other` to carry a
    /// [`SkipReason`] so a structured trace can tell a `.tmp`-file skip from a non-UTF-8-path drop
    /// (the `reaction` span discriminant in `server::reaction_kind` reads it) without the handler
    /// match needing observability-grade per-case variants.
    Other(SkipReason),
}

/// WP-RD7: why [`classify_event`] dropped an event into [`Reaction::Other`]. `#[non_exhaustive]`
/// so adding a future skip case doesn't break a downstream match.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum SkipReason {
    /// The event carried no usable (UTF-8) path.
    NoPath,
    /// The path is under an excluded directory (`.godot/`, `target/`, `.git/`, …).
    Excluded,
    /// A read-only `Access` event — never index-affecting.
    AccessOnly,
    /// A file with an extension gdls doesn't track (not `.gd` / `.gdextension` / doc-classes `.xml`).
    UnknownExtension,
    /// A file with no extension at all.
    NoExtension,
    /// An `.xml` that isn't under a `doc_classes/` directory.
    NotDocClasses,
}

impl SkipReason {
    /// Stable lowercase discriminant for the `watcher_event` span's `reaction` field.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            SkipReason::NoPath => "skip_no_path",
            SkipReason::Excluded => "skip_excluded",
            SkipReason::AccessOnly => "skip_access_only",
            SkipReason::UnknownExtension => "skip_unknown_extension",
            SkipReason::NoExtension => "skip_no_extension",
            SkipReason::NotDocClasses => "skip_not_doc_classes",
        }
    }
}

#[derive(Debug, Clone)]
pub enum FileChange {
    Created,
    Modified,
    Deleted,
    /// Both source and destination paths are known (the debouncer merged the create/remove pair).
    Renamed {
        from: Utf8PathBuf,
        to: Utf8PathBuf,
    },
}

/// Owns the [`Debouncer`] handle (dropping it stops the internal thread) and the receive end of
/// the debounced event stream.
pub struct FileWatcher {
    /// Kept alive: drop = thread stops. Field is unread by design — the channel is the API.
    ///
    /// `NoCache`, NOT the debouncer's `RecommendedCache`: on every platform except Linux/Android
    /// the recommended cache is a `FileIdMap` whose `add_root` synchronously walks the ENTIRE
    /// watched tree (no gdls exclusions — `.godot/`, `.git/` included) opening a handle per file
    /// to capture file IDs, and re-walks it on every rescan. On a 2.3k-script NTFS project that
    /// arming scan cost 7–9 s of startup and ~70 MB of RSS (issue #14). The file IDs exist only
    /// to pair rename halves; gdls already handles unpaired `From`/`To` events (the
    /// vanished-path → remove / `To` → modified-reindex arms in `server.rs`), so the cache buys
    /// nothing we need.
    _debouncer: Debouncer<RecommendedWatcher, NoCache>,
    rx: Receiver<DebounceEventResult>,
    /// The path handed to [`notify`] at construction. Retained so the main loop can re-stat it on
    /// a periodic tick (see [`Self::root_exists`]): notify's Windows backend silently `unwatch`es
    /// and stops emitting when the watched root is deleted/moved/unmounted, surfacing neither an
    /// error event nor a channel disconnect, so active liveness polling is the only signal there.
    root: Utf8PathBuf,
    /// WP-RD11 (5): the root directory's portable file identity (Windows file index + volume
    /// serial; Linux dev+inode) captured at construction via [`same_file`]. [`Self::root_exists`]
    /// compares the current identity against this so a delete+recreate — which leaves a *different*
    /// directory at the same path while notify keeps watching the dead inode — is detected as
    /// root-loss, not a false "still there". `None` when the capture failed (falls back to
    /// existence-only). Holding the handle for the session keeps the identity stable to compare
    /// against; `same_file` opens with shared access so it does not block the very delete we detect.
    root_handle: Option<same_file::Handle>,
}

impl FileWatcher {
    /// Start watching `root` recursively with the standard [`DEBOUNCE_QUIET_TIME`].
    ///
    /// The internal thread starts immediately; events begin to arrive on [`Self::events`]
    /// right away, so callers should construct *after* cold-index and call
    /// [`crate::Workspace::reconcile`] before forwarding events to the index (to swallow any
    /// drift the debouncer surfaces ahead of the reconciliation pass — reconcile is
    /// idempotent so double-application is harmless).
    pub fn new(root: &Utf8Path) -> anyhow::Result<Self> {
        Self::with_quiet_time(root, DEBOUNCE_QUIET_TIME)
    }

    /// Variant of [`Self::new`] taking an explicit debouncer quiet-time. Lets integration
    /// tests dial down the debounce window on a fast machine, or up on a slow CI runner,
    /// without hardcoding `DEBOUNCE_QUIET_TIME` into the test's wait budget. Production
    /// callers should use [`Self::new`].
    pub fn with_quiet_time(root: &Utf8Path, quiet_time: Duration) -> anyhow::Result<Self> {
        let (tx, rx) = unbounded::<DebounceEventResult>();
        // The Sender is accepted directly when the `crossbeam-channel` feature is on; no
        // adapter closure or bridge thread needed. `NoCache` — see the `_debouncer` field doc.
        let mut debouncer = new_debouncer_opt::<_, RecommendedWatcher, NoCache>(
            quiet_time,
            None,
            tx,
            NoCache::new(),
            notify::Config::default(),
        )
        .with_context(|| format!("FileWatcher::new failed to start debouncer for {root}"))?;
        debouncer
            .watch(root.as_std_path(), RecursiveMode::Recursive)
            .with_context(|| format!("FileWatcher failed to watch {root} recursively"))?;
        Ok(FileWatcher {
            _debouncer: debouncer,
            rx,
            root: root.to_path_buf(),
            // WP-RD11 (5): capture the root's file identity for the delete+recreate liveness check.
            root_handle: same_file::Handle::from_path(root.as_std_path()).ok(),
        })
    }

    /// Receiver end of the debounced event stream, for use in `crossbeam_channel::select!`.
    pub fn events(&self) -> &Receiver<DebounceEventResult> {
        &self.rx
    }

    /// The path this watcher was started against (the recursive watch root).
    pub fn root(&self) -> &Utf8Path {
        &self.root
    }

    /// Cheap, non-blocking liveness check: is the watch root still present *and the same
    /// directory*? Returns `false` when opening the root fails with
    /// [`std::io::ErrorKind::NotFound`] — the unambiguous "deleted / moved away / unmounted"
    /// signal, which is also how notify's Windows backend reaches the state where it has silently
    /// un-watched without emitting any error to the channel. Probes via
    /// [`same_file::Handle::from_path`], which opens the path *following* symlinks (like
    /// `File::open`, not `symlink_metadata`): when the root is itself a symlink, notify watches the
    /// resolved target, so a target that has been unmounted/deleted means freshness is dead even
    /// though the link node still lingers — opening through the link reports that correctly.
    ///
    /// Any *other* open error (a transient `PermissionDenied`, an I/O blip) returns `true` and is
    /// logged at debug, deliberately erring toward keeping a healthy watcher alive rather than
    /// tearing freshness down on a one-off hiccup (the "don't fire spuriously" guard).
    ///
    /// WP-RD11 (5): beyond existence, this also compares the root's portable file *identity*
    /// (`same_file`: Windows file index + volume serial, Linux dev+inode) against the one captured
    /// at construction. A delete+recreate leaves a directory at the path but a NEW identity, and
    /// notify is still watching the dead inode — so existence alone would falsely report "still
    /// watched" while freshness is actually dead. An identity mismatch returns `false` exactly like
    /// a delete, so the liveness arm disables the (inert) watcher and the WP-RD11 (4) reconcile
    /// fallback takes over.
    pub fn root_exists(&self) -> bool {
        match same_file::Handle::from_path(self.root.as_std_path()) {
            Ok(current) => match &self.root_handle {
                // Identity captured at construction: the watcher is live iff the path still points
                // at the SAME directory (not a recreated one notify never re-armed against).
                Some(original) => &current == original,
                // Couldn't capture identity at construction — fall back to existence-only (the
                // `Ok` here already proves the path exists).
                None => true,
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => false,
            Err(e) => {
                log::debug!(
                    "watcher: root liveness check of {} failed non-fatally ({e}); \
                     assuming still watched",
                    self.root
                );
                true
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Path classification + exclusion filter (WP-W2).
// ---------------------------------------------------------------------------

// The exclusion predicate now lives in `gd_project` so the cold index, `Workspace::reconcile`,
// and this watcher share one definition (they used to diverge — the cold index skipped only
// `.godot/`). Re-exported here so `watcher::is_excluded` keeps resolving for in-tree callers and
// the classifier below. Its tests moved with it (`gd_project::exclude`).
pub use gd_project::is_excluded;

/// Classify a single debounced event into a [`Reaction`]. Multiple paths in one event (the
/// rename case) are collapsed into a single [`Reaction::GdSource`] with [`FileChange::Renamed`]
/// when both ends are `.gd`; otherwise each path's reaction is determined independently and the
/// caller iterates [`classify_event`] for multi-path events.
///
/// The `project_root` argument is consulted only to recognize `extension_api.json` and
/// `project.godot` at the expected root; arbitrary `extension_api.json` under unrelated
/// directories (e.g. inside an addon's vendored copy) is not surfaced as
/// [`Reaction::ExtensionApiJson`] — only the project root's dump counts.
pub fn classify_event(event: &DebouncedEvent, project_root: &Utf8Path) -> Vec<Reaction> {
    use notify::event::{CreateKind, ModifyKind, RemoveKind, RenameMode};
    use notify::EventKind;

    // Convert `std::path::PathBuf` → `Utf8PathBuf` losing non-UTF-8 paths (drop them).
    let paths: Vec<Utf8PathBuf> = event
        .paths
        .iter()
        .filter_map(|p| Utf8PathBuf::from_path_buf(p.clone()).ok())
        .collect();
    if paths.is_empty() {
        log::debug!(
            "watcher: classifier dropping event (no usable paths — empty or non-UTF-8); kind={:?}",
            event.kind
        );
        return vec![Reaction::Other(SkipReason::NoPath)];
    }
    if paths.len() < event.paths.len() {
        // Some — but not all — paths were non-UTF-8 and silently dropped above. The dangerous case
        // is a two-path rename `Modify(Name(Both))` where one half is non-UTF-8: the surviving half
        // is then classified one-sided (a bare Created/Deleted) and the rename's other end never
        // updates, which can strand a stale interface in the index. Surface the partial drop
        // so an operator can `gdls diagnose --reconcile` rather than discover it as a
        // wrong nav result.
        log::warn!(
            "watcher: dropped {} non-UTF-8 path(s) from a {}-path event (kind={:?}); a partial \
             rename/move may leave a stale interface — run `gdls diagnose --reconcile` if freshness \
             looks off",
            event.paths.len() - paths.len(),
            event.paths.len(),
            event.kind
        );
    }

    // Rename with both ends → one merged reaction (the debouncer normalizes Modify(Name(Both))).
    if let EventKind::Modify(ModifyKind::Name(RenameMode::Both)) = event.kind {
        if let [from, to] = paths.as_slice() {
            // If either side is excluded, demote to a one-sided event.
            let from_excl = is_excluded(from, project_root);
            let to_excl = is_excluded(to, project_root);
            return match (from_excl, to_excl) {
                (true, true) => vec![Reaction::Other(SkipReason::Excluded)],
                (false, true) => vec![reaction_for_path(from, FileChange::Deleted, project_root)],
                (true, false) => vec![reaction_for_path(to, FileChange::Created, project_root)],
                (false, false) => match (from.extension(), to.extension()) {
                    (Some("gd"), Some("gd")) => vec![gd_source_renamed(from.clone(), to.clone())],
                    _ => vec![
                        reaction_for_path(from, FileChange::Deleted, project_root),
                        reaction_for_path(to, FileChange::Created, project_root),
                    ],
                },
            };
        }
        // Unexpected rename shape (single path or >2). Fall through to the generic per-path path.
    }

    // Map the EventKind to a coarse FileChange the rest of M4 cares about.
    let change = match &event.kind {
        EventKind::Create(_) => FileChange::Created,
        EventKind::Remove(_) => FileChange::Deleted,
        EventKind::Modify(ModifyKind::Name(_)) => FileChange::Modified, // partial rename info
        EventKind::Modify(_) => FileChange::Modified,
        EventKind::Access(_) => {
            log::debug!(
                "watcher: classifier dropping Access event for paths={paths:?} (read-only)",
            );
            return vec![Reaction::Other(SkipReason::AccessOnly)];
        }
        EventKind::Any | EventKind::Other => FileChange::Modified,
    };

    // Silence the unused-import warnings on platforms where these aren't matched.
    let _ = (CreateKind::Any, RemoveKind::Any);

    paths
        .iter()
        .filter(|p| !is_excluded(p, project_root))
        .map(|p| reaction_for_path(p, change.clone(), project_root))
        .collect()
}

/// Construct a `Reaction::GdSource` for a both-ends rename. The documented `path == change.to`
/// redundancy (which `Reaction::GdSource`'s doc comment promises) holds *by construction* here:
/// `path` is `to.clone()`. The two `debug_assert_eq!`s instead pin this constructor's *precondition*
/// — that both ends are `.gd` paths — so a future caller that routes a non-`.gd` rename through it
/// surfaces in dev tests rather than as a wrong reaction at the consumer. Production callers funnel
/// through this constructor instead of building the variant directly.
fn gd_source_renamed(from: Utf8PathBuf, to: Utf8PathBuf) -> Reaction {
    debug_assert_eq!(
        from.extension(),
        Some("gd"),
        "Renamed both-ends constructor requires from.extension() == .gd"
    );
    debug_assert_eq!(
        to.extension(),
        Some("gd"),
        "Renamed both-ends constructor requires to.extension() == .gd"
    );
    let path = to.clone();
    Reaction::GdSource {
        path,
        change: FileChange::Renamed { from, to },
    }
}

/// Construct a one-sided `Reaction::GdSource` (Created / Modified / Deleted — never `Renamed`),
/// pinning the documented invariant that for a one-sided change `path` IS the file the change
/// happened to. The `debug_assert!` complements [`gd_source_renamed`]'s (which pins
/// `path == change.to`) so BOTH constructors enforce `Reaction::GdSource`'s redundancy contract and
/// the variant is never built directly off the classification paths.
fn gd_source_one_sided(path: Utf8PathBuf, change: FileChange) -> Reaction {
    debug_assert!(
        !matches!(change, FileChange::Renamed { .. }),
        "gd_source_one_sided requires a one-sided change; Renamed goes through gd_source_renamed"
    );
    Reaction::GdSource { path, change }
}

/// M7 (#60): classify a client-delivered `workspace/didChangeWatchedFiles` event into the same
/// [`Reaction`] funnel the native watcher uses — identical exclusion filter and path
/// classification, so a client event can never reach an index mutation a native event couldn't.
/// (LSP has no rename events; a client-observed rename arrives as a Deleted + Created pair and
/// flows as two one-sided reactions, which the funnel already handles.)
pub fn classify_client_event(
    path: &Utf8Path,
    change: FileChange,
    project_root: &Utf8Path,
) -> Reaction {
    if is_excluded(path, project_root) {
        return Reaction::Other(SkipReason::Excluded);
    }
    reaction_for_path(path, change, project_root)
}

fn reaction_for_path(path: &Utf8Path, change: FileChange, project_root: &Utf8Path) -> Reaction {
    let name = path.file_name().unwrap_or("");
    let lower = name.to_ascii_lowercase();
    // `extension_api.json` at the project root → native-dump reaction.
    if lower == "extension_api.json" && path.parent() == Some(project_root) {
        return Reaction::ExtensionApiJson;
    }
    if lower == "project.godot" && path.parent() == Some(project_root) {
        return Reaction::ProjectGodot;
    }
    match path.extension() {
        Some("gd") => gd_source_one_sided(path.to_path_buf(), change),
        // M11 (#76): `.tscn` scene text → the scene index. `.scn` (binary) is deliberately not
        // handled — gdls parses scene TEXT only (anti-catalog W16) and the watcher doesn't watch it.
        Some("tscn") => Reaction::Scene {
            path: path.to_path_buf(),
            change,
        },
        Some("gdextension") => Reaction::Gdextension {
            path: path.to_path_buf(),
            change,
        },
        Some("xml") => {
            // Doc-classes XML lives under an addon's `doc_classes/` directory; we recognize that
            // pattern (any XML whose parent component name is `doc_classes` lowercase).
            if path
                .parent()
                .and_then(|p| p.file_name())
                .is_some_and(|n| n.eq_ignore_ascii_case("doc_classes"))
            {
                Reaction::DocClassesXml {
                    path: path.to_path_buf(),
                }
            } else {
                log::debug!(
                    "watcher: classifier dropping XML path {path} (not under doc_classes/)",
                );
                Reaction::Other(SkipReason::NotDocClasses)
            }
        }
        Some(ext) => {
            log::debug!("watcher: classifier dropping {path} (unhandled extension `.{ext}`)",);
            Reaction::Other(SkipReason::UnknownExtension)
        }
        None => {
            log::debug!("watcher: classifier dropping {path} (no extension)");
            Reaction::Other(SkipReason::NoExtension)
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    fn p(s: &str) -> Utf8PathBuf {
        Utf8PathBuf::from(s.replace('\\', "/"))
    }

    // The `is_excluded` unit tests moved with the predicate to `gd_project::exclude`. The
    // classifier tests below still exercise it indirectly via `classify_event`.

    #[test]
    fn classify_gd_create_returns_gdsource() {
        let root = p("/proj");
        let ev = mk_event(
            notify::EventKind::Create(notify::event::CreateKind::File),
            vec!["/proj/src/foo.gd"],
        );
        let r = classify_event(&ev, &root);
        assert!(matches!(
            r.as_slice(),
            [Reaction::GdSource {
                change: FileChange::Created,
                ..
            }]
        ));
    }

    #[test]
    fn classify_tscn_returns_scene() {
        // M11 (#76): a `.tscn` change must route to `Reaction::Scene`, not the `.gd` interner path
        // or an `Other` drop. This is what makes the `**/*.tscn` watcher glob's events actionable.
        let root = p("/proj");
        let ev = mk_event(
            notify::EventKind::Modify(notify::event::ModifyKind::Data(
                notify::event::DataChange::Any,
            )),
            vec!["/proj/src/Main.tscn"],
        );
        assert!(matches!(
            classify_event(&ev, &root).as_slice(),
            [Reaction::Scene {
                change: FileChange::Modified,
                ..
            }]
        ));
    }

    #[test]
    fn classify_client_tscn_returns_scene() {
        // The client-event funnel must classify `.tscn` identically to the native watcher.
        let root = p("/proj");
        let r = classify_client_event(&p("/proj/ui/Panel.tscn"), FileChange::Created, &root);
        assert!(matches!(
            r,
            Reaction::Scene {
                change: FileChange::Created,
                ..
            }
        ));
    }

    #[test]
    fn classify_scn_binary_is_not_a_scene_reaction() {
        // `.scn` (binary) is deliberately NOT handled — gdls parses scene TEXT only (W16). It falls
        // to the unknown-extension drop, never `Reaction::Scene`.
        let root = p("/proj");
        let r = classify_client_event(&p("/proj/ui/Panel.scn"), FileChange::Modified, &root);
        assert!(matches!(r, Reaction::Other(SkipReason::UnknownExtension)));
    }

    #[test]
    fn classify_extension_api_json_at_root() {
        let root = p("/proj");
        let ev = mk_event(
            notify::EventKind::Modify(notify::event::ModifyKind::Data(
                notify::event::DataChange::Any,
            )),
            vec!["/proj/extension_api.json"],
        );
        assert!(matches!(
            classify_event(&ev, &root).as_slice(),
            [Reaction::ExtensionApiJson]
        ));
    }

    #[test]
    fn classify_extension_api_json_under_addon_is_ignored() {
        // Only the project root's dump counts.
        let root = p("/proj");
        let ev = mk_event(
            notify::EventKind::Modify(notify::event::ModifyKind::Data(
                notify::event::DataChange::Any,
            )),
            vec!["/proj/addons/cool/extension_api.json"],
        );
        assert!(matches!(
            classify_event(&ev, &root).as_slice(),
            [Reaction::Other(_)]
        ));
    }

    #[test]
    fn classify_project_godot() {
        let root = p("/proj");
        let ev = mk_event(
            notify::EventKind::Modify(notify::event::ModifyKind::Data(
                notify::event::DataChange::Any,
            )),
            vec!["/proj/project.godot"],
        );
        assert!(matches!(
            classify_event(&ev, &root).as_slice(),
            [Reaction::ProjectGodot]
        ));
    }

    #[test]
    fn classify_excluded_path_returns_other() {
        let root = p("/proj");
        let ev = mk_event(
            notify::EventKind::Modify(notify::event::ModifyKind::Data(
                notify::event::DataChange::Any,
            )),
            vec!["/proj/.godot/uid_cache.bin"],
        );
        // is_excluded filter strips the path, so the iter is empty → empty Vec.
        let result = classify_event(&ev, &root);
        assert!(result.is_empty() || matches!(result.as_slice(), [Reaction::Other(_)]));
    }

    #[test]
    fn classify_rename_both_ends_gd_merges_to_single_renamed() {
        let root = p("/proj");
        let ev = mk_event(
            notify::EventKind::Modify(notify::event::ModifyKind::Name(
                notify::event::RenameMode::Both,
            )),
            vec!["/proj/src/old.gd", "/proj/src/new.gd"],
        );
        let result = classify_event(&ev, &root);
        match result.as_slice() {
            [Reaction::GdSource {
                change: FileChange::Renamed { from, to },
                ..
            }] => {
                assert!(from.as_str().ends_with("old.gd"));
                assert!(to.as_str().ends_with("new.gd"));
            }
            other => panic!("expected single Renamed GdSource, got {other:?}"),
        }
    }

    #[test]
    fn classify_doc_classes_xml() {
        let root = p("/proj");
        let ev = mk_event(
            notify::EventKind::Modify(notify::event::ModifyKind::Data(
                notify::event::DataChange::Any,
            )),
            vec!["/proj/addons/cool/doc_classes/MyClass.xml"],
        );
        assert!(matches!(
            classify_event(&ev, &root).as_slice(),
            [Reaction::DocClassesXml { .. }]
        ));
    }

    #[test]
    fn classify_unrelated_xml_is_other() {
        let root = p("/proj");
        let ev = mk_event(
            notify::EventKind::Modify(notify::event::ModifyKind::Data(
                notify::event::DataChange::Any,
            )),
            vec!["/proj/scenes/main.xml"],
        );
        assert!(matches!(
            classify_event(&ev, &root).as_slice(),
            [Reaction::Other(_)]
        ));
    }

    /// WP-RD14: the `gd_source_renamed` smart constructor enforces its `.gd`-both-ends precondition
    /// (a `debug_assert!`), so a non-`.gd` end must panic in debug rather than build a bad reaction.
    #[cfg(debug_assertions)]
    #[test]
    #[should_panic(expected = "from.extension() == .gd")]
    fn gd_source_renamed_rejects_non_gd_from() {
        let _ = gd_source_renamed(p("/proj/old.txt"), p("/proj/new.gd"));
    }

    /// WP-RD14: `gd_source_one_sided` rejects a `Renamed` change (those must go through
    /// `gd_source_renamed`), pinned by its `debug_assert!`.
    #[cfg(debug_assertions)]
    #[test]
    #[should_panic(expected = "one-sided change")]
    fn gd_source_one_sided_rejects_renamed() {
        let _ = gd_source_one_sided(
            p("/proj/x.gd"),
            FileChange::Renamed {
                from: p("/proj/a.gd"),
                to: p("/proj/b.gd"),
            },
        );
    }

    fn mk_event(kind: notify::EventKind, paths: Vec<&str>) -> DebouncedEvent {
        let event = notify::Event {
            kind,
            paths: paths.into_iter().map(std::path::PathBuf::from).collect(),
            attrs: notify::event::EventAttributes::default(),
        };
        DebouncedEvent::new(event, Instant::now())
    }

    // ----- Real-FS integration: spawn the watcher against a temp dir and verify events -----

    #[test]
    fn real_fs_create_emits_event_within_budget() {
        // Create a temp dir, start the watcher, write a .gd file, drain events for up to 2s.
        // 2s = debounce quiet-time (250ms) + filesystem latency + scheduler slack.
        let dir = tempfile::tempdir().expect("temp dir");
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("utf-8 temp dir");

        let watcher = FileWatcher::new(&root).expect("FileWatcher starts");

        // Write a .gd file after starting the watcher.
        std::fs::write(dir.path().join("hero.gd"), "extends Node\n").unwrap();

        // Drain events for up to 2s; expect at least one with `hero.gd` in the paths.
        let deadline = Instant::now() + Duration::from_secs(2);
        let mut saw_hero = false;
        while Instant::now() < deadline {
            match watcher.events().recv_timeout(Duration::from_millis(100)) {
                Ok(Ok(events)) => {
                    for ev in events {
                        if ev.paths.iter().any(|p| p.ends_with("hero.gd")) {
                            saw_hero = true;
                        }
                    }
                    if saw_hero {
                        break;
                    }
                }
                Ok(Err(_errors)) => {}
                Err(_timeout) => {}
            }
        }
        assert!(
            saw_hero,
            "FileWatcher did not surface a hero.gd event within 2s"
        );
    }

    /// WP-RD11 (5): `root_exists` reads a delete+recreate at the same path as root-loss (the new
    /// directory has a different `same_file` identity than the one captured at construction), not a
    /// false "still there". Skips gracefully where the recreate can't reproduce — on Windows the
    /// held notify + same_file handles can leave the name pending-delete so `create_dir` fails; the
    /// identity check is still exercised on Linux (inode reuse) and any FS that permits the recreate.
    #[test]
    fn root_exists_detects_identity_change_on_recreate() {
        let parent = tempfile::tempdir().expect("temp dir");
        let root = parent.path().join("proj");
        if std::fs::create_dir(&root).is_err() {
            return;
        }
        let Ok(root_utf8) = Utf8PathBuf::from_path_buf(root.clone()) else {
            return;
        };
        let Ok(watcher) = FileWatcher::new(&root_utf8) else {
            return;
        };
        assert!(
            watcher.root_exists(),
            "a freshly-created root must read as live"
        );

        if std::fs::remove_dir_all(&root).is_err() || std::fs::create_dir(&root).is_err() {
            // Pending-delete (Windows) or other platform limitation — can't reproduce the recreate.
            return;
        }
        assert!(
            !watcher.root_exists(),
            "a delete+recreate at the same path must read as root-loss (new same_file identity), \
             not a false 'still there'"
        );
    }

    #[test]
    fn rapid_writes_coalesce_into_far_fewer_events() {
        // The debouncer's whole reason to exist: a burst of writes to one file inside the
        // quiet-time window must collapse into far fewer DebouncedEvents than writes (atomic-save
        // editors do write-`.tmp`+rename; a user mashing save produces many Modify events in
        // <250 ms). Every other watcher test mutates once and asserts "an event arrived" — none
        // pins that coalescing actually happens, yet the per-save reindex budget (WP-T1) depends on
        // it; a regression that zeroed the quiet-time would pass every other test and storm the
        // index on every keystroke. `with_quiet_time` is used so the burst window (and the test's
        // wait budget) don't hardcode `DEBOUNCE_QUIET_TIME`.
        let dir = tempfile::tempdir().expect("temp dir");
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("utf-8 temp dir");
        let quiet = Duration::from_millis(200);
        let watcher = FileWatcher::with_quiet_time(&root, quiet).expect("FileWatcher starts");

        // Hammer one file far faster than the quiet-time: 12 writes over ~60 ms ≪ 200 ms window.
        let target = dir.path().join("hammered.gd");
        const WRITES: usize = 12;
        for i in 0..WRITES {
            std::fs::write(&target, format!("var x = {i}\n")).unwrap();
            std::thread::sleep(Duration::from_millis(5));
        }

        // Drain debounced batches until the stream goes quiet (no batch for a full 300 ms after the
        // burst settles). Count only the events the watcher actually ACTS on — `classify_event`'s
        // `Reaction::GdSource` (the index-affecting Create/Modify events whose per-save reindex
        // budget WP-T1 depends on coalescing). Counting *raw* events instead is what made this test
        // CI-flaky: notify-debouncer-full coalesces the burst's Create/Modify events for one path
        // into ~1, but it does NOT coalesce the `Access(Open/Close)` events each write also emits
        // (one open + one close per write). Those Access events are dropped by the production
        // classifier (`SkipReason::AccessOnly`) and are irrelevant to the reindex budget, yet on a
        // loaded CI runner ~2 Access events × 12 writes pushed the raw count to 24 — past `WRITES` —
        // and failed a perfectly-healthy debouncer. Classifying first measures the real contract.
        let mut index_events_for_target = 0usize;
        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline {
            match watcher.events().recv_timeout(Duration::from_millis(300)) {
                Ok(Ok(batch)) => {
                    for ev in &batch {
                        for reaction in classify_event(ev, &root) {
                            if let Reaction::GdSource { path, .. } = reaction {
                                if path.ends_with("hammered.gd") {
                                    index_events_for_target += 1;
                                }
                            }
                        }
                    }
                }
                Ok(Err(_errors)) => {}
                Err(_timeout) => break, // settled
            }
        }

        assert!(
            index_events_for_target >= 1,
            "the watcher must surface at least one index-affecting event for the hammered file"
        );
        assert!(
            index_events_for_target <= WRITES / 2,
            "the quiet-time must COALESCE the {WRITES}-write burst into far fewer index-affecting \
             events; got {index_events_for_target} (no coalescing would yield ~{WRITES})"
        );
    }
}
