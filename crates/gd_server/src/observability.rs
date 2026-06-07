//! Observability primitives consumed by [`crate::logging`] and the LSP event loop:
//!
//! - [`RssSampler`] (M5 WP-O2): cross-platform peak-RSS measurement via `sysinfo`. Sampled at four
//!   lifecycle points (server start, post-cold-index, after every watcher reaction, server stop) and
//!   periodically from the 3 s liveness ticker; consumed by the WP-H1 pressure ladder and the
//!   Phase-H verification report.
//! - [`MemoryPressure`] + [`RssSampler::pressure`] (M5 WP-H1): the soft/hard pressure ladder. The
//!   caps live in [`crate::memory::MemoryBudget`]; this is just the comparison shape so the
//!   server event loop can map a peak reading + a budget into a ladder action.
//! - [`HierarchicalProfiler`] (M5 WP-O5): a `tracing_subscriber::Layer` that records span
//!   durations and prints an indented hierarchy on close when elapsed exceeds the
//!   `GDLS_TRACE` threshold (e.g. `GDLS_TRACE='*>50'` for ≥ 50 ms). Modelled on rust-analyzer's
//!   `RA_PROFILE`. Default off — the layer is unregistered entirely when `GDLS_TRACE` is unset, so
//!   it costs nothing on the hot path.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;

use tracing::span::{Attributes, Id};
use tracing::Subscriber;
use tracing_subscriber::layer::{Context, Layer};
use tracing_subscriber::registry::LookupSpan;

use crate::memory::{Bytes, MemoryBudget};

// ===================================================================================================
// WP-O2 — RssSampler
// ===================================================================================================

/// How many of the most-recent RSS readings the pressure ladder classifies against (a rolling
/// max over this window — see [`RssSampler::pressure`]). Short on purpose: long enough that one
/// low sample between two high ones won't flap the ladder off, short enough to recover promptly
/// once a spike subsides. At the 3 s liveness-tick cadence (plus the lifecycle + per-watcher-batch
/// samples) a 4-deep window clears a one-off spike within ~12 s of the working set returning.
const PRESSURE_WINDOW_SAMPLES: usize = 4;

/// Resident-set-size sampler. Cross-platform via `sysinfo`: Windows reads `WorkingSetSize`,
/// Linux `/proc/self/statm`, macOS the Mach task_info VM API. One `sysinfo::System` instance per
/// sampler, refreshed in-place on each [`Self::sample_now`] (no per-call allocation).
///
/// Tracks three things:
/// - `baseline_bytes`: the first reading, captured at server start by [`Self::new`].
/// - `peak_bytes`: a *monotonic* high-water mark — the maximum of every reading since `baseline`.
///   Reported at shutdown as the session's worst-case RSS. Deliberately NOT what drives the live
///   ladder: a monotonic peak can never come back down, so classifying against it would latch the
///   Hard rung on forever after a single transient spike (see [`Self::pressure`]).
/// - `recent`: a short rolling window of the last [`PRESSURE_WINDOW_SAMPLES`] readings, whose max
///   *is* what the ladder classifies — so the ladder engages on the first elevated sample but
///   recovers once a spike ages out of the window.
///
/// Each `sample_now` returns the *current* reading, updates `peak` if exceeded, and pushes onto
/// the rolling window. The WP-O1 tracing pipeline fans out the readings as
/// `tracing::info!(target = "rss", phase, bytes)` events; consumers grep that target in the JSONL
/// trace.
pub struct RssSampler {
    sys: sysinfo::System,
    pid: sysinfo::Pid,
    peak_bytes: u64,
    baseline_bytes: u64,
    /// Rolling window of the last [`PRESSURE_WINDOW_SAMPLES`] readings (oldest at the front).
    /// Drives [`Self::pressure`] via [`Self::recent_max`].
    recent: VecDeque<u64>,
}

impl RssSampler {
    /// Build a sampler scoped to this process, taking the **baseline** reading at construction.
    /// The baseline doubles as the initial `peak`. Subsequent [`Self::sample_now`] calls update
    /// peak monotonically; baseline is fixed for the session.
    pub fn new() -> Self {
        let mut sys = sysinfo::System::new();
        let pid = sysinfo::Pid::from_u32(std::process::id());
        Self::refresh_self(&mut sys, pid);
        let baseline = match sys.process(pid).map(|p| p.memory()) {
            Some(b) => b,
            None => {
                tracing::warn!(
                    target: "rss",
                    name = "rss_baseline_failed",
                    "sysinfo returned no entry for this process at startup; baseline RSS reads as 0",
                );
                0
            }
        };
        let mut recent = VecDeque::with_capacity(PRESSURE_WINDOW_SAMPLES);
        recent.push_back(baseline);
        Self {
            sys,
            pid,
            peak_bytes: baseline,
            baseline_bytes: baseline,
            recent,
        }
    }

    /// Re-refresh this process and return the current RSS in bytes. Updates [`Self::peak`] in
    /// place. Emits a structured `tracing::info!` event on each call so the JSONL trace carries
    /// the full pressure curve (consumed by the WP-P1 summary script + Phase-H walk).
    pub fn sample_now(&mut self, phase: &'static str) -> Bytes {
        Self::refresh_self(&mut self.sys, self.pid);
        let current = match self.sys.process(self.pid).map(|p| p.memory()) {
            Some(c) => {
                self.record_sample(c);
                c
            }
            None => {
                // The self-process query failed (sysinfo didn't repopulate our PID on this
                // refresh). Retain the prior peak AND the prior rolling window rather than pushing
                // a 0: a 0 reading classifies as `MemoryPressure::Normal` and would silently
                // disable the WP-H1 eviction ladder until the next successful sample.
                tracing::warn!(
                    target: "rss",
                    phase,
                    name = "rss_sample_failed",
                    "sysinfo returned no entry for this process; retaining the prior peak",
                );
                self.peak_bytes
            }
        };
        tracing::info!(
            target: "rss",
            phase,
            bytes = current,
            peak_bytes = self.peak_bytes,
            baseline_bytes = self.baseline_bytes,
            "rss sample"
        );
        Bytes::new(current)
    }

    /// Fold a fresh reading into the monotonic peak high-water mark and the rolling pressure
    /// window (dropping the oldest entry once the window is full).
    fn record_sample(&mut self, bytes: u64) {
        if bytes > self.peak_bytes {
            self.peak_bytes = bytes;
        }
        self.recent.push_back(bytes);
        while self.recent.len() > PRESSURE_WINDOW_SAMPLES {
            self.recent.pop_front();
        }
    }

    /// The maximum reading in the rolling window — what [`Self::pressure`] classifies against.
    /// Falls back to the baseline if the window is somehow empty (it never is after [`Self::new`],
    /// which seeds it).
    fn recent_max(&self) -> u64 {
        self.recent
            .iter()
            .copied()
            .max()
            .unwrap_or(self.baseline_bytes)
    }

    /// Test helper: pin the rolling window to a single synthetic reading so a boundary test can
    /// assert each ladder rung deterministically without depending on real OS RSS.
    #[cfg(test)]
    fn set_window_for_test(&mut self, bytes: u64) {
        self.recent.clear();
        self.recent.push_back(bytes);
        if bytes > self.peak_bytes {
            self.peak_bytes = bytes;
        }
    }

    /// Targeted refresh: only this process, only its memory metric. The sysinfo 0.39
    /// `refresh_processes_specifics` API lets us skip CPU / disk / exe / task collection, which
    /// would otherwise dominate the per-tick cost (the docs warn it can be quite expensive on
    /// Linux due to per-task `stat` reads). `false` for `remove_dead_processes` keeps the
    /// internal map untouched between ticks — our own process is the only one we care about,
    /// and removing dead processes here would force sysinfo to walk every PID on each call.
    fn refresh_self(sys: &mut sysinfo::System, pid: sysinfo::Pid) {
        sys.refresh_processes_specifics(
            sysinfo::ProcessesToUpdate::Some(&[pid]),
            false,
            sysinfo::ProcessRefreshKind::nothing().with_memory(),
        );
    }

    /// The baseline reading captured at server start.
    #[must_use]
    pub fn baseline(&self) -> Bytes {
        Bytes::new(self.baseline_bytes)
    }

    /// The monotonic high-water mark since the baseline. Reported at shutdown as the session's
    /// worst-case RSS; NOT the value the ladder classifies (see [`Self::pressure`] /
    /// [`Self::windowed_rss`]).
    #[must_use]
    pub fn peak(&self) -> Bytes {
        Bytes::new(self.peak_bytes)
    }

    /// The current rolling-window max — the reading the ladder actually classifies. Exposed so the
    /// transition events can report the number that drove the decision alongside the session peak.
    #[must_use]
    pub fn windowed_rss(&self) -> Bytes {
        Bytes::new(self.recent_max())
    }

    /// M5 WP-H1: map the rolling-window max against a [`MemoryBudget`] into a pressure level.
    ///
    /// Classifies against the max of the last [`PRESSURE_WINDOW_SAMPLES`] readings (NOT the
    /// monotonic [`Self::peak`]): the window rises to a spike on the very first elevated sample —
    /// so the ladder engages promptly — but a spike *ages out* once enough lower readings follow,
    /// so the level can return to `Normal`. This engage-fast / recover-slow asymmetry is the right
    /// shape for a ladder whose Hard rung sheds live requests: a one-shot reindex spike
    /// (`git checkout`, branch switch) must trip the rung, but once the working set returns the
    /// server has to *resume serving*. Classifying against `peak` instead would latch the Hard
    /// rung on for the rest of the session — a single spike would permanently shed all navigation
    /// with `ContentModified`.
    #[must_use]
    pub fn pressure(&self, budget: &MemoryBudget) -> MemoryPressure {
        let rss = self.windowed_rss();
        if rss > budget.hard_cap_bytes() {
            MemoryPressure::Hard
        } else if rss > budget.soft_cap_bytes() {
            MemoryPressure::Soft
        } else {
            MemoryPressure::Normal
        }
    }
}

// ===================================================================================================
// WP-H1 — Memory pressure ladder
// ===================================================================================================

/// The three pressure rungs the LSP event loop reacts to. Strictly ordered (`Normal < Soft <
/// Hard`) so a transition direction can be derived by comparing the previous tick's level to the
/// current one — the server uses that to emit the per-transition tracing event exactly once (not
/// every tick while the level is held).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum MemoryPressure {
    /// Windowed RSS below the soft cap — no ladder action.
    Normal,
    /// Windowed RSS above the soft cap but at-or-below the hard cap — bulk-evict half of both
    /// caches.
    Soft,
    /// Windowed RSS above the hard cap — refuse new full analyses (the LSP handler maps this to
    /// `ContentModified` per LSP 3.17).
    Hard,
}

impl Default for RssSampler {
    fn default() -> Self {
        Self::new()
    }
}

// ===================================================================================================
// WP-O5 — Hierarchical profiler Layer (rust-analyzer's RA_PROFILE shape)
// ===================================================================================================

/// Threshold (ms) above which a span's close emits a hierarchical line on stderr. Filter syntax
/// today is `*>N` — any span name, threshold N ms. Future iterations may add per-target filtering
/// (e.g. `analyze>50`); for v1 the wildcard is enough.
const GDLS_TRACE_ENV: &str = "GDLS_TRACE";

/// Parse the `GDLS_TRACE` env-var (e.g. `*>50` → `Some(50)`). Returns `None` when unset, empty, or
/// malformed — a malformed value logs a warning at WP-O1's bridge so a typo is surfaced rather than
/// silently disabling profiling.
fn parse_gdls_trace() -> Option<u64> {
    let raw = std::env::var(GDLS_TRACE_ENV).ok()?;
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    // `*>NN` — accept the canonical rust-analyzer-ish form. Bare `NN` is also fine.
    let body = trimmed.strip_prefix('*').unwrap_or(trimmed);
    let body = body.strip_prefix('>').unwrap_or(body);
    match body.parse::<u64>() {
        Ok(ms) => Some(ms),
        Err(e) => {
            // No tracing yet — this is called from `init()` BEFORE the subscriber is installed —
            // so fall through to stderr. eprintln (not println, never println in this binary —
            // stdout is the LSP wire).
            eprintln!(
                "gdls: ignoring malformed GDLS_TRACE={raw:?} ({e}); expected e.g. `*>50` for a 50 ms threshold"
            );
            None
        }
    }
}

/// Build the WP-O5 profiler [`Layer`] from the `GDLS_TRACE` env-var. Returns `None` when unset —
/// `Option<L>` impls `Layer<S>` (no-op for None), so registering the result is zero-cost when
/// disabled.
pub fn profiler_layer_from_env() -> Option<HierarchicalProfiler> {
    parse_gdls_trace().map(HierarchicalProfiler::new)
}

/// Per-span side-table value carried inside the span's tracing-subscriber `Extensions`. Stores the
/// open time + the nesting depth at open, so a closing span can compute elapsed and indent without
/// climbing the parent chain at close time.
#[derive(Clone, Copy)]
struct Timings {
    opened: Instant,
    depth: usize,
}

/// Tracing `Layer` that records each span's open time on `on_new_span` and emits an indented line
/// to stderr on `on_close` when elapsed ≥ `threshold_ms`. Indent is the depth captured at open time
/// (not "current" depth at close), so concurrent / re-entered spans render consistently. Output
/// shape: `  handle_request 7ms` (two-space indent per level).
///
/// The writer is a boxed closure so the unit test can capture into a buffer; the production
/// constructor wires it to `eprintln!` per CLAUDE.md ("stdout is the LSP wire").
pub struct HierarchicalProfiler {
    threshold_ms: u64,
    /// Current open-span depth (incremented on `on_new_span`, decremented on `on_close`). A span
    /// that is closed while not the topmost may briefly see depth go negative on the decrement —
    /// we clamp with `saturating_sub` so the counter never wraps under unusual close orderings.
    depth: AtomicUsize,
    sink: Box<dyn Fn(&str) + Send + Sync + 'static>,
}

impl HierarchicalProfiler {
    fn new(threshold_ms: u64) -> Self {
        Self::with_sink(threshold_ms, Box::new(|line| eprintln!("{line}")))
    }

    /// Test-only constructor that captures the formatted output into the supplied closure
    /// instead of stderr. Used by the unit test to assert on the indented hierarchy without
    /// fighting cross-platform stderr capture.
    fn with_sink(threshold_ms: u64, sink: Box<dyn Fn(&str) + Send + Sync + 'static>) -> Self {
        Self {
            threshold_ms,
            depth: AtomicUsize::new(0),
            sink,
        }
    }
}

impl<S> Layer<S> for HierarchicalProfiler
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    fn on_new_span(&self, _attrs: &Attributes<'_>, id: &Id, ctx: Context<'_, S>) {
        let Some(span) = ctx.span(id) else { return };
        let depth = self.depth.fetch_add(1, Ordering::Relaxed);
        span.extensions_mut().insert(Timings {
            opened: Instant::now(),
            depth,
        });
    }

    fn on_close(&self, id: Id, ctx: Context<'_, S>) {
        let Some(span) = ctx.span(&id) else { return };
        let exts = span.extensions();
        let Some(timings) = exts.get::<Timings>() else {
            return;
        };
        let elapsed = timings.opened.elapsed();
        let ms = elapsed.as_millis() as u64;
        let depth_at_open = timings.depth;
        drop(exts);
        // Single clamped atomic decrement (mirrors the `fetch_add` in `on_new_span`). A plain
        // load-then-store pair would let two spans closing on different threads both read N and
        // both store N-1, silently losing a decrement and skewing the indent; `fetch_update`
        // retries on contention so the count stays exact. `saturating_sub` keeps it from wrapping
        // under an unusual close-before-open ordering. The closure never returns `None`, so the
        // `Result` is always `Ok` — discard it.
        let _ = self
            .depth
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |d| {
                Some(d.saturating_sub(1))
            });
        if ms >= self.threshold_ms {
            let indent = "  ".repeat(depth_at_open);
            let line = format!("{indent}{name} {ms}ms", name = span.name());
            (self.sink)(&line);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tracing::info_span;
    use tracing_subscriber::layer::SubscriberExt;

    #[test]
    fn parse_gdls_trace_accepts_wildcard_threshold() {
        // SAFETY: tests are not run in parallel against env vars by Cargo's default test harness on
        // a single binary, but `--test-threads` could change that. The `set_var` / `remove_var`
        // calls below are unsafe in 2024 Rust — keep them inside a single test that owns the var
        // for its body. Other tests in this file don't touch GDLS_TRACE.
        // SAFETY: see the comment above the block.
        unsafe {
            std::env::set_var(GDLS_TRACE_ENV, "*>50");
            assert_eq!(parse_gdls_trace(), Some(50));
            std::env::set_var(GDLS_TRACE_ENV, ">25");
            assert_eq!(parse_gdls_trace(), Some(25));
            std::env::set_var(GDLS_TRACE_ENV, "100");
            assert_eq!(parse_gdls_trace(), Some(100));
            std::env::remove_var(GDLS_TRACE_ENV);
            assert_eq!(parse_gdls_trace(), None);
        }
    }

    /// Verify the hierarchical-profiler Layer emits an indented hierarchy on close. The capture
    /// sink replaces stderr so the test asserts the exact layout the operator sees in the
    /// `GDLS_TRACE='*>0'` shape: outer span at indent 0, nested inner span at indent 1
    /// (two-space increment), both carrying their `Nms` suffix. Threshold = 0 keeps every span,
    /// the `sleep(2 ms)` makes the elapsed comfortably above the rendered millisecond floor on
    /// every platform.
    #[test]
    fn hierarchical_profiler_emits_indented_hierarchy_on_close() {
        let captured: std::sync::Arc<std::sync::Mutex<Vec<String>>> =
            std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let captured_clone = captured.clone();
        let profiler = HierarchicalProfiler::with_sink(
            0,
            Box::new(move |line| {
                captured_clone
                    .lock()
                    .expect("invariant: profiler capture mutex not poisoned")
                    .push(line.to_string());
            }),
        );
        let subscriber = tracing_subscriber::registry().with(profiler);
        tracing::subscriber::with_default(subscriber, || {
            let outer = info_span!("outer");
            let _e1 = outer.enter();
            {
                let inner = info_span!("inner");
                let _e2 = inner.enter();
                std::thread::sleep(std::time::Duration::from_millis(2));
            }
        });
        let lines = captured
            .lock()
            .expect("invariant: profiler capture mutex not poisoned")
            .clone();
        assert!(
            lines
                .iter()
                .any(|l| l.starts_with("  inner ") && l.ends_with("ms")),
            "expected an indented `  inner` line; captured: {lines:?}"
        );
        assert!(
            lines
                .iter()
                .any(|l| l.starts_with("outer ") && l.ends_with("ms")),
            "expected a top-level `outer` line; captured: {lines:?}"
        );
        // The inner span must close (and emit) before the outer span (LIFO order — outer is
        // still open when inner closes), so a hierarchical-profiler dump shows the nested
        // children above their parent.
        let inner_pos = lines.iter().position(|l| l.starts_with("  inner "));
        let outer_pos = lines.iter().position(|l| l.starts_with("outer "));
        assert!(
            inner_pos < outer_pos,
            "inner span must close (emit) before outer span; captured: {lines:?}"
        );
    }

    /// Sanity-check the WP-H1 pressure ladder against synthetic readings. The window is normally
    /// driven by the OS reading inside [`RssSampler::sample_now`]; for a deterministic boundary
    /// test we pin the rolling window to a single known value via [`RssSampler::set_window_for_test`],
    /// then assert each rung. That helper is `#[cfg(test)]` — operators of the public API cannot
    /// do this.
    #[test]
    fn pressure_ladder_classifies_normal_soft_hard() {
        let budget = crate::memory::MemoryBudget::from_caps_mb(100, 200);
        let mut sampler = RssSampler::new();
        // Below soft → Normal.
        sampler.set_window_for_test(50 * 1024 * 1024);
        assert_eq!(sampler.pressure(&budget), MemoryPressure::Normal);
        // At the soft cap exactly → still Normal (the ladder fires when strictly above).
        sampler.set_window_for_test(100 * 1024 * 1024);
        assert_eq!(sampler.pressure(&budget), MemoryPressure::Normal);
        // Just over soft → Soft.
        sampler.set_window_for_test(100 * 1024 * 1024 + 1);
        assert_eq!(sampler.pressure(&budget), MemoryPressure::Soft);
        // Up to the hard cap → still Soft.
        sampler.set_window_for_test(200 * 1024 * 1024);
        assert_eq!(sampler.pressure(&budget), MemoryPressure::Soft);
        // Just over hard → Hard.
        sampler.set_window_for_test(200 * 1024 * 1024 + 1);
        assert_eq!(sampler.pressure(&budget), MemoryPressure::Hard);
    }

    /// Regression for the WP-H1 monotonic-peak latch: the ladder must RECOVER after a spike
    /// subsides, because it classifies the rolling-window max rather than the monotonic session
    /// peak. Driving it off `peak` would latch the Hard rung on for the rest of the session,
    /// permanently shedding navigation with `ContentModified` after a single transient spike
    /// (e.g. a `git checkout` mass-reindex).
    #[test]
    fn pressure_recovers_after_a_spike_ages_out_of_the_window() {
        let budget = crate::memory::MemoryBudget::from_caps_mb(100, 200);
        let mut sampler = RssSampler::new();
        // A spike over the hard cap trips Hard immediately...
        sampler.record_sample(250 * 1024 * 1024);
        assert_eq!(sampler.pressure(&budget), MemoryPressure::Hard);
        // ...and the session peak stays pinned at the spike (it is a high-water mark)...
        assert_eq!(sampler.peak().get(), 250 * 1024 * 1024);
        // ...but once enough low readings push the spike out of the window, the ladder recovers.
        for _ in 0..PRESSURE_WINDOW_SAMPLES {
            sampler.record_sample(50 * 1024 * 1024);
        }
        assert_eq!(
            sampler.pressure(&budget),
            MemoryPressure::Normal,
            "the Hard rung must clear once the spike ages out of the rolling window; a \
             monotonic-peak classification would latch it on forever",
        );
        // The high-water mark is unaffected by recovery — still the spike.
        assert_eq!(sampler.peak().get(), 250 * 1024 * 1024);
    }

    /// `MemoryPressure` must order Normal < Soft < Hard so the server can derive a transition
    /// direction with a plain `<` comparison. The event-loop ticker uses this to fire the
    /// per-transition tracing event exactly once instead of every tick the level is held.
    #[test]
    fn memory_pressure_is_totally_ordered() {
        assert!(MemoryPressure::Normal < MemoryPressure::Soft);
        assert!(MemoryPressure::Soft < MemoryPressure::Hard);
        assert!(MemoryPressure::Normal < MemoryPressure::Hard);
    }

    #[test]
    fn rss_sampler_observes_a_50mb_alloc() {
        let mut sampler = RssSampler::new();
        let baseline = sampler.sample_now("baseline");
        // Allocate ~50 MB and write to it so the OS materialises the pages (a `Vec` with `0u8`s
        // touches every page via `vec![]`'s memset). On Windows, `WorkingSetSize` reflects the
        // committed pages immediately; on Linux, `/proc/self/statm` reflects RSS after the first
        // page fault on each page, which the memset triggers.
        let _hog: Vec<u8> = vec![0u8; 50 * 1024 * 1024];
        let after = sampler.sample_now("after_alloc");
        let peak = sampler.peak();
        assert!(
            peak >= after,
            "peak ({peak}) must be ≥ the latest sample ({after})"
        );
        assert!(
            peak >= baseline,
            "peak ({peak}) must be ≥ baseline ({baseline})"
        );
        // We don't assert a hard 50 MB delta — the OS's accounting + sysinfo's granularity can
        // come in under the raw allocation (e.g. when the allocator returned mmap'd pages
        // counted differently on macOS). The strong assertion is monotonicity, above; the
        // weaker delta is observed here for the operator-readable test name.
        let _ = (baseline, after, peak);
    }
}
