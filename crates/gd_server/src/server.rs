//! The LSP server: lifecycle handshake, capability advertisement, and the synchronous event loop
//! that dispatches requests and notifications (`docs/05-lsp-cc-integration.md`).

use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use camino::{Utf8Path, Utf8PathBuf};
use crossbeam_channel::{select, Receiver, Sender};
use lsp_server::{Connection, Message, Notification, Request, Response};
use lsp_types::{
    CallHierarchyServerCapability, CodeDescription, Diagnostic, DiagnosticSeverity, DiagnosticTag,
    DidChangeTextDocumentParams, DidCloseTextDocumentParams, DidOpenTextDocumentParams,
    DidSaveTextDocumentParams, DocumentLinkOptions, HoverProviderCapability,
    ImplementationProviderCapability, InitializeParams, InitializeResult, OneOf,
    PublishDiagnosticsParams, ServerCapabilities, ServerInfo, TextDocumentSyncCapability,
    TextDocumentSyncKind, Uri,
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
use crate::router::{Interrupt, RequestLifecycle, SessionShared};
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
/// JSON-RPC `InvalidRequest` (-32600) — returned for requests received after `shutdown`,
/// per LSP 3.17 §shutdown.
const ERR_INVALID_REQUEST: i32 = -32600;
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
///
/// Not `Copy`: the M8 (#64) completion gates carry owned `Vec`s (the supported
/// `completionItemKind` set, the `resolveSupport`/`itemDefaults` property lists), so the struct
/// is `Clone`-only. Handlers read its fields behind a shared `&state.caps` borrow.
#[derive(Debug, Clone, Default)]
pub(crate) struct ClientCaps {
    /// `textDocument.documentSymbol.hierarchicalDocumentSymbolSupport`. Absent ⇒ `false` ⇒ the
    /// flat 3.16 `SymbolInformation[]` documentSymbol shape (rust-analyzer's
    /// `.unwrap_or_default()` convention): a client that did not opt in must not receive the
    /// nested shape it declined.
    pub(crate) hierarchical_document_symbols: bool,
    /// `textDocument.publishDiagnostics.tagSupport.valueSet` contains `Unnecessary` — gates the
    /// unused/unreachable diagnostic tags (pyright-style: clients without the capability get
    /// byte-identical pre-tag diagnostics).
    pub(crate) diagnostic_tag_unnecessary: bool,
    /// `textDocument.publishDiagnostics.codeDescriptionSupport` — gates `codeDescription.href`
    /// (the per-warning documentation link) on every warning-coded diagnostic. Governs the pull
    /// path too once `textDocument/diagnostic` lands (M7 #61): LSP 3.17's pull-diagnostic client
    /// capabilities carry no codeDescription/tag fields of their own, so the publishDiagnostics
    /// capabilities are the convention for both (rust-analyzer does the same).
    pub(crate) code_description: bool,
    /// `window.workDoneProgress` — gates every server-initiated progress token (M7 #58). When
    /// absent, `window/workDoneProgress/create` is never sent (the spec forbids it) and the
    /// [`crate::progress::ProgressReporter`] is a no-op. Client-token progress (a
    /// `workDoneToken` inside request params) is independent of this flag — the token's
    /// presence is its own opt-in.
    pub(crate) work_done_progress: bool,
    /// `workspace.configuration` (M7 #59) — when true, a `workspace/didChangeConfiguration`
    /// notification triggers a `workspace/configuration` pull for the `"gdls"` section instead
    /// of trusting the notification's payload shape (many clients send `settings: null`); the
    /// modern-client convention.
    pub(crate) workspace_configuration: bool,
    /// `workspace.didChangeWatchedFiles.dynamicRegistration` (M7 #60) — when true, gdls sends
    /// `client/registerCapability` for its watch globs after `initialized`. The ONLY dynamic
    /// registration gdls performs (docs/09 §7.1): it is the one capability Helix honors only
    /// dynamically, and the spec forbids registering the same capability statically too.
    pub(crate) dynamic_watched_files: bool,
    /// `textDocument.hover.contentFormat` (M7 #62): the first kind in the client's preference
    /// order that gdls supports. Absent ⇒ Markdown — see
    /// [`crate::docs::ProseFormat`] for why that pragmatic default stands until the M7 exit
    /// harness captures the real editor profiles.
    pub(crate) hover_format: crate::docs::ProseFormat,
    /// The M8 (#64) completion gates, captured under `textDocument.completion`. Grouped in a
    /// sub-struct so the completion handler reads `state.caps.completion.<gate>` and the rest of
    /// [`ClientCaps`] stays a flat list of feature booleans.
    pub(crate) completion: CompletionCaps,
    /// The M8 (#65) signatureHelp gates, captured under `textDocument.signatureHelp`. Grouped in a
    /// sub-struct like [`CompletionCaps`] so the handler reads `state.caps.signature_help.<gate>`.
    pub(crate) signature_help: SignatureHelpCaps,
}

/// The `textDocument.completion` client capabilities gdls projects each item against (M8 #64).
/// Every field has a documented absent-default so a Godot-unaware / minimal client still gets a
/// well-formed (if downgraded) `CompletionList` — generic-LSP-first (#30).
#[derive(Debug, Clone, Default)]
pub(crate) struct CompletionCaps {
    /// `completionItem.snippetSupport` — when false, callable items insert a bare name (no
    /// `($0)`/`${1:}` placeholders) and `insertTextFormat` stays `PlainText`.
    pub(crate) snippet_support: bool,
    /// `completionItem.insertReplaceSupport` — when false, the item carries a plain
    /// [`lsp_types::TextEdit`] (the `Edit` arm) instead of an [`lsp_types::InsertReplaceEdit`].
    pub(crate) insert_replace_support: bool,
    /// `completionItem.commitCharactersSupport` — when false, items never carry
    /// `commitCharacters` (and even when true they are suppressed in string / new-identifier
    /// contexts).
    pub(crate) commit_characters_support: bool,
    /// `completionItem.documentationFormat` — the first kind gdls supports, reusing the
    /// [`crate::docs::ProseFormat`] negotiation `hover.contentFormat` uses. Absent ⇒ Markdown.
    /// Consumed by `completionItem/resolve` when it renders the lazy documentation.
    pub(crate) documentation_format: crate::docs::ProseFormat,
    /// `completionItem.resolveSupport.properties` — the property names the client will pull lazily
    /// via `completionItem/resolve`. gdls advertises `resolve_provider`, defers `documentation` +
    /// `detail`, and records the list so a future field can be gated on its membership. Empty when
    /// the client sent no `resolveSupport`. Captured-for-future-use this phase (the resolve set is
    /// fixed at documentation+detail).
    #[allow(dead_code)]
    pub(crate) resolve_properties: Vec<String>,
    /// `completionList.itemDefaults` — the `CompletionList.itemDefaults` keys the client honors.
    /// Recorded for a later phase that hoists shared `CommitCharacters`/`editRange` defaults onto
    /// the list; this phase renders each item fully and leaves `itemDefaults` empty. Empty when
    /// absent. Captured-for-future-use this phase.
    #[allow(dead_code)]
    pub(crate) list_item_defaults: Vec<String>,
    /// `completionItemKind.valueSet` — the [`lsp_types::CompletionItemKind`]s the client can
    /// render. `None` ⇒ the LSP default set (`Text`..=`Reference`, i.e. 1..=18); a server kind
    /// outside the negotiated set is dropped to `None` (the item still completes, just without an
    /// icon) rather than sent as an unknown number.
    pub(crate) kind_value_set: Option<Vec<lsp_types::CompletionItemKind>>,
}

/// The `textDocument.signatureHelp` client capabilities gdls projects each signature against
/// (M8 #65). Every field has a documented absent-default so a Godot-unaware / minimal client still
/// gets a well-formed (if downgraded) `SignatureHelp` — generic-LSP-first (#30).
#[derive(Debug, Clone, Default)]
pub(crate) struct SignatureHelpCaps {
    /// `signatureInformation.documentationFormat` — the first kind gdls supports, reusing the
    /// [`crate::docs::ProseFormat`] negotiation `hover.contentFormat` uses. Absent ⇒ PlainText (the
    /// same conservative downgrade [`CompletionCaps::documentation_format`] takes: a client that
    /// didn't enumerate formats can always render plaintext, and attaching un-asked-for markdown
    /// could surface raw `**`).
    pub(crate) documentation_format: crate::docs::ProseFormat,
    /// `signatureInformation.parameterInformation.labelOffsetSupport` — when true, each
    /// [`lsp_types::ParameterInformation`] carries `[start, end)` offsets into the signature label;
    /// when false, a substring label (which must be a literal substring of the signature label).
    pub(crate) label_offset_support: bool,
    /// `signatureInformation.activeParameterSupport` — when true, a per-signature
    /// [`lsp_types::SignatureInformation::active_parameter`] may be set; when false, only the
    /// top-level [`lsp_types::SignatureHelp::active_parameter`] is sent (the pre-3.16 shape).
    pub(crate) active_parameter_support: bool,
}

impl ClientCaps {
    fn negotiate(caps: &lsp_types::ClientCapabilities) -> Self {
        let td = caps.text_document.as_ref();
        ClientCaps {
            hierarchical_document_symbols: td
                .and_then(|t| t.document_symbol.as_ref())
                .and_then(|d| d.hierarchical_document_symbol_support)
                .unwrap_or(false),
            diagnostic_tag_unnecessary: td
                .and_then(|t| t.publish_diagnostics.as_ref())
                .and_then(|p| p.tag_support.as_ref())
                .is_some_and(|t| t.value_set.contains(&DiagnosticTag::UNNECESSARY)),
            code_description: td
                .and_then(|t| t.publish_diagnostics.as_ref())
                .and_then(|p| p.code_description_support)
                .unwrap_or(false),
            work_done_progress: caps
                .window
                .as_ref()
                .and_then(|w| w.work_done_progress)
                .unwrap_or(false),
            workspace_configuration: caps
                .workspace
                .as_ref()
                .and_then(|w| w.configuration)
                .unwrap_or(false),
            dynamic_watched_files: caps
                .workspace
                .as_ref()
                .and_then(|w| w.did_change_watched_files.as_ref())
                .and_then(|d| d.dynamic_registration)
                .unwrap_or(false),
            hover_format: td
                .and_then(|t| t.hover.as_ref())
                .and_then(|h| h.content_format.as_ref())
                .map(|formats| prose_format_from(formats))
                .unwrap_or_default(),
            completion: CompletionCaps::negotiate(td),
            signature_help: SignatureHelpCaps::negotiate(td),
        }
    }
}

/// The first [`crate::docs::ProseFormat`] gdls supports in a client's preference-ordered
/// `MarkupKind` list (Markdown preferred, PlainText accepted, anything else skipped). Shared by
/// `hover.contentFormat` (M7 #62) and `completionItem.documentationFormat` (M8 #64) so both honor
/// the same negotiation; an empty / all-unknown list falls back to the caller's `unwrap_or_default`
/// (Markdown).
fn prose_format_from(formats: &[lsp_types::MarkupKind]) -> crate::docs::ProseFormat {
    formats
        .iter()
        .find_map(|f| match f {
            f if *f == lsp_types::MarkupKind::Markdown => Some(crate::docs::ProseFormat::Markdown),
            f if *f == lsp_types::MarkupKind::PlainText => {
                Some(crate::docs::ProseFormat::PlainText)
            }
            _ => None,
        })
        .unwrap_or_default()
}

impl CompletionCaps {
    /// Walk `textDocument.completion`, mirroring the optional-path `.and_then`/`.unwrap_or`
    /// convention the rest of [`ClientCaps::negotiate`] uses. An absent `completion` capability (a
    /// client that never opted into completion) yields the all-default struct — every gate off,
    /// plaintext docs (the conservative downgrade), the LSP-default kind set.
    fn negotiate(td: Option<&lsp_types::TextDocumentClientCapabilities>) -> Self {
        let completion = td.and_then(|t| t.completion.as_ref());
        let item = completion.and_then(|c| c.completion_item.as_ref());
        CompletionCaps {
            snippet_support: item.and_then(|i| i.snippet_support).unwrap_or(false),
            insert_replace_support: item.and_then(|i| i.insert_replace_support).unwrap_or(false),
            commit_characters_support: item
                .and_then(|i| i.commit_characters_support)
                .unwrap_or(false),
            // Per phase-3 criterion 4 ("No documentationFormat → plaintext docs") the completion
            // documentation default is the conservative downgrade — PlainText — NOT hover's
            // Markdown default: a client that didn't enumerate documentation formats can always
            // render plaintext, and resolve attaching un-asked-for markdown could surface raw `**`.
            // A client that DID enumerate formats gets its preferred supported one via
            // `prose_format_from`.
            documentation_format: item
                .and_then(|i| i.documentation_format.as_ref())
                .map(|formats| prose_format_from(formats))
                .unwrap_or(crate::docs::ProseFormat::PlainText),
            resolve_properties: item
                .and_then(|i| i.resolve_support.as_ref())
                .map(|r| r.properties.clone())
                .unwrap_or_default(),
            list_item_defaults: completion
                .and_then(|c| c.completion_list.as_ref())
                .and_then(|l| l.item_defaults.clone())
                .unwrap_or_default(),
            kind_value_set: completion
                .and_then(|c| c.completion_item_kind.as_ref())
                .and_then(|k| k.value_set.clone()),
        }
    }
}

impl SignatureHelpCaps {
    /// Walk `textDocument.signatureHelp`, mirroring [`CompletionCaps::negotiate`]'s optional-path
    /// convention. An absent `signatureHelp` capability yields the all-default struct — plaintext
    /// docs (the conservative downgrade), no label offsets, no per-signature activeParameter.
    fn negotiate(td: Option<&lsp_types::TextDocumentClientCapabilities>) -> Self {
        let info = td
            .and_then(|t| t.signature_help.as_ref())
            .and_then(|s| s.signature_information.as_ref());
        SignatureHelpCaps {
            // PlainText default for the same reason `CompletionCaps` uses it (see that field):
            // a client that didn't enumerate documentation formats can always render plaintext.
            documentation_format: info
                .and_then(|i| i.documentation_format.as_ref())
                .map(|formats| prose_format_from(formats))
                .unwrap_or(crate::docs::ProseFormat::PlainText),
            label_offset_support: info
                .and_then(|i| i.parameter_information.as_ref())
                .and_then(|p| p.label_offset_support)
                .unwrap_or(false),
            active_parameter_support: info
                .and_then(|i| i.active_parameter_support)
                .unwrap_or(false),
        }
    }
}

/// M7 (#59): what an outstanding server→client request is FOR — how the worker applies its
/// response. (`window/workDoneProgress/create` never appears here; the router consumes those.)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OutboundKind {
    /// A `workspace/configuration` pull for the `"gdls"` section — the response carries one
    /// settings object per requested item; `result[0]` is applied as a runtime re-config.
    Configuration,
    /// The one-shot `client/registerCapability` for the watch globs (M7 #60) — the response is
    /// acknowledgment-only (success logs at debug; an error reply means the session runs on the
    /// native watcher alone, which stays armed regardless).
    RegisterWatchedFiles,
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
    /// M7 (#57): the request-lifecycle registry shared with the router thread. The router
    /// registers every forwarded request and flips cancel/stale flags as control messages
    /// arrive; [`dispatch_request`] reads the verdict at its entry (queued-interrupt
    /// short-circuit) and at its exit (the [`crate::router::SessionShared::finish`]
    /// linearization point). A cancel for an id NOT in the registry is a warn-log no-op
    /// (LSP 3.17 spec: unknown id is allowed).
    pub(crate) shared: Arc<SessionShared>,
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
    /// M7 (#59): outstanding server→client requests the WORKER must correlate (the router
    /// fully consumes `window/workDoneProgress/create` responses itself; everything else is
    /// forwarded here). Keyed by the `"gdls-out-{n}"` ids from
    /// [`crate::router::SessionShared::next_outgoing_id`]. A response with an unknown id is a
    /// warn-log no-op — never a "Method not found" bounce (anti-catalog W3).
    pub(crate) outbound: FxHashMap<RequestId, OutboundKind>,
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

    // M7 (#57): split the wire into router + worker (see `crate::router`). Spawned strictly
    // AFTER `initialize_finish` — the handshake above read `connection.receiver` directly, and
    // from here on the router is its only consumer; this thread reads the forwarded stream.
    // Spawning before the (potentially long) workspace load below means `$/cancelRequest` and
    // content mutations take effect even for requests that queue up while the cold index builds.
    let shared = Arc::new(SessionShared::default());
    let (forward_tx, forward_rx) = crossbeam_channel::unbounded::<Message>();
    let router =
        crate::router::spawn_router(connection.receiver.clone(), forward_tx, Arc::clone(&shared));

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
    //
    // M7 (#58): one progress token spans the whole cold start (load → startup reconcile) — a
    // single spinner arc in the client. Created before the load so the create + begin are on
    // the wire while the long walk runs; the router (already spawned) consumes the create's
    // response, including an error reply that poisons the reporter.
    let mut startup_progress = crate::progress::ProgressReporter::server_initiated(
        connection.sender.clone(),
        &shared,
        caps.work_done_progress,
    );
    startup_progress.begin("Indexing project", None);
    let workspace = Workspace::load_with_progress(&root, &options, &mut startup_progress);
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
        shared: Arc::clone(&shared),
        current_token: None,
        budget,
        memory_pressure: MemoryPressure::Normal,
        outbound: FxHashMap::default(),
        stub_cache: crate::stubs::StubCache::default(),
    };

    // M7 (#60): the one dynamic registration, sent once the session state exists (the
    // `initialized` notification itself was already consumed by `initialize_finish` during the
    // handshake — there is no later hook). No-op without
    // `workspace.didChangeWatchedFiles.dynamicRegistration`.
    register_watched_files(&mut state);

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
    let report =
        state
            .workspace
            .reconcile_with_progress(reconcile_mode, &open_paths, &mut startup_progress);
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
    startup_progress.end(Some(&format!(
        "indexed {} scripts",
        state.workspace.index.file_count()
    )));
    drop(startup_progress);

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
    // M7 (#57): set by the `shutdown` request; requests received after it answer InvalidRequest
    // (-32600) per LSP 3.17 until the `exit` notification breaks the loop.
    let mut shutting_down = false;

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
                            // Begun only on a REAL adoption — a no-op echo must not flash a
                            // spinner. The republish below is the user-visible bulk anyway.
                            let _progress = server_progress(&state, "Reloading Godot API");
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
            recv(forward_rx) -> msg => match msg {
                Ok(Message::Request(req)) => {
                    // WP-P3: record before dispatch so a panicking handler still leaves the
                    // request in the trace (the artifact is the only way to reproduce the panic).
                    if let Some(rec) = state.recorder.as_mut() {
                        rec.record_request(&req);
                    }
                    // M7 (#57): lsp-server's `Connection::handle_shutdown` is unusable here —
                    // it recv()s on `connection.receiver` waiting for `exit`, which the router
                    // now consumes (a guaranteed 30 s hang per shutdown). Spec-equivalent
                    // handling inline: answer `shutdown` with null and keep looping until the
                    // `exit` notification breaks the loop.
                    let resp = if req.method == "shutdown" {
                        shutting_down = true;
                        Response::new_ok(req.id, serde_json::Value::Null)
                    } else if shutting_down {
                        // Deregister the lifecycle the router opened for it, then refuse.
                        let _ = state.shared.finish(&req.id);
                        Response::new_err(
                            req.id,
                            ERR_INVALID_REQUEST,
                            "request received after shutdown".to_string(),
                        )
                    } else {
                        dispatch_request(&mut state, req)
                    };
                    if let Err(e) = state.sender.send(Message::Response(resp)) {
                        // Send only errors when the receiver is closed. The next select! tick
                        // will hit the Err(_) arm on the forwarded stream and break.
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
                Ok(Message::Response(resp)) => handle_outbound_response(&mut state, resp),
                Err(_) => break, // router hung up — connection closed or `exit` already forwarded
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
                        let mut progress = server_progress(&state, "Reconciling project");
                        let report = state.workspace.reconcile_with_progress(
                            crate::workspace::ReconcileMode::FullStat,
                            &open_paths,
                            &mut progress,
                        );
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
    // M7 (#57): the router has already exited on every path that breaks the loop above (it
    // forwarded `exit` and broke, or the connection disconnected and ended its iterator) — this
    // join only reaps the thread, it cannot hang.
    if router.join().is_err() {
        log::warn!("router thread panicked during shutdown; session was already ending");
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

/// M7 (#59): correlate a client response to an outstanding server→client request. Unknown ids
/// are a warn-log no-op — never a "Method not found" bounce (anti-catalog W3).
fn handle_outbound_response(state: &mut ServerState, resp: Response) {
    let Some(kind) = state.outbound.remove(&resp.id) else {
        log::warn!(
            "ignoring a response with unknown id {:?} (no outstanding server request)",
            resp.id
        );
        return;
    };
    match kind {
        OutboundKind::Configuration => {
            if let Some(err) = &resp.error {
                log::warn!(
                    "client rejected workspace/configuration ({}); keeping the previous \
                     configuration",
                    err.message
                );
                return;
            }
            // One settings value per requested item; we request exactly one ("gdls").
            let section = resp
                .result
                .as_ref()
                .and_then(|r| r.as_array())
                .and_then(|items| items.first())
                .cloned()
                .unwrap_or(serde_json::Value::Null);
            if section.is_null() {
                log::info!(
                    "workspace/configuration returned no \"gdls\" section; keeping the previous \
                     configuration"
                );
                return;
            }
            apply_runtime_config(state, &section);
        }
        OutboundKind::RegisterWatchedFiles => match &resp.error {
            Some(err) => log::warn!(
                "client rejected client/registerCapability for didChangeWatchedFiles ({}); \
                 running on the native watcher alone",
                err.message
            ),
            None => log::debug!("client acknowledged the didChangeWatchedFiles registration"),
        },
    }
}

/// M7 (#59): apply a runtime re-configuration payload — the same schema as
/// `initializationOptions`, with the runtime contract: malformed input keeps the PREVIOUS
/// configuration (logged + surfaced via `window/showMessage`), never a mid-session reset to
/// defaults; session-structural fields are warned about and retained; only genuinely changed
/// runtime-reloadable groups (`strict`, `analyzer`, `memory`) re-apply, so a no-op payload
/// causes no cache churn and no republish.
fn apply_runtime_config(state: &mut ServerState, raw: &serde_json::Value) {
    let new_options = match InitializationOptions::parse_runtime(raw) {
        Ok(parsed) => parsed,
        Err(e) => {
            log::warn!("invalid runtime configuration ({e}); keeping the previous configuration");
            show_message(
                state,
                lsp_types::MessageType::WARNING,
                "gdls: invalid configuration payload; keeping the previous configuration",
            );
            return;
        }
    };

    // Group-level presence gating: a top-level key ABSENT from the payload means "keep the
    // current session value" — an editor whose sparse `gdls` section only carries (say)
    // `strict` must not silently reset non-default `analyzer`/`memory` knobs configured in
    // `initializationOptions` at startup. A PRESENT group is taken as that group's complete
    // snapshot (the LSP configuration-section convention).
    let provided = |key: &str| raw.get(key).is_some();

    // Session-structural fields can't re-apply mid-session — each is baked into the workspace
    // load / watcher / dump topology at startup. Warn per drifted (and provided) field, keep
    // the old value.
    let old = &state.options;
    let structural: [(&str, bool); 6] = [
        (
            "projectRoot",
            provided("projectRoot") && new_options.project_root != old.project_root,
        ),
        (
            "extensionApiPath",
            provided("extensionApiPath")
                && new_options.extension_api_path != old.extension_api_path,
        ),
        (
            "godotBinaryPath",
            provided("godotBinaryPath") && new_options.godot_binary_path != old.godot_binary_path,
        ),
        (
            "autoDumpExtensionApi",
            provided("autoDumpExtensionApi")
                && new_options.auto_dump_extension_api != old.auto_dump_extension_api,
        ),
        (
            "embeddedApiFallback",
            provided("embeddedApiFallback")
                && new_options.embedded_api_fallback != old.embedded_api_fallback,
        ),
        (
            "stubCacheDir",
            provided("stubCacheDir") && new_options.stub_cache_dir != old.stub_cache_dir,
        ),
    ];
    for (field, drifted) in structural {
        if drifted {
            log::warn!(
                "runtime configuration changes `{field}`, which is session-structural — keeping \
                 the current value (restart gdls to apply it)"
            );
        }
    }

    let strict_changed = provided("strict") && new_options.strict != state.options.strict;
    let analyzer_changed = provided("analyzer") && new_options.analyzer != state.options.analyzer;
    let memory_changed = provided("memory") && new_options.memory != state.options.memory;

    if strict_changed {
        log::info!(
            "runtime configuration: strict profile/overrides changed; rebuilding the warning \
             policy and republishing open buffers"
        );
        state.options.strict = new_options.strict;
        state.workspace.apply_strict(&state.options.strict);
    }
    if analyzer_changed {
        log::info!("runtime configuration: analyzer knobs changed; invalidating cached analyses");
        state.options.analyzer = new_options.analyzer;
        state.workspace.set_analyzer_config(&state.options.analyzer);
    }
    if memory_changed {
        log::info!("runtime configuration: memory budget/caches changed");
        state.options.memory = new_options.memory;
        state.budget = MemoryBudget::resolve(&state.options.memory, bench_budget_path().as_deref());
        state
            .workspace
            .set_cache_capacity(state.options.memory.cache_capacity());
    }
    if strict_changed || analyzer_changed {
        republish_all_open_buffers(state);
    }
}

/// Send a `window/showMessage` notification — the operator-facing channel for conditions that
/// deserve more than a stderr log line (M7 §5 showMessage conventions: used sparingly, never as
/// log spam).
fn show_message(state: &ServerState, kind: lsp_types::MessageType, message: &str) {
    let note = Notification {
        method: "window/showMessage".to_string(),
        params: serde_json::json!({ "type": kind, "message": message }),
    };
    let _ = state.sender.send(Message::Notification(note));
}

/// M7 (#58): a begun server-initiated progress reporter for a mid-session phase (re-index,
/// reconcile). No-op without `window.workDoneProgress`. The caller reports into it and either
/// ends it with a summary or lets the drop guard close the arc.
fn server_progress(state: &ServerState, title: &str) -> crate::progress::ProgressReporter {
    let mut reporter = crate::progress::ProgressReporter::server_initiated(
        state.sender.clone(),
        &state.shared,
        state.caps.work_done_progress,
    );
    reporter.begin(title, None);
    reporter
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
        let mut progress = server_progress(state, "Reconciling project");
        let report = state.workspace.reconcile_with_progress(
            crate::workspace::ReconcileMode::FullStat,
            &open_paths,
            &mut progress,
        );
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

    // Clone the project root ONCE per batch — classification and application both need it by
    // reference; cloning per reaction would scale linearly with the (typically large) batch.
    let root = state.workspace.project.root.clone();
    let reactions: Vec<Reaction> = events
        .iter()
        .flat_map(|ev| watcher::classify_event(ev, &root))
        .collect();
    apply_reaction_batch(state, reactions, &root, &open_paths);
}

/// Apply one classified batch of [`Reaction`]s — the shared applier behind the native watcher
/// (`handle_watcher`) and client-delivered `workspace/didChangeWatchedFiles` events (M7 #60), so
/// every mutation from either source flows through the same `Workspace::reindex`/`remove` →
/// `Index::txn` → `Index::verify()` funnel and the same coalesced project/native reload.
fn apply_reaction_batch(
    state: &mut ServerState,
    reactions: Vec<Reaction>,
    root: &Utf8Path,
    open_paths: &FxHashSet<Utf8PathBuf>,
) {
    // WP-RD11 (3): coalesce the project/native-DB reload. The per-file `GdSource` reactions are
    // applied as they come (each is an independent index mutation), but a batch that touches
    // `project.godot` AND two `.gdextension` files AND `extension_api.json` must reload the native
    // DB + re-enumerate ONCE — not four times, each followed by a full `republish_all_open_buffers`.
    // Scan the batch into two booleans and do the (expensive) reload + republish at most once after.
    let mut project_changed = false;
    let mut native_changed = false;
    for reaction in reactions {
        match reaction {
            // Coalesce the project/native-DB reactions into the post-batch reload below.
            Reaction::ProjectGodot
            | Reaction::Gdextension { .. }
            | Reaction::DocClassesXml { .. } => project_changed = true,
            Reaction::ExtensionApiJson => native_changed = true,
            // GdSource (per-file index mutation) and Other (dropped) both flow through
            // `apply_reaction` so each still opens a `watcher_event` span — the WP-RD7
            // `SkipReason` on an `Other` surfaces in the trace there.
            other => apply_reaction(state, other, root, open_paths),
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
        let _progress = server_progress(state, "Reloading Godot API");
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
            // Begun only on a REAL change — a torn-read keep or post-adoption echo must not
            // flash a spinner. The republish below is the user-visible bulk anyway.
            let _progress = server_progress(state, "Reloading Godot API");
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

/// M7 (#60): translate client-delivered `FileEvent`s into [`Reaction`]s (same classification
/// gate as the native watcher — `watcher::classify_client_event`) and run them through the
/// shared batch applier. Out-of-root and excluded paths drop in classification/`apply_reaction`
/// exactly as native events do; open buffers keep winning over disk.
fn handle_client_file_events(state: &mut ServerState, changes: Vec<lsp_types::FileEvent>) {
    if changes.is_empty() {
        return;
    }
    let root = state.workspace.project.root.clone();
    let open_paths = open_buffer_paths(state);
    let reactions: Vec<Reaction> = changes
        .iter()
        .filter_map(|ev| {
            let path = uri_to_path(&ev.uri)?;
            let path = gd_project::normalize_path(&path);
            let change = match ev.typ {
                lsp_types::FileChangeType::CREATED => FileChange::Created,
                lsp_types::FileChangeType::CHANGED => FileChange::Modified,
                lsp_types::FileChangeType::DELETED => FileChange::Deleted,
                other => {
                    log::debug!("didChangeWatchedFiles: dropping unknown change type {other:?}");
                    return None;
                }
            };
            Some(watcher::classify_client_event(&path, change, &root))
        })
        .collect();
    apply_reaction_batch(state, reactions, &root, &open_paths);
}

/// M7 (#60): the one dynamic registration gdls performs — `client/registerCapability` for the
/// watch globs, sent after `initialized` iff the client advertised
/// `workspace.didChangeWatchedFiles.dynamicRegistration`. Deliberately broad `**/` globs: the
/// classification funnel re-applies the same root/exclusion rules the native watcher uses, so
/// over-delivery converges to identical semantics. (`**/*.tscn` joins the list in M11.)
fn register_watched_files(state: &mut ServerState) {
    if !state.caps.dynamic_watched_files {
        return;
    }
    let watcher = |glob: &str| serde_json::json!({ "globPattern": glob });
    let id = state.shared.next_outgoing_id();
    state
        .outbound
        .insert(id.clone(), OutboundKind::RegisterWatchedFiles);
    let req = Request {
        id,
        method: "client/registerCapability".to_string(),
        params: serde_json::json!({
            "registrations": [{
                "id": "gdls-watched-files",
                "method": "workspace/didChangeWatchedFiles",
                "registerOptions": {
                    "watchers": [
                        watcher("**/*.gd"),
                        watcher("**/project.godot"),
                        watcher("**/*.gdextension"),
                        watcher("**/extension_api.json"),
                        watcher("**/doc_classes/*.xml"),
                    ],
                },
            }],
        }),
    };
    if state.sender.send(Message::Request(req)).is_err() {
        log::warn!("client/registerCapability send failed (client disconnected?)");
    }
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
                            // M7 (#60): duplicate-delivery gate — the same on-disk change can
                            // arrive from BOTH the native watcher and a client's
                            // didChangeWatchedFiles. A reindex is not a free no-op (it bumps the
                            // epoch and forces re-analysis), so identical content skips it. The
                            // stat refresh still runs to keep the warm-start table current.
                            if state.workspace.disk_apply_is_duplicate(&path, &text) {
                                log::debug!(
                                    "watcher: duplicate delivery for {path}; reindex skipped"
                                );
                                state.workspace.update_stat_from_disk(&path);
                                return;
                            }
                            state
                                .workspace
                                .reindex(&path, &gd_syntax::parse(&text).tree);
                            state.workspace.record_disk_apply(&path, &text);
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
                            state.workspace.record_disk_apply(&to, &text);
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
        // M7 (#58): the two genuinely long requests advertise workDoneProgress so clients send a
        // workDoneToken in their params; the other providers stay bare booleans.
        workspace_symbol_provider: Some(OneOf::Right(lsp_types::WorkspaceSymbolOptions {
            work_done_progress_options: lsp_types::WorkDoneProgressOptions {
                work_done_progress: Some(true),
            },
            resolve_provider: None,
        })),
        definition_provider: Some(OneOf::Left(true)),
        references_provider: Some(OneOf::Right(lsp_types::ReferencesOptions {
            work_done_progress_options: lsp_types::WorkDoneProgressOptions {
                work_done_progress: Some(true),
            },
        })),
        // M9 (#67): documentHighlight — the in-file subset of references, with Read/Write kinds.
        // It runs on cursor-rest (a hot request) over the current file only, so it carries no
        // workDoneProgress (no project-wide fan-out to report) — a plain options struct.
        document_highlight_provider: Some(OneOf::Right(lsp_types::DocumentHighlightOptions {
            work_done_progress_options: Default::default(),
        })),
        hover_provider: Some(HoverProviderCapability::Simple(true)),
        implementation_provider: Some(ImplementationProviderCapability::Simple(true)),
        call_hierarchy_provider: Some(CallHierarchyServerCapability::Simple(true)),
        // M8 (#64): completion. Trigger characters are the NON-identifier characters that should
        // auto-pop the list (`.` member access, `$`/`%` node paths — currently the deferred-node
        // policy, `"` resource/string contexts, `@` annotations); identifier characters never go
        // here (the client triggers on those itself). `resolve_provider: true` defers
        // documentation/detail to a `completionItem/resolve` round-trip (lazy — the list stays
        // cheap). `label_details_support: true` lets resolve attach the structured label detail in
        // a later phase.
        completion_provider: Some(lsp_types::CompletionOptions {
            resolve_provider: Some(true),
            trigger_characters: Some(vec![
                ".".to_string(),
                "$".to_string(),
                "%".to_string(),
                "\"".to_string(),
                "@".to_string(),
            ]),
            all_commit_characters: None,
            work_done_progress_options: Default::default(),
            completion_item: Some(lsp_types::CompletionOptionsCompletionItem {
                label_details_support: Some(true),
            }),
        }),
        // M8 (#65): signatureHelp. `(` opens an argument list and `,` advances to the next
        // argument — both should (re)compute the hint; `)` closes a call, so it only RE-triggers
        // (updates/closes an already-showing hint) rather than opening one. Mirrors Godot's editor
        // and rust-analyzer's trigger set.
        signature_help_provider: Some(lsp_types::SignatureHelpOptions {
            trigger_characters: Some(vec!["(".to_string(), ",".to_string()]),
            retrigger_characters: Some(vec![")".to_string()]),
            work_done_progress_options: Default::default(),
        }),
        document_link_provider: Some(DocumentLinkOptions {
            resolve_provider: Some(false),
            work_done_progress_options: Default::default(),
        }),
        // M7 (#61): pull diagnostics. interFileDependencies — a dependency's interface edit
        // changes this file's report (the resultId's epoch component tracks it).
        // workspaceDiagnostics stays false permanently: project-wide pull conflicts with the
        // per-file-diagnostics principle (docs/00 §4; documented skip in docs/09 §5).
        diagnostic_provider: Some(lsp_types::DiagnosticServerCapabilities::Options(
            lsp_types::DiagnosticOptions {
                identifier: Some("gdls".to_string()),
                inter_file_dependencies: true,
                workspace_diagnostics: false,
                work_done_progress_options: Default::default(),
            },
        )),
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
    // M7 (#57) interrupt plumbing: look up (or, with no router in front, register) the request's
    // lifecycle, stash its analyzer token in `current_token` so handlers can read it without
    // extra plumbing, then on handler return deregister + read the interrupt verdict. A tripped
    // lifecycle replaces the handler's response with the matching error response (LSP 3.17):
    // RequestCancelled (-32800) for a client cancel, ContentModified (-32801) for a result
    // invalidated by an intervening edit.
    let req_id = id.clone();
    let lifecycle = state.shared.lifecycle(&req_id);
    // Queued-interrupt short-circuit: the router registered this request when it was read off
    // the wire, so a cancel (or a content mutation) that landed while it sat in the forward
    // queue is already recorded — answer without running the handler. The client's retry, which
    // arrives after the mutation in wire order, runs against the new text.
    if let Some(interrupt) = lifecycle.interrupt() {
        let _ = state.shared.finish(&req_id);
        tracing::info!(
            target: "cancel",
            id = %req_id,
            method = %method,
            interrupt = ?interrupt,
            "request short-circuited before dispatch"
        );
        return interrupt_response(interrupt, req_id);
    }
    state.current_token = Some(lifecycle.token());
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
            // M9 (#67): documentHighlight lazy-analyzes the current file for the same cursor→symbol
            // classification references uses (resolved member vs local vs autoload) — analysis-
            // priced, so it sheds at Hard memory pressure with ContentModified like references.
            | "textDocument/documentHighlight"
            | "callHierarchy/incomingCalls"
            | "callHierarchy/outgoingCalls"
            // M8 (#64): `completion` runs `analyze_if_gd` to resolve the base expression's type
            // (the ATTRIBUTE arm) — analysis-priced, so it sheds at Hard with ContentModified
            // exactly like hover. `completionItem/resolve` is deliberately NOT here: it only reads
            // the native DB / cached interface and never starts a fresh analyze, so shedding it
            // would reclaim nothing.
            | "textDocument/completion"
            // M8 (#65): `signatureHelp` runs `analyze_if_gd` to resolve the call receiver's type
            // (the `base.method(` arm), exactly like completion's ATTRIBUTE path — analysis-priced,
            // so it sheds at Hard with ContentModified too.
            | "textDocument/signatureHelp"
    );
    if state.memory_pressure == MemoryPressure::Hard && analyze_using {
        // Re-record the request as cancelled-cum-shed so the per-handler trace still shows the
        // refused request rather than a silent drop. Cleanup is the same as the bottom of the fn
        // (deregister via `finish_request`, clear current_token), so jump straight to the end
        // via early return.
        state.current_token = None;
        tracing::warn!(
            target: "shed",
            id = %req_id,
            method = %method,
            "request shed at Hard memory pressure",
        );
        return finish_request(
            &state.shared,
            Response::new_err(
                req_id,
                ERR_CONTENT_MODIFIED,
                "server is shedding requests under memory pressure; please retry".to_string(),
            ),
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
        // M9 (#67): documentHighlight. Returns `DocumentHighlight[]` for the symbol under the
        // cursor scoped to the request file (or `null` when the cursor isn't on an identifier).
        "textDocument/documentHighlight" => handle!(handlers::document_highlight),
        "textDocument/implementation" => handle!(handlers::implementation),
        "textDocument/prepareCallHierarchy" => handle!(handlers::prepare_call_hierarchy),
        "callHierarchy/incomingCalls" => handle!(handlers::incoming_calls),
        "callHierarchy/outgoingCalls" => handle!(handlers::outgoing_calls),
        "workspace/symbol" => handle!(handlers::workspace_symbol),
        // M8 (#64): completion + its lazy resolve. `completion` returns a `CompletionList`
        // (never a bare array — W18); `resolve` fills documentation/detail and leaves the
        // ranking/edit fields untouched.
        "textDocument/completion" => handle!(handlers::completion),
        "completionItem/resolve" => handle!(handlers::completion_item_resolve),
        // M8 (#65): signatureHelp. Returns `SignatureHelp` (or `null` when the cursor is in no
        // call). `serde_json::to_value(None)` serializes to `null`, which is what the wire wants.
        "textDocument/signatureHelp" => handle!(handlers::signature_help),
        // M7 (#61): pull diagnostics. NOT in the Hard-pressure shed list above — `analyze_gd`
        // self-degrades to parser-only + cached results there, exactly like the push path, so
        // pull and push stay byte-identical under pressure too.
        "textDocument/diagnostic" => handle!(document_diagnostic),
        _ => Response::new_err(
            id,
            ERR_METHOD_NOT_FOUND,
            format!("unhandled method: {method}"),
        ),
    };
    // M7 (#57) — deregister the request and, if its lifecycle was interrupted during the
    // handler's run, replace the (potentially partial) response with the matching error.
    // The LSP 3.17 spec lets a cancelled request return either a partial result or this error;
    // we always use the error response so a client that respects RequestCancelled doesn't
    // have to special-case which results may be partial. The lifecycle's token already pushed
    // `analyzer: request cancelled` into the analyzer's diagnostic stream — the
    // operator-visible breadcrumb of when the interrupt landed (the partial result it tagged
    // is discarded along with the rest of the response, and `Workspace::analyze_with_options`
    // never caches a bailed result).
    state.current_token = None;
    finish_request(&state.shared, resp)
}

/// M7 (#57) interrupt gate: deregister the response's request from the in-flight registry —
/// the removal under the registry lock is the staleness linearization point (`crate::router`
/// module doc) — and, when the lifecycle was interrupted, replace the (possibly partial)
/// response with the matching LSP 3.17 error. A response whose id was never registered (e.g.
/// a lifecycle already consumed by the queued-interrupt short-circuit) passes through.
fn finish_request(shared: &SessionShared, resp: Response) -> Response {
    let lifecycle = shared.finish(&resp.id);
    match lifecycle.as_deref().and_then(RequestLifecycle::interrupt) {
        Some(interrupt) => {
            tracing::info!(target: "cancel", id = %resp.id, interrupt = ?interrupt, "request interrupted");
            interrupt_response(interrupt, resp.id)
        }
        None => resp,
    }
}

/// Project an [`Interrupt`] verdict into its LSP 3.17 error response: `Cancelled` →
/// `RequestCancelled` (-32800), `Stale` → `ContentModified` (-32801; the client retries against
/// the new content).
fn interrupt_response(interrupt: Interrupt, id: RequestId) -> Response {
    match interrupt {
        Interrupt::Cancelled => {
            Response::new_err(id, REQUEST_CANCELLED, "request cancelled".to_string())
        }
        Interrupt::Stale => Response::new_err(
            id,
            ERR_CONTENT_MODIFIED,
            "content modified during request; please retry".to_string(),
        ),
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
            // M7 (#57): the router already flipped the matching lifecycle the moment this
            // notification came off the wire (that immediacy IS the preemption — see
            // `crate::router`); by the time the forwarded copy reaches this arm the request has
            // often already been answered and deregistered. Re-cancel idempotently as a
            // belt-and-suspenders backstop, at debug level — the router owns the operator-facing
            // logging (cancel_requested trace / unknown-id warn).
            if let Ok(p) = parse_params::<CancelParams>(&method, params) {
                let id = crate::router::request_id_from_number_or_string(p.id);
                let found = state.shared.cancel(&id);
                log::debug!("$/cancelRequest for {id:?} reached the worker (in-flight: {found})");
            }
        }
        "workspace/didChangeWatchedFiles" => {
            // M7 (#60): client-observed file events merge into the same Reaction funnel the
            // native watcher feeds — same exclusion filter, same classification, same
            // batch-coalescing applier, so `Index::verify()` guards these mutations too. The
            // native watcher stays armed; duplicate delivery of one change is collapsed by the
            // content-fingerprint gate in `apply_reaction_inner`.
            if let Ok(p) = parse_params::<lsp_types::DidChangeWatchedFilesParams>(&method, params) {
                handle_client_file_events(state, p.changes);
            }
        }
        "workspace/didChangeConfiguration" => {
            // M7 (#59): runtime re-config. With `workspace.configuration` advertised, the
            // notification's payload is ignored (many clients send `settings: null` by
            // convention) and the real settings are pulled via `workspace/configuration`;
            // otherwise the payload itself is applied — accepting either a sectioned
            // `settings.gdls` object or the bare settings object.
            if state.caps.workspace_configuration {
                let id = state.shared.next_outgoing_id();
                state
                    .outbound
                    .insert(id.clone(), OutboundKind::Configuration);
                let req = Request {
                    id: id.clone(),
                    method: "workspace/configuration".to_string(),
                    params: serde_json::json!({
                        "items": [{ "section": "gdls" }],
                    }),
                };
                if state.sender.send(Message::Request(req)).is_err() {
                    // Undelivered ⇒ no response will ever correlate; drop the entry rather
                    // than leak it for the session's lifetime.
                    state.outbound.remove(&id);
                    log::warn!("workspace/configuration send failed (client disconnected?)");
                }
            } else if let Ok(p) =
                parse_params::<lsp_types::DidChangeConfigurationParams>(&method, params)
            {
                let raw = match &p.settings {
                    serde_json::Value::Object(map) if map.contains_key("gdls") => {
                        p.settings["gdls"].clone()
                    }
                    other => other.clone(),
                };
                apply_runtime_config(state, &raw);
            }
        }
        // NOTE: the `initialized` notification never reaches this dispatcher — lsp-server's
        // `initialize_finish` consumes it as part of the handshake. Post-initialized work
        // (e.g. the M7 #60 dynamic registration) lives in `serve_inner` after state
        // construction instead.
        "initialized" => log::info!("client reported initialized"),
        other => log::debug!("ignoring notification: {other}"),
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
            // Disk-sourced like the watcher arms: record for the M7 (#60) duplicate-delivery
            // gate (the close-time disk state often echoes right back as a watcher event).
            state.workspace.record_disk_apply(&path, &text);
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
    let params = PublishDiagnosticsParams {
        diagnostics: diagnostic_items(state, &uri),
        uri,
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

/// Parse + analyze the open buffer for `uri` into the merged diagnostic set — the single
/// computation behind `textDocument/publishDiagnostics` (push) and `textDocument/diagnostic`
/// (pull), so the two wire shapes carry byte-identical items by construction. No open buffer
/// yields the empty set (the `didClose` clear-path). The pull-only `resultId` is deliberately
/// NOT computed here: the push path would discard it on every didOpen/didChange
/// ([`result_id_for`] is the pull handler's job, exactly once per pull).
fn diagnostic_items(state: &mut ServerState, uri: &Uri) -> Vec<Diagnostic> {
    // v1.0.4 (#34): stub buffers never self-diagnose — a materialized native API page need not
    // be analyzable GDScript, only readable as it. Matched against the stubs BASE root (any
    // version/hash: an old-hash stub can stay open across a mid-session dump swap). The caller
    // still publishes the empty set, so a client that somehow held diagnostics clears them.
    let is_stub = crate::stubs::is_stub_uri(uri, state.options.stub_cache_dir.as_deref());
    let items: Vec<Diagnostic> = if is_stub {
        Vec::new()
    } else {
        match state.vfs.get(uri.as_str()).map(|d| d.text()) {
            Some(text) => {
                let parsed = state.workspace.parse(&CanonicalKey::for_uri(uri), &text);
                // Only `.gd` files go through the analyzer — for any other open buffer the syntax
                // diagnostics carry the publish on their own.
                let analyzed = analyze_gd(state, uri, &parsed.tree, &text);
                // Related-location memo BEFORE the doc borrow: every distinct cross-file target
                // (a SHADOWED_VARIABLE_BASE_CLASS base script) is read once into a rope for the
                // encoding-correct projection below. Bounded by the related entries one file's
                // diagnostics carry — zero for almost every publish.
                let related_texts = related_location_texts(state, analyzed.as_deref());
                match state.vfs.get(uri.as_str()) {
                    Some(doc) => {
                        let mapper = PositionMapper::new(&doc.rope, state.encoding);
                        collect_diagnostics(
                            &mapper,
                            state.encoding,
                            &state.caps,
                            uri,
                            &related_texts,
                            &parsed.diagnostics,
                            analyzed.as_deref(),
                        )
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
    items
}

/// The pull-diagnostics `resultId` for `uri`'s CURRENT state — cheap (no parse, no analysis):
/// `version:contentFingerprint:dependencyEpoch:analysisGeneration`.
///
/// - `version` + fingerprint: the buffer's own content identity.
/// - [`gd_project::Index::epoch_of`]: dependency-aware — an edit to a dependency's *interface*
///   bumps this file's epoch through the reverse-dependency closure, which is exactly what the
///   advertised `interFileDependencies: true` promises a pulling client.
/// - [`Workspace::analysis_generation`]: wholesale invalidations (native/project reloads) the
///   other two components cannot see.
///
/// `None` (never matches, so a pull always recomputes) when there is no open buffer, or when a
/// `.gd` buffer's analysis would be / was shed under Hard memory pressure with no cached result —
/// a degraded report must never be pinned as `unchanged`.
fn result_id_for(state: &mut ServerState, uri: &Uri) -> Option<String> {
    let doc = state.vfs.get(uri.as_str())?;
    let doc_version = doc.version;
    let text = doc.text();
    let is_stub = crate::stubs::is_stub_uri(uri, state.options.stub_cache_dir.as_deref());
    let path = uri_to_path(uri);
    let is_gd = !is_stub && path.as_deref().is_some_and(|p| p.extension() == Some("gd"));
    if is_gd && state.memory_pressure == MemoryPressure::Hard {
        let key = CanonicalKey::for_uri(uri);
        let gd_path = path.as_deref().expect("invariant: is_gd implies a path");
        // A shed (uncached) analysis must never be pinned as `unchanged` — no id at all.
        state.workspace.cached_analysis(&key, gd_path, &text)?;
    }
    let fingerprint = crate::workspace::fingerprint(&text);
    let epoch = path
        .as_deref()
        .and_then(|p| state.workspace.index.file_id(p))
        .map_or(0, |fid| state.workspace.index.epoch_of(fid));
    let generation = state.workspace.analysis_generation();
    Some(format!(
        "{doc_version}:{fingerprint:016x}:{epoch}:{generation}"
    ))
}

/// `textDocument/diagnostic` (M7 #61) — pull diagnostics. The same computation as push (items
/// byte-identical); a matching `previousResultId` short-circuits to an `unchanged` report
/// without parsing or analyzing. `workspace/diagnostic` stays deliberately unimplemented
/// (`docs/09 §5` skip row: it conflicts with the per-file-diagnostics principle), and push
/// stays on for older clients.
fn document_diagnostic(
    state: &mut ServerState,
    params: lsp_types::DocumentDiagnosticParams,
) -> lsp_types::DocumentDiagnosticReportResult {
    use lsp_types::{
        DocumentDiagnosticReport, DocumentDiagnosticReportResult, FullDocumentDiagnosticReport,
        RelatedFullDocumentDiagnosticReport, RelatedUnchangedDocumentDiagnosticReport,
        UnchangedDocumentDiagnosticReport,
    };
    let uri = params.text_document.uri;
    // Computed exactly once per pull: it is valid both before and after `diagnostic_items`
    // below — running the analysis changes none of the id's inputs (version, content
    // fingerprint, dependency epoch, generation) on this single-threaded path.
    let current = result_id_for(state, &uri);
    if let (Some(previous), Some(current)) = (params.previous_result_id, current.clone()) {
        if previous == current {
            return DocumentDiagnosticReportResult::Report(DocumentDiagnosticReport::Unchanged(
                RelatedUnchangedDocumentDiagnosticReport {
                    related_documents: None,
                    unchanged_document_diagnostic_report: UnchangedDocumentDiagnosticReport {
                        result_id: current,
                    },
                },
            ));
        }
    }
    DocumentDiagnosticReportResult::Report(DocumentDiagnosticReport::Full(
        RelatedFullDocumentDiagnosticReport {
            related_documents: None,
            full_document_diagnostic_report: FullDocumentDiagnosticReport {
                items: diagnostic_items(state, &uri),
                result_id: current,
            },
        },
    ))
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
///
/// The `tags` / `codeDescription` fields are LSP-projection-only additions: Godot's own output
/// never serializes them, so message strings, spans, and severities stay byte-identical to the
/// faithful stream (`.out` conformance untouched). Tags are gated on the client's
/// `publishDiagnostics.tagSupport` (pyright-style); the docs link ships ungated
/// (rust-analyzer-style — clients ignore unknown members).
fn collect_diagnostics(
    mapper: &PositionMapper,
    enc: PositionEncoding,
    caps: &ClientCaps,
    request_uri: &Uri,
    related_texts: &FxHashMap<gd_project::FileId, (Uri, ropey::Rope)>,
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
                tags: if caps.diagnostic_tag_unnecessary {
                    unnecessary_tag(d.warning_code())
                } else {
                    None
                },
                code_description: if caps.code_description {
                    d.warning_code().map(|code| CodeDescription {
                        href: warning_docs_uri(code),
                    })
                } else {
                    None
                },
                related_information: project_related(
                    d.related(),
                    mapper,
                    enc,
                    request_uri,
                    related_texts,
                ),
                ..Default::default()
            });
        }
    }
    out
}

/// Load the text of every DISTINCT cross-file related-location target referenced by `analyzed`'s
/// diagnostics (open buffer wins over disk). Unreadable or unindexed targets are simply absent —
/// their entries drop at projection, never the diagnostic itself.
fn related_location_texts(
    state: &mut ServerState,
    analyzed: Option<&gd_analyze::AnalysisResult>,
) -> FxHashMap<gd_project::FileId, (Uri, ropey::Rope)> {
    let mut out: FxHashMap<gd_project::FileId, (Uri, ropey::Rope)> = FxHashMap::default();
    let Some(result) = analyzed else {
        return out;
    };
    for d in &result.diagnostics {
        for rel in d.related() {
            let Some(fid) = rel.file else { continue };
            if out.contains_key(&fid) {
                continue;
            }
            let Some(path) = state.workspace.index.path(fid).map(|p| p.to_path_buf()) else {
                continue;
            };
            let Some(uri) = crate::uri::path_to_file_uri(&path) else {
                continue;
            };
            let text = match state.vfs.get(uri.as_str()).map(|d| d.text()) {
                Some(t) => t,
                None => match std::fs::read_to_string(path.as_std_path()) {
                    Ok(t) => t,
                    Err(e) => {
                        log::debug!(
                            "related location target {path} unreadable ({e}); its entries drop"
                        );
                        continue;
                    }
                },
            };
            out.insert(fid, (uri, ropey::Rope::from_str(&text)));
        }
    }
    out
}

/// Project a diagnostic's [`gd_analyze::RelatedInfo`] entries into LSP
/// `DiagnosticRelatedInformation`: same-file entries map through the request mapper; cross-file
/// entries through the memo'd target rope (a missing target drops the ENTRY, never the
/// diagnostic). `None` when empty, so diagnostics without related locations serialize
/// byte-identically to before.
fn project_related(
    related: &[gd_analyze::RelatedInfo],
    mapper: &PositionMapper,
    enc: PositionEncoding,
    request_uri: &Uri,
    texts: &FxHashMap<gd_project::FileId, (Uri, ropey::Rope)>,
) -> Option<Vec<lsp_types::DiagnosticRelatedInformation>> {
    let mut out = Vec::new();
    for rel in related {
        let (uri, range) = match rel.file {
            None => (request_uri.clone(), mapper.span_to_range(rel.span)),
            Some(fid) => {
                let Some((uri, rope)) = texts.get(&fid) else {
                    continue;
                };
                (
                    uri.clone(),
                    PositionMapper::new(rope, enc).span_to_range(rel.span),
                )
            }
        };
        out.push(lsp_types::DiagnosticRelatedInformation {
            location: lsp_types::Location { uri, range },
            message: rel.message.clone(),
        });
    }
    (!out.is_empty()).then_some(out)
}

/// The unused/unreachable warning family editors render FADED via `DiagnosticTag::UNNECESSARY`
/// (rust-analyzer, clangd, pyright, and tsserver all tag their equivalents). Keyed on the
/// warning CODE, not the published severity, so a strict-mode-promoted UNUSED_* keeps its tag
/// (the clangd behavior).
fn unnecessary_tag(code: Option<gd_analyze::warnings::WarningCode>) -> Option<Vec<DiagnosticTag>> {
    use gd_analyze::warnings::WarningCode::*;
    match code? {
        UnusedVariable
        | UnusedLocalConstant
        | UnusedPrivateClassVariable
        | UnusedParameter
        | UnusedSignal
        | UnreachableCode
        | UnreachablePattern => Some(vec![DiagnosticTag::UNNECESSARY]),
        _ => None,
    }
}

/// Documentation link for one warning code — the `codeDescription.href` target. Every active
/// warning has a `debug/gdscript/warnings/<lower_name>` project setting documented in the
/// ProjectSettings class reference (`gdscript.cpp`'s `GLOBAL_DEF` loop registers one per code),
/// and Godot's docs generator derives a stable Sphinx anchor for each property mechanically
/// (`doc/tools/make_rst.py`: `class_ProjectSettings_property_<path>`, with `/` and `_` rendered
/// as `-`), so the link lands on that warning's own description. The three deprecated codes are
/// registered as *internal* settings (hidden from the class reference, verified against
/// `doc/classes/ProjectSettings.xml` at 4.6.3-stable) and fall back to the warning-system
/// overview page. The whole mapping lives in this one function so a docs-site layout change is
/// a one-line fix.
fn warning_docs_uri(code: gd_analyze::warnings::WarningCode) -> Uri {
    use gd_analyze::warnings::WarningCode::{
        ConstantUsedAsFunction, FunctionUsedAsProperty, PropertyUsedAsFunction,
    };
    match code {
        PropertyUsedAsFunction | ConstantUsedAsFunction | FunctionUsedAsProperty => {
            static OVERVIEW: std::sync::OnceLock<Uri> = std::sync::OnceLock::new();
            OVERVIEW
                .get_or_init(|| {
                    "https://docs.godotengine.org/en/stable/tutorials/scripting/gdscript/warning_system.html"
                        .parse()
                        .expect("invariant: the static Godot docs URL parses as a Uri")
                })
                .clone()
        }
        _ => {
            // Sphinx renders the RST label's `_` (and the setting path's `/`) as `-` in the
            // HTML anchor id — verified live against the published stable page.
            let name = gd_analyze::name_from_code(code)
                .to_ascii_lowercase()
                .replace('_', "-");
            format!(
                "https://docs.godotengine.org/en/stable/classes/class_projectsettings.html\
                 #class-projectsettings-property-debug-gdscript-warnings-{name}"
            )
            .parse()
            .expect("invariant: a lowercased warning name embeds in a valid docs Uri")
        }
    }
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
            shared: Arc::new(SessionShared::default()),
            current_token: None,
            // The watcher-path tests don't exercise the WP-H1 ladder; a synthetic budget with
            // caps far above what a small tempdir workspace will ever observe keeps the ticker
            // arm at MemoryPressure::Normal across the run.
            budget: MemoryBudget::from_caps_mb(u64::MAX / 2, u64::MAX / 2),
            memory_pressure: MemoryPressure::Normal,
            outbound: FxHashMap::default(),
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
            shared: Arc::new(SessionShared::default()),
            current_token: None,
            budget,
            memory_pressure: MemoryPressure::Normal,
            outbound: FxHashMap::default(),
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
        // The shed path must clean up the request's lifecycle (no leak into the registry).
        assert_eq!(
            state.shared.in_flight_len(),
            0,
            "shed must deregister the request"
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

        // M8 (#64): `completion` IS analyze-using (it resolves the base expression's type), so it
        // sheds with ContentModified at Hard pressure exactly like hover.
        let completion = Request {
            id: lsp_server::RequestId::from(4),
            method: "textDocument/completion".to_string(),
            params: serde_json::json!({
                "textDocument": { "uri": "file:///test/a.gd" },
                "position": { "line": 0, "character": 0 }
            }),
        };
        let resp = dispatch_request(&mut state, completion);
        assert_eq!(
            resp.error.as_ref().map(|e| e.code),
            Some(ERR_CONTENT_MODIFIED),
            "completion is analyze-using and must be shed at Hard pressure; got {:?}",
            resp.error
        );

        // M8 (#65): `signatureHelp` IS analyze-using (it resolves the call receiver's type), so it
        // sheds with ContentModified at Hard pressure exactly like completion.
        let signature_help = Request {
            id: lsp_server::RequestId::from(6),
            method: "textDocument/signatureHelp".to_string(),
            params: serde_json::json!({
                "textDocument": { "uri": "file:///test/a.gd" },
                "position": { "line": 0, "character": 0 }
            }),
        };
        let resp = dispatch_request(&mut state, signature_help);
        assert_eq!(
            resp.error.as_ref().map(|e| e.code),
            Some(ERR_CONTENT_MODIFIED),
            "signatureHelp is analyze-using and must be shed at Hard pressure; got {:?}",
            resp.error
        );

        // M8 (#64): `completionItem/resolve` is NOT analyze-using — it only reads the native DB /
        // cached interface, so it must stay served at Hard pressure (shedding it would reclaim
        // nothing). A resolve with no `data` returns the item unchanged: success, not -32801.
        let resolve = Request {
            id: lsp_server::RequestId::from(5),
            method: "completionItem/resolve".to_string(),
            params: serde_json::json!({ "label": "x" }),
        };
        let resp = dispatch_request(&mut state, resolve);
        assert_ne!(
            resp.error.as_ref().map(|e| e.code),
            Some(ERR_CONTENT_MODIFIED),
            "completionItem/resolve is not analyze-using and must not be shed at Hard pressure; \
             got {:?}",
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

    /// M7 (#63): every active warning code links its own ProjectSettings property anchor
    /// (verified live against the published stable docs); the three deprecated codes are
    /// internal settings with no class-reference entry and fall back to the warning-system
    /// overview page.
    #[test]
    fn warning_docs_uri_maps_per_code_anchors_with_deprecated_fallback() {
        use gd_analyze::warnings::WarningCode;
        assert_eq!(
            warning_docs_uri(WarningCode::UnusedVariable).as_str(),
            "https://docs.godotengine.org/en/stable/classes/class_projectsettings.html\
             #class-projectsettings-property-debug-gdscript-warnings-unused-variable"
        );
        // Multi-word name: every `_` in the setting name renders as `-` in the Sphinx anchor.
        assert_eq!(
            warning_docs_uri(WarningCode::GetNodeDefaultWithoutOnready).as_str(),
            "https://docs.godotengine.org/en/stable/classes/class_projectsettings.html\
             #class-projectsettings-property-debug-gdscript-warnings-get-node-default-without-onready"
        );
        for deprecated in [
            WarningCode::PropertyUsedAsFunction,
            WarningCode::ConstantUsedAsFunction,
            WarningCode::FunctionUsedAsProperty,
        ] {
            assert!(
                warning_docs_uri(deprecated)
                    .as_str()
                    .ends_with("warning_system.html"),
                "deprecated codes have no ProjectSettings entry and must use the overview page"
            );
        }
    }

    /// M7 (#57): the interrupt gate deregisters the request and maps its lifecycle verdict —
    /// pass-through when clean, RequestCancelled (-32800) on cancel, ContentModified (-32801) on
    /// stale-by-edit, cancelled winning when both flags are set, and pass-through for an
    /// unregistered id (its lifecycle was already consumed by the queued-interrupt
    /// short-circuit). The analyzer-level cancel that flips the embedded token is covered by
    /// `gd_analyze/tests/governor.rs`; the wire-level races are covered by
    /// `tests/concurrent_dispatch.rs`.
    #[test]
    fn finish_request_maps_interrupts_and_passes_clean_responses() {
        let ok = |id: i32| {
            Response::new_ok(
                lsp_server::RequestId::from(id),
                serde_json::json!({ "ok": true }),
            )
        };

        // Clean lifecycle → the handler's response passes through (and is deregistered).
        let shared = SessionShared::default();
        let _ = shared.lifecycle(&lsp_server::RequestId::from(7));
        let passed = finish_request(&shared, ok(7));
        assert!(
            passed.error.is_none(),
            "an uninterrupted request must keep its handler response"
        );
        assert_eq!(shared.in_flight_len(), 0, "finish must deregister");

        // Cancelled → RequestCancelled (-32800).
        let shared = SessionShared::default();
        shared.lifecycle(&lsp_server::RequestId::from(8)).cancel();
        let err = finish_request(&shared, ok(8))
            .error
            .expect("a cancelled request must yield an error response");
        assert_eq!(
            err.code, REQUEST_CANCELLED,
            "a cancelled request must return RequestCancelled (-32800)"
        );
        assert!(err.message.contains("cancelled"));

        // Stale-by-edit → ContentModified (-32801).
        let shared = SessionShared::default();
        shared
            .lifecycle(&lsp_server::RequestId::from(9))
            .mark_stale();
        let err = finish_request(&shared, ok(9))
            .error
            .expect("a stale request must yield an error response");
        assert_eq!(
            err.code, ERR_CONTENT_MODIFIED,
            "a stale request must return ContentModified (-32801)"
        );

        // Both flags → cancelled wins (the client retracted; it discards the response anyway).
        let shared = SessionShared::default();
        let lifecycle = shared.lifecycle(&lsp_server::RequestId::from(10));
        lifecycle.mark_stale();
        lifecycle.cancel();
        let err = finish_request(&shared, ok(10))
            .error
            .expect("an interrupted request must yield an error response");
        assert_eq!(err.code, REQUEST_CANCELLED, "cancelled wins over stale");

        // Unregistered id → pass-through.
        let shared = SessionShared::default();
        let unregistered = finish_request(&shared, ok(11));
        assert!(unregistered.error.is_none());
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
            shared: Arc::new(SessionShared::default()),
            current_token: None,
            budget,
            memory_pressure: MemoryPressure::Normal,
            outbound: FxHashMap::default(),
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
