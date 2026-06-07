//! Subscriber setup (M5 WP-O1 — adopt `tracing` + `tracing-subscriber`; WP-P1 — JSONL mode).
//!
//! **All** output goes to stderr — stdout is reserved exclusively for the LSP JSON-RPC stream, and
//! any stray byte on stdout corrupts the protocol (CLAUDE.md). The subscriber is composed from three
//! layers, in registration order: the `GDLS_LOG` env-filter (default `info`), the stderr formatter
//! (uptime timer + target, or newline-delimited JSON when `GDLS_LOG_FORMAT=json`), and — when
//! `GDLS_TRACE` is set — the WP-O5 hierarchical-profiler Layer that dumps an indented span hierarchy
//! on close. A `tracing_log::LogTracer` bridge is installed up front so every `log::*` callsite
//! elsewhere in the workspace (e.g. inside `gd_syntax`, `gd_project`, `gd_analyze`) flows into the
//! tracing pipeline without source changes.
//!
//! `GDLS_LOG_FORMAT=json` (WP-P1) switches the fmt layer to newline-delimited JSON and turns on
//! synthetic close events (`FmtSpan::CLOSE`) so each span emits one JSON object on exit carrying the
//! recorded fields (`elapsed_us`, `file_count`, `diagnostics_count`, …). This is the JSONL stream
//! `scripts/summarize-spans.py` consumes to print the per-span percentile table fed into
//! `bench/budget.toml` (WP-P5).
//!
//! Idempotent via [`Once`] — safe to call from each `serve` and repeatedly from tests. A second
//! call returns immediately; it never re-installs the global default subscriber (would panic) or
//! re-runs `LogTracer::init` (returns `SetLoggerError`).

use std::sync::Once;

use tracing_subscriber::{
    fmt, layer::SubscriberExt, registry::LookupSpan, util::SubscriberInitExt, EnvFilter, Layer,
};

use crate::observability;

static INIT: Once = Once::new();

/// Env-var that switches the fmt layer to newline-delimited JSON (WP-P1). Any value other than
/// `json` (or unset) keeps the default human-readable format.
const GDLS_LOG_FORMAT_ENV: &str = "GDLS_LOG_FORMAT";

/// Install the global tracing subscriber. See the module doc for layer composition.
pub fn init() {
    INIT.call_once(|| {
        // Bridge `log::*` → `tracing` so the rest of the workspace's existing log callsites keep
        // emitting through the same pipeline. Errors here are ignored on purpose: the only failure
        // mode is "a global logger is already set" (e.g. a second invocation across crates within
        // one process), which is the same outcome we want — keep going, don't kill init.
        let _ = tracing_log::LogTracer::init();

        let filter = match EnvFilter::try_from_env("GDLS_LOG") {
            Ok(f) => f,
            Err(e) => {
                // A present-but-malformed `GDLS_LOG` directive (a typo'd target, a stray comma)
                // must not be swallowed silently: an operator who set it to chase a bug would
                // otherwise get `info` with no hint the filter was rejected and conclude logging
                // is broken. Mirrors the sibling env parsers (`parse_gdls_trace`, the
                // `bench/budget.toml` loader, `initializationOptions`) that all surface a bad
                // value. Guarded on the var actually being set so an unset environment never
                // warns. This runs BEFORE the subscriber is installed, so the notice goes to
                // stderr directly — never stdout, which is the LSP wire.
                if std::env::var_os("GDLS_LOG").is_some() {
                    eprintln!("gdls: ignoring malformed GDLS_LOG ({e}); falling back to `info`");
                }
                EnvFilter::new("info")
            }
        };
        // WP-O5: `Option<L>` impls `Layer<S>`, so this is a no-op (zero per-span overhead) when
        // `GDLS_TRACE` is unset, and a registered Layer when it is.
        let profiler = observability::profiler_layer_from_env();
        // WP-P1: switch the fmt layer to newline-delimited JSON when `GDLS_LOG_FORMAT=json`. The
        // two formatter types differ enough (`fmt::Layer<S, JsonFields, Format<Json,…>, _>` vs
        // `fmt::Layer<S, DefaultFields, Format<Full,…>, _>`) that one expression can't return both;
        // splitting the install at the dispatch keeps each branch monomorphic so Rust can infer
        // the subscriber type without manual boxing.
        if json_format_requested() {
            tracing_subscriber::registry()
                .with(filter)
                .with(json_fmt_layer())
                .with(profiler)
                .init();
        } else {
            tracing_subscriber::registry()
                .with(filter)
                .with(text_fmt_layer())
                .with(profiler)
                .init();
        }
    });
}

/// Default human-readable fmt layer (uptime timer + target). The shape Phase B shipped with.
fn text_fmt_layer<S>() -> impl Layer<S>
where
    S: tracing::Subscriber + for<'a> LookupSpan<'a>,
{
    fmt::layer()
        .with_writer(std::io::stderr)
        .with_target(true)
        .with_thread_ids(false)
        .with_timer(fmt::time::uptime())
}

/// WP-P1 JSONL fmt layer. `FmtSpan::CLOSE` turns each span's exit into a synthetic event carrying
/// the recorded fields (`elapsed_us`, `file_count`, `diagnostics_count`, …) and the automatic
/// `time.busy`/`time.idle` counters. That's the line `scripts/summarize-spans.py` keys on: one
/// JSON object per span close + one JSON object per `tracing::info!` event. `with_span_list(false)`
/// drops the parent chain to keep each line a single tidy object (the JSONL stream stays one
/// object per line, no array of nested span objects per event).
fn json_fmt_layer<S>() -> impl Layer<S>
where
    S: tracing::Subscriber + for<'a> LookupSpan<'a>,
{
    fmt::layer()
        .with_writer(std::io::stderr)
        .json()
        .with_current_span(true)
        .with_span_list(false)
        .with_span_events(fmt::format::FmtSpan::CLOSE)
        .with_target(true)
}

/// True when `GDLS_LOG_FORMAT=json`. Case-insensitive on the value so `JSON` also works.
fn json_format_requested() -> bool {
    std::env::var(GDLS_LOG_FORMAT_ENV)
        .ok()
        .is_some_and(|v| v.eq_ignore_ascii_case("json"))
}

#[cfg(test)]
mod tests {
    use std::io::Write;
    use std::sync::{Arc, Mutex};

    use tracing::{info, info_span};
    use tracing_subscriber::{fmt, layer::SubscriberExt, EnvFilter};

    /// Capture-into-buffer writer adapter — `tracing_subscriber::fmt::MakeWriter` returns one of
    /// these per event, and each one appends to the shared buffer the test inspects on shutdown.
    #[derive(Clone)]
    struct VecWriter(Arc<Mutex<Vec<u8>>>);

    impl Write for VecWriter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0
                .lock()
                .expect(
                    "invariant: VecWriter buffer mutex is never poisoned — only this test holds it",
                )
                .extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'a> fmt::MakeWriter<'a> for VecWriter {
        type Writer = VecWriter;
        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }

    /// Compose the same subscriber shape `init()` builds — env-filter + fmt::layer — but with a
    /// capture writer and scoped (`set_default`) installation, so this test can assert on the
    /// formatted output without racing with the global subscriber other tests may have installed.
    #[test]
    fn fmt_layer_emits_span_name_and_event_fields() {
        let buf = Arc::new(Mutex::new(Vec::<u8>::new()));
        let writer = VecWriter(buf.clone());
        let filter = EnvFilter::new("debug");
        let fmt_layer = fmt::layer()
            .with_writer(writer)
            .with_target(true)
            .with_ansi(false)
            .with_timer(fmt::time::uptime());
        let subscriber = tracing_subscriber::registry().with(filter).with(fmt_layer);

        tracing::subscriber::with_default(subscriber, || {
            let span = info_span!("handle_request", method = "textDocument/hover", id = 7i64);
            let _enter = span.enter();
            info!(field_a = 1, "marker event inside span");
        });

        let captured = String::from_utf8(
            buf.lock()
                .expect("invariant: VecWriter buffer mutex is never poisoned")
                .clone(),
        )
        .expect("subscriber writes UTF-8");
        assert!(
            captured.contains("handle_request"),
            "expected the span name in output; got: {captured}"
        );
        assert!(
            captured.contains("marker event inside span"),
            "expected the event message; got: {captured}"
        );
        assert!(
            captured.contains("field_a"),
            "expected the event field key; got: {captured}"
        );
    }

    /// WP-P1: with `GDLS_LOG_FORMAT=json`, the fmt layer emits newline-delimited JSON and one
    /// synthetic close event per span carrying its recorded fields (here `elapsed_us`). The test
    /// composes the JSON layer directly (not via `init()`, which would race the global default with
    /// other tests in the binary) and asserts the close-event line is a valid JSON object that
    /// names the closed span and surfaces the recorded `elapsed_us`. This is the exact shape
    /// `scripts/summarize-spans.py` parses.
    #[test]
    fn json_layer_emits_close_event_with_recorded_fields() {
        let buf = Arc::new(Mutex::new(Vec::<u8>::new()));
        let writer = VecWriter(buf.clone());
        let filter = EnvFilter::new("info");
        let json_layer = fmt::layer()
            .with_writer(writer)
            .json()
            .with_current_span(true)
            .with_span_list(false)
            .with_span_events(fmt::format::FmtSpan::CLOSE)
            .with_target(true);
        let subscriber = tracing_subscriber::registry().with(filter).with(json_layer);

        tracing::subscriber::with_default(subscriber, || {
            let span = info_span!(
                "analyze",
                file = "fixture.gd",
                elapsed_us = tracing::field::Empty,
            );
            let _enter = span.enter();
            span.record("elapsed_us", 4242u64);
        });

        let captured = String::from_utf8(
            buf.lock()
                .expect("invariant: VecWriter buffer mutex is never poisoned")
                .clone(),
        )
        .expect("subscriber writes UTF-8");
        // Each event is one line.
        let close_line = captured
            .lines()
            .find(|l| l.contains("\"analyze\""))
            .unwrap_or_else(|| {
                panic!("expected a JSON line naming the analyze span; got: {captured}")
            });
        let value: serde_json::Value = serde_json::from_str(close_line)
            .unwrap_or_else(|e| panic!("close line is not valid JSON: {e}; line={close_line}"));
        let span = &value["span"];
        assert_eq!(
            span["name"].as_str(),
            Some("analyze"),
            "expected span.name=analyze; got: {value}"
        );
        assert_eq!(
            span["elapsed_us"].as_u64(),
            Some(4242),
            "expected span.elapsed_us=4242 (recorded before close); got: {value}"
        );
        assert_eq!(
            span["file"].as_str(),
            Some("fixture.gd"),
            "expected span.file=fixture.gd; got: {value}"
        );
    }

    /// `init()` is idempotent and never panics on second call — the `Once::call_once` plus the
    /// ignored `LogTracer::init` error keep a second `serve()` (or a test running after a prior
    /// test) safe.
    #[test]
    fn init_is_idempotent() {
        super::init();
        super::init();
    }
}
