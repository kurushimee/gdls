//! Server-side `$/cancelRequest` plumbing (M5 WP-O4, made preemptive by M7 #57).
//!
//! The cancellation primitive is [`gd_analyze::CancellationToken`] (re-exported here for the
//! server's convenience): a clone-cheap `Arc<AtomicBool>` wrapper. The analyzer's
//! [`gd_analyze::AnalyzeOptions::cancellation`] field references it; one token lives inside each
//! [`crate::router::RequestLifecycle`] in the session's shared in-flight registry, where both
//! the `$/cancelRequest` path and the stale-by-edit sweep flip it for an in-flight request.
//!
//! Wire model — router thread + synchronous worker loop (`crate::router` module doc):
//! - The router registers every request's lifecycle as it is read off the wire and flips its
//!   token the moment a `$/cancelRequest` (or a content-mutating notification) arrives — even
//!   while a handler is mid-run on the worker. The analyzer's
//!   [`gd_analyze::AnalysisContext::checkpoint`] sees the flip on its 256-node gate and bails.
//! - The worker's `dispatch_request` reads the interrupt verdict before dispatch (queued
//!   requests answer without running) and again at completion, replacing the handler's response
//!   with a [`REQUEST_CANCELLED`] error per LSP 3.17. Unknown ids are warn-logged (LSP spec: a
//!   cancel for a non-existent id is a no-op).
//!
//! The LSP 3.17 `code` for cancelled requests is `-32800` ([`REQUEST_CANCELLED`]); a result
//! invalidated by an intervening edit instead returns `ContentModified` (-32801).

pub use gd_analyze::CancellationToken;

/// JSON-RPC error code for a cancelled request per LSP 3.17
/// (<https://microsoft.github.io/language-server-protocol/specifications/lsp/3.17/specification/#errorCodes>).
pub const REQUEST_CANCELLED: i32 = -32800;
