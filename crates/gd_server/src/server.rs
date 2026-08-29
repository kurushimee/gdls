//! The LSP server: lifecycle handshake, capability advertisement, and the synchronous event loop
//! that dispatches requests and notifications (`docs/05-lsp-cc-integration.md`).

use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use camino::{Utf8Path, Utf8PathBuf};
use crossbeam_channel::{select, Receiver, Sender};
use lsp_server::{Connection, Message, Notification, Request, Response};
use lsp_types::{
    CallHierarchyServerCapability, CodeDescription, ColorProviderCapability, DeclarationCapability,
    Diagnostic, DiagnosticSeverity, DiagnosticTag, DidChangeTextDocumentParams,
    DidCloseTextDocumentParams, DidOpenTextDocumentParams, DidSaveTextDocumentParams,
    DocumentLinkOptions, FoldingRangeProviderCapability, HoverProviderCapability,
    ImplementationProviderCapability, InitializeParams, InitializeResult, OneOf,
    PublishDiagnosticsParams, SelectionRangeProviderCapability, ServerCapabilities, ServerInfo,
    TextDocumentSyncCapability, TextDocumentSyncKind, TypeDefinitionProviderCapability, Uri,
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
pub(crate) const ERR_INVALID_PARAMS: i32 = -32602;
/// JSON-RPC `InvalidRequest` (-32600) — returned for requests received after `shutdown`,
/// per LSP 3.17 §shutdown.
const ERR_INVALID_REQUEST: i32 = -32600;
/// LSP 3.17 `ContentModified` (-32801). Used by the WP-H1 Hard-pressure gate as "the server is
/// intentionally not answering"; per the spec it signals the client to retry — exactly the
/// behavior we want once peak RSS drops back below Hard.
const ERR_CONTENT_MODIFIED: i32 = -32801;
/// LSP 3.17 `RequestFailed` (-32803): "A request failed but it was syntactically correct, e.g the
/// method name was known and the parameters were valid. The error message should contain human
/// readable information about why the request failed." Used by M9 #66 `rename`/`prepareRename` to
/// refuse a target that is not an editable project source (a native engine symbol or a generated
/// API stub) — the refusal is a typed error carrying a human message, never a silent null or a
/// corrupting edit.
pub(crate) const ERR_REQUEST_FAILED: i32 = -32803;

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
    /// order that gdls supports. Absent ⇒ PlainText (#261) — the captured editor profiles all
    /// declare markdown explicitly, so the default is reached only by a client that has told the
    /// server nothing; see [`crate::docs::ProseFormat`].
    pub(crate) hover_format: crate::docs::ProseFormat,
    /// The M8 (#64) completion gates, captured under `textDocument.completion`. Grouped in a
    /// sub-struct so the completion handler reads `state.caps.completion.<gate>` and the rest of
    /// [`ClientCaps`] stays a flat list of feature booleans.
    pub(crate) completion: CompletionCaps,
    /// The M8 (#65) signatureHelp gates, captured under `textDocument.signatureHelp`. Grouped in a
    /// sub-struct like [`CompletionCaps`] so the handler reads `state.caps.signature_help.<gate>`.
    pub(crate) signature_help: SignatureHelpCaps,
    /// `textDocument.foldingRange.rangeLimit` (M9 #70) — the maximum number of folding ranges the
    /// client prefers per document. A hint, not a hard cap (the spec lets the server choose how to
    /// honor it): `foldingRange` sorts its folds deterministically, then truncates to this many.
    /// `None` ⇒ no limit (return every fold).
    pub(crate) folding_range_limit: Option<u32>,
    /// `textDocument.foldingRange.lineFoldingOnly` (M9 #70) — when true the client ignores
    /// `startCharacter`/`endCharacter` and folds whole lines, so `foldingRange` omits the column
    /// fields entirely (whole-line ranges). Absent ⇒ `false` ⇒ ranges carry their columns.
    pub(crate) folding_line_folding_only: bool,
    /// `workspace.symbol.resolveSupport` (M9 #71) — when present the client will issue
    /// `workspaceSymbol/resolve` to fill lazily-deferred properties (the `location.range`), so
    /// `workspace/symbol` returns the 3.17 `WorkspaceSymbol[]` shape with a location-sans-range +
    /// a compact `data` blob (path + name span) and defers the precise range to resolve. Absent ⇒
    /// `false` ⇒ the byte-identical flat 3.16 `SymbolInformation[]` path with eager full ranges
    /// (every client accepts it). Read as presence-of-`resolveSupport` (matching how
    /// `completionItem.resolveSupport` is captured): the spec's `resolveSupport` is itself the
    /// opt-in, its `properties` list (usually `["location.range"]`) merely names what gdls already
    /// defers, so a non-`None` value is the gate.
    pub(crate) symbol_resolve_support: bool,
    /// `textDocument.rename.prepareSupport` (M9 #66) — when true the client issues
    /// `textDocument/prepareRename` ahead of a rename and can render a placeholder, so prepare
    /// returns the rich [`lsp_types::PrepareRenameResponse::RangeWithPlaceholder`] (identifier
    /// range + current name). Absent ⇒ `false` ⇒ prepare still answers (the keybinding must work)
    /// but with a bare [`lsp_types::PrepareRenameResponse::Range`] — a client that didn't opt into
    /// placeholder support gets only the range it knows how to consume.
    pub(crate) rename_prepare_support: bool,
    /// `workspace.workspaceEdit.documentChanges` (M9 #66) — when true a `rename`'s
    /// [`lsp_types::WorkspaceEdit`] uses the versioned `documentChanges`
    /// ([`lsp_types::DocumentChanges::Edits`]) shape, each [`lsp_types::TextDocumentEdit`] carrying
    /// the affected file's current open-buffer version (so the client rejects an edit it has since
    /// changed). Absent ⇒ `false` ⇒ the legacy `changes` URI→edits map (no version), which every
    /// client accepts. Exactly one of the two fields is populated; the other is `None`.
    pub(crate) workspace_edit_document_changes: bool,
    /// The M10 (#72) semanticTokens gates, captured under `textDocument.semanticTokens` +
    /// `workspace.semanticTokens`. Grouped in a sub-struct like [`CompletionCaps`] so the handlers
    /// read `state.caps.semantic_tokens.<gate>`.
    pub(crate) semantic_tokens: SemanticTokensCaps,
    /// The M10 (#73) inlayHint gates, captured under `textDocument.inlayHint` +
    /// `workspace.inlayHint`. Grouped in a sub-struct like [`SemanticTokensCaps`] so the handler
    /// reads `state.caps.inlay_hint.<gate>`.
    pub(crate) inlay_hint: InlayHintCaps,
    /// The M10 (#75) codeAction gates, captured under `textDocument.codeAction` +
    /// `textDocument.publishDiagnostics`. Grouped in a sub-struct like [`InlayHintCaps`] so the
    /// handlers read `state.caps.code_action.<gate>`.
    pub(crate) code_action: CodeActionCaps,
    /// `workspace.diagnostics.refreshSupport` (#255) — gates the server→client
    /// `workspace/diagnostic/refresh` request. gdls advertises `diagnosticProvider` with
    /// `interFileDependencies: true`, which promises that editing one file can change ANOTHER
    /// file's diagnostics; refresh is the protocol's only way to tell a pull client its cached
    /// results for those other files are stale. Absent ⇒ never sent (the client can't consume it)
    /// and the pull client re-requests on its own cadence — typically only for the document it is
    /// editing, which is exactly the staleness this closes for clients that DO advertise it.
    pub(crate) diagnostic_refresh_support: bool,
    /// The M11 (#79) workspace file-operation gates, captured under
    /// `workspace.fileOperations`. Grouped in a sub-struct like [`CodeActionCaps`] so the handler
    /// reads `state.caps.file_operations.<gate>` and the advertised capability mirrors them.
    pub(crate) file_operations: FileOperationsCaps,
}

/// The `workspace.fileOperations` client capabilities gdls branches on (M11 #79). Each flag both
/// (a) gates ADVERTISING the matching server capability — gdls advertises a file-operation provider
/// ONLY when the client opted into sending that operation (a client that won't send
/// `willRenameFiles` must not be told gdls handles it) — and (b) is therefore the precondition for
/// the matching handler ever running. Absent ⇒ all `false` ⇒ no file-operation capability is
/// advertised at all and the native watcher alone carries index freshness (generic-LSP-first, #30).
#[derive(Debug, Clone, Default)]
pub(crate) struct FileOperationsCaps {
    /// `workspace.fileOperations.willRename` — the client will send a `workspace/willRenameFiles`
    /// REQUEST before applying a rename and apply the returned [`lsp_types::WorkspaceEdit`]. Gates
    /// advertising `workspace.fileOperations.willRename`; the handler rewrites `res://` `preload`/
    /// `load` literals that positively resolve to a renamed file.
    pub(crate) will_rename: bool,
    /// `workspace.fileOperations.didRename` — the client will send `workspace/didRenameFiles`
    /// NOTIFICATIONS after a rename. An index nudge (the old path drops, the new path enters),
    /// deduped against the native watcher by the content-fingerprint gate.
    pub(crate) did_rename: bool,
    /// `workspace.fileOperations.didCreate` — the client will send `workspace/didCreateFiles`
    /// notifications. An index nudge (the new path enters), deduped against the native watcher.
    pub(crate) did_create: bool,
    /// `workspace.fileOperations.didDelete` — the client will send `workspace/didDeleteFiles`
    /// notifications. An index nudge (the path drops), deduped against the native watcher.
    pub(crate) did_delete: bool,
}

/// The `textDocument.codeAction` + `textDocument.publishDiagnostics` client capabilities the
/// codeAction pipeline branches on (M10 #75). Every field has a documented absent-default so a
/// minimal / Godot-unaware client still gets a working pipeline (degraded to the `Command` + eager-
/// edit path) — generic-LSP-first (#30).
#[derive(Debug, Clone, Default)]
pub(crate) struct CodeActionCaps {
    /// `textDocument.codeAction.codeActionLiteralSupport` (presence) — when the client advertises it,
    /// `textDocument/codeAction` returns `CodeAction` literals (kind-tagged, resolvable). Absent ⇒ the
    /// client only understands the legacy `Command[]` shape, so each action is returned as a
    /// [`lsp_types::Command`] routed through `workspace/executeCommand` (→ the `workspace/applyEdit`
    /// fallback). Read as presence-of-`codeActionLiteralSupport`, matching how the other literal/
    /// resolve gates are captured.
    pub(crate) literal_support: bool,
    /// `textDocument.codeAction.resolveSupport` (presence) — meaningful only when `literal_support`
    /// is also set. When present, a `CodeAction`'s `edit` is DEFERRED to a `codeAction/resolve`
    /// round-trip (the action ships with a `data` blob resolve turns into the edit); absent ⇒ the
    /// `edit` is computed EAGERLY in the `codeAction` response (a client that can't resolve still gets
    /// an applicable action). Read as presence (matching `inlayHint`/`completionItem`).
    pub(crate) resolve_support: bool,
    /// `textDocument.publishDiagnostics.dataSupport` — gates the additive `Diagnostic.data` tag
    /// attached at publish time (the per-warning fix payload a later phase consumes). When absent the
    /// client won't round-trip `data`, so the tag is omitted (and the codeAction path falls back to
    /// the diagnostic's `code` — which it does regardless, see
    /// [`crate::code_action`]). Byte-identical pre-tag diagnostics for a client without it.
    pub(crate) diagnostic_data_support: bool,
}

/// The `textDocument.semanticTokens` + `workspace.semanticTokens` client capabilities gdls projects
/// every semantic-token emission against (M10 #72). Every field has a documented absent-default so a
/// minimal/Godot-unaware client still gets standard-legend coloring — generic-LSP-first (#30).
#[derive(Debug, Clone, Default)]
pub(crate) struct SemanticTokensCaps {
    /// The client's advertised `tokenTypes`/`tokenModifiers` as an ALLOW-FILTER over gdls's own
    /// standard legend: gdls always emits its own (server-advertised) legend indices and modifier
    /// bit positions (LSP 3.17: the wire integers index the server legend), and DROPS any
    /// type/modifier the client didn't declare. An absent / empty client legend yields
    /// [`crate::semantic_tokens::ClientLegend::full`] (gdls's own legend; every standard name is
    /// universally understood — rust-analyzer does the same).
    pub(crate) legend: crate::semantic_tokens::ClientLegend,
    /// `workspace.semanticTokens.refreshSupport` — gates the server→client
    /// `workspace/semanticTokens/refresh` request. When absent the refresh is never sent (the
    /// client can't consume it); the client re-requests tokens on its own edit cadence instead.
    pub(crate) refresh_support: bool,
}

/// The `textDocument.inlayHint` + `workspace.inlayHint` client capabilities the inlay-hint handler
/// branches on (M10 #73). Every field has a documented absent-default so a minimal client still
/// gets complete, eager hints — generic-LSP-first (#30).
#[derive(Debug, Clone, Default)]
pub(crate) struct InlayHintCaps {
    /// `textDocument.inlayHint.resolveSupport` (presence) — when the client advertises it, the
    /// tooltip is DEFERRED to an `inlayHint/resolve` round-trip (the hint ships without a `tooltip`,
    /// carrying a `data` blob resolve uses to fill it). Absent ⇒ the tooltip is embedded EAGERLY in
    /// the initial hint (a client that can't resolve still gets the full hint). Read as
    /// presence-of-`resolveSupport`, matching `workspaceSymbol`/`completionItem` — the `properties`
    /// list merely names what gdls already defers, so a non-`None` value is the gate. The textEdit
    /// is ALWAYS eager (never deferred), so an apply works without a resolve round-trip.
    pub(crate) resolve_support: bool,
    /// `workspace.inlayHint.refreshSupport` — gates the server→client `workspace/inlayHint/refresh`
    /// request emitted when the inlay-hint config toggles. When absent the refresh is never sent;
    /// the client re-requests hints on its own cadence (typically on the next edit / scroll).
    pub(crate) refresh_support: bool,
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
    /// [`crate::docs::ProseFormat`] negotiation `hover.contentFormat` uses. Absent ⇒ PlainText
    /// (the conservative floor, see [`crate::docs::ProseFormat`]).
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
    /// `completionItem.tagSupport.valueSet` contains `Deprecated` (#258). When true, a member
    /// whose `##` doc carries `@deprecated` ships `tags: [1]`; when false — the absent-capability
    /// default — the same member ships the pre-3.15 `deprecated: true` boolean instead, so a
    /// minimal client still strikes it through. Never both: `tags` supersedes the deprecated
    /// boolean in LSP 3.15+, and sending both to a tag-aware client is redundant.
    pub(crate) tag_support_deprecated: bool,
}

/// The `textDocument.signatureHelp` client capabilities gdls projects each signature against
/// (M8 #65). Every field has a documented absent-default so a Godot-unaware / minimal client still
/// gets a well-formed (if downgraded) `SignatureHelp` — generic-LSP-first (#30).
#[derive(Debug, Clone, Default)]
pub(crate) struct SignatureHelpCaps {
    /// `signatureInformation.documentationFormat` — the first kind gdls supports, reusing the
    /// [`crate::docs::ProseFormat`] negotiation `hover.contentFormat` uses. Absent ⇒ PlainText (the
    /// same conservative downgrade [`CompletionCaps::documentation_format`] and `hover_format`
    /// take: a client that didn't enumerate formats can always render plaintext, and attaching
    /// un-asked-for markdown could surface raw `**`).
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
    /// `raw` is the client's `capabilities` object exactly as it arrived. Everything reads through
    /// the typed `caps` except [`Self::diagnostic_refresh_support`], whose standard key lsp-types
    /// 0.97 misspells — see there.
    fn negotiate(caps: &lsp_types::ClientCapabilities, raw: &serde_json::Value) -> Self {
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
            // M9 (#70): foldingRange projection hints. Same optional-path `.and_then`/`.unwrap_or`
            // convention as the flags above — an absent `foldingRange` capability yields no limit
            // and full (columned) ranges.
            folding_range_limit: td
                .and_then(|t| t.folding_range.as_ref())
                .and_then(|f| f.range_limit),
            folding_line_folding_only: td
                .and_then(|t| t.folding_range.as_ref())
                .and_then(|f| f.line_folding_only)
                .unwrap_or(false),
            // M9 (#71): the presence of `workspace.symbol.resolveSupport` is the opt-in for the
            // 3.17 partial `WorkspaceSymbol[]` shape — same `is_some()` reading
            // `completionItem.resolveSupport` gets (the `properties` list names what is deferred,
            // not whether resolve is supported).
            symbol_resolve_support: caps
                .workspace
                .as_ref()
                .and_then(|w| w.symbol.as_ref())
                .and_then(|s| s.resolve_support.as_ref())
                .is_some(),
            // M9 (#66): rename gates. `prepareSupport` rides `textDocument.rename` (same
            // optional-path convention as the flags above); `documentChanges` rides
            // `workspace.workspaceEdit`. Both absent-default to `false` — prepare downgrades to a
            // bare range and the edit downgrades to the legacy `changes` map, each of which every
            // client accepts.
            rename_prepare_support: td
                .and_then(|t| t.rename.as_ref())
                .and_then(|r| r.prepare_support)
                .unwrap_or(false),
            workspace_edit_document_changes: caps
                .workspace
                .as_ref()
                .and_then(|w| w.workspace_edit.as_ref())
                .and_then(|w| w.document_changes)
                .unwrap_or(false),
            // #255/#277: `workspace.diagnostics.refreshSupport` — PLURAL, per LSP 3.17. lsp-types
            // 0.97 names the field `workspace.diagnostic` (singular), which under its
            // `rename_all = "camelCase"` deserializes the wrong wire key, so the typed path is
            // blind to what VS Code, Neovim, Zed and Sublime actually send (all four of the
            // captured client profiles use the plural). Read the spec key off the raw object and
            // keep the typed field as a fallback for any client that follows lsp-types instead.
            diagnostic_refresh_support: raw
                .get("workspace")
                .and_then(|w| w.get("diagnostics"))
                .and_then(|d| d.get("refreshSupport"))
                .and_then(serde_json::Value::as_bool)
                .unwrap_or_else(|| {
                    caps.workspace
                        .as_ref()
                        .and_then(|w| w.diagnostic.as_ref())
                        .and_then(|d| d.refresh_support)
                        .unwrap_or(false)
                }),
            semantic_tokens: SemanticTokensCaps::negotiate(caps),
            inlay_hint: InlayHintCaps::negotiate(caps),
            code_action: CodeActionCaps::negotiate(caps),
            file_operations: FileOperationsCaps::negotiate(caps),
        }
    }
}

impl FileOperationsCaps {
    /// Walk `workspace.fileOperations`, mirroring the optional-path convention the rest of
    /// [`ClientCaps::negotiate`] uses. Each `.{will,did}_<op>` flag is a per-operation opt-in:
    /// absent ⇒ `false` ⇒ that file-operation capability is neither advertised nor handled.
    fn negotiate(caps: &lsp_types::ClientCapabilities) -> Self {
        let fo = caps
            .workspace
            .as_ref()
            .and_then(|w| w.file_operations.as_ref());
        FileOperationsCaps {
            will_rename: fo.and_then(|f| f.will_rename).unwrap_or(false),
            did_rename: fo.and_then(|f| f.did_rename).unwrap_or(false),
            did_create: fo.and_then(|f| f.did_create).unwrap_or(false),
            did_delete: fo.and_then(|f| f.did_delete).unwrap_or(false),
        }
    }
}

impl CodeActionCaps {
    /// Walk `textDocument.codeAction` (literal + resolve support) + `textDocument.publishDiagnostics`
    /// (data support), mirroring the optional-path convention the rest of [`ClientCaps::negotiate`]
    /// uses. An absent `codeAction` capability yields no literal support (the `Command[]` fallback),
    /// no resolve (eager edits), and no diagnostic-data tag — a client that didn't opt in still gets a
    /// working pipeline.
    fn negotiate(caps: &lsp_types::ClientCapabilities) -> Self {
        let td = caps.text_document.as_ref();
        let code_action = td.and_then(|t| t.code_action.as_ref());
        let literal_support = code_action
            .and_then(|c| c.code_action_literal_support.as_ref())
            .is_some();
        let resolve_support = code_action
            .and_then(|c| c.resolve_support.as_ref())
            .is_some();
        let diagnostic_data_support = td
            .and_then(|t| t.publish_diagnostics.as_ref())
            .and_then(|p| p.data_support)
            .unwrap_or(false);
        CodeActionCaps {
            literal_support,
            resolve_support,
            diagnostic_data_support,
        }
    }
}

impl InlayHintCaps {
    /// Walk `textDocument.inlayHint` (resolve support) + `workspace.inlayHint` (refresh support),
    /// mirroring the optional-path convention the rest of [`ClientCaps::negotiate`] uses. An absent
    /// `inlayHint` capability yields no resolve (tooltips ship eagerly) and no refresh — a client
    /// that didn't opt in still gets complete hints, just no server-pushed refresh.
    fn negotiate(caps: &lsp_types::ClientCapabilities) -> Self {
        let resolve_support = caps
            .text_document
            .as_ref()
            .and_then(|t| t.inlay_hint.as_ref())
            .and_then(|h| h.resolve_support.as_ref())
            .is_some();
        let refresh_support = caps
            .workspace
            .as_ref()
            .and_then(|w| w.inlay_hint.as_ref())
            .and_then(|h| h.refresh_support)
            .unwrap_or(false);
        InlayHintCaps {
            resolve_support,
            refresh_support,
        }
    }
}

impl SemanticTokensCaps {
    /// Walk `textDocument.semanticTokens` (the advertised legend) + `workspace.semanticTokens`
    /// (refresh support), mirroring the optional-path convention the rest of [`ClientCaps::negotiate`]
    /// uses. An absent `semanticTokens` capability yields the permissive default legend
    /// ([`crate::semantic_tokens::ClientLegend::full`]) and no refresh — a client that didn't opt in
    /// still gets standard-legend coloring, just no server-pushed refresh.
    fn negotiate(caps: &lsp_types::ClientCapabilities) -> Self {
        let legend = caps
            .text_document
            .as_ref()
            .and_then(|t| t.semantic_tokens.as_ref())
            .map(|s| {
                crate::semantic_tokens::ClientLegend::from_client(
                    &s.token_types,
                    &s.token_modifiers,
                )
            })
            .unwrap_or_default();
        let refresh_support = caps
            .workspace
            .as_ref()
            .and_then(|w| w.semantic_tokens.as_ref())
            .and_then(|s| s.refresh_support)
            .unwrap_or(false);
        SemanticTokensCaps {
            legend,
            refresh_support,
        }
    }
}

/// The first [`crate::docs::ProseFormat`] gdls supports in a client's preference-ordered
/// `MarkupKind` list (Markdown preferred, PlainText accepted, anything else skipped). Shared by
/// `hover.contentFormat` (M7 #62) and `completionItem.documentationFormat` (M8 #64) so both honor
/// the same negotiation; an empty / all-unknown list falls back to
/// [`crate::docs::ProseFormat::PlainText`] — the same floor an absent capability takes (#261),
/// since a client that enumerated only kinds gdls doesn't know has still said nothing about
/// markdown.
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
            tag_support_deprecated: item.and_then(|i| i.tag_support.as_ref()).is_some_and(|t| {
                t.value_set
                    .contains(&lsp_types::CompletionItemTag::DEPRECATED)
            }),
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
    /// M10 (#72): a `workspace/semanticTokens/refresh` request — the response is acknowledgment-only
    /// (the client re-requests tokens for its visible documents). Sent after an index-wide change
    /// (native DB reload / warm-start adoption) where every document's tokens may have shifted.
    SemanticTokensRefresh,
    /// M10 (#73): a `workspace/inlayHint/refresh` request — the response is acknowledgment-only (the
    /// client re-requests hints for its visible documents). Sent when the inlay-hint config toggles
    /// (`workspace/didChangeConfiguration`) so already-shown hints reflect the new policy live.
    InlayHintRefresh,
    /// #255: a `workspace/diagnostic/refresh` request — the response is acknowledgment-only (the
    /// client re-pulls `textDocument/diagnostic` for its open documents). Sent when a reindex
    /// invalidated a file OTHER than the one the client just edited, which is precisely when a
    /// pull client's cache goes stale without it noticing.
    WorkspaceDiagnosticRefresh,
    /// M10 (#75): a `workspace/applyEdit` request — gdls's FIRST server→client request that expects a
    /// meaningful RESPONSE ([`lsp_types::ApplyWorkspaceEditResponse`] `{ applied }`). Sent by the
    /// `gdls.applyWarningIgnore` command (the `codeActionLiteralSupport` fallback path) FIRE-AND-FORGET
    /// — the worker must not block on the reply (it is the sole consumer of the response channel).
    /// The reply is correlated HERE: `applied: true` ⇒ debug log, `applied: false` / an error ⇒ warn
    /// log; neither crashes, neither bounces (anti-catalog W3). gdls owns no buffer, so a rejected
    /// edit needs no rollback.
    ApplyEdit,
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
    /// The id of the currently-dispatching request, alongside [`Self::current_token`]. Read by the
    /// off-worker formatter bridge (M11 #135/#136) to tag the [`crate::formatter::FormatDone`] it
    /// sends back from the format thread. `Some` only while inside [`dispatch_request`]; cleared to
    /// `None` on handler return (same lifetime as `current_token`).
    pub(crate) current_request_id: Option<RequestId>,
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
    /// M10 (#72): per-URI semantic-tokens result cache, for `semanticTokens/full/delta`. Maps a
    /// document URI to the last result id gdls minted for it plus the token array it returned. A
    /// `full/delta` request whose `previous_result_id` matches the stored id diffs against the stored
    /// array; an unknown id (the client's record and ours diverged) falls back to a fresh `full`.
    /// Per-session, in-memory only (rebuilt on the next `full`); never persisted.
    pub(crate) semantic_tokens_cache: FxHashMap<Uri, SemanticTokensCacheEntry>,
    /// M10 (#72): monotonic counter minting `semanticTokens` result ids. A new `full` (or a delta
    /// fall-back) stamps `"st-{n}"`; the id is opaque to the client and only used to correlate the
    /// next delta request.
    pub(crate) semantic_tokens_result_seq: u64,
    /// M11 (#80): the set of external-formatter failure classes already surfaced via
    /// `window/showMessage(Warning)` THIS SESSION. `textDocument/formatting` runs on every save; a
    /// persistently-misconfigured formatter would otherwise spam a warning per save. The handler
    /// warns at most once per distinct [`crate::formatter::FormatterFailure`] (spawn / non-zero exit
    /// / timeout / non-UTF-8 output) — the first occurrence shows the message and records the class
    /// here; later occurrences of the same class stay silent (the full detail still goes to stderr).
    pub(crate) formatter_warned: FxHashSet<crate::formatter::FormatterFailure>,
    /// M11 (#135/#136): the off-worker external-formatter bridge. `textDocument/formatting` runs the
    /// subprocess on a dedicated thread (not the request worker) so a slow/blocked format can't stall
    /// unrelated requests (#135 head-of-line blocking); the worker applies each result back on the
    /// event-loop thread via the [`crate::formatter::FormatBridge::done_rx`] `select!` arm. A
    /// `$/cancelRequest` for an in-flight format kills its subprocess promptly (#136). Holds the
    /// long-lived result channel + the per-document supersession map.
    pub(crate) format_bridge: crate::formatter::FormatBridge,
}

/// M10 (#72): one entry of [`ServerState::semantic_tokens_cache`] — the last result id + token array
/// gdls returned for a document, so the next `full/delta` can diff against it.
pub(crate) struct SemanticTokensCacheEntry {
    pub(crate) result_id: String,
    pub(crate) tokens: Vec<lsp_types::SemanticToken>,
}

/// How a session ended — the input to the process exit code LSP 3.17 §exit specifies: "The
/// server should exit with `success` code 0 if the shutdown request has been received before;
/// otherwise with `error` code 1." Only the stdio binary path acts on this; an in-memory
/// `serve` in tests just gets it back as a value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SessionEnd {
    /// `exit` arrived after a `shutdown` request — a clean handshake.
    CleanExit,
    /// `exit` arrived with no prior `shutdown` — the spec's error case.
    ExitWithoutShutdown,
    /// The transport closed (stdin EOF, router hang-up) without an `exit` notification. Not a
    /// protocol violation — the client is simply gone — so this exits 0 like a clean shutdown.
    TransportClosed,
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
    let end = serve_inner(connection, recorder, WatcherSource::Real)?;
    io_threads.join()?;
    log::info!("gdls stopped");
    // LSP 3.17 §exit: `exit` without a prior `shutdown` is an error the client can detect only
    // through the process status. Exit AFTER the join + log above so stderr is flushed first.
    if end == SessionEnd::ExitWithoutShutdown {
        log::warn!("exit received without a prior shutdown request; exiting with status 1");
        std::process::exit(1);
    }
    Ok(())
}

/// Deserialize the `initialize` params, recovering field by field rather than failing the
/// handshake.
///
/// A whole-struct `from_value` is all-or-nothing, and `InitializeParams` is large: one field a
/// client sends in a shape lsp-types 0.97 does not model — a `rootUri` it cannot parse, a
/// capability value of the wrong JSON type — used to abort `serve_inner` before it could answer,
/// so the client saw the server vanish with no response and no error (#280). That contradicts both
/// "never crash, never lie" and the generic-LSP contract: a server must not require params in
/// exactly the shape one Rust crate models.
///
/// Only four fields are read downstream, so recovery is cheap and lossless: `capabilities` (also
/// read raw, per #277), `initializationOptions`, `workspaceFolders` and the deprecated `rootUri`.
/// Each is parsed independently and defaults on failure, which lets `resolve_root` fall through
/// its existing `projectRoot` → workspace-folder → cwd ladder.
fn parse_initialize_params(raw: &serde_json::Value) -> InitializeParams {
    match serde_json::from_value::<InitializeParams>(raw.clone()) {
        Ok(init) => init,
        Err(err) => {
            log::warn!(
                "initialize params did not deserialize ({err}); recovering the fields gdls \
                 actually reads and continuing — the handshake still completes"
            );
            let field = |name: &str| raw.get(name).cloned().unwrap_or(serde_json::Value::Null);
            let mut init = InitializeParams::default();
            if let Ok(caps) = serde_json::from_value(field("capabilities")) {
                init.capabilities = caps;
            }
            init.initialization_options = raw.get("initializationOptions").cloned();
            init.workspace_folders = serde_json::from_value(field("workspaceFolders")).ok();
            #[allow(deprecated)]
            {
                init.root_uri = serde_json::from_value(field("rootUri")).ok();
            }
            init
        }
    }
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
    serve_inner(connection, recorder, WatcherSource::Real)?;
    Ok(())
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
    serve_inner(connection, None, WatcherSource::Injected(watcher_rx))?;
    Ok(())
}

fn serve_inner(
    connection: Connection,
    recorder: Option<BenchRecorder>,
    watcher_source: WatcherSource,
) -> Result<SessionEnd> {
    // --- Lifecycle: split handshake so we can read the client's offered encodings first. ---
    let (init_id, init_value) = connection.initialize_start()?;
    // Keep the RAW capabilities object alongside the typed one: lsp-types 0.97 misspells one
    // standard key, and that key gates a real feature — see `ClientCaps::negotiate`.
    let raw_caps = init_value.get("capabilities").cloned().unwrap_or_default();
    let init = parse_initialize_params(&init_value);

    let encoding = PositionEncoding::negotiate(&init.capabilities);
    let caps = ClientCaps::negotiate(&init.capabilities, &raw_caps);
    let options = InitializationOptions::parse(init.initialization_options.as_ref());
    let root = resolve_root(&options, &init);

    let result = InitializeResult {
        capabilities: capabilities(encoding, &caps, &options.formatter),
        server_info: Some(ServerInfo {
            name: "gdls".to_string(),
            version: Some(env!("CARGO_PKG_VERSION").to_string()),
        }),
    };
    // M9 (#69): advertise `typeHierarchyProvider`. lsp-types 0.97.0 models the type-hierarchy
    // request/param/item types AND the *client* capability, but omits the *server* capability
    // field from `ServerCapabilities` (no `type_hierarchy_provider`, no extras/flatten escape
    // hatch but `experimental`). `typeHierarchyProvider` is a standard LSP 3.17 key a spec
    // client reads to enable the feature — putting it under `experimental` would gate it off for
    // every real editor (and lie about W15), so we inject the *real* key into the already-
    // serialized capabilities object instead of bumping the pinned dependency. Boolean form is
    // the simplest of the `boolean | options | registration-options` shapes the spec allows.
    let mut result_value = serde_json::to_value(result)?;
    if let Some(caps) = result_value
        .get_mut("capabilities")
        .and_then(serde_json::Value::as_object_mut)
    {
        caps.insert(
            "typeHierarchyProvider".to_string(),
            serde_json::Value::Bool(true),
        );
    }
    connection.initialize_finish(init_id, result_value)?;
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
        current_request_id: None,
        budget,
        memory_pressure: MemoryPressure::Normal,
        outbound: FxHashMap::default(),
        stub_cache: crate::stubs::StubCache::default(),
        semantic_tokens_cache: FxHashMap::default(),
        semantic_tokens_result_seq: 0,
        formatter_warned: FxHashSet::default(),
        format_bridge: crate::formatter::FormatBridge::default(),
    };

    // M7 (#60): the one dynamic registration, sent once the session state exists (the
    // `initialized` notification itself was already consumed by `initialize_finish` during the
    // handshake — there is no later hook). No-op without
    // `workspace.didChangeWatchedFiles.dynamicRegistration`.
    // #264: the `**/*` catch-all is a FALLBACK, not a default — pass whether gdls armed its own
    // OS watcher. Same predicate as `reconcile_mode` just below, for the same reason: without a
    // live native watcher, freshness is already degraded and the client is the only channel left.
    register_watched_files(&mut state, watcher.is_some());

    // #259: say once, on the wire, which native surface this session actually got. The embedded
    // fallback is a complete STOCK 4.6.3 surface (documentation included), but it is not the
    // user's engine: a different Godot version, and every GDExtension class, is simply absent —
    // which is why it carries `Generic` provenance and why the analyzer will not turn its misses
    // into errors. A stderr line is invisible to most clients; "never lie" covers a degraded
    // surface the user cannot see. One notification per session, at startup, never repeated.
    notify_dialect(&state);

    if state.workspace.native.provenance() == gd_types::ApiProvenance::Generic {
        show_message(
            &state,
            lsp_types::MessageType::INFO,
            "gdls is using its built-in stock Godot 4.6.3 API surface — no project dump was \
             found. Engine classes and their documentation are available, but classes from your \
             own Godot build or from GDExtensions are not. To use your engine's real API, set \
             `godotBinaryPath` (or the GDLS_GODOT environment variable), or point \
             `extensionApiPath` at an extension_api.json.",
        );
    }

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
    // M11 (#135/#136): a clone of the off-worker formatter result channel. The `select!` arm below
    // can't name `state.format_bridge.done_rx` directly (the arm body also takes `&mut state`), so
    // bind a long-lived clone here. The original receiver stays owned by `state.format_bridge`; both
    // ends of the channel persist for the whole session.
    let format_bridge_rx = state.format_bridge.done_rx.clone();
    // WP-RD11 (4): liveness ticks elapsed since the watcher arm was disabled. Once the watcher is
    // down (MaxFilesWatch / root-loss), the index would otherwise freeze until restart; this counts
    // 3-second ticks so a low-frequency reconcile fallback can re-sync on-disk drift.
    let mut disabled_reconcile_ticks: u32 = 0;
    // M7 (#57): set by the `shutdown` request; requests received after it answer InvalidRequest
    // (-32600) per LSP 3.17 until the `exit` notification breaks the loop.
    let mut shutting_down = false;
    // #262: which of LSP 3.17 §exit's two cases the session ends in. Set at each `break` below and
    // returned so the stdio entry point can pick the process status.
    let mut session_end = SessionEnd::TransportClosed;

    loop {
        let watcher_arm = watcher_rx.as_ref().unwrap_or(&dummy);
        let dump_arm = dump_rx.as_ref().unwrap_or(&dump_dummy);
        select! {
            // M11 (#135/#136): an off-worker format finished. Apply it on the event-loop thread:
            // warn-once on a real failure, drop the supersession entry, then route the response
            // through `finish_request` (so a cancel/edit the router tripped while the format ran
            // overrides it with RequestCancelled/ContentModified — the mutating-consumer firewall:
            // no late edit after cancel/stale) and send it. Stays active during `shutting_down`: a
            // format that predated `shutdown` must still be answered + deregistered.
            recv(format_bridge_rx) -> done => match done {
                Ok(done) => {
                    let (id, value) = crate::formatter::apply_format_done(&mut state, done);
                    let resp = finish_request(&state.shared, Response::new_ok(id, value));
                    if let Err(e) = state.sender.send(Message::Response(resp)) {
                        log::warn!(
                            "format response send failed (client likely disconnected): {e}; \
                             loop will exit on next receiver tick"
                        );
                    }
                }
                // Both ends live in `state.format_bridge` for the whole session, so the channel
                // never disconnects while the loop runs; a recv error is unreachable but handled.
                Err(_) => log::warn!("format bridge channel closed unexpectedly"),
            },
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
                            // M10 (#72): the native DB shifted (a class may have become native /
                            // gained members) — ask the client to re-request semantic tokens.
                            send_semantic_tokens_refresh(&mut state);
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
                    // `None` ⇒ the handler answers asynchronously (the off-worker formatter — M11
                    // #135/#136 — sends its response later via the `recv(format_bridge_rx)` arm
                    // below); send nothing now.
                    let resp = if req.method == "shutdown" {
                        shutting_down = true;
                        // M11 (#178): cancel every in-flight off-worker format so its poll-kill
                        // (CANCEL_POLL_INTERVAL) reaps the subprocess during the shutdown→exit
                        // round-trip, before the loop breaks on `exit`. Otherwise a format still
                        // running at `exit` gets no response and its child reparents to init (a
                        // bounded, transient orphan). The done channel is NOT drained here — that
                        // would block shutdown on a slow format (head-of-line-at-shutdown, the
                        // class #135 fixes); a format that finishes within the round-trip is still
                        // answered by its done arm, which stays active during `shutting_down`.
                        state.format_bridge.cancel_all_in_flight();
                        Some(Response::new_ok(req.id, serde_json::Value::Null))
                    } else if shutting_down {
                        // Deregister the lifecycle the router opened for it, then refuse.
                        let _ = state.shared.finish(&req.id);
                        Some(Response::new_err(
                            req.id,
                            ERR_INVALID_REQUEST,
                            "request received after shutdown".to_string(),
                        ))
                    } else {
                        dispatch_request(&mut state, req)
                    };
                    if let Some(resp) = resp {
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
                }
                Ok(Message::Notification(note)) => {
                    if let Some(rec) = state.recorder.as_mut() {
                        rec.record_notification(&note);
                    }
                    if note.method == "exit" {
                        session_end = if shutting_down {
                            SessionEnd::CleanExit
                        } else {
                            SessionEnd::ExitWithoutShutdown
                        };
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
    Ok(session_end)
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
        OutboundKind::SemanticTokensRefresh => match &resp.error {
            Some(err) => log::debug!(
                "client declined workspace/semanticTokens/refresh ({}); it will re-request on its \
                 own cadence",
                err.message
            ),
            None => log::debug!("client acknowledged the semanticTokens refresh"),
        },
        OutboundKind::InlayHintRefresh => match &resp.error {
            Some(err) => log::debug!(
                "client declined workspace/inlayHint/refresh ({}); it will re-request on its own \
                 cadence",
                err.message
            ),
            None => log::debug!("client acknowledged the inlayHint refresh"),
        },
        OutboundKind::WorkspaceDiagnosticRefresh => match &resp.error {
            Some(err) => log::debug!(
                "client declined workspace/diagnostic/refresh ({}); its pull results for                  dependents stay stale until it re-pulls on its own cadence",
                err.message
            ),
            None => log::debug!("client acknowledged the workspace diagnostic refresh"),
        },
        // M10 (#75): the `workspace/applyEdit` reply for a `gdls.applyWarningIgnore` command. Correlate
        // it (the W3 requirement: a server→client request MUST be correlated, never bounced) — accept
        // and reject both end here, neither crashes the session. An error reply (the client refused
        // the request outright) or `applied: false` (the client declined to apply) are both warn-logged;
        // gdls owns no buffer, so there is nothing to roll back.
        OutboundKind::ApplyEdit => {
            if let Some(err) = &resp.error {
                log::warn!(
                    "client errored on workspace/applyEdit ({}); the @warning_ignore edit was not \
                     applied",
                    err.message
                );
                return;
            }
            // Parse the `{ applied }` payload; a malformed/absent result is treated as not-applied
            // (defensive — every conformant client sends it).
            let applied = resp
                .result
                .as_ref()
                .and_then(|r| {
                    serde_json::from_value::<lsp_types::ApplyWorkspaceEditResponse>(r.clone()).ok()
                })
                .map(|r| r.applied)
                .unwrap_or(false);
            if applied {
                log::debug!("client applied the @warning_ignore workspace edit");
            } else {
                log::warn!(
                    "client declined the @warning_ignore workspace edit (applied: false); no \
                     suppression was inserted"
                );
            }
        }
    }
}

/// M10 (#72): ask the client to re-request semantic tokens for its visible documents — sent after an
/// index-wide change (native DB reload / warm-start adoption) that may have shifted every document's
/// classification (e.g. a class that became native, a newly-resolved member). Gated on the client's
/// `workspace.semanticTokens.refreshSupport`; a no-op (never sent) otherwise. Fire-and-forget: the
/// response is acknowledgment-only (correlated to [`OutboundKind::SemanticTokensRefresh`]).
fn send_semantic_tokens_refresh(state: &mut ServerState) {
    if !state.caps.semantic_tokens.refresh_support {
        return;
    }
    let id = state.shared.next_outgoing_id();
    state
        .outbound
        .insert(id.clone(), OutboundKind::SemanticTokensRefresh);
    let req = Request {
        id,
        method: "workspace/semanticTokens/refresh".to_string(),
        params: serde_json::Value::Null,
    };
    if state.sender.send(Message::Request(req)).is_err() {
        log::warn!("workspace/semanticTokens/refresh send failed (client disconnected?)");
    }
}

/// M10 (#73): ask the client to re-request inlay hints for its visible documents — sent when the
/// inlay-hint config toggles, so already-shown hints reflect the new policy without an edit. Gated on
/// the client's `workspace.inlayHint.refreshSupport`; a no-op (never sent) otherwise. Fire-and-forget:
/// the response is acknowledgment-only (correlated to [`OutboundKind::InlayHintRefresh`]).
fn send_inlay_hint_refresh(state: &mut ServerState) {
    if !state.caps.inlay_hint.refresh_support {
        return;
    }
    let id = state.shared.next_outgoing_id();
    state
        .outbound
        .insert(id.clone(), OutboundKind::InlayHintRefresh);
    let req = Request {
        id,
        method: "workspace/inlayHint/refresh".to_string(),
        params: serde_json::Value::Null,
    };
    if state.sender.send(Message::Request(req)).is_err() {
        log::warn!("workspace/inlayHint/refresh send failed (client disconnected?)");
    }
}

/// #255: ask the client to re-pull `textDocument/diagnostic` for its open documents — sent when a
/// reindex invalidated files BEYOND the one the client just edited (a dependency's interface
/// changed, a `class_name` appeared, a project-wide policy or native-DB reload). The push path
/// republishes open buffers directly; a pull client has no equivalent signal, and gdls advertises
/// `diagnosticProvider.interFileDependencies: true` — this is the request that promise implies.
///
/// Gated on the client's `workspace.diagnostics.refreshSupport`; a no-op (never sent) otherwise.
/// Fire-and-forget: the response is acknowledgment-only (correlated to
/// [`OutboundKind::WorkspaceDiagnosticRefresh`]). Callers send it at most ONCE per reindex batch —
/// a project-wide change fans one refresh out, never one per dependent.
fn send_workspace_diagnostic_refresh(state: &mut ServerState) {
    if !state.caps.diagnostic_refresh_support {
        return;
    }
    let id = state.shared.next_outgoing_id();
    state
        .outbound
        .insert(id.clone(), OutboundKind::WorkspaceDiagnosticRefresh);
    let req = Request {
        id,
        method: "workspace/diagnostic/refresh".to_string(),
        params: serde_json::Value::Null,
    };
    if state.sender.send(Message::Request(req)).is_err() {
        log::warn!("workspace/diagnostic/refresh send failed (client disconnected?)");
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
    let structural: [(&str, bool); 7] = [
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
        // M11 (#80): the formatter is session-structural — it gates whether
        // `documentFormattingProvider` was advertised at the handshake, and a capability can't be
        // added/removed mid-session. A drifted value is warned about and the startup value retained.
        (
            "formatter",
            provided("formatter") && new_options.formatter != old.formatter,
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
    // M10 (#73): inlay-hint toggles are runtime-reloadable. A genuine change re-stores them and asks
    // the client to re-request hints (refresh) so already-displayed hints reflect the new policy.
    let inlay_changed = provided("inlayHint") && new_options.inlay_hint != state.options.inlay_hint;

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
    // Inlay hints aren't carried on `publishDiagnostics` — instead of a republish, ask the client to
    // re-request hints. Done last so it's independent of the diagnostics republish above.
    if inlay_changed {
        log::info!(
            "runtime configuration: inlay-hint toggles changed; requesting an inlayHint refresh"
        );
        state.options.inlay_hint = new_options.inlay_hint;
        send_inlay_hint_refresh(state);
    }
}

/// Send a `window/showMessage` notification — the operator-facing channel for conditions that
/// deserve more than a stderr log line (M7 §5 showMessage conventions: used sparingly, never as
/// log spam).
pub(crate) fn show_message(state: &ServerState, kind: lsp_types::MessageType, message: &str) {
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
        if state.workspace.reload_project_and_native(&state.options) {
            rebuild_workspace_for_dialect_change(state);
        }
        republish_all_open_buffers(state);
        // M10 (#72): project/native surface changed → re-request semantic tokens for visible docs.
        send_semantic_tokens_refresh(state);
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
            // M10 (#72): native DB changed → re-request semantic tokens for visible docs.
            send_semantic_tokens_refresh(state);
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
pub(crate) fn handle_client_file_events(
    state: &mut ServerState,
    changes: Vec<lsp_types::FileEvent>,
) {
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
///
/// `native_watcher_armed` decides whether the `**/*` asset catch-all is included — see the
/// comment at its entry. Everything else is registered either way: the engine-managed files are
/// few, and a duplicate delivery costs one content-fingerprint comparison.
fn register_watched_files(state: &mut ServerState, native_watcher_armed: bool) {
    if !state.caps.dynamic_watched_files {
        return;
    }
    let watcher = |glob: &str| serde_json::json!({ "globPattern": glob });
    // #226 / #264: the arbitrary-asset catch-all. Arbitrary assets are defined by EXCLUSION
    // (everything that is not a script / scene / engine-managed file), so no positive extension
    // allowlist can express the set — an extension-less `res://LICENSE` is a listable asset too,
    // and `**/*` is the only glob matching `AssetIndex::build`'s own definition. It exists for the
    // client whose ONLY freshness channel is `didChangeWatchedFiles` (the Helix scenario): without
    // it, a newly-created `icon.png` never reaches the asset index and `load`/`preload` completion
    // goes stale until a restart.
    //
    // But it asks the client to watch the entire workspace — `.git/`, `.import/`, `build/`,
    // exported binaries, every asset — which on a large project is a great many inotify handles
    // and a steady stream of notifications gdls then discards, and some clients cap or warn on
    // watcher breadth. So it is registered only when it BUYS something: when gdls armed its own
    // OS watcher, that watcher already reports asset create/delete, and the catch-all is pure
    // client-side cost for a channel the server does not need. `classify_client_event` re-applies
    // the same `is_excluded` server-side filter either way, so the two paths converge to identical
    // semantics — the difference is only who pays for the delivery.
    let asset_catch_all = (!native_watcher_armed).then(|| watcher("**/*"));
    if native_watcher_armed {
        log::debug!(
            "watch registration: native watcher is armed, omitting the `**/*` asset catch-all              (asset freshness rides the native watcher)"
        );
    }
    let id = state.shared.next_outgoing_id();
    state
        .outbound
        .insert(id.clone(), OutboundKind::RegisterWatchedFiles);
    let watchers: Vec<serde_json::Value> = [
        watcher("**/*.gd"),
        // M11 (#76): `.tscn` scene files feed the scene index (node/script/instance relations).
        // `.scn` (binary) is intentionally NOT watched — gdls parses scene TEXT only (anti-catalog
        // W16), and a binary `.scn` has no text form.
        watcher("**/*.tscn"),
        watcher("**/project.godot"),
        watcher("**/*.gdextension"),
        watcher("**/extension_api.json"),
        watcher("**/doc_classes/*.xml"),
    ]
    .into_iter()
    .chain(asset_catch_all)
    .collect();
    let req = Request {
        id,
        method: "client/registerCapability".to_string(),
        params: serde_json::json!({
            "registrations": [{
                "id": "gdls-watched-files",
                "method": "workspace/didChangeWatchedFiles",
                "registerOptions": { "watchers": watchers },
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
        Reaction::Scene { .. } => "scene",
        Reaction::Asset { .. } => "asset",
    }
}

/// The per-reaction file path string for span attribution. Only `GdSource` carries a file path;
/// every other reaction surfaces its own log line with the affected path. Returning `None` here
/// (rendered as the empty string in the span) keeps the span field uniform without forcing
/// every non-GdSource reaction to invent a synthetic path label.
fn event_path(reaction: &Reaction) -> Option<String> {
    match reaction {
        Reaction::GdSource { path, .. } => Some(path.to_string()),
        Reaction::Scene { path, .. } => Some(path.to_string()),
        Reaction::Asset { path, .. } => Some(path.to_string()),
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
                            let tree = state.workspace.parse_source(&text).tree;
                            state.workspace.reindex(&path, &tree);
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
                            let tree = state.workspace.parse_source(&text).tree;
                            state.workspace.reindex(&to, &tree);
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
        // M11 (#76): a `.tscn` change keeps the scene index live mid-session. Bounded to the project
        // root like GdSource. Disk-sourced (the scene index is not fed from editor buffers). It does
        // NOT re-diagnose the scene's attached scripts: a valid `$`/`%` types as bare `NATIVE Node`
        // from the enclosing class/function alone (`reduce_get_node`), independent of the scene, so a
        // scene edit cannot change a script's diagnostics — re-publishing them would be byte-identical
        // churn. (Precise scene-derived types are navigation-only and pull-based — `docs/02` §11 —
        // so they need no dirty-marking either.) Only the index itself is mutated, keeping queries
        // that DO read scenes (future precise hover/completion) live.
        Reaction::Scene { path, change } => {
            if !path_is_within(&path, project_root) {
                log::warn!("watcher: dropping out-of-root scene event for {path}");
                return;
            }
            match change {
                FileChange::Created | FileChange::Modified => state.workspace.reindex_scene(&path),
                FileChange::Deleted => state.workspace.remove_scene(&path),
                FileChange::Renamed { from, to } => {
                    state.workspace.remove_scene(&from);
                    state.workspace.reindex_scene(&to);
                }
            }
        }
        // #127: an arbitrary asset change keeps the AssetIndex live mid-session so `load`/`preload`
        // path completion offers newly-added textures/audio/`.tres`/… without a restart. Bounded to
        // the project root like Scene/GdSource. Disk-sourced (assets are never fed from editor
        // buffers — they're not open documents). It does NOT re-diagnose any script: an asset is just
        // a `res://` path string in the completion list, and no script's diagnostics depend on which
        // sibling files exist — so re-publishing would be byte-identical churn (same reasoning as
        // Scene). Only the index is mutated.
        Reaction::Asset { path, change } => {
            if !path_is_within(&path, project_root) {
                log::warn!("watcher: dropping out-of-root asset event for {path}");
                return;
            }
            match change {
                FileChange::Created | FileChange::Modified => state.workspace.reindex_asset(&path),
                FileChange::Deleted => state.workspace.remove_asset(&path),
                FileChange::Renamed { from, to } => {
                    state.workspace.remove_asset(&from);
                    state.workspace.reindex_asset(&to);
                }
            }
        }
        // A dropped `Other` is a no-op here — its `SkipReason` was already recorded on the
        // surrounding `watcher_event` span (WP-RD7).
        Reaction::Other(_) => {}
        // WP-RD11 (3): the project/native-DB reactions (ProjectGodot, ExtensionApiJson,
        // Gdextension, DocClassesXml) are no longer reloaded per-event — `handle_watcher` scans the
        // whole batch and coalesces their reload + `republish_all_open_buffers` into one post-batch
        // pass. So `apply_reaction` does per-file work only for `GdSource`, `Scene`, and `Asset`.
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
                gd_project::extract_interface(&state.workspace.parse_source(&disk_text).tree)
                    .signature_hash();
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
    // #255, the pull half: the loop above only reaches OPEN buffers, and only the push channel. A
    // pull client's cached `textDocument/diagnostic` results for every invalidated file — open or
    // not — are now stale, and nothing else tells it so. Send ONE refresh per drained batch, and
    // only when the batch reached past the file the caller already handled: a plain body-only edit
    // dirties just that file, which the client is re-pulling anyway, so it must not trigger a
    // project-wide re-pull on every keystroke.
    let skip_path = skip
        .and_then(uri_to_path)
        .map(|p| gd_project::normalize_path(&p));
    if dirty_set.iter().any(|p| Some(p) != skip_path.as_ref()) {
        send_workspace_diagnostic_refresh(state);
    }
}

/// Republish diagnostics for every open buffer — used after a project-wide policy change
/// (`project.godot`) or native DB reload (`extension_api.json` / gdextension surface) where the
/// `Index.dirty` set won't capture the change (the change isn't interface-keyed; it affects every
/// file's analysis).
/// Tell the user which Godot version their scripts are being read as, but only when gdls had to
/// guess or correct rather than read a declared one.
///
/// A project that names a supported version stays silent — that is the normal case and needs no
/// commentary. The noteworthy cases (no version declared, or one outside the supported range) all
/// mean diagnostics may not match the engine the user is actually running, which is exactly the
/// kind of degraded surface a stderr line would hide.
fn notify_dialect(state: &ServerState) {
    let Some(message) = gd_project::dialect_notice(
        state.workspace.dialect,
        state.workspace.dialect_origin,
        state.workspace.project.declared_engine_version,
    ) else {
        return;
    };
    show_message(state, lsp_types::MessageType::WARNING, &message);
}

/// Rebuild the whole workspace after `project.godot` moved the project to another Godot version.
///
/// Every parse tree in the session was produced under the old dialect, interfaces in the index
/// included, so nothing already cached can be trusted. This is deliberately the blunt instrument:
/// one cold startup's worth of work on an event that essentially never happens, in exchange for not
/// having to reason about which derived state survives a semantics change. The warm-start cache
/// misses on the new key, which is exactly right.
///
/// Open buffers are re-indexed from the VFS afterwards, since the fresh index was built from disk
/// and would otherwise serve a saved file's interface in place of the buffer's unsaved content.
fn rebuild_workspace_for_dialect_change(state: &mut ServerState) {
    let root = state.workspace.project.root.clone();
    let dialect = state.workspace.dialect;
    log::info!("rebuilding the workspace under Godot {dialect} semantics");
    let mut sink = crate::progress::NoopSink;
    state.workspace = Workspace::load_with_progress(&root, &state.options, &mut sink);
    for uri in open_buffer_uris(state) {
        reindex_open_buffer(state, &uri);
    }
    notify_dialect(state);
    send_semantic_tokens_refresh(state);
}

fn republish_all_open_buffers(state: &mut ServerState) {
    for uri in open_buffer_uris(state) {
        publish_diagnostics(state, uri, None);
    }
    // #255: the project-wide analogue — one refresh for the whole reload, never one per file.
    send_workspace_diagnostic_refresh(state);
}

/// Advertise exactly the v1 capability set Claude Code consumes (`docs/05-lsp-cc-integration.md`).
///
/// `caps` carries the negotiated client capabilities so the M11 (#79) `workspace.fileOperations`
/// block is advertised per-operation only when the client offered that operation — gdls never tells
/// a client it handles a file operation the client won't send (anti-catalog W15).
///
/// `formatter` is the session-structural [`FormatterConfig`] (M11 #80): `documentFormattingProvider`
/// is advertised ONLY when a formatter command is configured (same W15 rule — never advertise an
/// unconfigured/unimplemented surface). It is read here, at the one-shot `initialize`, because a
/// capability cannot be added mid-session.
fn capabilities(
    encoding: PositionEncoding,
    caps: &ClientCaps,
    formatter: &crate::config::FormatterConfig,
) -> ServerCapabilities {
    ServerCapabilities {
        position_encoding: Some(encoding.to_kind()),
        // #260: the OPTIONS form, not the bare `TextDocumentSyncKind` number. Both are legal
        // (`textDocumentSync?: TextDocumentSyncOptions | TextDocumentSyncKind`) and every
        // mainstream client normalises the number to `{ openClose: true, change: N }` — but the
        // number states nothing, and every per-file surface here is keyed on an OPEN buffer, so
        // `openClose` is a hard requirement rather than something to leave a minimal client to
        // assume. `save.include_text = false` is the truthful value: `didSave` is routed and
        // deliberately mutates nothing (the buffer is already authoritative, `router.rs`), so
        // re-sending the text would be pure waste. `will_save` / `will_save_wait_until` stay
        // unset — not implemented, and W15 forbids advertising a surface with nothing behind it.
        text_document_sync: Some(TextDocumentSyncCapability::Options(
            lsp_types::TextDocumentSyncOptions {
                open_close: Some(true),
                change: Some(TextDocumentSyncKind::INCREMENTAL),
                will_save: None,
                will_save_wait_until: None,
                save: Some(lsp_types::TextDocumentSyncSaveOptions::SaveOptions(
                    lsp_types::SaveOptions {
                        include_text: Some(false),
                    },
                )),
            },
        )),
        document_symbol_provider: Some(OneOf::Left(true)),
        // M7 (#58): the two genuinely long requests advertise workDoneProgress so clients send a
        // workDoneToken in their params; the other providers stay bare booleans.
        // M9 (#71): `resolve_provider: Some(true)` advertises `workspaceSymbol/resolve` — a client
        // with `workspace.symbol.resolveSupport` then receives the partial `WorkspaceSymbol[]`
        // shape (location sans full range) and pulls each precise range lazily via resolve.
        workspace_symbol_provider: Some(OneOf::Right(lsp_types::WorkspaceSymbolOptions {
            work_done_progress_options: lsp_types::WorkDoneProgressOptions {
                work_done_progress: Some(true),
            },
            resolve_provider: Some(true),
        })),
        definition_provider: Some(OneOf::Left(true)),
        // M9 (#68): declaration === definition (GDScript has no separate declare/define construct,
        // so the handler delegates straight to `definition`). typeDefinition jumps to the declaring
        // site of the symbol's TYPE (project `class_name` site / native stub header, else null).
        // Both are plain `Simple(true)` providers with no client-capability path to gate on —
        // advertised unconditionally like `definition`/`implementation`.
        declaration_provider: Some(DeclarationCapability::Simple(true)),
        type_definition_provider: Some(TypeDefinitionProviderCapability::Simple(true)),
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
        // auto-pop the list (`.` member access, `$`/`%` node paths, `"` resource/string contexts,
        // `@` annotations); identifier characters never go here (the client triggers on those
        // itself). `resolve_provider: true` defers
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
        // M9 (#70): foldingRange + selectionRange. Both are pure parse-priced projections (AST
        // spans + the comment side-channel → ranges) with no project fan-out, so they advertise as
        // bare booleans and are served even at Hard memory pressure (not in `analyze_using`).
        folding_range_provider: Some(FoldingRangeProviderCapability::Simple(true)),
        selection_range_provider: Some(SelectionRangeProviderCapability::Simple(true)),
        // M10 (#74): documentColor + colorPresentation for `Color` literals. Parse-priced (a token
        // scan, no analyzer, no fan-out), so it advertises as a bare `Simple(true)` provider and is
        // served even at Hard memory pressure (not in the `analyze_using` shed set), exactly like
        // foldingRange.
        color_provider: Some(ColorProviderCapability::Simple(true)),
        // M10 (#72): semanticTokens. The legend is gdls's fixed STANDARD-only legend (10 standard
        // token types + 6 standard modifiers — ZERO custom names; this is the #30 highlighting
        // target). `full: Delta{delta: true}` advertises both `semanticTokens/full` and
        // `.../full/delta` (10k-line files need delta); `range: true` advertises
        // `semanticTokens/range` (parse-priced, served even at Hard memory pressure). The full/delta
        // requests are analysis-priced and shed at Hard pressure; range stays served. The legend is
        // advertised ALWAYS at full width (stable wire indices for delta correlation); per-client
        // legend intersection happens at emit time, never by shrinking the advertised legend.
        semantic_tokens_provider: Some(
            lsp_types::SemanticTokensServerCapabilities::SemanticTokensOptions(
                lsp_types::SemanticTokensOptions {
                    work_done_progress_options: Default::default(),
                    legend: crate::semantic_tokens::legend(),
                    range: Some(true),
                    full: Some(lsp_types::SemanticTokensFullOptions::Delta { delta: Some(true) }),
                },
            ),
        ),
        // M10 (#73): inlayHint. `resolve_provider: true` advertises `inlayHint/resolve` — a client
        // with `inlayHint.resolveSupport` then receives hints WITHOUT a tooltip (carrying a `data`
        // blob) and pulls each tooltip lazily; a client without it receives complete hints eagerly.
        // The textEdit is always eager (an apply never needs a resolve round-trip). Analysis-priced
        // (type table + call resolution), so the request sheds at Hard memory pressure
        // (ContentModified, in the `analyze_using` set above); `inlayHint/resolve` is NOT shed (it
        // reads the cached `data` blob only — never a fresh analyze, like `completionItem/resolve`).
        inlay_hint_provider: Some(OneOf::Right(
            lsp_types::InlayHintServerCapabilities::Options(lsp_types::InlayHintOptions {
                work_done_progress_options: Default::default(),
                resolve_provider: Some(true),
            }),
        )),
        // M9 (#66): rename + prepareRename. `prepare_provider: true` advertises that the client may
        // pre-flight a rename with `textDocument/prepareRename` (range + placeholder); the rename
        // itself reuses the `references` resolution to collect every edit site, validates the new
        // name (identifier, non-keyword, non-colliding) before assembling any edit, and refuses a
        // native/stub target with a typed request error — never a partial or corrupting edit. No
        // project fan-out beyond what `references` already does, so it carries no workDoneProgress.
        rename_provider: Some(OneOf::Right(lsp_types::RenameOptions {
            prepare_provider: Some(true),
            work_done_progress_options: Default::default(),
        })),
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
        // M10 (#75): codeAction. `resolve_provider: true` advertises `codeAction/resolve` — a client
        // with `codeAction.resolveSupport` receives the action sans `edit` and pulls the edit lazily.
        // `code_action_kinds` lists EXACTLY the kinds gdls offers (only `quickfix` this phase) so a
        // `source.fixAll` filter naturally excludes the suppression action (anti-catalog W15: advertise
        // only what is implemented).
        code_action_provider: Some(lsp_types::CodeActionProviderCapability::Options(
            lsp_types::CodeActionOptions {
                code_action_kinds: Some(crate::code_action::offered_kinds()),
                resolve_provider: Some(true),
                work_done_progress_options: Default::default(),
            },
        )),
        // M10 (#75): the `workspace/executeCommand` command list — EXACTLY the commands gdls handles
        // (anti-catalog W15: never advertise an empty/broken list that errors when invoked). The list
        // is the same constant `execute_command`'s unknown-command guard checks, so the two cannot
        // drift.
        execute_command_provider: Some(lsp_types::ExecuteCommandOptions {
            commands: crate::code_action::COMMANDS
                .iter()
                .map(|c| c.to_string())
                .collect(),
            work_done_progress_options: Default::default(),
        }),
        // M11 (#80): `documentFormattingProvider` — advertised ONLY when a formatter command is
        // configured (`formatter.command` set), so `None` (the default) means gdls never claims to
        // format when no external tool is wired up (anti-catalog W15). The handler shells out to the
        // configured command with NO shell (argv vector), pipes the buffer through stdin/stdout under
        // a bounded timeout, and returns minimal-diff edits on success / no edits + a deduped
        // showMessage(Warning) on failure — the buffer is never corrupted. `rangeFormatting` is
        // deliberately NOT advertised: GDScript formatters (gdformat) are whole-file, and the spec
        // says to advertise range formatting only when the tool supports it (a future config flag
        // could add it).
        document_formatting_provider: crate::formatter::document_formatting_provider(formatter),
        // M11 (#79): the `workspace.fileOperations` block — advertised per-operation only when the
        // client opted in (a `None` block means gdls never claims a file operation it can't receive).
        // `willRename` is the mutating surface (returns a `WorkspaceEdit` rewriting `res://`
        // `preload`/`load` literals that resolve to the renamed file); the `did*` operations are
        // index nudges. Each filter scopes to `**/*.gd` + `**/*.tscn` — the only file kinds gdls
        // tracks (W16: scene TEXT only). `willCreate`/`willDelete` are intentionally absent (gdls has
        // no edit to contribute on a create/delete).
        workspace: crate::file_operations::workspace_server_capabilities(&caps.file_operations),
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

/// Dispatch one request. Returns `Some(response)` to send immediately, or `None` when the request
/// was handed off-worker and its response will arrive later (the `textDocument/formatting` bridge —
/// M11 #135/#136 — is the only handler that returns `None`; every other arm returns `Some`).
fn dispatch_request(state: &mut ServerState, req: Request) -> Option<Response> {
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
        return Some(interrupt_response(interrupt, req_id));
    }
    state.current_token = Some(lifecycle.token());
    state.current_request_id = Some(req_id.clone());
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
    // M9 (#66): the fallible variant for handlers that may REFUSE a syntactically-valid request
    // with a typed error (rename/prepareRename: a native or stub target, an invalid new name).
    // Same params-deserialize prologue as `handle!`; the handler returns
    // `Result<T, crate::handlers::RequestRefusal>` so an `Ok(value)` serializes as a normal result
    // and an `Err(refusal)` becomes a `Response::new_err` carrying the refusal's JSON-RPC code +
    // human message (NOT a silent null — the corruption firewall is that the client SEES the
    // refusal). Runs through the same `finish_request` interrupt gate as every other arm.
    macro_rules! handle_fallible {
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
                Ok(p) => match $h(state, p) {
                    Ok(value) => Response::new_ok(id, value),
                    Err(refusal) => {
                        let crate::handlers::RequestRefusal { code, message } = refusal;
                        log::info!("{method} refused: {message}");
                        Response::new_err(id, code, message)
                    }
                },
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
            // M9 (#68): `typeDefinition` runs `analyze_if_gd` to resolve the cursor symbol's type
            // before mapping it to a declaring Location — analysis-priced, so it sheds at Hard like
            // hover. `declaration` is intentionally absent: it delegates to `definition`, which is
            // itself absent from this set, so `declaration` inherits identical under-pressure
            // behavior (the two stay byte-identical — `definition` may still analyze on some arms,
            // but neither is shed here).
            | "textDocument/typeDefinition"
            // M9 (#66): `rename` reuses the full `references` engine (per-candidate analyze) to
            // collect every edit site, and `prepareRename` runs the same cursor→symbol resolution
            // (definition / native-stub gate analyzes); both are analysis-priced, so they shed at
            // Hard memory pressure with ContentModified exactly like `references`.
            | "textDocument/rename"
            | "textDocument/prepareRename"
            // M10 (#72): `semanticTokens/full` + `.../full/delta` re-classify the whole document
            // against a fresh `analyze` (member/native/enum/static precision) — analysis-priced, so
            // they shed at Hard memory pressure with ContentModified like hover. `semanticTokens/
            // range` is intentionally ABSENT: it classifies against `cached_analysis` only (never a
            // fresh analyze) and must stay served at Hard pressure, exactly like foldingRange.
            | "textDocument/semanticTokens/full"
            | "textDocument/semanticTokens/full/delta"
            // M10 (#73): `inlayHint` runs a fresh `analyze` to read the inferred-type table and
            // resolve call-site parameter names — analysis-priced, so it sheds at Hard memory
            // pressure with ContentModified like hover. `inlayHint/resolve` is intentionally ABSENT:
            // it only reads the hint's `data` blob (no fresh analyze), so shedding it would reclaim
            // nothing — mirroring `completionItem/resolve`'s exclusion.
            | "textDocument/inlayHint"
            // M10 (#75): the MUTATING `codeAction` warning quickfixes run the ERROR backstop — they
            // apply each candidate edit in memory and re-`analyze` it (and `_`-prefix reuses the
            // `rename`/`references` engine) to prove the edit introduces no new error before offering
            // it. That makes `codeAction` (and `codeAction/resolve`, which re-runs the backstop at
            // resolve) analysis-priced, so both shed at Hard memory pressure with ContentModified like
            // `rename`. A shed mutating-fix lightbulb is the right trade under pressure — the
            // suppression-only phase-4 path was parse-only, but the backstop changed the pricing.
            | "textDocument/codeAction"
            | "codeAction/resolve"
            // M11 (#79): `workspace/willRenameFiles` is the MUTATING reference-rewrite — it parses
            // EVERY indexed `.gd` to find `res://` literals that resolve to a renamed file (a
            // project-wide fan-out like `references`). Analysis-priced by that fan-out, so it sheds at
            // Hard memory pressure with ContentModified; the client falls back to moving the file
            // without rewriting refs (a missed rewrite, never a broken one — correctness over coverage).
            | "workspace/willRenameFiles"
    );
    if state.memory_pressure == MemoryPressure::Hard && analyze_using {
        // Re-record the request as cancelled-cum-shed so the per-handler trace still shows the
        // refused request rather than a silent drop. Cleanup is the same as the bottom of the fn
        // (deregister via `finish_request`, clear current_token), so jump straight to the end
        // via early return.
        state.current_token = None;
        state.current_request_id = None;
        tracing::warn!(
            target: "shed",
            id = %req_id,
            method = %method,
            "request shed at Hard memory pressure",
        );
        return Some(finish_request(
            &state.shared,
            Response::new_err(
                req_id,
                ERR_CONTENT_MODIFIED,
                "server is shedding requests under memory pressure; please retry".to_string(),
            ),
        ));
    }
    // M11 (#135/#136): `textDocument/formatting` runs OFF the request worker. It is handled before
    // the synchronous dispatch table because its control flow is different: a real format returns
    // `Pending` — the worker sends NO response now (it arrives via the `recv(format_bridge_rx)` arm
    // once the off-worker thread finishes) and the lifecycle stays REGISTERED across the gap, so a
    // cancel/edit the router trips meanwhile is still applied by `finish_request` on the done arm.
    // Only the defensive `Immediate` paths (no command / no buffer) answer synchronously here.
    if method == "textDocument/formatting" {
        let _span = tracing::info_span!("handle_request", method = %method, id = %id);
        let _enter = _span.enter();
        let dispatch = match serde_json::from_value(params) {
            Ok(p) => crate::formatter::formatting(state, p),
            Err(e) => {
                // A malformed formatting request: answer the param error synchronously (and clear
                // the per-request scratch like every other arm).
                state.current_token = None;
                state.current_request_id = None;
                return Some(finish_request(
                    &state.shared,
                    invalid_params_response(id, &method, e),
                ));
            }
        };
        state.current_token = None;
        state.current_request_id = None;
        return match dispatch {
            crate::formatter::FormatDispatch::Immediate(edits) => {
                let value = serde_json::to_value(edits).unwrap_or(serde_json::Value::Null);
                Some(finish_request(&state.shared, Response::new_ok(id, value)))
            }
            // Pending: do NOT finish the lifecycle and do NOT send a response — both happen on the
            // done arm. The off-worker thread now owns answering this id.
            crate::formatter::FormatDispatch::Pending => None,
        };
    }
    let resp = match method.as_str() {
        "textDocument/documentSymbol" => handle!(handlers::document_symbol),
        "textDocument/documentLink" => handle!(handlers::document_link),
        // LSP says hover returns `null` when there's nothing to say — `serde_json::to_value(None)`
        // serializes to `null`, which is what the wire wants.
        "textDocument/hover" => handle!(handlers::hover),
        "textDocument/definition" => handle!(handlers::definition),
        // M9 (#68): declaration === definition (no separate declare/define in GDScript), so this
        // delegates to the same handler and returns byte-identical targets. typeDefinition resolves
        // the cursor symbol's type and jumps to that type's declaring site (or `null` for
        // Builtin/Variant/unresolved). `serde_json::to_value(None)` → `null`, the LSP wire shape.
        "textDocument/declaration" => handle!(handlers::declaration),
        "textDocument/typeDefinition" => handle!(handlers::type_definition),
        "textDocument/references" => handle!(handlers::references),
        // M9 (#67): documentHighlight. Returns `DocumentHighlight[]` for the symbol under the
        // cursor scoped to the request file (or `null` when the cursor isn't on an identifier).
        "textDocument/documentHighlight" => handle!(handlers::document_highlight),
        "textDocument/implementation" => handle!(handlers::implementation),
        "textDocument/prepareCallHierarchy" => handle!(handlers::prepare_call_hierarchy),
        "callHierarchy/incomingCalls" => handle!(handlers::incoming_calls),
        "callHierarchy/outgoingCalls" => handle!(handlers::outgoing_calls),
        "workspace/symbol" => handle!(handlers::workspace_symbol),
        // M9 (#71): the lazy companion of `workspace/symbol`. When the client advertised
        // `workspace.symbol.resolveSupport`, the query returned `WorkspaceSymbol[]` with a
        // location-sans-range; `resolve` reads the item's `data` (path + name span), touches that
        // one file, and fills the precise `Location`. Index-/parse-priced (one file), so it is NOT
        // in the Hard-pressure shed set above — mirroring `completionItem/resolve`'s exclusion.
        "workspaceSymbol/resolve" => handle!(handlers::workspace_symbol_resolve),
        // M8 (#64): completion + its lazy resolve. `completion` returns a `CompletionList`
        // (never a bare array — W18); `resolve` fills documentation/detail and leaves the
        // ranking/edit fields untouched.
        "textDocument/completion" => handle!(handlers::completion),
        "completionItem/resolve" => handle!(handlers::completion_item_resolve),
        // M8 (#65): signatureHelp. Returns `SignatureHelp` (or `null` when the cursor is in no
        // call). `serde_json::to_value(None)` serializes to `null`, which is what the wire wants.
        "textDocument/signatureHelp" => handle!(handlers::signature_help),
        // M9 (#70): foldingRange returns `FoldingRange[]` (compound-node blocks, comment runs, and
        // `#region`/`#endregion` pairs); selectionRange returns one `SelectionRange` ancestor chain
        // per requested position. Both are parse-priced (NOT in the Hard-pressure shed set above).
        "textDocument/foldingRange" => handle!(handlers::folding_range),
        "textDocument/selectionRange" => handle!(handlers::selection_range),
        // M10 (#74): documentColor returns `ColorInformation[]` (a swatch per `Color` literal —
        // numeric ctor, named constant, or hex/name string form); colorPresentation returns the
        // constructor form(s) for a picked color as `ColorPresentation[]` (whole-literal textEdit,
        // lossless round-trip). Both are token-scan / parse-priced (NOT in the Hard-pressure shed
        // set above), served like foldingRange.
        "textDocument/documentColor" => handle!(handlers::document_color),
        "textDocument/colorPresentation" => handle!(handlers::color_presentation),
        // M11 (#80, #135/#136): `textDocument/formatting` is handled BEFORE this match (it runs
        // off-worker; see the dedicated branch above), so it never reaches the synchronous table.
        // M10 (#72): semanticTokens. `full` returns the whole delta-encoded token set (fresh result
        // id); `full/delta` returns flat-array edits vs the prior id (or a fresh full on an unknown
        // id); `range` returns only the intersecting tokens. `full`/`full/delta` are analysis-priced
        // (in the `analyze_using` shed set above → ContentModified at Hard pressure); `range` is
        // parse-priced (cached-analysis only) and stays served at Hard pressure, like foldingRange.
        "textDocument/semanticTokens/full" => handle!(handlers::semantic_tokens_full),
        "textDocument/semanticTokens/full/delta" => {
            handle!(handlers::semantic_tokens_full_delta)
        }
        "textDocument/semanticTokens/range" => handle!(handlers::semantic_tokens_range),
        // M10 (#73): inlayHint. `inlayHint` returns `InlayHint[]` for the requested range — inferred
        // `:= `/`for`-var TYPE hints and call-site PARAMETER-name hints, each config-toggleable. The
        // tooltip is deferred to `inlayHint/resolve` only for a `resolveSupport` client (eager
        // otherwise); the textEdit is always eager. `serde_json::to_value(None)` → the LSP `null`
        // wire shape. `inlayHint` is analysis-priced (sheds at Hard, see `analyze_using`); `resolve`
        // reads the hint's `data` blob only (not shed).
        "textDocument/inlayHint" => handle!(handlers::inlay_hint),
        "inlayHint/resolve" => handle!(handlers::inlay_hint_resolve),
        // M10 (#75): codeAction pipeline. `codeAction` returns `CodeActionOrCommand[]` (a `quickfix`
        // per `@warning_ignore`-able diagnostic in range; `Command` shape for a client without
        // `codeActionLiteralSupport`, `CodeAction` otherwise — edit eager or deferred per
        // `resolveSupport`); `codeAction/resolve` fills a deferred action's `edit`. Both are driven by
        // `context.diagnostics` / the action's `data` (never a fresh analyze), so neither is in the
        // Hard-pressure `analyze_using` shed set above — served like foldingRange / `inlayHint/resolve`.
        "textDocument/codeAction" => handle!(handlers::code_action),
        "codeAction/resolve" => handle!(handlers::code_action_resolve),
        // M10 (#75): `workspace/executeCommand` — runs a server command (only `gdls.applyWarningIgnore`,
        // which sends the `workspace/applyEdit` fallback fire-and-forget). The fallible arm: an UNKNOWN
        // command returns a typed error (never a panic — anti-catalog W15). A handled command answers
        // `null` (the applyEdit reply is correlated separately via `handle_outbound_response`). Args-
        // driven (no analyze), so it is NOT in the shed set.
        "workspace/executeCommand" => handle_fallible!(handlers::execute_command),
        // M9 (#66): rename + prepareRename — the fallible arms (a syntactically-valid request may
        // still be REFUSED with a typed error: a native/stub target, or an invalid new name).
        // `prepareRename` returns the identifier range (+ placeholder when the client advertised
        // `rename.prepareSupport`) or refuses; `rename` returns the workspace-wide `WorkspaceEdit`
        // (versioned `documentChanges` or the legacy `changes` map, per the client's
        // `workspace.workspaceEdit.documentChanges`) built from the SAME `references` set, or
        // refuses with zero edits. `serde_json::to_value(None)` → the LSP `null` wire shape when the
        // cursor lands on no identifier at all.
        "textDocument/prepareRename" => handle_fallible!(handlers::prepare_rename),
        "textDocument/rename" => handle_fallible!(handlers::rename),
        // M11 (#79): `workspace/willRenameFiles` — the MUTATING file-operation hook. Returns a
        // `WorkspaceEdit` rewriting `res://` `preload`/`load` literals that POSITIVELY resolve to a
        // renamed/moved `.gd`/`.tscn` (fail-closed: only literals whose resolved identity equals a
        // renamed file are touched, and only the path between the quotes). `serde_json::to_value(None)`
        // → the LSP `null` wire shape when nothing needs rewriting. Advertised only when the client
        // offered `workspace.fileOperations.willRename`.
        "workspace/willRenameFiles" => handle!(crate::file_operations::will_rename_files),
        // M9 (#69): typeHierarchy. `prepareTypeHierarchy` resolves the class under the cursor to
        // one `TypeHierarchyItem` carrying a compact `data` blob (the type's identity); the
        // follow-ups walk the extends graph one level from that blob — `supertypes` UP (parent
        // project class / native base), `subtypes` DOWN (direct project subclasses). All three are
        // index/parse-priced (registry + interface + native DB, like `implementation`), so none is
        // in the Hard-pressure `analyze_using` shed set above. `serde_json::to_value(None)` → the
        // LSP `null` wire shape when the cursor resolves to no class / the blob names no type.
        "textDocument/prepareTypeHierarchy" => handle!(handlers::prepare_type_hierarchy),
        "typeHierarchy/supertypes" => handle!(handlers::type_hierarchy_supertypes),
        "typeHierarchy/subtypes" => handle!(handlers::type_hierarchy_subtypes),
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
    state.current_request_id = None;
    Some(finish_request(&state.shared, resp))
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
        // M11 (#79): the `did*` file-operation notifications — index NUDGES routed through the SAME
        // `handle_client_file_events` → `apply_reaction_batch` funnel as `didChangeWatchedFiles`, so
        // the content-fingerprint gate dedupes a change the native watcher also observed (no
        // double-processing). A rename is delete(old)+create(new); create/delete map directly.
        "workspace/didRenameFiles" => {
            if let Ok(p) = parse_params::<lsp_types::RenameFilesParams>(&method, params) {
                crate::file_operations::did_rename_files(state, p);
            }
        }
        "workspace/didCreateFiles" => {
            if let Ok(p) = parse_params::<lsp_types::CreateFilesParams>(&method, params) {
                crate::file_operations::did_create_files(state, p);
            }
        }
        "workspace/didDeleteFiles" => {
            if let Ok(p) = parse_params::<lsp_types::DeleteFilesParams>(&method, params) {
                crate::file_operations::did_delete_files(state, p);
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
            let tree = state.workspace.parse_source(&text).tree;
            state.workspace.reindex(&path, &tree);
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
/// The `tags` / `codeDescription` / `data` fields are LSP-projection-only additions: Godot's own
/// output never serializes them, so message strings, spans, and severities stay byte-identical to the
/// faithful stream (`.out` conformance untouched). Tags are gated on the client's
/// `publishDiagnostics.tagSupport` (pyright-style); the docs link ships ungated
/// (rust-analyzer-style — clients ignore unknown members); the M10 (#75) `data` fix payload is gated
/// on `publishDiagnostics.dataSupport` (a client that won't round-trip `data` gets none).
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
                // M10 (#75): the additive fix-payload tag. Carries the warning's `PNAME` so a later
                // phase can offer a quickfix without re-deriving the code. Gated on
                // `publishDiagnostics.dataSupport`; ABSENT changes nothing else (message/range/severity
                // are untouched above — fidelity), so the acceptance sweep stays byte-identical. The
                // codeAction path does NOT depend on this (it reads the diagnostic's `code`).
                data: if caps.code_action.diagnostic_data_support {
                    warning_diagnostic_data(d.warning_code())
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

/// M10 (#75): the additive `Diagnostic.data` fix payload — the warning's `PNAME`, so a quickfix
/// consumer can offer a fix without re-deriving the code. Namespaced under a `gdls` key so it can
/// never collide with another producer's `data` in a buffer the client merges across servers; the
/// inner field is the upper-case warning name (the same `code` carried on the diagnostic). `None` for
/// a bare type/semantic error (no `warning_code`) — those carry no fix payload. This is the ONLY use
/// of `Diagnostic.data` in gdls; the codeAction path keys off the diagnostic's `code` instead, so this
/// tag is pure forward-looking enrichment, gated on `publishDiagnostics.dataSupport`.
fn warning_diagnostic_data(
    code: Option<gd_analyze::warnings::WarningCode>,
) -> Option<serde_json::Value> {
    let name = gd_analyze::warnings::name_from_code(code?);
    Some(serde_json::json!({ "gdls": { "warningCode": name } }))
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
            current_request_id: None,
            // The watcher-path tests don't exercise the WP-H1 ladder; a synthetic budget with
            // caps far above what a small tempdir workspace will ever observe keeps the ticker
            // arm at MemoryPressure::Normal across the run.
            budget: MemoryBudget::from_caps_mb(u64::MAX / 2, u64::MAX / 2),
            memory_pressure: MemoryPressure::Normal,
            outbound: FxHashMap::default(),
            stub_cache: crate::stubs::StubCache::default(),
            semantic_tokens_cache: FxHashMap::default(),
            semantic_tokens_result_seq: 0,
            formatter_warned: FxHashSet::default(),
            format_bridge: crate::formatter::FormatBridge::default(),
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
            current_request_id: None,
            budget,
            memory_pressure: MemoryPressure::Normal,
            outbound: FxHashMap::default(),
            stub_cache: crate::stubs::StubCache::default(),
            semantic_tokens_cache: FxHashMap::default(),
            semantic_tokens_result_seq: 0,
            formatter_warned: FxHashSet::default(),
            format_bridge: crate::formatter::FormatBridge::default(),
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
        let resp = dispatch_request(&mut state, hover).expect("non-formatting → Some(response)");
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
        let resp =
            dispatch_request(&mut state, doc_symbol).expect("non-formatting → Some(response)");
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
        let resp =
            dispatch_request(&mut state, definition).expect("non-formatting → Some(response)");
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
        let resp =
            dispatch_request(&mut state, completion).expect("non-formatting → Some(response)");
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
        let resp =
            dispatch_request(&mut state, signature_help).expect("non-formatting → Some(response)");
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
        let resp = dispatch_request(&mut state, resolve).expect("non-formatting → Some(response)");
        assert_ne!(
            resp.error.as_ref().map(|e| e.code),
            Some(ERR_CONTENT_MODIFIED),
            "completionItem/resolve is not analyze-using and must not be shed at Hard pressure; \
             got {:?}",
            resp.error
        );
    }

    /// M10 (#72): the semanticTokens shed-set contract. `semanticTokens/full` and `.../full/delta`
    /// re-classify against a fresh `analyze`, so they shed with `ContentModified` (-32801) at Hard
    /// memory pressure exactly like hover. `semanticTokens/range` classifies against
    /// `cached_analysis` only (never a fresh analyze) and MUST stay served at Hard pressure, like
    /// foldingRange — it is deliberately absent from the `analyze_using` set. Driving the dispatch
    /// shed gate directly (`memory_pressure = Hard`) is the deterministic seam — the RSS pressure
    /// sampler is `#[cfg(test)]`/`pub(crate)` and can't be forced through the Connection loop, so
    /// the existing shed set has no integration test either; this mirrors `hover`'s in-crate
    /// coverage above.
    #[test]
    fn hard_pressure_sheds_semantic_tokens_full_and_delta_but_serves_range() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("utf-8 temp dir");
        std::fs::write(dir.path().join("project.godot"), "config_version=5\n").unwrap();
        let (mut state, _rx) = state_on(&root);
        state.memory_pressure = MemoryPressure::Hard;

        // `full` is analyze-using → shed with ContentModified.
        let full = Request {
            id: lsp_server::RequestId::from(1),
            method: "textDocument/semanticTokens/full".to_string(),
            params: serde_json::json!({ "textDocument": { "uri": "file:///test/a.gd" } }),
        };
        let resp = dispatch_request(&mut state, full).expect("non-formatting → Some(response)");
        assert_eq!(
            resp.error.as_ref().map(|e| e.code),
            Some(ERR_CONTENT_MODIFIED),
            "semanticTokens/full is analyze-using and must be shed at Hard pressure; got {:?}",
            resp.error
        );

        // `full/delta` is analyze-using → shed with ContentModified.
        let delta = Request {
            id: lsp_server::RequestId::from(2),
            method: "textDocument/semanticTokens/full/delta".to_string(),
            params: serde_json::json!({
                "textDocument": { "uri": "file:///test/a.gd" },
                "previousResultId": "st-1"
            }),
        };
        let resp = dispatch_request(&mut state, delta).expect("non-formatting → Some(response)");
        assert_eq!(
            resp.error.as_ref().map(|e| e.code),
            Some(ERR_CONTENT_MODIFIED),
            "semanticTokens/full/delta is analyze-using and must be shed at Hard pressure; got {:?}",
            resp.error
        );

        // `range` is parse-priced (cached-analysis only) → must NOT be shed. No buffer is open, so
        // the handler returns a null/empty result, but crucially NOT the -32801 shed error.
        let range = Request {
            id: lsp_server::RequestId::from(3),
            method: "textDocument/semanticTokens/range".to_string(),
            params: serde_json::json!({
                "textDocument": { "uri": "file:///test/a.gd" },
                "range": {
                    "start": { "line": 0, "character": 0 },
                    "end": { "line": 0, "character": 0 }
                }
            }),
        };
        let resp = dispatch_request(&mut state, range).expect("non-formatting → Some(response)");
        assert_ne!(
            resp.error.as_ref().map(|e| e.code),
            Some(ERR_CONTENT_MODIFIED),
            "semanticTokens/range is parse-priced and must stay served at Hard pressure; got {:?}",
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
            current_request_id: None,
            budget,
            memory_pressure: MemoryPressure::Normal,
            outbound: FxHashMap::default(),
            stub_cache: crate::stubs::StubCache::default(),
            semantic_tokens_cache: FxHashMap::default(),
            semantic_tokens_result_seq: 0,
            formatter_warned: FxHashSet::default(),
            format_bridge: crate::formatter::FormatBridge::default(),
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
