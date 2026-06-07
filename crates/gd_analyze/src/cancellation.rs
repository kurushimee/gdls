//! Cancellation primitive shared between [`crate::analyze_with_options`] and the LSP server's
//! `$/cancelRequest` plumbing (M5 WP-O4). Lives in `gd_analyze` because the analyzer is where the
//! `is_cancelled()` checks fire (inside the hot reducer / resolver loops) — putting the type here
//! keeps the dependency direction (`gd_server` → `gd_analyze`) clean and lets the analyzer's
//! [`AnalyzeOptions`](crate::AnalyzeOptions) reference it without an inversion.
//!
//! Token-based, not panic-throw, per the locked decision in the M5 plan §2.4 — checkpointing the
//! token every 256 nodes costs one branch on the hot path; the alternative would force
//! `std::panic::catch_unwind` in every analyzer caller. The token is a thin wrapper around
//! `Arc<AtomicBool>` with `Acquire`/`Release` ordering so a `cancel()` from the LSP event-loop
//! thread is observed by the per-request analyzer pass with no further synchronization.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

/// A cooperative cancellation flag. Clone-cheap (just an `Arc` bump) and freely shareable across
/// threads. Cancellation is one-way: once [`Self::cancel`] is called the token is permanently
/// cancelled (matches LSP 3.17's `$/cancelRequest` semantics — a cancel applies to a specific
/// request id and is never "un-cancelled").
#[derive(Clone, Debug, Default)]
pub struct CancellationToken {
    inner: Arc<AtomicBool>,
}

impl CancellationToken {
    /// Build a fresh, not-yet-cancelled token.
    pub fn new() -> Self {
        Self::default()
    }

    /// Trip the token. Every subsequent [`Self::is_cancelled`] returns `true`.
    pub fn cancel(&self) {
        self.inner.store(true, Ordering::Release);
    }

    /// `true` iff some clone of this token has called [`Self::cancel`].
    pub fn is_cancelled(&self) -> bool {
        self.inner.load(Ordering::Acquire)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_token_is_not_cancelled() {
        let tok = CancellationToken::new();
        assert!(!tok.is_cancelled());
    }

    #[test]
    fn cancel_propagates_to_every_clone() {
        let tok = CancellationToken::new();
        let clone = tok.clone();
        assert!(!clone.is_cancelled());
        tok.cancel();
        assert!(clone.is_cancelled());
        assert!(tok.is_cancelled());
    }
}
