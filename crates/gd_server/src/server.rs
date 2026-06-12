//! The LSP server: lifecycle handshake, capability advertisement, and the synchronous event loop
//! that dispatches requests and notifications (`docs/05-lsp-cc-integration.md`).

use std::time::{Duration, Instant};

use anyhow::Result;
use camino::{Utf8Path, Utf8PathBuf};
use crossbeam_channel::{select, Receiver, Sender};
use lsp_server::{Connection, Message, Notification, Request, Response};
use lsp_types::{
    CallHierarchyServerCapability, Diagnostic, DiagnosticSeverity, DidChangeTextDocumentParams,
    DidCloseTextDocumentParams, DidOpenTextDocumentParams, DidSaveTextDocumentParams,
    DocumentLinkOptions, HoverProviderCapability, ImplementationProviderCapability,
    InitializeParams, InitializeResult, OneOf, PublishDiagnosticsParams, ServerCapabilities,
    ServerInfo, TextDocumentSyncCapability, TextDocumentSyncKind, Uri,
};
use notify_debouncer_full::{DebounceEventResult, DebouncedEvent};
use rustc_hash::FxHashSet;

use crate::bench::{self, BenchRecorder};
use crate::cancellation::{CancellationToken, REQUEST_CANCELLED};
use crate::config::InitializationOptions;
use crate::handlers;
use crate::memory::{Bytes, MemoryBudget};
use crate::observability::{MemoryPressure, RssSampler};
use crate::position::{PositionEncoding, PositionMapper};
use crate::uri::{uri_to_path, CanonicalKey};
use crate::vfs::Vfs;
use crate::watcher::{self, FileChange, FileWatcher, Reaction};
use crate::workspace::Workspace;
use gd_syntax::ParseTree;
use lsp_server::RequestId;
use lsp_types::{CancelParams, NumberOrString};
use rustc_hash::FxHashMap;

// JSON-RPC error codes (LSP uses the JSON-RPC reserved range).
const ERR_METHOD_NOT_FOUND: i32 = -32601;
const ERR_INVALID_PARAMS: i32 = -32602;
/// LSP 3.17 `ContentModified` (-32801). Used by the WP-H1 Hard-pressure gate as "the server is
/// intentionally not answering"; per the spec it signals the client to retry — exactly the
/// behavior we want once peak RSS drops back below Hard.
const ERR_CONTENT_MODIFIED: i32 = -32801;

/// How often the event loop re-stats the watch root to detect silent watcher death (notify's
/// Windows backend un-watches a deleted/moved/unmounted root without emitting an error — see
/// [`crate::watcher::FileWatcher::root_exists`]). 3 s is responsive enough for an operator to see
/// the actionable error promptly while a single `symlink_metadata` every 3 s is a non-event for CPU.
const WATCHER_LIVENESS_INTERVAL: Duration = Duration::from_secs(3);

/// Client capabilities gdls branches on, captured once at `initialize` (the position encoding
/// negotiates separately into [`ServerState::encoding`]).
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct ClientCaps {
    /// `textDocument.documentSymbol.hierarchicalDocumentSymbolSupport`. Absent ⇒ `false` ⇒ the
    /// flat 3.16 `SymbolInformation[]` documentSymbol shape (rust-analyzer's
    /// `.unwrap_or_default()` convention): a client that did not opt in must not receive the
    /// nested shape it declined.
    pub(crate) hierarchical_document_symbols: bool,
}

impl ClientCaps {
    fn negotiate(caps: &lsp_types::ClientCapabilities) -> Self {
        ClientCaps {
            hierarchical_document_symbols: caps
                .text_document
                .as_ref()
                .and_then(|t| t.document_symbol.as_ref())
                .and_then(|d| d.hierarchical_document_symbol_support)
                .unwrap_or(false),
        }
    }
}

/// All mutable server state for one session.
pub struct ServerState {
    pub(crate) encoding: PositionEncoding,
    /// Client capabilities captured at `initialize` — see [`ClientCaps`].
    pub(crate) caps: ClientCaps,
    /// Parsed `initializationOptions`. Consumed by [`Workspace::load`] at startup to seed the
    /// native API path and the strict-mode policy; retained on the server state so the
    /// filesystem watcher can call [`Workspace::reload_project_and_native`] and
    /// [`Workspace::reload_native`] on `project.godot` / `extension_api.json` changes without
    /// re-`initialize`-ing.
    pub(crate) options: InitializationOptions,
    /// Native DB + project model + interface index + parse cache (the `res://` root lives in
    /// `workspace.project.root`).
    pub(crate) workspace: Workspace,
    pub(crate) vfs: Vfs,
    pub(crate) sender: Sender<Message>,
    /// WP-P3: optional ring-buffer recorder for the `bench --record` / `--replay` flow. `Some`
    /// when [`crate::bench::BenchRecorder::from_env`] or [`serve_with_recorder`] injected one;
    /// `None` is the production path (no recording cost).
    pub(crate) recorder: Option<BenchRecorder>,
    /// M5 WP-O2: cross-platform peak-RSS sampler. Constructed before [`Workspace::load`] so its
    /// baseline reading captures the server-start memory floor (pre-cold-index). Sampled at the
    /// four lifecycle points (baseline, post-cold-index, end-of-each-watcher-batch, shutdown)
    /// plus the 3-second liveness ticker; consumed by the Phase-H verification report via
    /// [`RssSampler::peak`] and by the WP-H1 pressure ladder via [`RssSampler::pressure`]'s
    /// rolling window.
    pub(crate) rss: RssSampler,
    /// M5 WP-O4: per-request cancellation tokens. The [`dispatch_request`] handler
    /// inserts a fresh token here keyed by the LSP request id BEFORE invoking the handler, and
    /// removes it once the handler returns. The `$/cancelRequest` notification arm in
    /// [`dispatch_notification`] looks up the id and calls
    /// [`CancellationToken::cancel`] on the token; the handler's analyzer pass sees the flip on
    /// its next checkpoint (every 256 nodes) and bails with a synthetic diagnostic. A cancel
    /// for an id NOT in this map is a warn-log no-op (LSP 3.17 spec: unknown id is allowed).
    pub(crate) pending_requests: FxHashMap<RequestId, CancellationToken>,
    /// M5 WP-O4: the token for the currently-dispatching request. Handler code that wants to
    /// thread cancellation to [`Workspace::analyze_with_options`] reads this. Cloned before
    /// the borrow on `state.workspace` to avoid an aliasing borrow. `Some` only while inside
    /// the [`dispatch_request`] macro; cleared back to `None` on handler return.
    pub(crate) current_token: Option<CancellationToken>,
    /// M5 WP-H1: resolved memory budget owned by the session. Read by the WP-H1 ticker arm to
    /// classify the current peak RSS into a [`MemoryPressure`] level (via
    /// [`RssSampler::pressure`]) and act on the transition. Resolved once at startup from
    /// `bench/budget.toml` + `initializationOptions.memory` overrides (see
    /// [`crate::memory::MemoryBudget::resolve`]).
    pub(crate) budget: MemoryBudget,
    /// M5 WP-H1: the last observed pressure level. Tracked so the ladder fires the per-level
    /// tracing event exactly **once on transition** rather than on every tick the level is
    /// held — e.g. a session sitting at Soft for an hour emits one
    /// `memory_soft_cap_evicted` event, not 1 200 of them. Defaults to `Normal` at startup; a
    /// first-tick transition to Soft/Hard fires the corresponding event.
    pub(crate) memory_pressure: MemoryPressure,
    /// Per-session cache of rendered native API pages (#34) — see
    /// [`crate::stubs::StubCache`]. Interior-mutable so shared-`&` request paths fill it;
    /// keyed by the dump's content hash, so a mid-session dump adoption invalidates it
    /// naturally.
    pub(crate) stub_cache: crate::stubs::StubCache,
}

/// Build and run the server on stdio. This is the binary entry point's worker.
pub fn run() -> Result<()> {
    run_with_recorder(BenchRecorder::from_env())
}

/// `run` with an explicitly-injected [`BenchRecorder`]. The plain [`run`] wrapper sources the
/// recorder from `$GDLS_BENCH_RECORD_TO`; the `bench --record` CLI subcommand calls this
/// directly so it doesn't have to mutate the global environment.
pub fn run_with_recorder(recorder: Option<BenchRecorder>) -> Result<()> {
    crate::logging::init();
    log::info!("gdls {} starting on stdio", env!("CARGO_PKG_VERSION"));
    let (connection, io_threads) = Connection::stdio();
    serve_with_recorder(connection, recorder)?;
    io_threads.join()?;
    log::info!("gdls stopped");
    Ok(())
}

/// Run the server over an arbitrary [`Connection`] (stdio in production, in-memory in tests).
/// Equivalent to [`serve_with_recorder`] with the recorder sourced from the gating env var.
pub fn serve(connection: Connection) -> Result<()> {
    serve_with_recorder(connection, BenchRecorder::from_env())
}

/// Where the event loop sources filesystem-watcher events (WP-RD3). `Real` constructs a live
/// [`FileWatcher`] on the project root (production); `Injected` takes a caller-supplied receiver so
/// the dark watcher branches (channel death, fatal `notify` errors, reactions, `need_rescan`) can
/// be driven deterministically from `tests/watcher_event_loop.rs` without depending on real OS
/// events. No real watcher handle exists in the injected case, so the liveness ticker is inert.
enum WatcherSource {
    Real,
    Injected(Receiver<DebounceEventResult>),
}

/// `serve` with an explicitly-injected [`BenchRecorder`]. Used by `bench --record` (passes its
/// own recorder built from the CLI arg) and by `tests/bench_record.rs` (avoids global env
/// mutation in test code).
pub fn serve_with_recorder(connection: Connection, recorder: Option<BenchRecorder>) -> Result<()> {
    serve_inner(connection, recorder, WatcherSource::Real)
}

/// WP-RD3 test seam: run the full server lifecycle but with the filesystem watcher's event
/// receiver INJECTED rather than constructed from a real [`FileWatcher`]. The caller (the
/// `watcher_event_loop` integration test) holds the `Sender` half and feeds
/// [`DebounceEventResult`]s — including `Err(_)` batches (fatal notify errors) and a dropped sender
/// (channel death) — to drive each dark branch of the [`run_event_loop`] watcher arm deterministically.
pub fn serve_with_injected_watcher(
    connection: Connection,
    watcher_rx: Receiver<DebounceEventResult>,
) -> Result<()> {
    serve_inner(connection, None, WatcherSource::Injected(watcher_rx))
}

fn serve_inner(
    connection: Connection,
    recorder: Option<BenchRecorder>,
    watcher_source: WatcherSource,
) -> Result<()> {
    // --- Lifecycle: split handshake so we can read the client's offered encodings first. ---
    let (init_id, init_value) = connection.initialize_start()?;
    let init: InitializeParams = serde_json::from_value(init_value)?;

    let encoding = PositionEncoding::negotiate(&init.capabilities);
    let caps = ClientCaps::negotiate(&init.capabilities);
    let options = InitializationOptions::parse(init.initialization_options.as_ref());
    let root = resolve_root(&options, &init);

    let result = InitializeResult {
        capabilities: capabilities(encoding),
        server_info: Some(ServerInfo {
            name: "gdls".to_string(),
            version: Some(env!("CARGO_PKG_VERSION").to_string()),
        }),
    };
    connection.initialize_finish(init_id, serde_json::to_value(result)?)?;
    log::info!(
        "gdls ready (root={root}, encoding={encoding:?}, strict={:?})",
        options.strict.profile
    );

    // M5 WP-O2: construct the RSS sampler BEFORE `Workspace::load` so its baseline reading is
    // taken at the server-start memory floor — before the cold-index walk allocates the parse +
    // analysis caches + interface index. Emit the baseline as the first `peak_rss_bytes` event so
    // a structured-trace consumer sees the whole pressure curve starting from t≈0.
    let mut rss = RssSampler::new();
    rss.sample_now("baseline");

    // M5 WP-H1: resolve the soft/hard RSS budget BEFORE the cold-index walk so a too-low cap
    // would be reported via the first ticker tick of the live session. The bench/budget.toml
    // file is sibling to the gdls binary's working directory in production (the operator's
    // project root) — same path the WP-P1 calibration writes to. A missing file is the
    // documented WP-H1 fallback (defaults + one warn).
    let budget = MemoryBudget::resolve(&options.memory, bench_budget_path().as_deref());
    tracing::info!(
        name = "memory_budget_resolved",
        soft_cap_mb = budget.soft_cap_mb(),
        hard_cap_mb = budget.hard_cap_mb(),
        source = ?budget.source(),
        "memory budget resolved"
    );

    // Arm the filesystem watcher BEFORE the workspace loads (issue #14): every modification that
    // lands while the load's stat pass runs is then a queued channel event replayed once the
    // loop arms — which is exactly what lets the startup reconcile run in `DiscoverOnly` mode
    // (enumeration-only for known files) instead of re-statting the whole tree. The watcher only
    // needs the root path; `ProjectModel::load` stores that same path verbatim. WP-RD3: the
    // injected source replaces the real watcher with a caller-fed receiver (no handle,
    // never-ticker) so the dark watcher branches are drivable from `tests/watcher_event_loop.rs`.
    let (watcher, mut watcher_rx, ticker): (
        Option<FileWatcher>,
        Option<Receiver<DebounceEventResult>>,
        Receiver<Instant>,
    ) = match watcher_source {
        WatcherSource::Real => {
            let arm_start = Instant::now();
            let watcher = match FileWatcher::new(&root) {
                Ok(w) => Some(w),
                Err(e) => {
                    log::warn!("FileWatcher disabled: {e}; freshness will be open-buffer-only");
                    None
                }
            };
            // Issue #14 attribution: arming used to hide a full-tree FileIdMap scan inside this
            // call; keep the timing visible so a regression can't masquerade as reconcile cost.
            log::info!(
                "watcher: armed on {root} in {} ms",
                arm_start.elapsed().as_millis()
            );
            let rx = watcher.as_ref().map(|w| w.events().clone());
            (
                watcher,
                rx,
                crossbeam_channel::tick(WATCHER_LIVENESS_INTERVAL),
            )
        }
        WatcherSource::Injected(rx) => (None, Some(rx), crossbeam_channel::never::<Instant>()),
    };

    // Build the workspace (native DB + project model + cold index) only after the `initialize`
    // response is sent, so a large scan never stalls the handshake (WP-F: start inline). The
    // load itself never spawns Godot (issue #25): it resolves the best CURRENTLY-available
    // native source (cached dump → root file → embedded stock → empty) and serves on that.
    let workspace = Workspace::load(&root, &options);
    // Kick the auto-dump off on a background thread when the cache is stale/missing — the dump
    // (a full Godot boot; up to a 5 min timeout when the binary wedges) must never sit between
    // `initialize` and the first served request. Its completion is a select! arm below: adopt,
    // reload, republish — the first run converges to the same state as every later run,
    // mid-session.
    let mut dump_rx = crate::api_dump::spawn_background_dump(&options, &workspace.project, &root);
    let post_cold_index_rss = rss.sample_now("post_cold_index");
    // M5 WP-H1: a one-shot startup check so an operator gets an immediate, actionable signal when
    // the resolved cap is already below this project's steady-state working set — rather than
    // discovering it later as silently-shed navigation on a ticker tick. The pressure ladder can
    // only evict the bounded parse/analysis caches; it cannot shed the interface index + native DB,
    // which dominate RSS on a large tree, so an index already over a cap will not recover.
    warn_if_cold_index_exceeds_budget(post_cold_index_rss, &budget);
    let mut state = ServerState {
        encoding,
        caps,
        options,
        workspace,
        vfs: Vfs::default(),
        sender: connection.sender.clone(),
        recorder,
        rss,
        pending_requests: FxHashMap::default(),
        current_token: None,
        budget,
        memory_pressure: MemoryPressure::Normal,
        stub_cache: crate::stubs::StubCache::default(),
    };

    // At startup no buffers are open yet, so this set is empty; building it via the same helper the
    // watcher uses keeps the "open buffer wins" rule uniform and correct even if a future change
    // opens a buffer before this point. The dirty set this reconcile produces is drained lazily —
    // each file clears its own bit on first `analyze`, so there is no
    // startup-time republish to perform here.
    let open_paths = open_buffer_paths(&state);
    // The settle pass. With a real watcher armed (BEFORE the load, above), every known file was
    // just stat-validated by the load itself and modifications in the gap are queued events —
    // so the backstop only needs added/removed discovery (`DiscoverOnly`). Without a live
    // watcher (construction failed, or the injected test seam) freshness is already degraded,
    // so buy the full stat-diff insurance.
    let reconcile_mode = if watcher.is_some() {
        crate::workspace::ReconcileMode::DiscoverOnly
    } else {
        crate::workspace::ReconcileMode::FullStat
    };
    let report = state.workspace.reconcile_with(reconcile_mode, &open_paths);
    // M5 WP-O1 — preserved verbatim marker (operators & log-greppers depend on this exact label).
    // The reconcile span has already closed at this point (it lives inside Workspace::reconcile),
    // so this event arrives at root scope and stands alone in the trace — exactly the way it
    // appeared before WP-O1, just emitted via tracing instead of log so the structured-trace
    // consumer (Phase H's report) can index it by name.
    tracing::info!(
        "post_cold_reconcile added={} modified={} removed={} walked={}",
        report.added,
        report.modified,
        report.removed,
        report.walked
    );
    // Persist the settled index to the warm-start cache (fire-and-forget; never errors).
    // Called AFTER build + reconcile so the cache reflects a consistent, post-reconcile state.
    state.workspace.save_cache();

    // --- Event loop: select! over LSP receiver + the watcher receiver + a liveness ticker. The
    // watcher arm is disabled when `watcher_rx` is None: `unwrap_or(&dummy)` returns the
    // never-channel and `select!` blocks only on the LSP + ticker arms. Rebinding `watcher_arm`
    // inside the loop is what lets us clear `watcher_rx` to None from the disconnect / liveness arm
    // without a borrow conflict.
    //
    // The `ticker` fires every `WATCHER_LIVENESS_INTERVAL` and re-stats the watch root. notify's
    // Windows backend, on a deleted/moved/unmounted root, silently `unwatch`es and returns WITHOUT
    // emitting an error to the channel and WITHOUT dropping its sender — so neither the watcher
    // arm's `Ok(Err(_))` nor its `Err(_)` disconnect ever fires, and freshness would otherwise stop
    // dead with no log and no recovery. Active polling is the only signal there. The ticker is left
    // running once the watcher is disabled (its arm becomes a no-op below); the cost is negligible.
    // WP-RD3: `watcher_rx` and `ticker` are produced by the watcher-source match above (real or
    // injected); only the never-channel `dummy` is local to the loop.
    let dummy = crossbeam_channel::never::<DebounceEventResult>();
    // One-shot arm for the background auto-dump (issue #25); `never` once it has fired/closed.
    let dump_dummy = crossbeam_channel::never::<crate::api_dump::DumpOutcome>();
    // WP-RD11 (4): liveness ticks elapsed since the watcher arm was disabled. Once the watcher is
    // down (MaxFilesWatch / root-loss), the index would otherwise freeze until restart; this counts
    // 3-second ticks so a low-frequency reconcile fallback can re-sync on-disk drift.
    let mut disabled_reconcile_ticks: u32 = 0;

    loop {
        let watcher_arm = watcher_rx.as_ref().unwrap_or(&dummy);
        let dump_arm = dump_rx.as_ref().unwrap_or(&dump_dummy);
        select! {
            recv(dump_arm) -> outcome => {
                // One-shot: whether it reported or the thread died, retire the arm.
                dump_rx = None;
                match outcome {
                    Ok(crate::api_dump::DumpOutcome::Adopted { classes, version }) => {
                        log::info!(
                            "native API: background dump adopted ({classes} classes, {version}); \
                             reloading + republishing open buffers"
                        );
                        if state.workspace.reload_native(&state.options) {
                            republish_all_open_buffers(&mut state);
                            // The warm-start cache key includes the native DB; re-save so the
                            // NEXT session warm-loads instead of cold-indexing on key mismatch.
                            state.workspace.save_cache();
                        }
                    }
                    Ok(crate::api_dump::DumpOutcome::Failed(e)) => {
                        log::warn!("native API: background auto-dump failed ({e}); keeping the current source");
                    }
                    Err(_) => {
                        log::warn!("native API: background dump thread ended without reporting");
                    }
                }
            },
            recv(connection.receiver) -> msg => match msg {
                Ok(Message::Request(req)) => {
                    // WP-P3: record before dispatch so a panicking handler still leaves the
                    // request in the trace (the artifact is the only way to reproduce the panic).
                    if let Some(rec) = state.recorder.as_mut() {
                        rec.record_request(&req);
                    }
                    if connection.handle_shutdown(&req)? {
                        break;
                    }
                    let resp = dispatch_request(&mut state, req);
                    if let Err(e) = state.sender.send(Message::Response(resp)) {
                        // Send only errors when the receiver is closed. The next select! tick
                        // will hit the Err(_) arm on the connection.receiver and break.
                        // Once-per-session event; warn so production logs surface it.
                        log::warn!(
                            "response send failed (client likely disconnected): {e}; \
                             loop will exit on next receiver tick"
                        );
                    }
                }
                Ok(Message::Notification(note)) => {
                    if let Some(rec) = state.recorder.as_mut() {
                        rec.record_notification(&note);
                    }
                    if note.method == "exit" {
                        break;
                    }
                    dispatch_notification(&mut state, note);
                }
                Ok(Message::Response(_)) => {} // the server issues no client-bound requests in M0
                Err(_) => break, // LSP channel closed — peer hung up
            },
            recv(watcher_arm) -> result => match result {
                Ok(Ok(events)) => handle_watcher(&mut state, events),
                Ok(Err(errors)) => {
                    // Inspect the kind of each notify error: most are transient (a watched
                    // dir vanished mid-session, a permission glitch) and the watcher loop
                    // can keep running. Two kinds are TERMINAL:
                    //   - MaxFilesWatch: Linux exceeded fs.inotify.max_user_watches; every
                    //     subsequent event will be silently dropped.
                    //   - PathNotFound on the watched root: the project directory itself
                    //     vanished; we're watching nothing.
                    // Treat either as fatal degradation: log at error with the actionable
                    // next step, disable the watcher arm (so the LSP session keeps serving
                    // open buffers), and stop processing this error batch.
                    let mut fatal = false;
                    for e in errors {
                        use notify::ErrorKind;
                        match e.kind {
                            ErrorKind::MaxFilesWatch => {
                                log::error!(
                                    "watcher: MaxFilesWatch reached — every subsequent file \
                                     change will be silently dropped. Raise the OS limit \
                                     (Linux: `sysctl fs.inotify.max_user_watches=524288`) and \
                                     restart gdls. Disabling the watcher for this session."
                                );
                                fatal = true;
                            }
                            ErrorKind::PathNotFound => {
                                log::error!(
                                    "watcher: PathNotFound — a watched path is gone (network \
                                     share unmounted, project dir deleted, or remote-FS hiccup): \
                                     {e}. Disabling the watcher for this session; reissue \
                                     `initialize` after the path comes back."
                                );
                                fatal = true;
                            }
                            _ => log::warn!("watcher error (non-fatal): {e}"),
                        }
                    }
                    if fatal {
                        watcher_rx = None;
                    }
                }
                Err(_) => {
                    // Watcher dropped its sender — debouncer thread died. Disable the arm so
                    // the LSP session keeps serving; next iteration's `unwrap_or(&dummy)`
                    // returns the never-channel.
                    log::warn!("watcher channel closed unexpectedly; freshness disabled");
                    watcher_rx = None;
                }
            },
            recv(ticker) -> _ => {
                // M5 WP-O2: periodic RSS sample. One syscall every 3 s; on Windows this is a
                // process query that costs sub-millisecond, and on Linux it's a `/proc/self/statm`
                // read. The sampler updates `peak_bytes` in place and emits a `tracing::info!`
                // event tagged `phase="tick"` so the structured trace carries the whole pressure
                // curve, not just the four explicit lifecycle samples.
                state.rss.sample_now("tick");
                // M5 WP-H1: classify the current peak against the resolved budget and react on
                // transition. Reacting only on transition (not on every tick the level is held)
                // keeps the trace clean: Soft fires its eviction event exactly once when the
                // level rises, not 1 200 times across an hour at Soft. The plan calls for a
                // 2-second poll cadence; running this inside the existing 3-second liveness
                // ticker is one tick slower but avoids a second timer and the inevitable
                // double-fire race when both arms wake at once. The Phase-H walk confirms a
                // 3-second cadence is fast enough on a large real-world project.
                react_to_memory_pressure(&mut state);
                // Liveness poll. Only meaningful while the watcher is still live: once `watcher_rx`
                // is None the arm above is the never-channel and there is nothing to re-stat (and
                // re-statting a since-recreated root would just churn). Guard on both the receiver
                // being present and the handle existing so a stat is only ever issued for a watcher
                // we are actually relying on for freshness.
                if watcher_rx.is_some() {
                    if let Some(w) = watcher.as_ref() {
                        if !w.root_exists() {
                            // Root deleted / moved / unmounted: notify's Windows backend has
                            // silently stopped without erroring, so this is the ONLY place we
                            // learn freshness is dead. Disable the (now-inert) watcher arm exactly
                            // like the disconnect path — the next iteration swaps in the
                            // never-channel and the loop keeps serving LSP requests on open buffers
                            // without busy-spinning.
                            let root = w.root();
                            log::error!(
                                "watcher: project root {root} is gone (deleted, moved, or \
                                 unmounted); the filesystem watcher has been silently un-watched \
                                 and freshness is now DISABLED — open buffers still work, but \
                                 on-disk changes are no longer tracked. Restore the path and run \
                                 `gdls diagnose --reconcile`, or restart gdls."
                            );
                            watcher_rx = None;
                        }
                    }
                }
                // WP-RD11 (4): reconcile fallback while the watcher arm is DISABLED. Without it the
                // index freezes until restart once MaxFilesWatch / root-loss disables the arm. Count
                // 3-second liveness ticks; every ~60 s (20 ticks) run a full reconcile so on-disk
                // drift is eventually re-synced. Reset the counter whenever the watcher is live.
                if watcher_rx.is_none() {
                    disabled_reconcile_ticks += 1;
                    if disabled_reconcile_ticks >= 20 {
                        disabled_reconcile_ticks = 0;
                        let open_paths = open_buffer_paths(&state);
                        let report = state.workspace.reconcile(&open_paths);
                        tracing::info!(
                            "watcher_disabled_reconcile added={} modified={} removed={} walked={}",
                            report.added,
                            report.modified,
                            report.removed,
                            report.walked
                        );
                        republish_dirty_open_buffers(&mut state);
                    }
                } else {
                    disabled_reconcile_ticks = 0;
                }
            }
        }
    }

    // M5 WP-O2: shutdown sample. Emitted before any recorder flush so the trace carries the
    // session's final RSS reading + peak independent of whether the recorder is enabled. The
    // peak field on the event is the authoritative "max RSS observed across the whole session"
    // number the Phase-H verification report quotes.
    state.rss.sample_now("shutdown");
    tracing::info!(
        peak_bytes = state.rss.peak().get(),
        baseline_bytes = state.rss.baseline().get(),
        "session_peak_rss"
    );
    // Persist the index cache on clean shutdown so the next launch can warm-start. Called
    // at shutdown (not just at post-cold-reconcile) so a session that processed many edits
    // leaves a fresh cache for the next launch. Fire-and-forget (log-only on failure).
    // Use save_cache_excluding_open so any unsaved buffer interfaces are NOT persisted as disk
    // truth: warm-load will re-parse those files from disk and recover the correct on-disk state
    // ("never lie" guarantee — Issue 1 never-lie fix).
    let open_paths = open_buffer_paths(&state);
    state.workspace.save_cache_excluding_open(&open_paths);

    // WP-P3 flush: only fires when a recorder was injected (env var set or test-driven). A flush
    // failure is logged at warn (the LSP session is exiting anyway; killing the parent process
    // on a debug-artifact-write error would mask the real reason it stopped).
    if let Some(rec) = state.recorder.take() {
        let buffers = bench::snapshot_buffers(&state.vfs);
        if let Err(e) = rec.flush(buffers) {
            log::warn!("bench recorder flush failed: {e}");
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// M5 WP-H1 — memory pressure ladder.
// ---------------------------------------------------------------------------

/// Resolve the path the ladder loads `bench/budget.toml` from. Probes (in order): the
/// `$GDLS_BENCH_BUDGET` env override (used by the WP-H1 integration test + a Phase-H operator
/// pointing at a custom calibration), then `<cwd>/bench/budget.toml` (the repo layout — what the
/// WP-P1 calibration writes). Returns `None` when neither candidate yields a path; the loader's
/// `Ok(None)` documented-fallback then kicks in (defaults + one warn).
fn bench_budget_path() -> Option<Utf8PathBuf> {
    if let Ok(s) = std::env::var("GDLS_BENCH_BUDGET") {
        return Some(Utf8PathBuf::from(s));
    }
    let cwd = std::env::current_dir().ok()?;
    let utf8 = Utf8PathBuf::from_path_buf(cwd).ok()?;
    Some(utf8.join("bench").join("budget.toml"))
}

/// M5 WP-H1 startup guard: warn once, immediately after the cold index settles, if the resolved
/// RSS already sits at or above a cap — instead of leaving the operator to discover it later as
/// silently-shed navigation. The pressure ladder's only reclaimable memory is the bounded
/// parse/analysis caches; it CANNOT shed the interface index + native DB that dominate RSS on a
/// large tree, so an index already over the hard cap will latch the Hard rung and refuse analysis
/// for the rest of the session. The fix is operator-side (raise the caps for the project's scale),
/// hence a loud log rather than an automatic cap bump that would just defer the real OOM.
fn warn_if_cold_index_exceeds_budget(rss: Bytes, budget: &MemoryBudget) {
    let rss_mb = rss.get() / (1024 * 1024);
    if rss > budget.hard_cap_bytes() {
        tracing::error!(
            name = "memory_cold_index_over_hard_cap",
            rss_mb,
            hard_cap_mb = budget.hard_cap_mb(),
            "cold-index RSS is already at/above the hard memory cap: the ladder will refuse new \
             analyses (ContentModified) and can only evict the bounded parse/analysis caches — it \
             cannot shed the interface index + native DB that dominate RSS here. Raise hardCapMb / \
             the bench/budget.toml caps for this project's scale, or expect degraded navigation.",
        );
    } else if rss > budget.soft_cap_bytes() {
        tracing::warn!(
            name = "memory_cold_index_over_soft_cap",
            rss_mb,
            soft_cap_mb = budget.soft_cap_mb(),
            "cold-index RSS is already at/above the soft memory cap: the ladder will keep evicting \
             the parse/analysis caches, but those are not the dominant consumer here (the interface \
             index + native DB are, and the ladder cannot shed them). Consider raising the \
             bench/budget.toml caps for this project's scale.",
        );
    }
}

/// M5 WP-H1: classify the current peak against the budget; on a TRANSITION fire the
/// corresponding tracing event + ladder action. Held levels are silent (no per-tick spam). The
/// ladder is strictly monotonic on its three levels, so a transition is any change to
/// `state.memory_pressure` — both *into* a higher level (the alarming direction) and *out of* it
/// (informational, but recorded so operators can correlate "RSS came back down" with whatever
/// the session did between the two ticks).
pub(crate) fn react_to_memory_pressure(state: &mut ServerState) {
    let new_level = state.rss.pressure(&state.budget);
    if new_level == state.memory_pressure {
        return; // held level — no per-tick event, no action.
    }
    let prev_level = state.memory_pressure;
    state.memory_pressure = new_level;
    // `peak_mb` is the monotonic session high-water mark (context); `window_max_mb` is the rolling
    // value that actually drove this transition. Reporting both lets an operator see *why* the
    // ladder recovered even while the session peak is still high.
    let peak_mb = state.rss.peak().get() / (1024 * 1024);
    let window_max_mb = state.rss.windowed_rss().get() / (1024 * 1024);
    match new_level {
        MemoryPressure::Normal => {
            // Recovery from Soft/Hard back to Normal: an informational breadcrumb so the
            // operator can correlate "RSS came back down" with the session activity between the
            // last alarming tick and now. No action — the LRU caches refill naturally as the
            // session keeps running.
            tracing::info!(
                name = "memory_pressure_recovered",
                prev = ?prev_level,
                peak_mb,
                window_max_mb,
                soft_cap_mb = state.budget.soft_cap_mb(),
                hard_cap_mb = state.budget.hard_cap_mb(),
                "memory pressure recovered"
            );
        }
        MemoryPressure::Soft => {
            // Bulk-evict half of both caches and emit the WP-H1 event. The evicted count is the
            // single most useful field for an operator triaging "did the ladder do anything?";
            // include the cache lengths before + after so the trace shows the magnitude of the
            // shed.
            let (parse_before, analysis_before) = state.workspace.cache_lens();
            let evicted = state.workspace.evict_half();
            let (parse_after, analysis_after) = state.workspace.cache_lens();
            tracing::warn!(
                name = "memory_soft_cap_evicted",
                evicted_count = evicted,
                peak_mb,
                window_max_mb,
                soft_cap_mb = state.budget.soft_cap_mb(),
                hard_cap_mb = state.budget.hard_cap_mb(),
                parse_cache_before = parse_before,
                parse_cache_after = parse_after,
                analysis_cache_before = analysis_before,
                analysis_cache_after = analysis_after,
                prev = ?prev_level,
                "memory soft cap exceeded; evicted half of both LRU caches"
            );
        }
        MemoryPressure::Hard => {
            // A direct Normal→Hard jump can skip the Soft rung, so shed the oldest cache half here
            // too. If we arrived from Soft, that rung already shed; avoid dropping another half on
            // the very next tick. The handler-side gates are still the live-request shed mechanism
            // — this cache drop is the bounded-memory backstop.
            let (parse_before, analysis_before) = state.workspace.cache_lens();
            let evicted = if prev_level == MemoryPressure::Soft {
                0
            } else {
                state.workspace.evict_half()
            };
            let (parse_after, analysis_after) = state.workspace.cache_lens();
            tracing::error!(
                name = "memory_pressure_shed",
                evicted_count = evicted,
                peak_mb,
                window_max_mb,
                soft_cap_mb = state.budget.soft_cap_mb(),
                hard_cap_mb = state.budget.hard_cap_mb(),
                parse_cache_before = parse_before,
                parse_cache_after = parse_after,
                analysis_cache_before = analysis_before,
                analysis_cache_after = analysis_after,
                prev = ?prev_level,
                "memory hard cap exceeded; evicted half of both LRU caches and new full analyses will be refused until RSS comes back down"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Watcher event dispatch (WP-W3).
// ---------------------------------------------------------------------------

/// Apply a batch of debounced events from the filesystem watcher to the workspace. On overflow
/// (`need_rescan` set on any event in the batch), runs a full reconciliation pass instead of
/// per-event dispatch. After all events apply, republishes diagnostics for any open URI whose
/// interface dependents shifted (via the Index dirty set; see [`republish_dirty_open_buffers`]).
fn handle_watcher(state: &mut ServerState, events: Vec<DebouncedEvent>) {
    // Snapshot the editor's open buffers ONCE per batch. The open buffer is the source of truth
    // over disk (docs/01, `vfs.rs`), so neither the reconcile pass nor any per-event reindex may
    // clobber a file the user has open with its on-disk copy. Threaded into every mutation path.
    let open_paths = open_buffer_paths(state);
    if events.iter().any(|e| e.need_rescan()) {
        log::info!("watcher: need_rescan set; running reconciliation");
        let report = state.workspace.reconcile(&open_paths);
        // M5 WP-O1 — preserved verbatim marker (operators & log-greppers depend on this exact
        // label). Migrated to tracing::info! so the watcher-overflow path lights up the same
        // way the cold-start path does in any structured trace consumer.
        tracing::info!(
            "watcher_reconciled added={} modified={} removed={} walked={}",
            report.added,
            report.modified,
            report.removed,
            report.walked
        );
        republish_dirty_open_buffers(state);
        return;
    }

    // Clone the project root ONCE per batch — the inner loop hits classify_event +
    // apply_reaction per reaction, both of which need the root by reference. Cloning per
    // reaction would scale linearly with the (typically large) debounced batch size.
    let root = state.workspace.project.root.clone();
    // WP-RD11 (3): coalesce the project/native-DB reload. The per-file `GdSource` reactions are
    // applied as they come (each is an independent index mutation), but a batch that touches
    // `project.godot` AND two `.gdextension` files AND `extension_api.json` must reload the native
    // DB + re-enumerate ONCE — not four times, each followed by a full `republish_all_open_buffers`.
    // Scan the batch into two booleans and do the (expensive) reload + republish at most once after.
    let mut project_changed = false;
    let mut native_changed = false;
    for ev in events {
        for reaction in watcher::classify_event(&ev, &root) {
            match reaction {
                // Coalesce the project/native-DB reactions into the post-batch reload below.
                Reaction::ProjectGodot
                | Reaction::Gdextension { .. }
                | Reaction::DocClassesXml { .. } => project_changed = true,
                Reaction::ExtensionApiJson => native_changed = true,
                // GdSource (per-file index mutation) and Other (dropped) both flow through
                // `apply_reaction` so each still opens a `watcher_event` span — the WP-RD7
                // `SkipReason` on an `Other` surfaces in the trace there.
                other => apply_reaction(state, other, &root, &open_paths),
            }
        }
    }
    republish_dirty_open_buffers(state);
    // The coalesced project/native reload (WP-RD11 (3)). `project_changed` subsumes
    // `native_changed` (it reloads the native DB too), so the `else if` avoids a double reload.
    if project_changed {
        log::info!(
            "watcher: project.godot / GDExtension surface changed; reloading project model + \
             native DB (coalesced once for the batch)"
        );
        state.workspace.reload_project_and_native(&state.options);
        republish_all_open_buffers(state);
    } else if native_changed {
        log::info!(
            "watcher: extension_api.json changed; reloading native DB (coalesced for the batch)"
        );
        // `reload_native` reports whether the live DB actually changed: a torn read of a
        // mid-write dump (kept prior) or the post-adoption echo (identical content) must not
        // re-analyze every open buffer for nothing.
        if state.workspace.reload_native(&state.options) {
            republish_all_open_buffers(state);
            // The warm-start cache key includes the native DB — re-save so the next session
            // warm-loads against the new key. (The background-dump completion arm does the
            // same; whichever path adopts first wins, the other dedupes by content hash.)
            state.workspace.save_cache();
        }
    }
    // M5 WP-O2: post-watcher-batch sample. A mass reindex driven by a `git checkout` or branch
    // switch can balloon the parse + analysis caches in one go; this catches that burst without
    // waiting for the 3-second ticker.
    state.rss.sample_now("post_watcher");
}

/// Dispatch a single classified [`Reaction`] to the right [`Workspace`] mutator. Per-reaction
/// errors are logged and swallowed: a single bad event must not take down the whole watcher loop.
///
/// Project-root bounding: `GdSource` mutations are gated on the path being inside the
/// workspace's project root. A symlinked-in shared library outside the root would otherwise
/// pass `is_excluded` (excluded-component check finds no match on its absolute path), reach
/// `reindex`, and pollute the index with an out-of-project file — potentially shadowing a
/// project class's `class_name` with the shared lib's. Drop with a `warn` so operators can
/// audit unexpected symlinks.
fn apply_reaction(
    state: &mut ServerState,
    reaction: Reaction,
    project_root: &Utf8Path,
    open_paths: &FxHashSet<Utf8PathBuf>,
) {
    // M5 WP-O1: watcher_event span. `reaction_kind()` returns a stable string discriminant so the
    // span can be faceted by reaction type without keeping a reference to `reaction` (which is
    // consumed by the `match` below). `event_path()` projects the per-reaction path slug for
    // `GdSource` (the only reaction that names a specific file); other reactions surface their
    // own root-level path implicitly via the marker logs they emit. The body runs inside an inner
    // closure so the on-close `elapsed_us` recording happens after every exit path (including the
    // many early returns inside the GdSource arm).
    let _start = std::time::Instant::now();
    let _span = tracing::info_span!(
        "watcher_event",
        reaction = reaction_kind(&reaction),
        path = event_path(&reaction).unwrap_or_default(),
        elapsed_us = tracing::field::Empty,
    );
    let _enter = _span.enter();
    apply_reaction_inner(state, reaction, project_root, open_paths);
    _span.record("elapsed_us", _start.elapsed().as_micros() as u64);
}

/// Stable discriminant for the [`Reaction`] enum — used as the `reaction` field on the
/// `watcher_event` span so a structured-trace consumer can facet by reaction kind without
/// having to parse the per-arm log lines. Kept in this module (and not on `Reaction` itself in
/// `watcher.rs`) because the names are span-attribute display strings, not part of the watcher's
/// public surface.
fn reaction_kind(reaction: &Reaction) -> &'static str {
    match reaction {
        // WP-RD7: surface WHY the event was dropped (the SkipReason discriminant) instead of a flat
        // "other", so a structured trace can distinguish a `.tmp`-file skip from a non-UTF-8 drop.
        Reaction::Other(reason) => reason.as_str(),
        Reaction::GdSource { .. } => "gd_source",
        Reaction::ProjectGodot => "project_godot",
        Reaction::ExtensionApiJson => "extension_api_json",
        Reaction::Gdextension { .. } => "gdextension",
        Reaction::DocClassesXml { .. } => "doc_classes_xml",
    }
}

/// The per-reaction file path string for span attribution. Only `GdSource` carries a file path;
/// every other reaction surfaces its own log line with the affected path. Returning `None` here
/// (rendered as the empty string in the span) keeps the span field uniform without forcing
/// every non-GdSource reaction to invent a synthetic path label.
fn event_path(reaction: &Reaction) -> Option<String> {
    match reaction {
        Reaction::GdSource { path, .. } => Some(path.to_string()),
        _ => None,
    }
}

/// Body of [`apply_reaction`] extracted so the surrounding tracing span can record its on-close
/// field once after every early-return path inside the match. The signature mirrors the caller's
/// bindings 1:1.
fn apply_reaction_inner(
    state: &mut ServerState,
    reaction: Reaction,
    project_root: &Utf8Path,
    open_paths: &FxHashSet<Utf8PathBuf>,
) {
    match reaction {
        Reaction::GdSource { path, change } => {
            // Bound every GdSource mutation to within the project root. The event `path` is NOT
            // pre-normalized — `classify_event` only does `Utf8PathBuf::from_path_buf` on the raw
            // notify path, so on Windows it still carries OS-native backslashes here. It is
            // `path_is_within` that normalizes BOTH operands (via `normalize_path`) to forward
            // slashes before its component-aware `starts_with`, so a false partial prefix
            // (`/proj/x` vs `/projlong/x`) is impossible regardless of slash direction.
            if !path_is_within(&path, project_root) {
                log::warn!(
                    "watcher: dropping out-of-root event for {path} (project root: {project_root}); \
                     this is usually a symlink — investigate before relying on it for freshness"
                );
                return;
            }
            // A rename's SOURCE path no longer exists on disk; drop its now-stale interface from the
            // index up front — BEFORE the open-buffer guard on the destination below. Without this,
            // a rename whose DESTINATION is an open buffer hit that guard and returned early,
            // stranding the closed source's interface in the index forever. Respect
            // "open buffer wins" for the rare open source, and stay bounded to the project root.
            if let FileChange::Renamed { from, .. } = &change {
                if path_is_within(from, project_root) && !open_paths.contains(from) {
                    state.workspace.remove(from);
                }
            }
            // Open buffer wins over disk (docs/01, `vfs.rs`): if the editor has this file open, its
            // last-`didChange` interface is authoritative and already live in the index. An
            // external on-disk edit or delete (git checkout, formatter, stash) must NOT overwrite
            // or drop it — classify the disk change for the operator log, then ignore it.
            if open_paths.contains(&path) {
                classify_open_buffer_disk_change(state, &path);
                return;
            }
            match change {
                FileChange::Created | FileChange::Modified => {
                    match std::fs::read_to_string(&path) {
                        Ok(text) => {
                            state
                                .workspace
                                .reindex(&path, &gd_syntax::parse(&text).tree);
                            // Disk-sourced: refresh stat_table so the next warm-load can skip this
                            // file if it hasn't changed again (Issue 1 perf fix).
                            state.workspace.update_stat_from_disk(&path);
                        }
                        // A NotFound here is the partial-rename case: the watcher delivered
                        // Modify(Name(_)) for the source half of a cross-mountpoint rename
                        // that classify_event couldn't merge. The deleted half's interface
                        // would otherwise stay in the index forever; route to `remove` so
                        // the next definition lookup doesn't jump to a dead path.
                        // Mirrors `reindex_from_disk`'s NotFound branch.
                        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                            log::info!(
                                "watcher: {path} vanished between event and read \
                                 (cross-mount rename or transient delete); removing from index"
                            );
                            state.workspace.remove(&path);
                        }
                        Err(e) => log::warn!(
                            "watcher: cannot read {path}: {e}; keeping last-known interface for now"
                        ),
                    }
                }
                FileChange::Deleted => state.workspace.remove(&path),
                FileChange::Renamed { to, .. } => {
                    // The source half was already removed above (independent of whether `to` is an
                    // open buffer). (Re)index the destination from disk.
                    match std::fs::read_to_string(&to) {
                        Ok(text) => {
                            state.workspace.reindex(&to, &gd_syntax::parse(&text).tree);
                            // Disk-sourced rename target: refresh stat_table (Issue 1 perf fix).
                            state.workspace.update_stat_from_disk(&to);
                        }
                        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                            log::info!(
                                "watcher: rename target {to} vanished before read; \
                                 leaving removed-from index"
                            );
                        }
                        Err(e) => log::warn!("watcher: cannot read rename target {to}: {e}"),
                    }
                }
            }
        }
        // A dropped `Other` is a no-op here — its `SkipReason` was already recorded on the
        // surrounding `watcher_event` span (WP-RD7).
        Reaction::Other(_) => {}
        // WP-RD11 (3): the project/native-DB reactions (ProjectGodot, ExtensionApiJson,
        // Gdextension, DocClassesXml) are no longer reloaded per-event — `handle_watcher` scans the
        // whole batch and coalesces their reload + `republish_all_open_buffers` into one post-batch
        // pass. So `apply_reaction` does per-file work only for `GdSource`.
        _ => {}
    }
}

/// The set of files the editor currently has open, normalized to the index's forward-slash key
/// form. Enforces "open buffer wins over disk" on every watcher-driven mutation: a file in this set
/// must not be reindexed from disk or dropped, because its authoritative interface is the open
/// buffer's (already live in the index from [`reindex_open_buffer`]).
fn open_buffer_paths(state: &ServerState) -> FxHashSet<Utf8PathBuf> {
    open_buffer_uris(state)
        .iter()
        .filter_map(uri_to_path)
        .map(|p| gd_project::normalize_path(&p))
        .collect()
}

/// Parse every open buffer's stored URI string back into a [`Uri`], logging + skipping any that no
/// longer parses (near-impossible — each parsed cleanly at `didOpen`). The shared open-buffer
/// prologue of both republish paths and [`open_buffer_paths`].
fn open_buffer_uris(state: &ServerState) -> Vec<Uri> {
    state
        .vfs
        .open_uris()
        .filter_map(|u| match u.parse::<Uri>() {
            Ok(parsed) => Some(parsed),
            Err(e) => {
                log::debug!("republish: skipping open URI {u} — no longer parseable as a Uri: {e}");
                None
            }
        })
        .collect()
}

/// Log how an on-disk change to an *open* buffer relates to the buffer's authoritative interface,
/// then do nothing else — the open buffer wins (see [`apply_reaction`]). Compares the disk file's
/// interface signature against the one already in the index (set from the buffer by
/// [`reindex_open_buffer`]): equal ⇒ a benign no-op (often the user's own save); different ⇒ a
/// genuine external edit (git checkout / formatter / stash) the operator should know diverged from
/// the unsaved buffer; missing on disk ⇒ a transient delete/stash. Never mutates the index.
fn classify_open_buffer_disk_change(state: &ServerState, path: &Utf8Path) {
    let buf_iface_hash = state
        .workspace
        .index
        .interface_of(path)
        .map(|i| i.signature_hash());
    match std::fs::read_to_string(path) {
        Ok(disk_text) => {
            let disk_hash =
                gd_project::extract_interface(&gd_syntax::parse(&disk_text).tree).signature_hash();
            if Some(disk_hash) == buf_iface_hash {
                log::debug!(
                    "watcher: {path} changed on disk but its interface matches the open buffer; no-op"
                );
            } else {
                log::warn!(
                    "watcher: external edit to open file {path} (git checkout / formatter / stash); \
                     the open-buffer interface is authoritative — ignoring the on-disk change until \
                     the buffer is saved or closed"
                );
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => log::info!(
            "watcher: open file {path} vanished on disk (transient delete/stash); keeping the \
             open-buffer interface"
        ),
        Err(e) => log::warn!(
            "watcher: open file {path} changed on disk but is unreadable ({e}); keeping the \
             open-buffer interface"
        ),
    }
}

/// Republish diagnostics for any open URI whose path is in the Index's dirty set — keeps the LSP
/// client view in sync with the on-disk reindex when an open dependent's interface base changed
/// underneath it.
///
/// WP-RD8: **drains** the dirty set ([`gd_project::Index::take_dirty`]). The set's only remaining
/// job is republish targeting; cache *validity* is keyed on the per-file epoch, so a closed
/// dirtied dependent dropped from the set on drain still re-analyzes when it is next opened (its
/// cache entry's stamped epoch no longer matches the current one). This retires the earlier
/// non-draining `dirty_paths` + `clear_dirty_one` dance, whose only reason for being was that
/// `analyze` keyed its cache-miss override on `is_dirty` and had to clear the bit itself.
fn republish_dirty_open_buffers(state: &mut ServerState) {
    republish_dirty_open_buffers_except(state, None);
}

/// [`republish_dirty_open_buffers`] minus one URI: the didOpen/didChange handlers drain the
/// dirty set their own reindex populated (the edited file + its open dependents) right after
/// their direct, version-tagged publish — `skip` keeps that file from being published twice in
/// the same turn. Draining at the edit chokepoint (v1.0.2) does two jobs: open DEPENDENTS get
/// fresh diagnostics immediately (previously they waited for the next unrelated watcher batch
/// to drain the set), and that later batch no longer "republishes" untouched buffers.
fn republish_dirty_open_buffers_except(state: &mut ServerState, skip: Option<&Uri>) {
    let dirty = state.workspace.index.take_dirty();
    if dirty.is_empty() {
        return;
    }
    let dirty_set: FxHashSet<Utf8PathBuf> = dirty
        .iter()
        .map(|p| gd_project::normalize_path(p))
        .collect();
    let stale_uris: Vec<Uri> = open_buffer_uris(state)
        .into_iter()
        .filter(|u| skip != Some(u))
        .filter(|u| {
            uri_to_path(u)
                .map(|p| gd_project::normalize_path(&p))
                .is_some_and(|p| dirty_set.contains(&p))
        })
        .collect();
    for uri in stale_uris {
        publish_diagnostics(state, uri, None);
    }
}

/// Republish diagnostics for every open buffer — used after a project-wide policy change
/// (`project.godot`) or native DB reload (`extension_api.json` / gdextension surface) where the
/// `Index.dirty` set won't capture the change (the change isn't interface-keyed; it affects every
/// file's analysis).
fn republish_all_open_buffers(state: &mut ServerState) {
    for uri in open_buffer_uris(state) {
        publish_diagnostics(state, uri, None);
    }
}

/// Advertise exactly the v1 capability set Claude Code consumes (`docs/05-lsp-cc-integration.md`).
fn capabilities(encoding: PositionEncoding) -> ServerCapabilities {
    ServerCapabilities {
        position_encoding: Some(encoding.to_kind()),
        text_document_sync: Some(TextDocumentSyncCapability::Kind(
            TextDocumentSyncKind::INCREMENTAL,
        )),
        document_symbol_provider: Some(OneOf::Left(true)),
        workspace_symbol_provider: Some(OneOf::Left(true)),
        definition_provider: Some(OneOf::Left(true)),
        references_provider: Some(OneOf::Left(true)),
        hover_provider: Some(HoverProviderCapability::Simple(true)),
        implementation_provider: Some(ImplementationProviderCapability::Simple(true)),
        call_hierarchy_provider: Some(CallHierarchyServerCapability::Simple(true)),
        document_link_provider: Some(DocumentLinkOptions {
            resolve_provider: Some(false),
            work_done_progress_options: Default::default(),
        }),
        // `textDocument/publishDiagnostics` is a server→client push, not a capability field.
        ..Default::default()
    }
}

/// Resolve the `res://` root, in priority order (WP-F): explicit `projectRoot` → the LSP workspace
/// folder / `rootUri` → the nearest `project.godot` above the cwd → the cwd itself. Always returns a
/// path (`.` as the ultimate floor) so the workspace can always be built.
fn resolve_root(options: &InitializationOptions, init: &InitializeParams) -> Utf8PathBuf {
    if let Some(pr) = &options.project_root {
        return Utf8PathBuf::from(pr);
    }
    if let Some(dir) = workspace_folder_root(init) {
        return dir;
    }
    match std::env::current_dir()
        .ok()
        .and_then(|p| Utf8PathBuf::from_path_buf(p).ok())
    {
        Some(cwd) => nearest_project_godot(&cwd).unwrap_or(cwd),
        None => Utf8PathBuf::from("."),
    }
}

/// The first LSP workspace folder, or the (deprecated) `rootUri`, as a filesystem path.
fn workspace_folder_root(init: &InitializeParams) -> Option<Utf8PathBuf> {
    if let Some(folder) = init.workspace_folders.as_ref().and_then(|f| f.first()) {
        if let Some(path) = uri_to_path(&folder.uri) {
            return Some(path);
        }
    }
    #[allow(deprecated)]
    init.root_uri.as_ref().and_then(uri_to_path)
}

/// True when `path` is the same as `root` or a descendant of it, using component-aware
/// matching so `/projlong/x` is NOT falsely accepted as inside `/proj`. Used by
/// [`apply_reaction`] to gate `GdSource` mutations on the project-root invariant.
///
/// Both inputs are normalized to forward slashes first so a Windows backslash event path
/// matches a forward-slash project root (the `Index` does the same normalization). The fall
/// back to `path.starts_with(root)` after that handles already-normalized inputs.
fn path_is_within(path: &Utf8Path, root: &Utf8Path) -> bool {
    gd_project::normalize_path(path).starts_with(gd_project::normalize_path(root))
}

/// Walk up from `start` to the nearest directory containing a `project.godot`.
fn nearest_project_godot(start: &Utf8Path) -> Option<Utf8PathBuf> {
    let mut dir = Some(start);
    while let Some(d) = dir {
        if d.join("project.godot").exists() {
            return Some(d.to_path_buf());
        }
        dir = d.parent();
    }
    None
}

/// Log + return an `ERR_INVALID_PARAMS` response. Mirrors the notification path's
/// `parse_params` which `log::warn!`s before returning Err, so a client filing "the server
/// returned -32602 for my request" has a server-side trace to correlate.
fn invalid_params_response(
    id: lsp_server::RequestId,
    method: &str,
    e: serde_json::Error,
) -> Response {
    log::warn!("invalid params for {method}: {e}");
    Response::new_err(id, ERR_INVALID_PARAMS, format!("invalid params: {e}"))
}

fn dispatch_request(state: &mut ServerState, req: Request) -> Response {
    let Request { id, method, params } = req;
    // M5 WP-O1: every request handler runs inside a `handle_request` span. The macro is the
    // single span-injection chokepoint that covers all 9 LSP handlers at once (the highest-
    // leverage move in the plan). `%id` / `%method` are recorded by `Display` at span
    // construction — no borrow held past that, so the arms can still consume `id` / `params` /
    // `method`. The on-close `elapsed_us` field is recorded just before the `_enter` guard
    // drops, so the on-close event carries the full request-handling cost.
    //
    // Every request arm has the identical shape: deserialize `params`, hand the typed value to a
    // handler and wrap its (serializable) return, or map a deserialization failure to a -32602
    // response. This local macro keeps that shape in one place so the dispatch table reads as a flat
    // method→handler map and a new handler is a single row. `params`/`id`/`method`/`state` resolve to
    // this fn's bindings (a `macro_rules!` defined inside a fn can name its locals); only one match
    // arm ever runs, so the single-use moves of `params` and `id` are sound — same as the prior
    // hand-expanded arms.
    // M5 WP-O4 cancellation plumbing: allocate one token per request, register it in
    // `pending_requests` so a `$/cancelRequest` notification can find it, stash it in
    // `current_token` so handlers can read it without extra plumbing, then on handler return
    // unregister + check whether the token was tripped during the handler's run. A tripped
    // token replaces the handler's response with a RequestCancelled error response (LSP 3.17).
    let req_id_for_cancel = id.clone();
    let token = CancellationToken::new();
    state
        .pending_requests
        .insert(req_id_for_cancel.clone(), token.clone());
    state.current_token = Some(token.clone());
    macro_rules! handle {
        ($h:path) => {{
            let _start = std::time::Instant::now();
            let _span = tracing::info_span!(
                "handle_request",
                method = %method,
                id = %id,
                elapsed_us = tracing::field::Empty,
            );
            let _enter = _span.enter();
            let resp = match serde_json::from_value(params) {
                Ok(p) => Response::new_ok(id, $h(state, p)),
                Err(e) => invalid_params_response(id, &method, e),
            };
            _span.record("elapsed_us", _start.elapsed().as_micros() as u64);
            resp
        }};
    }
    // M5 WP-H1: at Hard pressure, refuse new full analyses with ContentModified (-32801) per LSP
    // 3.17. "Analyze-using" is exactly the set of handlers that re-enter a per-handler analyze span
    // (`handlers::analyze_with_request_token` / `analyze_if_gd`): that span holds enough Rc weight
    // to keep cache entries alive past LRU eviction, so refusing it is what actually relieves the
    // pressure. The parse-only / index-only handlers — `documentSymbol`, `workspace/symbol`, and
    // (verified against their bodies) `definition`, `implementation`, `prepareCallHierarchy` — do
    // NOT run the analyzer, so shedding them would break navigation while reclaiming nothing; they
    // stay served, matching `bench/budget.toml`, which records them as index-only (~0.1 ms). The
    // decision is intentionally **not** "cache hit vs miss" — a Hard transition is the OS telling
    // us the working set is at risk. The compromise the plan calls for — "diagnostics that are
    // already cached still serve" — is handled in `publishDiagnostics` by consulting
    // `Workspace::cached_analysis` without running a fresh analyzer pass.
    let analyze_using = matches!(
        method.as_str(),
        "textDocument/hover"
            | "textDocument/references"
            | "callHierarchy/incomingCalls"
            | "callHierarchy/outgoingCalls"
    );
    if state.memory_pressure == MemoryPressure::Hard && analyze_using {
        // Re-record the request as cancelled-cum-shed so the per-handler trace still shows the
        // refused request rather than a silent drop. Cleanup is the same as the bottom of the fn
        // (unregister token, clear current_token), so jump straight to the end via early return.
        state.current_token = None;
        state.pending_requests.remove(&req_id_for_cancel);
        tracing::warn!(
            target: "shed",
            id = %req_id_for_cancel,
            method = %method,
            "request shed at Hard memory pressure",
        );
        return Response::new_err(
            req_id_for_cancel,
            ERR_CONTENT_MODIFIED,
            "server is shedding requests under memory pressure; please retry".to_string(),
        );
    }
    let resp = match method.as_str() {
        "textDocument/documentSymbol" => handle!(handlers::document_symbol),
        "textDocument/documentLink" => handle!(handlers::document_link),
        // LSP says hover returns `null` when there's nothing to say — `serde_json::to_value(None)`
        // serializes to `null`, which is what the wire wants.
        "textDocument/hover" => handle!(handlers::hover),
        "textDocument/definition" => handle!(handlers::definition),
        "textDocument/references" => handle!(handlers::references),
        "textDocument/implementation" => handle!(handlers::implementation),
        "textDocument/prepareCallHierarchy" => handle!(handlers::prepare_call_hierarchy),
        "callHierarchy/incomingCalls" => handle!(handlers::incoming_calls),
        "callHierarchy/outgoingCalls" => handle!(handlers::outgoing_calls),
        "workspace/symbol" => handle!(handlers::workspace_symbol),
        _ => Response::new_err(
            id,
            ERR_METHOD_NOT_FOUND,
            format!("unhandled method: {method}"),
        ),
    };
    // M5 WP-O4 — unregister the per-request token and, if the token was tripped during the
    // handler's run, replace the (potentially partial) response with a RequestCancelled error.
    // The LSP 3.17 spec lets a cancelled request return either a partial result or this error;
    // for v1 we always use the error response so a client that respects RequestCancelled doesn't
    // have to special-case which results may be partial. The `cancellation::CancellationToken`
    // already pushed `analyzer: request cancelled` into the analyzer's diagnostic stream — the
    // operator-visible breadcrumb of when the cancel landed.
    state.current_token = None;
    state.pending_requests.remove(&req_id_for_cancel);
    apply_cancellation_gate(&token, req_id_for_cancel, resp)
}

/// WP-O4 cancellation gate: if the per-request `token` was tripped during the handler's run,
/// replace its (possibly partial) response with a `RequestCancelled` (-32800) error per LSP 3.17;
/// otherwise pass the response through. Extracted from [`dispatch_request`] so the gate is
/// unit-testable without the mid-flight wire interrupt the single-threaded loop can't produce.
fn apply_cancellation_gate(
    token: &CancellationToken,
    id: lsp_server::RequestId,
    resp: Response,
) -> Response {
    if token.is_cancelled() {
        tracing::info!(target: "cancel", id = %id, "request_cancelled");
        Response::new_err(id, REQUEST_CANCELLED, "request cancelled".to_string())
    } else {
        resp
    }
}

fn dispatch_notification(state: &mut ServerState, note: Notification) {
    let Notification { method, params } = note;
    match method.as_str() {
        "textDocument/didOpen" => {
            match parse_params::<DidOpenTextDocumentParams>(&method, params) {
                Ok(p) => {
                    let td = p.text_document;
                    state
                        .vfs
                        .open(td.uri.as_str().to_string(), td.text, td.version);
                    reindex_open_buffer(state, &td.uri);
                    let uri = td.uri;
                    publish_diagnostics(state, uri.clone(), Some(td.version));
                    republish_dirty_open_buffers_except(state, Some(&uri));
                }
                Err(()) => {
                    // The client thinks the file is open; gdls did not register it. Every
                    // subsequent hover/definition/diagnostic for this URI will silently
                    // return null/empty. Loud-fail with the same pattern didChange uses.
                    log::error!(
                        "dropped a textDocument/didOpen — the client thinks the file is open but \
                     gdls did not register it; all subsequent requests for this URI will \
                     silently return null until a successful didChange"
                    );
                }
            }
        }
        "textDocument/didChange" => {
            match parse_params::<DidChangeTextDocumentParams>(&method, params) {
                Ok(p) => {
                    let uri = p.text_document.uri;
                    let version = p.text_document.version;
                    let enc = state.encoding;
                    state
                        .vfs
                        .apply_changes(uri.as_str(), p.content_changes, version, enc);
                    reindex_open_buffer(state, &uri);
                    publish_diagnostics(state, uri.clone(), Some(version));
                    republish_dirty_open_buffers_except(state, Some(&uri));
                }
                Err(()) => {
                    // `parse_params` already log::warn'd the deserialize error. Re-flag at error
                    // level so the data-loss case (the edit silently won't apply, the in-memory
                    // rope stays behind the client) is louder than a generic invalid-params hit.
                    log::error!(
                        "dropped a textDocument/didChange — buffer stays at the prior version; the client and gdls are now out of sync until the next valid notification"
                    );
                }
            }
        }
        "textDocument/didSave" => {
            match parse_params::<DidSaveTextDocumentParams>(&method, params) {
                Ok(p) => {
                    // The buffer was re-indexed on the preceding edits; just re-publish.
                    publish_diagnostics(state, p.text_document.uri, None);
                }
                Err(()) => {
                    // `parse_params` already log::warn'd the deserialize error; re-flag the
                    // consequence at the same level as the sibling handlers (didOpen/didChange/
                    // didClose) so the skipped save-triggered re-publish is visible: the client
                    // may keep showing stale diagnostics until the next valid notification.
                    log::warn!(
                        "dropped a textDocument/didSave — save-triggered diagnostics re-publish \
                         skipped; the client may show stale diagnostics until the next edit"
                    );
                }
            }
        }
        "textDocument/didClose" => {
            match parse_params::<DidCloseTextDocumentParams>(&method, params) {
                Ok(p) => {
                    let uri = p.text_document.uri;
                    state.vfs.close(uri.as_str());
                    state.workspace.forget(&CanonicalKey::for_uri(&uri));
                    // The buffer is gone — re-index from disk so the index reflects the on-disk file (an
                    // unsaved buffer's edits are discarded), then push an empty set to clear diagnostics.
                    reindex_from_disk(state, &uri);
                    publish_diagnostics(state, uri.clone(), None);
                    republish_dirty_open_buffers_except(state, Some(&uri));
                }
                Err(()) => {
                    // The VFS entry leaks (the client moved on; we still think the buffer is
                    // open). Disk-fresh diagnostics for that file will publish against stale
                    // buffer text until a `didOpen` recovers it. Loud so this is debuggable.
                    log::error!(
                        "dropped a textDocument/didClose — the VFS entry stays open forever from \
                     gdls's perspective; reopen the file or restart the server to recover"
                    );
                }
            }
        }
        "$/cancelRequest" => {
            // M5 WP-O4: client retracting an in-flight (or queued) request. Look up the
            // matching token in `state.pending_requests` and call `.cancel()`; the next
            // `AnalysisContext::checkpoint` inside the handler's analyze sees the flip on its
            // 256-node gate and bails. If the id is not in the map, this is either a stale
            // cancel (the response was already sent — race condition, spec-allowed no-op) or a
            // typo from a non-conforming client; warn-log so the operator sees the breadcrumb.
            match parse_params::<CancelParams>(&method, params) {
                Ok(p) => {
                    let id = request_id_from_number_or_string(p.id);
                    match state.pending_requests.get(&id) {
                        Some(tok) => {
                            tok.cancel();
                            tracing::info!(target: "cancel", id = %id, "cancel_requested");
                        }
                        None => log::warn!(
                            "$/cancelRequest for {id:?}: no in-flight request with that id; \
                             ignoring (LSP 3.17 §$/cancelRequest: unknown ids are allowed)"
                        ),
                    }
                }
                Err(()) => log::warn!(
                    "dropped a $/cancelRequest — params failed to parse; in-flight requests \
                     will run to completion"
                ),
            }
        }
        "initialized" => log::info!("client reported initialized"),
        other => log::debug!("ignoring notification: {other}"),
    }
}

/// Project `lsp_types::NumberOrString` (the on-wire id form for `$/cancelRequest.params.id`) into
/// `lsp_server::RequestId` (the form `state.pending_requests` is keyed on). The two enums carry
/// the same I32 / String variants; this is purely a type bridge across the lsp-types ↔ lsp-server
/// crate boundary.
fn request_id_from_number_or_string(id: NumberOrString) -> RequestId {
    match id {
        NumberOrString::Number(n) => RequestId::from(n),
        NumberOrString::String(s) => RequestId::from(s),
    }
}

/// Re-extract an open `.gd` buffer's interface into the index (keeps cross-file resolution
/// fresh on every edit; complements the on-disk watcher for unsaved buffers). Non-`file://`
/// or non-`.gd` URIs are ignored.
fn reindex_open_buffer(state: &mut ServerState, uri: &Uri) {
    let Some(path) = uri_to_path(uri) else {
        return;
    };
    if path.extension() != Some("gd") {
        return;
    }
    let Some(text) = state.vfs.get(uri.as_str()).map(|d| d.text()) else {
        return;
    };
    let parsed = state.workspace.parse(&CanonicalKey::for_uri(uri), &text);
    state.workspace.reindex(&path, &parsed.tree);
}

/// Re-index a `.gd` file from disk (on close), or drop it from the index if it no longer exists.
/// After a successful reindex also refreshes `stat_table` so the next warm-start can skip
/// re-parsing this file if it hasn't changed again (Issue 1 perf fix).
fn reindex_from_disk(state: &mut ServerState, uri: &Uri) {
    let Some(path) = uri_to_path(uri) else {
        return;
    };
    if path.extension() != Some("gd") {
        return;
    }
    match std::fs::read_to_string(&path) {
        Ok(text) => {
            state
                .workspace
                .reindex(&path, &gd_syntax::parse(&text).tree);
            // Disk-sourced reindex: update stat_table so the next warm-load can skip this file
            // if it hasn't changed again. Must NOT be called on the buffer path (see
            // Workspace::update_stat_from_disk doc).
            state.workspace.update_stat_from_disk(&path);
        }
        // Genuinely gone ⇒ drop it from the index (remove also drops the stat_table entry).
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => state.workspace.remove(&path),
        // Still on disk but unreadable (locked, perms, non-UTF-8): keep the last-known interface
        // rather than phantom-deleting a live class (which would silently break dependents). Surface
        // why we didn't refresh instead of degrading without a trace.
        Err(e) => log::warn!("keeping last-known index for {path}; re-read on close failed: {e}"),
    }
}

fn parse_params<P: serde::de::DeserializeOwned>(
    method: &str,
    params: serde_json::Value,
) -> Result<P, ()> {
    serde_json::from_value(params).map_err(|e| {
        log::warn!("invalid params for {method}: {e}");
    })
}

/// Parse + analyze the open buffer for `uri` and push the merged diagnostic set. If no buffer is
/// open (e.g. after `didClose`), pushes an empty set, which clears any diagnostics the client is
/// showing for the file.
///
/// Two diagnostic streams flow through the same publish:
/// - `gd_syntax::parse` errors — the M1 per-file parser, anchored by byte span. Severity is always
///   `Error`, code is omitted (the parser doesn't carry a code).
/// - `gd_analyze::analyze` errors + warnings (WP-G) — gated to `.gd` files only, since the analyzer
///   is GDScript-specific. Severity tracks the analyzer's enum (1=Error, 2=Warning, matching LSP's
///   discriminants), and the LSP `code` field carries Godot's warning name (e.g.
///   `"UNUSED_VARIABLE"`) or `"error"` for bare type errors — what every Godot warning page
///   documents the user to type into an `@warning_ignore`.
///
/// Both parses and analyses are cached on the workspace, keyed by [`CanonicalKey`] and validated
/// by a content fingerprint, so the same `didChange` doesn't pay the cost twice when
/// `documentSymbol` and `publishDiagnostics` race on identical buffer text.
fn publish_diagnostics(state: &mut ServerState, uri: Uri, version: Option<i32>) {
    // v1.0.4 (#34): stub buffers never self-diagnose — a materialized native API page need not
    // be analyzable GDScript, only readable as it. Matched against the stubs BASE root (any
    // version/hash: an old-hash stub can stay open across a mid-session dump swap). The publish
    // below still runs with the empty set, so a client that somehow held diagnostics for the
    // path clears them.
    let is_stub = crate::stubs::is_stub_uri(&uri, state.options.stub_cache_dir.as_deref());
    let diagnostics: Vec<Diagnostic> = if is_stub {
        Vec::new()
    } else {
        match state.vfs.get(uri.as_str()).map(|d| d.text()) {
            Some(text) => {
                let parsed = state.workspace.parse(&CanonicalKey::for_uri(&uri), &text);
                // Only `.gd` files go through the analyzer — for any other open buffer the syntax
                // diagnostics carry the publish on their own.
                let analyzed = analyze_gd(state, &uri, &parsed.tree, &text);
                match state.vfs.get(uri.as_str()) {
                    Some(doc) => {
                        let mapper = PositionMapper::new(&doc.rope, state.encoding);
                        collect_diagnostics(&mapper, &parsed.diagnostics, analyzed.as_deref())
                    }
                    None => Vec::new(),
                }
            }
            None => {
                // No open buffer: the expected `didClose` clear-path, or a publish for a URI we never
                // opened. We deliberately push an empty set either way, but log it so an empty result
                // is never silently indistinguishable from "parsed clean".
                log::debug!("publishing empty diagnostics for {uri:?}: no open buffer");
                Vec::new()
            }
        }
    };

    let params = PublishDiagnosticsParams {
        uri,
        diagnostics,
        version,
    };
    let value = serde_json::to_value(params).expect(
        "invariant: PublishDiagnosticsParams has no field whose serde::Serialize impl can fail \
         (every field is String / Vec / Option<i32> / serde_json::Value with infallible writers)",
    );
    let notif = Notification {
        method: "textDocument/publishDiagnostics".to_string(),
        params: value,
    };
    if let Err(e) = state.sender.send(Message::Notification(notif)) {
        // Diagnostics publish is fire-and-forget — but a wedged channel means every
        // subsequent edit silently fails to update the editor view. Warn so production
        // logs surface the case at default level.
        log::warn!("publishDiagnostics send failed (client likely disconnected): {e}");
    }
}

/// Run the analyzer for a `.gd` buffer (returns `None` for other URIs or non-`file://` schemes).
/// The analyzer is the GDScript-specific phase that produces type errors + warnings; other open
/// buffers (untitled scratch, unknown extensions) get parser-only diagnostics.
fn analyze_gd(
    state: &mut ServerState,
    uri: &Uri,
    tree: &ParseTree,
    text: &str,
) -> Option<std::rc::Rc<gd_analyze::AnalysisResult>> {
    let path = uri_to_path(uri)?;
    if path.extension() != Some("gd") {
        return None;
    }
    let key = CanonicalKey::for_uri(uri);
    if state.memory_pressure == MemoryPressure::Hard {
        let cached = state.workspace.cached_analysis(&key, &path, text);
        if cached.is_none() {
            tracing::warn!(
                target: "shed",
                uri = uri.as_str(),
                "publishDiagnostics served parser-only diagnostics at Hard memory pressure"
            );
        }
        return cached;
    }
    Some(state.workspace.analyze(&key, &path, tree, text))
}

/// Project syntax + analyzer diagnostics through one [`PositionMapper`] into the LSP type. Order
/// matches the source-position publish order each stream already uses: syntax errors first (parser
/// emits them in source order), then analyzer diagnostics (the sink sorts by `span.start` at
/// `finish`). The merged stream is what the editor highlights.
fn collect_diagnostics(
    mapper: &PositionMapper,
    syntax: &[gd_syntax::Diagnostic],
    analyzed: Option<&gd_analyze::AnalysisResult>,
) -> Vec<Diagnostic> {
    let mut out = Vec::with_capacity(syntax.len() + analyzed.map_or(0, |a| a.diagnostics.len()));
    for d in syntax {
        out.push(Diagnostic {
            range: mapper.span_to_range(d.span),
            severity: Some(DiagnosticSeverity::ERROR),
            source: Some("gdls".to_string()),
            message: d.message.clone(),
            ..Default::default()
        });
    }
    if let Some(result) = analyzed {
        for d in &result.diagnostics {
            // `d.line()` (WP-R3 override) is deliberately ignored at the LSP boundary — clients
            // render against ranges, and the span carries the byte range a Godot-faithful renderer
            // wants. The override only matters for `.out`-style line-number diffing (conformance).
            out.push(Diagnostic {
                range: mapper.span_to_range(d.span()),
                severity: Some(analyzer_severity(d.severity())),
                code: Some(NumberOrString::String(d.code().to_owned())),
                source: Some("gdls".to_string()),
                message: d.message().to_owned(),
                ..Default::default()
            });
        }
    }
    out
}

/// `gd_analyze::Severity` was deliberately laid out with the same discriminants as
/// `lsp_types::DiagnosticSeverity` (1=Error, 2=Warning, see `diagnostic.rs`'s `#[repr]`), but the
/// projection still goes through an explicit match so a future variant addition forces an update
/// here rather than silently mapping to a wrong level.
fn analyzer_severity(severity: gd_analyze::Severity) -> DiagnosticSeverity {
    match severity {
        gd_analyze::Severity::Error => DiagnosticSeverity::ERROR,
        gd_analyze::Severity::Warning => DiagnosticSeverity::WARNING,
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    /// Build a minimal in-crate [`ServerState`] over `root` (no client, no open buffers). The
    /// sender's receiver is returned and held by the caller so the fire-and-forget
    /// `publishDiagnostics` sends in [`republish_dirty_open_buffers`] never fail on a dropped rx.
    fn state_on(root: &Utf8Path) -> (ServerState, crossbeam_channel::Receiver<Message>) {
        let options = InitializationOptions::parse(Some(&serde_json::json!({
            "projectRoot": root.as_str(),
        })));
        let workspace = Workspace::load(root, &options);
        let (tx, rx) = crossbeam_channel::unbounded::<Message>();
        let state = ServerState {
            encoding: PositionEncoding::Utf16,
            caps: ClientCaps::default(),
            options,
            workspace,
            vfs: Vfs::default(),
            sender: tx,
            recorder: None,
            rss: RssSampler::new(),
            pending_requests: FxHashMap::default(),
            current_token: None,
            // The watcher-path tests don't exercise the WP-H1 ladder; a synthetic budget with
            // caps far above what a small tempdir workspace will ever observe keeps the ticker
            // arm at MemoryPressure::Normal across the run.
            budget: MemoryBudget::from_caps_mb(u64::MAX / 2, u64::MAX / 2),
            memory_pressure: MemoryPressure::Normal,
            stub_cache: crate::stubs::StubCache::default(),
        };
        (state, rx)
    }

    /// A debounced event carrying notify's `Rescan` flag — the live-stream analog of a kernel
    /// event-queue overflow. `need_rescan()` is true for it, which is the only thing
    /// [`handle_watcher`]'s overflow branch keys on.
    fn rescan_event() -> DebouncedEvent {
        let event =
            notify::Event::new(notify::EventKind::Any).set_flag(notify::event::Flag::Rescan);
        DebouncedEvent::new(event, Instant::now())
    }

    /// Build a state with a forced tiny budget so the ladder reads Hard on the very first tick —
    /// the test exercises the transition orchestrator without depending on actual OS RSS values
    /// (the sampler is real; the budget is synthetic).
    fn state_with_tiny_budget(
        root: &Utf8Path,
    ) -> (ServerState, crossbeam_channel::Receiver<Message>) {
        let options = InitializationOptions::parse(Some(&serde_json::json!({
            "projectRoot": root.as_str(),
            "memory": { "cacheCapacity": 32 },
        })));
        let workspace = Workspace::load(root, &options);
        let (tx, rx) = crossbeam_channel::unbounded::<Message>();
        // `from_caps_mb(0, 0)` is clamped to a (1 MB, 1 MB) hard floor by the resolver's
        // `hard < soft` check; instead build a budget tiny enough that the running process's
        // baseline RSS (always > 5 MB on Windows + Linux for any Rust process) is above hard.
        let budget = MemoryBudget::from_caps_mb(1, 2);
        let mut rss = RssSampler::new();
        rss.sample_now("test_baseline");
        let state = ServerState {
            encoding: PositionEncoding::Utf16,
            caps: ClientCaps::default(),
            options,
            workspace,
            vfs: Vfs::default(),
            sender: tx,
            recorder: None,
            rss,
            pending_requests: FxHashMap::default(),
            current_token: None,
            budget,
            memory_pressure: MemoryPressure::Normal,
            stub_cache: crate::stubs::StubCache::default(),
        };
        (state, rx)
    }

    /// WP-H1: a single call to [`react_to_memory_pressure`] in a state whose peak RSS is well
    /// above the synthetic hard cap (any real process is) flips `state.memory_pressure` from
    /// `Normal` straight to `Hard`. The transition direction is "monotonic up" and the second
    /// call (level held at Hard) is a no-op — the per-tick event would otherwise spam.
    #[test]
    fn react_to_memory_pressure_climbs_to_hard_then_holds() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("utf-8 temp dir");
        std::fs::write(dir.path().join("project.godot"), "config_version=5\n").unwrap();
        let (mut state, _rx) = state_with_tiny_budget(&root);
        assert_eq!(state.memory_pressure, MemoryPressure::Normal);
        let dummy = gd_syntax::parse("# x\n");
        for i in 0..6 {
            let key = CanonicalKey::for_uri(
                &format!("file:///hard_{i}.gd")
                    .parse::<lsp_types::Uri>()
                    .unwrap(),
            );
            state
                .workspace
                .debug_insert_parse_entry(key, i as u64, dummy.clone());
        }

        react_to_memory_pressure(&mut state);
        assert_eq!(
            state.memory_pressure,
            MemoryPressure::Hard,
            "with 1 MB / 2 MB caps and a real process baseline ≥ 5 MB, the first tick must \
             climb past Hard"
        );
        assert_eq!(
            state.workspace.cache_lens().0,
            3,
            "a direct Normal→Hard transition must still shed half the cache"
        );

        // Held level: react_to_memory_pressure is a no-op on a repeated call at the same level.
        let level_before = state.memory_pressure;
        react_to_memory_pressure(&mut state);
        assert_eq!(
            state.memory_pressure, level_before,
            "held level — no transition"
        );
    }

    /// WP-H1: at Hard memory pressure, an analyze-using request (`hover`) is shed with
    /// `ContentModified` (-32801) per LSP 3.17, while a parse-only / index-only request
    /// (`documentSymbol`) is exempt and still serves. Pins the wire contract the ladder's Hard rung
    /// promises — the analyze_using set + the error code + the per-request token cleanup on the
    /// shed path — none of which any other test asserts.
    #[test]
    fn hard_pressure_sheds_analyze_using_requests_with_content_modified() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("utf-8 temp dir");
        std::fs::write(dir.path().join("project.godot"), "config_version=5\n").unwrap();
        let (mut state, _rx) = state_on(&root);
        state.memory_pressure = MemoryPressure::Hard;

        let hover = Request {
            id: lsp_server::RequestId::from(1),
            method: "textDocument/hover".to_string(),
            params: serde_json::json!({
                "textDocument": { "uri": "file:///test/a.gd" },
                "position": { "line": 0, "character": 0 }
            }),
        };
        let resp = dispatch_request(&mut state, hover);
        let err = resp.error.expect("hover must be shed at Hard pressure");
        assert_eq!(
            err.code, ERR_CONTENT_MODIFIED,
            "a shed analyze-using request must return ContentModified (-32801)"
        );
        // The shed path must clean up the per-request token (no leak into pending_requests).
        assert!(
            state.pending_requests.is_empty(),
            "shed must unregister the token"
        );
        assert!(
            state.current_token.is_none(),
            "shed must clear current_token"
        );

        // A parse-only method is exempt — documentSymbol is NOT in the analyze_using set.
        let doc_symbol = Request {
            id: lsp_server::RequestId::from(2),
            method: "textDocument/documentSymbol".to_string(),
            params: serde_json::json!({ "textDocument": { "uri": "file:///test/a.gd" } }),
        };
        let resp = dispatch_request(&mut state, doc_symbol);
        assert_ne!(
            resp.error.as_ref().map(|e| e.code),
            Some(ERR_CONTENT_MODIFIED),
            "documentSymbol must not be shed at Hard pressure; got {:?}",
            resp.error
        );

        // An index-only nav method is also exempt — `definition` is parse + index only (it never
        // re-enters an analyze span), so shedding it would break go-to-definition while reclaiming
        // nothing. It must stay served at Hard pressure, like documentSymbol.
        let definition = Request {
            id: lsp_server::RequestId::from(3),
            method: "textDocument/definition".to_string(),
            params: serde_json::json!({
                "textDocument": { "uri": "file:///test/a.gd" },
                "position": { "line": 0, "character": 0 }
            }),
        };
        let resp = dispatch_request(&mut state, definition);
        assert_ne!(
            resp.error.as_ref().map(|e| e.code),
            Some(ERR_CONTENT_MODIFIED),
            "definition is index-only and must not be shed at Hard pressure; got {:?}",
            resp.error
        );
    }

    #[test]
    fn hard_pressure_publish_diagnostics_does_not_start_uncached_analysis() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("utf-8 temp dir");
        std::fs::write(dir.path().join("project.godot"), "config_version=5\n").unwrap();
        let script = root.join("a.gd");
        std::fs::write(&script, "extends Node\nsignal unused\n").unwrap();
        let (mut state, rx) = state_on(&root);
        let uri = crate::uri::path_to_file_uri(&script).expect("file uri");
        state.vfs.open(
            uri.as_str().to_owned(),
            "extends Node\nsignal unused\n".to_owned(),
            1,
        );
        reindex_open_buffer(&mut state, &uri);
        state.memory_pressure = MemoryPressure::Hard;

        publish_diagnostics(&mut state, uri, Some(1));
        let Message::Notification(note) = rx.recv().expect("publishDiagnostics notification")
        else {
            panic!("expected publishDiagnostics notification");
        };
        let params: PublishDiagnosticsParams = serde_json::from_value(note.params).unwrap();
        assert!(
            params.diagnostics.is_empty(),
            "Hard pressure with no cached analysis must publish parser-only diagnostics, not run analyzer; got {:?}",
            params.diagnostics
        );
    }

    /// WP-O4: the cancellation gate maps a tripped per-request token to a RequestCancelled
    /// (-32800) error response and passes an un-cancelled handler response through untouched. The
    /// single-threaded loop can't interrupt a handler mid-flight (see `tests/cancellation.rs`'s
    /// module doc), so this unit-tests the gate directly; the analyzer-level cancel that flips the
    /// token is covered by `gd_analyze/tests/governor.rs`.
    #[test]
    fn cancellation_gate_maps_tripped_token_to_request_cancelled() {
        let token = CancellationToken::new();
        let ok = Response::new_ok(
            lsp_server::RequestId::from(7),
            serde_json::json!({ "ok": true }),
        );
        // Not cancelled → the handler's response passes through.
        let passed = apply_cancellation_gate(&token, lsp_server::RequestId::from(7), ok.clone());
        assert!(
            passed.error.is_none(),
            "an un-cancelled request must keep its handler response"
        );
        // Cancelled → replaced with RequestCancelled (-32800).
        token.cancel();
        let cancelled = apply_cancellation_gate(&token, lsp_server::RequestId::from(7), ok);
        let err = cancelled
            .error
            .expect("a cancelled request must yield an error response");
        assert_eq!(
            err.code, REQUEST_CANCELLED,
            "a cancelled request must return RequestCancelled (-32800)"
        );
        assert!(err.message.contains("cancelled"));
    }

    /// WP-H1 Soft action: when the ladder transitions to Soft, `evict_half` runs on the
    /// workspace's caches. Build a state with a budget that puts the running process between
    /// soft and hard (peak > soft, peak ≤ hard), stuff the cache, fire react, then assert the
    /// cache shrunk.
    #[test]
    fn react_to_memory_pressure_at_soft_calls_evict_half() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("utf-8 temp dir");
        std::fs::write(dir.path().join("project.godot"), "config_version=5\n").unwrap();
        let options = InitializationOptions::parse(Some(&serde_json::json!({
            "projectRoot": root.as_str(),
            "memory": { "cacheCapacity": 32 },
        })));
        let workspace = Workspace::load(&root, &options);
        let (tx, _rx) = crossbeam_channel::unbounded::<Message>();
        let mut rss = RssSampler::new();
        rss.sample_now("test_baseline");
        // Hard cap WAY above any plausible process RSS (cap below u64::MAX so * 1MB doesn't
        // overflow — saturating_mul saves us either way, but this is cleaner): peak < hard.
        // Soft cap of 1 MB ensures peak > soft for any Rust process.
        let budget = MemoryBudget::from_caps_mb(1, u64::MAX / (1024 * 1024 * 2));
        let mut state = ServerState {
            encoding: PositionEncoding::Utf16,
            caps: ClientCaps::default(),
            options,
            workspace,
            vfs: Vfs::default(),
            sender: tx,
            recorder: None,
            rss,
            pending_requests: FxHashMap::default(),
            current_token: None,
            budget,
            memory_pressure: MemoryPressure::Normal,
            stub_cache: crate::stubs::StubCache::default(),
        };

        // Pre-populate the cache via the dev/test debug-insert helpers. Without entries to
        // evict, the eviction step is silent; with entries the post-transition lens are halved.
        let dummy_text = "# synthetic\n";
        let parse = gd_syntax::parse(dummy_text);
        for i in 0..8 {
            let key = CanonicalKey::for_uri(
                &format!("file:///synthetic_{i}.gd")
                    .parse::<lsp_types::Uri>()
                    .unwrap(),
            );
            state
                .workspace
                .debug_insert_parse_entry(key, i as u64, parse.clone());
        }
        let (parse_before, _) = state.workspace.cache_lens();
        assert_eq!(parse_before, 8);

        react_to_memory_pressure(&mut state);
        assert_eq!(
            state.memory_pressure,
            MemoryPressure::Soft,
            "soft cap = 1 MB so peak > soft; hard cap huge so peak ≤ hard"
        );
        let (parse_after, _) = state.workspace.cache_lens();
        assert_eq!(
            parse_after, 4,
            "the Soft transition's evict_half must drop floor(len / 2) = 4 of the 8 entries"
        );
    }

    /// Recovery transition: state at Soft, peak comes back down (here: synthetic budget shift
    /// — the process didn't actually shrink), ladder fires the `memory_pressure_recovered`
    /// event and resets `memory_pressure` to Normal without touching the caches.
    #[test]
    fn react_to_memory_pressure_records_recovery_without_eviction() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("utf-8 temp dir");
        std::fs::write(dir.path().join("project.godot"), "config_version=5\n").unwrap();
        let (mut state, _rx) = state_on(&root);

        // Stuff the cache so we can prove eviction didn't fire on recovery.
        let dummy = gd_syntax::parse("# x\n");
        for i in 0..6 {
            let key = CanonicalKey::for_uri(
                &format!("file:///r_{i}.gd")
                    .parse::<lsp_types::Uri>()
                    .unwrap(),
            );
            state
                .workspace
                .debug_insert_parse_entry(key, i as u64, dummy.clone());
        }
        let parse_before = state.workspace.cache_lens().0;
        assert_eq!(parse_before, 6);

        // Simulate "we were at Soft on the last tick" — the recovery branch's precondition.
        state.memory_pressure = MemoryPressure::Soft;
        // state's budget (from state_on) caps soft + hard at u64::MAX / 2 MB so the current peak
        // is far below both → react classifies as Normal.
        react_to_memory_pressure(&mut state);
        assert_eq!(state.memory_pressure, MemoryPressure::Normal);
        assert_eq!(
            state.workspace.cache_lens().0,
            parse_before,
            "recovery transition is informational; the caches must not be touched"
        );
    }

    /// WP-W3 overflow path: when any event in a batch has `need_rescan` set,
    /// [`handle_watcher`] must run a full [`Workspace::reconcile`] (a disk re-walk) and SKIP
    /// per-event dispatch — the recovery path that exists for the 10k-file kernel-queue-overflow
    /// case. `reconcile` is unit-tested in isolation (`cache_coherence.rs`), but the *wiring* — that
    /// a rescan flag routes there at all — had no test, so a refactor of the dispatch could silently
    /// drop overflow recovery with every other test still green. The drift here (`late.gd`) is NOT
    /// named in the event's paths, so only a re-walk can discover it; per-event dispatch never would.
    #[test]
    fn need_rescan_event_runs_reconcile_that_recovers_undelivered_drift() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("utf-8 temp dir");
        std::fs::write(dir.path().join("project.godot"), "config_version=5\n").unwrap();
        std::fs::write(
            dir.path().join("hero.gd"),
            "class_name Hero\nextends Node\n",
        )
        .unwrap();

        let (mut state, _rx) = state_on(&root);
        assert!(
            state.workspace.index.registry().contains("Hero"),
            "cold index should have picked up hero.gd's class_name"
        );
        assert!(
            !state.workspace.index.registry().contains("Latecomer"),
            "Latecomer must not exist before the drift is written"
        );

        // Drift the tree on disk. No per-file event is ever delivered for this change — it stands
        // in for an event the OS coalesced away under queue pressure.
        std::fs::write(
            dir.path().join("late.gd"),
            "class_name Latecomer\nextends Node\n",
        )
        .unwrap();

        // A rescan-flagged batch whose paths do NOT mention late.gd. Routing it to reconcile is the
        // only way late.gd can enter the index.
        handle_watcher(&mut state, vec![rescan_event()]);

        assert!(
            state.workspace.index.registry().contains("Latecomer"),
            "need_rescan must trigger a full reconcile that re-walks the tree and indexes the \
             drifted-in late.gd; the rescan branch must bypass (not depend on) per-event dispatch"
        );
    }
}
