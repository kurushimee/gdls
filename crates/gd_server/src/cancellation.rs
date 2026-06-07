//! M5 WP-O4 — server-side `$/cancelRequest` plumbing.
//!
//! The cancellation primitive is [`gd_analyze::CancellationToken`] (re-exported here for the
//! server's convenience): a clone-cheap `Arc<AtomicBool>` wrapper. The analyzer's
//! [`gd_analyze::AnalyzeOptions::cancellation`] field references it; the LSP server's
//! [`crate::server::ServerState`] owns a per-request token map so the
//! `$/cancelRequest` notification arm can flip the token for an in-flight request.
//!
//! Wire model — synchronous LSP loop today:
//! - On every request the [`dispatch_request`](crate::server) function allocates a fresh token,
//!   inserts it in `state.pending_requests[id]`, dispatches the handler, removes the entry on
//!   completion, and — when the token has been flipped — replaces the handler's response with a
//!   [`REQUEST_CANCELLED`] error per LSP 3.17.
//! - The `$/cancelRequest` notification arm in [`crate::server::dispatch_notification`] looks
//!   up `state.pending_requests[id]` and calls [`CancellationToken::cancel`]. Unknown ids are
//!   warn-logged (LSP spec: a cancel for a non-existent id is a no-op).
//!
//! Architectural caveat: the LSP main loop is single-threaded today. A cancel notification that
//! arrives during a handler's run does NOT interrupt that handler; it sits in the channel
//! buffer until the handler returns and the loop re-enters `select!`. By that point the
//! handler has already completed. Effective cancellation in this model requires either (a)
//! queue pile-up — multiple requests pending, cancel arrives for a not-yet-dispatched one —
//! or (b) the handler itself periodically polls the token while looping (which the analyzer
//! does via [`gd_analyze::AnalysisContext::checkpoint`], so a slow per-file analyze IS
//! interruptible if a future architecture moves the analyzer onto a worker thread).
//!
//! The LSP 3.17 `code` for cancelled requests is `-32800` ([`REQUEST_CANCELLED`]).

pub use gd_analyze::CancellationToken;

/// JSON-RPC error code for a cancelled request per LSP 3.17
/// (<https://microsoft.github.io/language-server-protocol/specifications/lsp/3.17/specification/#errorCodes>).
pub const REQUEST_CANCELLED: i32 = -32800;
