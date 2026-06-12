//! M7 (#58) — `workDoneProgress`: server-initiated progress for the long lifecycle phases
//! (cold index, warm start, reconcile, mid-session re-index) and client-token progress on the
//! genuinely long requests (`references`, `workspace/symbol`).
//!
//! One [`ProgressReporter`] owns one progress token for its whole begin → report* → end arc.
//! Server-initiated reporters send `window/workDoneProgress/create` first and **only when the
//! client advertised `window.workDoneProgress`** — without the capability the spec forbids the
//! create, so the disabled reporter is a total no-op. The create's response is correlated by
//! the router (`crate::router::SessionShared`), which poisons the reporter on an error reply;
//! we deliberately do not wait for the reply (the rust-analyzer convention), so a `begin` can
//! already be on the wire when a rejection arrives — spec-tolerated, and everything after the
//! poison is suppressed.
//!
//! Reports are throttled (≥ [`MIN_REPORT_INTERVAL`] apart) so a per-file loop over thousands of
//! files doesn't flood the wire; `begin` and `end` are never throttled, and a `Drop` guard
//! auto-ends an abandoned arc so no client spinner is left stuck (an orphaned token is the
//! one protocol sin progress can commit).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crossbeam_channel::Sender;
use lsp_server::Message;
use lsp_types::{
    ProgressParams, ProgressParamsValue, ProgressToken, WorkDoneProgress, WorkDoneProgressBegin,
    WorkDoneProgressCreateParams, WorkDoneProgressEnd, WorkDoneProgressReport,
};

use crate::router::SessionShared;

/// Minimum spacing between `report` notifications. 100 ms ≈ a spinner's repaint rate; anything
/// faster is wire noise the client coalesces anyway.
const MIN_REPORT_INTERVAL: Duration = Duration::from_millis(100);

/// One progress token's begin → report* → end arc. Construct via [`Self::server_initiated`] or
/// [`Self::for_client_token`]; every method on a disabled reporter is a no-op.
pub(crate) struct ProgressReporter {
    /// `None` ⇒ disabled (capability absent): no create was sent and nothing else ever is.
    sender: Option<Sender<Message>>,
    token: ProgressToken,
    /// Set by the router when the client answers the create with an error (server-initiated
    /// only); suppresses everything after the flip.
    poisoned: Option<Arc<AtomicBool>>,
    begun: bool,
    ended: bool,
    last_report: Option<Instant>,
}

impl ProgressReporter {
    /// Server-initiated progress. When `supported` is false (the client did not advertise
    /// `window.workDoneProgress`) the reporter is disabled and **nothing is ever sent — not even
    /// the create**. Otherwise sends `window/workDoneProgress/create` and registers its id with
    /// the router for error-reply poisoning, without waiting for the reply.
    pub(crate) fn server_initiated(
        sender: Sender<Message>,
        shared: &SessionShared,
        supported: bool,
    ) -> Self {
        if !supported {
            return Self::disabled();
        }
        let id = shared.next_outgoing_id();
        let token = ProgressToken::String(format!("gdls/progress/{id}"));
        let poisoned = shared.register_outgoing_create(id.clone());
        let create = lsp_server::Request {
            id,
            method: "window/workDoneProgress/create".to_string(),
            params: serde_json::to_value(WorkDoneProgressCreateParams {
                token: token.clone(),
            })
            .expect("invariant: WorkDoneProgressCreateParams always serializes"),
        };
        if sender.send(Message::Request(create)).is_err() {
            // Client gone mid-session; the loop will notice on its own — degrade to disabled.
            return Self::disabled();
        }
        ProgressReporter {
            sender: Some(sender),
            token,
            poisoned: Some(poisoned),
            begun: false,
            ended: false,
            last_report: None,
        }
    }

    /// Progress bound to a client-supplied `workDoneToken` from a request's
    /// `WorkDoneProgressParams`. No create request and no capability gate — the token's
    /// presence in the request IS the client's opt-in (LSP 3.17 §workDoneProgress).
    pub(crate) fn for_client_token(sender: Sender<Message>, token: ProgressToken) -> Self {
        ProgressReporter {
            sender: Some(sender),
            token,
            poisoned: None,
            begun: false,
            ended: false,
            last_report: None,
        }
    }

    fn disabled() -> Self {
        ProgressReporter {
            sender: None,
            token: ProgressToken::Number(0),
            poisoned: None,
            begun: false,
            ended: false,
            last_report: None,
        }
    }

    fn is_poisoned(&self) -> bool {
        self.poisoned
            .as_ref()
            .is_some_and(|flag| flag.load(Ordering::Acquire))
    }

    fn send(&self, value: WorkDoneProgress) {
        let Some(sender) = &self.sender else { return };
        let params = ProgressParams {
            token: self.token.clone(),
            value: ProgressParamsValue::WorkDone(value),
        };
        let note = lsp_server::Notification {
            method: "$/progress".to_string(),
            params: serde_json::to_value(params)
                .expect("invariant: ProgressParams always serializes"),
        };
        // A send failure means the client hung up; the event loop notices independently.
        let _ = sender.send(Message::Notification(note));
    }

    /// Open the arc. A second `begin`, or one after `end`/poisoning, is a no-op.
    pub(crate) fn begin(&mut self, title: &str, message: Option<&str>) {
        if self.sender.is_none() || self.begun || self.ended || self.is_poisoned() {
            return;
        }
        self.begun = true;
        self.send(WorkDoneProgress::Begin(WorkDoneProgressBegin {
            title: title.to_string(),
            cancellable: Some(false),
            message: message.map(str::to_string),
            percentage: None,
        }));
    }

    /// Mid-arc report, throttled to [`MIN_REPORT_INTERVAL`]. No-op before `begin`, after `end`,
    /// or when poisoned. `percentage` is 0–100 when the total is known.
    pub(crate) fn report(&mut self, message: Option<&str>, percentage: Option<u32>) {
        if !self.begun || self.ended || self.is_poisoned() {
            return;
        }
        if self
            .last_report
            .is_some_and(|at| at.elapsed() < MIN_REPORT_INTERVAL)
        {
            return;
        }
        self.last_report = Some(Instant::now());
        self.send(WorkDoneProgress::Report(WorkDoneProgressReport {
            cancellable: Some(false),
            message: message.map(str::to_string),
            percentage,
        }));
    }

    /// Close the arc. Idempotent; never throttled (a stuck client spinner is worse than any
    /// extra notification).
    pub(crate) fn end(&mut self, message: Option<&str>) {
        if !self.begun || self.ended {
            return;
        }
        self.ended = true;
        if self.is_poisoned() {
            return;
        }
        self.send(WorkDoneProgress::End(WorkDoneProgressEnd {
            message: message.map(str::to_string),
        }));
    }
}

/// Early returns and panics must not orphan a begun token (the client's spinner would spin
/// forever) — the guard closes any arc the owner forgot to.
impl Drop for ProgressReporter {
    fn drop(&mut self) {
        self.end(None);
    }
}

/// What the long-running workspace phases (`Workspace::load`, reconcile) report into, keeping
/// them decoupled from the LSP wire types. `done`/`total` describe the phase's own unit of work
/// (files, usually); `total: None` means indeterminate (message-only spinner).
pub(crate) trait ProgressSink {
    fn progress(&mut self, done: usize, total: Option<usize>, message: &str);
}

/// The default sink for callers with no progress to show (tests, `gdls diagnose`).
pub(crate) struct NoopSink;

impl ProgressSink for NoopSink {
    fn progress(&mut self, _done: usize, _total: Option<usize>, _message: &str) {}
}

impl ProgressSink for ProgressReporter {
    fn progress(&mut self, done: usize, total: Option<usize>, message: &str) {
        let percentage = total
            .filter(|t| *t > 0)
            .map(|t| ((done.saturating_mul(100) / t).min(100)) as u32);
        let detail = match total {
            Some(t) => format!("{message} ({done}/{t})"),
            None => format!("{message} ({done})"),
        };
        self.report(Some(&detail), percentage);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossbeam_channel::Receiver;

    fn rig(supported: bool) -> (ProgressReporter, Receiver<Message>, Arc<SessionShared>) {
        let shared = Arc::new(SessionShared::default());
        let (tx, rx) = crossbeam_channel::unbounded();
        let reporter = ProgressReporter::server_initiated(tx, &shared, supported);
        (reporter, rx, shared)
    }

    fn drain(rx: &Receiver<Message>) -> Vec<Message> {
        rx.try_iter().collect()
    }

    fn kinds(messages: &[Message]) -> Vec<String> {
        messages
            .iter()
            .map(|m| match m {
                Message::Request(r) => r.method.clone(),
                Message::Notification(n) => {
                    let params: ProgressParams = serde_json::from_value(n.params.clone()).unwrap();
                    match params.value {
                        ProgressParamsValue::WorkDone(WorkDoneProgress::Begin(_)) => "begin".into(),
                        ProgressParamsValue::WorkDone(WorkDoneProgress::Report(_)) => {
                            "report".into()
                        }
                        ProgressParamsValue::WorkDone(WorkDoneProgress::End(_)) => "end".into(),
                    }
                }
                Message::Response(_) => "response".into(),
            })
            .collect()
    }

    #[test]
    fn unsupported_client_gets_nothing_not_even_the_create() {
        let (mut reporter, rx, _shared) = rig(false);
        reporter.begin("work", None);
        reporter.report(Some("step"), Some(50));
        reporter.end(Some("done"));
        assert!(drain(&rx).is_empty(), "no create, no $/progress — ever");
    }

    #[test]
    fn supported_client_gets_create_then_paired_begin_end() {
        let (mut reporter, rx, _shared) = rig(true);
        reporter.begin("Indexing project", None);
        reporter.end(Some("done"));
        assert_eq!(
            kinds(&drain(&rx)),
            vec!["window/workDoneProgress/create", "begin", "end"]
        );
    }

    #[test]
    fn throttle_suppresses_rapid_reports_but_never_end() {
        let (mut reporter, rx, _shared) = rig(true);
        reporter.begin("work", None);
        reporter.report(Some("1"), None);
        reporter.report(Some("2"), None); // inside the interval — dropped
        reporter.end(None); // immediately after — must still go out
        assert_eq!(
            kinds(&drain(&rx)),
            vec!["window/workDoneProgress/create", "begin", "report", "end"]
        );
    }

    #[test]
    fn report_before_begin_and_after_end_are_noops() {
        let (mut reporter, rx, _shared) = rig(true);
        reporter.report(Some("early"), None);
        reporter.begin("work", None);
        reporter.end(None);
        reporter.report(Some("late"), None);
        reporter.end(None); // idempotent
        assert_eq!(
            kinds(&drain(&rx)),
            vec!["window/workDoneProgress/create", "begin", "end"]
        );
    }

    #[test]
    fn drop_guard_auto_ends_a_begun_arc() {
        let (mut reporter, rx, _shared) = rig(true);
        reporter.begin("work", None);
        drop(reporter);
        assert_eq!(
            kinds(&drain(&rx)),
            vec!["window/workDoneProgress/create", "begin", "end"]
        );
    }

    #[test]
    fn poisoned_reporter_goes_silent() {
        let (mut reporter, rx, _shared) = rig(true);
        reporter
            .poisoned
            .as_ref()
            .expect("server-initiated reporter has a poison flag")
            .store(true, Ordering::Release);
        reporter.begin("work", None);
        reporter.report(Some("step"), None);
        reporter.end(None);
        assert_eq!(
            kinds(&drain(&rx)),
            vec!["window/workDoneProgress/create"],
            "everything after the poison is suppressed"
        );
    }

    #[test]
    fn client_token_reporter_sends_no_create_and_uses_the_client_token() {
        let (tx, rx) = crossbeam_channel::unbounded();
        let token = ProgressToken::String("tok-1".to_string());
        let mut reporter = ProgressReporter::for_client_token(tx, token.clone());
        reporter.begin("References", None);
        reporter.end(None);
        let messages = drain(&rx);
        assert_eq!(kinds(&messages), vec!["begin", "end"]);
        for m in &messages {
            let Message::Notification(n) = m else {
                panic!("client-token progress is notifications only")
            };
            let params: ProgressParams = serde_json::from_value(n.params.clone()).unwrap();
            assert_eq!(params.token, token);
        }
    }

    #[test]
    fn sink_adapter_renders_percentage_and_counts() {
        let (mut reporter, rx, _shared) = rig(true);
        reporter.begin("Indexing project", None);
        ProgressSink::progress(&mut reporter, 50, Some(200), "parsing scripts");
        let messages = drain(&rx);
        let report = messages
            .iter()
            .find_map(|m| {
                let Message::Notification(n) = m else {
                    return None;
                };
                let params: ProgressParams = serde_json::from_value(n.params.clone()).ok()?;
                match params.value {
                    ProgressParamsValue::WorkDone(WorkDoneProgress::Report(r)) => Some(r),
                    _ => None,
                }
            })
            .expect("a report notification");
        assert_eq!(report.percentage, Some(25));
        assert_eq!(report.message.as_deref(), Some("parsing scripts (50/200)"));
    }
}
