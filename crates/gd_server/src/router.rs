//! M7 (#57) — concurrent request dispatch: the shared request-lifecycle registry the event loop
//! and the router thread cooperate on.
//!
//! The session runs two threads over one LSP connection:
//!
//! - the **router** drains `Connection.receiver` as fast as messages arrive, acts on the two
//!   message classes that must take effect *immediately* (`$/cancelRequest` flips the matching
//!   in-flight token; content-mutating notifications mark every in-flight request stale), and
//!   forwards **every** message in arrival order over an internal channel;
//! - the **event loop** (the worker) consumes the forwarded stream and dispatches handlers
//!   synchronously, exactly as before — so handlers keep `&mut ServerState` and the analysis
//!   caches need no synchronization.
//!
//! A request is registered here at *forward* time and deregistered by the worker under the
//! [`SessionShared::in_flight`] lock immediately before its response is chosen — that lock
//! acquisition is the linearization point for staleness: a router sweep that found the entry
//! happened-before the removal (its flag store is visible to the worker's read); a sweep after
//! the removal misses the entry, and the response was already committed.
//!
//! Interrupt outcomes map to LSP 3.17 error codes: `Cancelled` → `RequestCancelled` (-32800),
//! `Stale` → `ContentModified` (-32801). When both flags are set, **cancelled wins** — the
//! client explicitly retracted the request and discards the response either way.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use crossbeam_channel::{Receiver, Sender};
use lsp_server::{Message, RequestId};
use lsp_types::{CancelParams, NumberOrString};
use rustc_hash::FxHashMap;

use crate::cancellation::CancellationToken;

/// Why an in-flight request must abandon its work. Ordered by precedence: a request that is both
/// cancelled and stale reports [`Interrupt::Cancelled`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Interrupt {
    /// The client retracted the request via `$/cancelRequest` → `RequestCancelled` (-32800).
    Cancelled,
    /// A content-mutating notification (`didOpen`/`didChange`/`didClose`) was read off the wire
    /// while the request was in flight → `ContentModified` (-32801). Cross-file analysis means
    /// *any* buffer edit can invalidate *any* in-flight result, so staleness is not scoped to
    /// the edited document.
    Stale,
}

/// Per-request interrupt state. The `cancelled`/`stale` flags record *why* the request should
/// abandon its work; the embedded [`CancellationToken`] is what the analyzer's 256-node
/// checkpoint actually polls — both setters flip it, so a mid-analysis request bails on the next
/// checkpoint regardless of which interrupt landed.
#[derive(Debug, Default)]
pub(crate) struct RequestLifecycle {
    cancelled: AtomicBool,
    stale: AtomicBool,
    token: CancellationToken,
}

impl RequestLifecycle {
    /// Record a client `$/cancelRequest` and trip the analyzer token.
    pub(crate) fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
        self.token.cancel();
    }

    /// Record an intervening content mutation and trip the analyzer token.
    pub(crate) fn mark_stale(&self) {
        self.stale.store(true, Ordering::Release);
        self.token.cancel();
    }

    /// Clone of the analyzer-facing token, for `ServerState::current_token`.
    pub(crate) fn token(&self) -> CancellationToken {
        self.token.clone()
    }

    /// The interrupt verdict, if any. Cancelled wins over stale (see [`Interrupt`]).
    pub(crate) fn interrupt(&self) -> Option<Interrupt> {
        if self.cancelled.load(Ordering::Acquire) {
            Some(Interrupt::Cancelled)
        } else if self.stale.load(Ordering::Acquire) {
            Some(Interrupt::Stale)
        } else {
            None
        }
    }
}

/// State shared between the router thread and the event loop. Lock discipline: the `in_flight`
/// mutex guards only tiny map operations and flag sweeps — never held across a channel send and
/// never nested with another lock.
#[derive(Debug, Default)]
pub(crate) struct SessionShared {
    /// Registered by the router at forward time (the event loop's [`Self::lifecycle`] fallback
    /// covers messages that bypass the router); removed by the worker under this lock immediately
    /// before the response is chosen + sent (the staleness linearization point — see module doc).
    in_flight: Mutex<FxHashMap<RequestId, Arc<RequestLifecycle>>>,
}

impl SessionShared {
    /// Lock the in-flight map, recovering from poisoning: the critical sections are pure map/flag
    /// operations, so the data is consistent even if a panic unwound through one; refusing to
    /// serve the rest of the session over it would be the worse failure ("never crash").
    fn lock(&self) -> MutexGuard<'_, FxHashMap<RequestId, Arc<RequestLifecycle>>> {
        self.in_flight
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// Register `id`, or return its existing lifecycle. The router registers at forward time;
    /// the event loop's dispatch calls this too, inserting only when no router is in front of it.
    pub(crate) fn lifecycle(&self, id: &RequestId) -> Arc<RequestLifecycle> {
        Arc::clone(self.lock().entry(id.clone()).or_default())
    }

    /// Flip the cancel flag for `id`. Returns `false` when the id is unknown — already responded
    /// (a spec-allowed stale cancel) or never seen.
    pub(crate) fn cancel(&self, id: &RequestId) -> bool {
        match self.lock().get(id) {
            Some(lifecycle) => {
                lifecycle.cancel();
                true
            }
            None => false,
        }
    }

    /// Mark every in-flight request stale (a content-mutating notification was read off the
    /// wire). Queued requests short-circuit to `ContentModified` without running; the currently
    /// executing one bails at its next analyzer checkpoint.
    pub(crate) fn mark_all_stale(&self) {
        for lifecycle in self.lock().values() {
            lifecycle.mark_stale();
        }
    }

    /// Deregister `id`, returning its lifecycle so the caller can read the interrupt verdict.
    /// This removal is the staleness linearization point (see module doc) — call it immediately
    /// before choosing the response.
    pub(crate) fn finish(&self, id: &RequestId) -> Option<Arc<RequestLifecycle>> {
        self.lock().remove(id)
    }

    /// Number of registered in-flight requests — test-only observability.
    #[cfg(test)]
    pub(crate) fn in_flight_len(&self) -> usize {
        self.lock().len()
    }
}

/// Spawn the router thread. Once spawned it must be `receiver`'s **only** consumer — the event
/// loop reads the forwarded stream instead (the `initialize` handshake happens strictly before
/// the spawn, so the two never read concurrently).
///
/// Per message, in arrival order:
/// - **Requests** are registered in `shared` before forwarding, so a later cancel or content
///   mutation finds them whether they are still queued or already executing. `shutdown` is
///   lifecycle-exempt: cancelling or staling it makes no sense, and a stray stale sweep must
///   not corrupt its `null` response into an error.
/// - **`$/cancelRequest`** flips the matching lifecycle immediately — this is the preemption:
///   the worker may be mid-handler, and the analyzer's 256-node checkpoint sees the flip. The
///   notification is still forwarded so the bench recorder's trace keeps the cancel traffic.
///   Unknown ids are warn-logged here (the spec allows them; the worker stays quiet to avoid
///   double-logging the common already-responded race).
/// - **Content mutations** (`didOpen`/`didChange`/`didClose`) mark every in-flight request
///   stale before being forwarded: with cross-file analysis, any buffer change can invalidate
///   any in-flight result. `didSave` mutates nothing (the buffer is already authoritative) and
///   disk/watcher events apply between messages on the worker, so neither flags.
/// - **Responses** (to server-initiated requests) are forwarded for the worker to correlate.
///
/// The thread exits when the connection closes (`receiver` disconnects) or after forwarding
/// `exit`; both paths drop `forward`, which is what unblocks the worker's `recv` if it is
/// still waiting.
pub(crate) fn spawn_router(
    receiver: Receiver<Message>,
    forward: Sender<Message>,
    shared: Arc<SessionShared>,
) -> std::thread::JoinHandle<()> {
    std::thread::Builder::new()
        .name("gdls-router".to_string())
        .spawn(move || {
            for msg in receiver.iter() {
                match &msg {
                    Message::Request(req) => {
                        if req.method != "shutdown" {
                            let _ = shared.lifecycle(&req.id);
                        }
                    }
                    Message::Notification(note) => match note.method.as_str() {
                        "$/cancelRequest" => {
                            match serde_json::from_value::<CancelParams>(note.params.clone()) {
                                Ok(p) => {
                                    let id = request_id_from_number_or_string(p.id);
                                    if shared.cancel(&id) {
                                        tracing::info!(target: "cancel", id = %id, "cancel_requested");
                                    } else {
                                        log::warn!(
                                            "$/cancelRequest for {id:?}: no in-flight request \
                                             with that id; ignoring (LSP 3.17 §$/cancelRequest: \
                                             unknown ids are allowed)"
                                        );
                                    }
                                }
                                Err(e) => log::warn!(
                                    "dropped a $/cancelRequest — params failed to parse ({e}); \
                                     in-flight requests will run to completion"
                                ),
                            }
                        }
                        "textDocument/didOpen"
                        | "textDocument/didChange"
                        | "textDocument/didClose" => {
                            shared.mark_all_stale();
                        }
                        _ => {}
                    },
                    Message::Response(_) => {}
                }
                let is_exit = matches!(&msg, Message::Notification(n) if n.method == "exit");
                if forward.send(msg).is_err() {
                    // The worker hung up first (it only does so after the connection died or
                    // `exit` was forwarded — both of which also end this loop); nothing left
                    // to route.
                    break;
                }
                if is_exit {
                    break;
                }
            }
            // `forward` drops here on every exit path, disconnecting the worker's receive arm.
        })
        .expect("invariant: spawning the router thread cannot fail outside of OOM")
}

/// Project `lsp_types::NumberOrString` (the on-wire id form for `$/cancelRequest.params.id`) into
/// `lsp_server::RequestId` (the form the in-flight registry is keyed on). The two enums carry
/// the same I32 / String variants; this is purely a type bridge across the lsp-types ↔ lsp-server
/// crate boundary.
pub(crate) fn request_id_from_number_or_string(id: NumberOrString) -> RequestId {
    match id {
        NumberOrString::Number(n) => RequestId::from(n),
        NumberOrString::String(s) => RequestId::from(s),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fresh_lifecycle_has_no_interrupt_and_untripped_token() {
        let lifecycle = RequestLifecycle::default();
        assert_eq!(lifecycle.interrupt(), None);
        assert!(!lifecycle.token().is_cancelled());
    }

    #[test]
    fn cancel_sets_interrupt_and_trips_token() {
        let lifecycle = RequestLifecycle::default();
        let token = lifecycle.token();
        lifecycle.cancel();
        assert_eq!(lifecycle.interrupt(), Some(Interrupt::Cancelled));
        assert!(token.is_cancelled());
    }

    #[test]
    fn mark_stale_sets_interrupt_and_trips_token() {
        let lifecycle = RequestLifecycle::default();
        let token = lifecycle.token();
        lifecycle.mark_stale();
        assert_eq!(lifecycle.interrupt(), Some(Interrupt::Stale));
        assert!(token.is_cancelled());
    }

    #[test]
    fn cancelled_wins_over_stale_in_either_order() {
        let both_orders = [true, false];
        for cancel_first in both_orders {
            let lifecycle = RequestLifecycle::default();
            if cancel_first {
                lifecycle.cancel();
                lifecycle.mark_stale();
            } else {
                lifecycle.mark_stale();
                lifecycle.cancel();
            }
            assert_eq!(lifecycle.interrupt(), Some(Interrupt::Cancelled));
        }
    }

    #[test]
    fn lifecycle_is_get_or_insert_and_finish_removes() {
        let shared = SessionShared::default();
        let id = RequestId::from(7);
        let first = shared.lifecycle(&id);
        let second = shared.lifecycle(&id);
        assert!(
            Arc::ptr_eq(&first, &second),
            "same id must yield the same lifecycle"
        );
        let finished = shared.finish(&id).expect("registered id finishes");
        assert!(Arc::ptr_eq(&first, &finished));
        assert!(shared.finish(&id).is_none(), "finish is a one-shot remove");
    }

    #[test]
    fn cancel_reports_unknown_ids() {
        let shared = SessionShared::default();
        let id = RequestId::from(1);
        assert!(!shared.cancel(&id), "unknown id");
        let lifecycle = shared.lifecycle(&id);
        assert!(shared.cancel(&id), "registered id");
        assert_eq!(lifecycle.interrupt(), Some(Interrupt::Cancelled));
    }

    #[test]
    fn mark_all_stale_sweeps_every_registered_request() {
        let shared = SessionShared::default();
        let a = shared.lifecycle(&RequestId::from(1));
        let b = shared.lifecycle(&RequestId::from(2));
        shared.mark_all_stale();
        assert_eq!(a.interrupt(), Some(Interrupt::Stale));
        assert_eq!(b.interrupt(), Some(Interrupt::Stale));
    }
}
