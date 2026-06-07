//! WP-P3 (M5) bench `--record` / `--replay` reproducer.
//!
//! Captures the last N JSON-RPC requests + open-buffer rope snapshots into a JSON artifact so a
//! local-bench or Phase H WP-Q2 regression is reproducible in isolation. The complementary
//! [`replay`] driver feeds the artifact back through a fresh in-memory server and times each
//! request, printing a `method,request_id,elapsed_us,timed_out` CSV (and returning the same
//! metrics for programmatic consumers).
//!
//! **Recording is opt-in**: [`BenchRecorder::from_env`] returns `Some` only when
//! `GDLS_BENCH_RECORD_TO=<path>` is set, so the production LSP run-path pays no cost. Tests inject
//! a recorder directly via [`crate::serve_with_recorder`] to avoid mutating the global env.
//!
//! **No new dependency**: per the M5 plan §3, the artifact format reuses `serde_json` only — the
//! `text` field on each [`BufferSnapshot`] is a plain JSON string (UTF-8 buffer contents
//! round-trip through `serde_json` without escaping pain), not base64.
//!
//! **Ring-buffer eviction**: [`BenchRecorder::record`] evicts the oldest entry when capacity is
//! reached, so a long-running session keeps only the most recent N entries (default `64`).
//! Configurable via [`BenchRecorder::with_capacity`].

use std::collections::VecDeque;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use crossbeam_channel::RecvTimeoutError;
use lsp_server::{Connection, Message, Notification, Request, RequestId, Response};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::vfs::Vfs;

/// Default ring-buffer size for the request trace. Matches the M5 plan's default `N=64`.
pub const DEFAULT_TRACE_CAPACITY: usize = 64;

/// Artifact-format schema version. Bump on any breaking change to the JSON shape.
pub const ARTIFACT_VERSION: u32 = 1;

/// Environment variable that, when set, makes [`crate::serve`] construct a [`BenchRecorder`] and
/// flush on shutdown.
pub const ENV_RECORD_TO: &str = "GDLS_BENCH_RECORD_TO";

/// Environment variable for overriding the ring-buffer capacity (positive integer); unset ⇒
/// [`DEFAULT_TRACE_CAPACITY`].
pub const ENV_RECORD_CAPACITY: &str = "GDLS_BENCH_RECORD_CAPACITY";

/// Best-effort snapshot of `rustc` / target / OS / package versions; recorded so a regression
/// artifact knows which build produced it. Every field is a string so unknown values can degrade
/// to `"unknown"` without changing the schema.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct BenchEnv {
    pub rustc_version: String,
    pub target_triple: String,
    pub parser_version: String,
    pub analyzer_version: String,
    pub os: String,
    pub cpu_model: String,
}

impl BenchEnv {
    /// Capture the runtime environment. `rustc_version` / `cpu_model` aren't surfaced through the
    /// stdlib so they default to `"unknown"` unless `RUSTC_VERSION` / `GDLS_BENCH_CPU_MODEL` are
    /// exposed in the calling shell.
    pub fn capture() -> Self {
        Self {
            rustc_version: option_env!("RUSTC_VERSION")
                .map(str::to_string)
                .unwrap_or_else(|| "unknown".to_string()),
            target_triple: format!("{}-{}", std::env::consts::ARCH, std::env::consts::OS),
            parser_version: env!("CARGO_PKG_VERSION").to_string(),
            analyzer_version: env!("CARGO_PKG_VERSION").to_string(),
            os: format!("{} {}", std::env::consts::OS, std::env::consts::FAMILY),
            cpu_model: std::env::var("GDLS_BENCH_CPU_MODEL")
                .unwrap_or_else(|_| "unknown".to_string()),
        }
    }
}

/// One open buffer captured at flush time. The replay driver re-opens each buffer as a
/// `textDocument/didOpen` before driving the trace.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct BufferSnapshot {
    pub uri: String,
    pub text: String,
    #[serde(default)]
    pub version: i32,
}

/// One recorded JSON-RPC entry. The `kind` discriminator keeps requests and notifications
/// distinguishable in the JSON; the replay driver dispatches accordingly.
#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(tag = "kind")]
pub enum TraceEntry {
    /// `textDocument/didOpen` / `didChange` / etc. — no response is expected.
    Notification { method: String, params: Value },
    /// `textDocument/hover` / `definition` / etc. — a response is expected.
    Request {
        request_id: i64,
        method: String,
        params: Value,
    },
}

/// On-disk JSON artifact: env metadata, captured open buffers, and the ring-buffer trace.
#[derive(Serialize, Deserialize, Debug)]
pub struct BenchArtifact {
    pub version: u32,
    pub captured_at_unix_secs: u64,
    pub env: BenchEnv,
    pub open_buffers: Vec<BufferSnapshot>,
    pub trace: Vec<TraceEntry>,
}

/// Ring buffer of recent JSON-RPC entries; owned by the live `ServerState` and emitted to disk on
/// shutdown when the gating env var is set.
pub struct BenchRecorder {
    capacity: usize,
    dump_path: PathBuf,
    trace: VecDeque<TraceEntry>,
}

impl BenchRecorder {
    /// Construct a recorder that, on [`flush`](Self::flush), writes the artifact to `dump_path`.
    pub fn new(capacity: usize, dump_path: PathBuf) -> Self {
        let capacity = capacity.max(1);
        Self {
            capacity,
            dump_path,
            trace: VecDeque::with_capacity(capacity),
        }
    }

    /// Try to construct a recorder from the gating env vars. Returns `None` when
    /// [`ENV_RECORD_TO`] is unset (production case — no recording).
    pub fn from_env() -> Option<Self> {
        let path = std::env::var_os(ENV_RECORD_TO)?;
        let capacity = std::env::var(ENV_RECORD_CAPACITY)
            .ok()
            .and_then(|s| s.parse::<usize>().ok())
            .filter(|&n| n > 0)
            .unwrap_or(DEFAULT_TRACE_CAPACITY);
        Some(Self::new(capacity, PathBuf::from(path)))
    }

    /// Override the ring-buffer capacity (used by tests that want to assert eviction behaviour).
    #[must_use]
    pub fn with_capacity(mut self, capacity: usize) -> Self {
        self.capacity = capacity.max(1);
        self
    }

    /// Append one entry, evicting the oldest when at capacity.
    pub fn record(&mut self, entry: TraceEntry) {
        if self.trace.len() == self.capacity {
            self.trace.pop_front();
        }
        self.trace.push_back(entry);
    }

    /// Record a request. The `RequestId` is normalised to an `i64` when possible; LSP requests use
    /// integer ids exclusively in practice, so the `String` variant falls back to `0` to keep the
    /// schema's `request_id` an integer.
    ///
    /// Lifecycle-handshake requests (`initialize`, `shutdown`) are filtered out — the replay
    /// driver runs its own handshake against a fresh server, so re-feeding the recorded ones
    /// would deadlock the second handshake or trip `lsp_server`'s "unexpected message during
    /// shutdown" guard.
    pub fn record_request(&mut self, req: &Request) {
        if is_lifecycle_request(&req.method) {
            return;
        }
        self.record(TraceEntry::Request {
            request_id: request_id_to_i64(&req.id),
            method: req.method.clone(),
            params: req.params.clone(),
        });
    }

    /// Record a notification (no response). Filters the lifecycle `initialized` / `exit`
    /// notifications for the same reason `record_request` filters lifecycle requests.
    pub fn record_notification(&mut self, note: &Notification) {
        if is_lifecycle_notification(&note.method) {
            return;
        }
        self.record(TraceEntry::Notification {
            method: note.method.clone(),
            params: note.params.clone(),
        });
    }

    /// Number of entries currently in the ring buffer.
    pub fn len(&self) -> usize {
        self.trace.len()
    }

    /// `true` when no entries have been recorded yet.
    pub fn is_empty(&self) -> bool {
        self.trace.is_empty()
    }

    /// Write the artifact to [`Self::dump_path`]. Consumes `self` because the recorder isn't
    /// useful after flush — re-construct if more recording is needed.
    pub fn flush(self, open_buffers: Vec<BufferSnapshot>) -> Result<()> {
        let artifact = BenchArtifact {
            version: ARTIFACT_VERSION,
            captured_at_unix_secs: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0),
            env: BenchEnv::capture(),
            open_buffers,
            trace: self.trace.into_iter().collect(),
        };
        let json =
            serde_json::to_string_pretty(&artifact).context("serialize bench artifact to JSON")?;
        if let Some(parent) = self.dump_path.parent() {
            if !parent.as_os_str().is_empty() {
                fs::create_dir_all(parent).with_context(|| {
                    format!("create parent dir for {}", self.dump_path.display())
                })?;
            }
        }
        fs::write(&self.dump_path, json)
            .with_context(|| format!("write bench artifact to {}", self.dump_path.display()))?;
        Ok(())
    }
}

/// LSP handshake requests that the recorder skips (replay drives them itself).
fn is_lifecycle_request(method: &str) -> bool {
    matches!(method, "initialize" | "shutdown")
}

/// LSP handshake notifications that the recorder skips.
fn is_lifecycle_notification(method: &str) -> bool {
    matches!(method, "initialized" | "exit")
}

/// Convert an `lsp_server::RequestId` to a stable `i64`. Real LSP clients always use integer ids
/// (the spec allows strings but no production client does); a string id falls back to `0`. The
/// recorded id is informational only — the replay driver renumbers ids fresh to avoid collisions
/// with the in-memory server's own counters.
fn request_id_to_i64(id: &RequestId) -> i64 {
    // `RequestId` is opaque; round-trip through serde_json to get the integer.
    serde_json::to_value(id)
        .ok()
        .and_then(|v| v.as_i64())
        .unwrap_or(0)
}

/// Snapshot the open buffers from the live VFS, sorted by URI for stable artifact diffs.
pub(crate) fn snapshot_buffers(vfs: &Vfs) -> Vec<BufferSnapshot> {
    let mut out: Vec<BufferSnapshot> = vfs
        .open_uris()
        .filter_map(|uri| {
            vfs.get(uri).map(|doc| BufferSnapshot {
                uri: uri.to_string(),
                text: doc.text(),
                version: doc.version,
            })
        })
        .collect();
    out.sort_by(|a, b| a.uri.cmp(&b.uri));
    out
}

/// One row of replay metrics. `elapsed_us` is the wall time from sending the entry to receiving
/// the response (notifications report `0` since they have no response). `request_id` reflects the
/// id assigned during *replay*, not the original (the replay renumbers fresh — see
/// [`request_id_to_i64`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReplayMetric {
    pub method: String,
    pub request_id: i64,
    pub elapsed_us: u128,
    /// `true` for notifications (no response was awaited).
    pub notification: bool,
    /// `true` when a request's response never arrived within the replay deadline. Such a row's
    /// `elapsed_us` is the timeout window, NOT a real latency — a developer triaging a perf
    /// regression from the CSV must be able to tell a 5 s-but-completed request from a hang.
    pub timed_out: bool,
}

/// Replay an artifact against a fresh in-memory server: open every captured buffer, then drive
/// each trace entry, timing the per-request wall clock. Returns the metrics so callers can assert
/// or print them; convenience [`replay_to_csv`] wraps this with a stdout writer.
pub fn replay(artifact_path: &Path) -> Result<Vec<ReplayMetric>> {
    let raw = fs::read_to_string(artifact_path)
        .with_context(|| format!("read bench artifact at {}", artifact_path.display()))?;
    let artifact: BenchArtifact = serde_json::from_str(&raw)
        .with_context(|| format!("parse bench artifact at {}", artifact_path.display()))?;
    if artifact.version != ARTIFACT_VERSION {
        anyhow::bail!(
            "bench artifact version {} is not supported by this gdls build (expected {})",
            artifact.version,
            ARTIFACT_VERSION
        );
    }

    let (server_conn, client_conn) = Connection::memory();
    let server_thread = std::thread::spawn(move || crate::serve(server_conn));

    // Handshake: send a minimal `initialize` so the server reaches the dispatch loop. Replay never
    // pretends to be a real client — capabilities are defaults; root URI is empty.
    let init_id = 1_i32;
    client_conn
        .sender
        .send(Message::Request(Request {
            id: RequestId::from(init_id),
            method: "initialize".to_string(),
            params: serde_json::to_value(lsp_types::InitializeParams::default())?,
        }))
        .context("replay: send initialize")?;
    // Drain until the matching response (no spurious requests in the lifecycle handshake, but
    // diagnostics may interleave).
    drain_until_response(&client_conn, init_id, Duration::from_secs(5))
        .context("replay: initialize did not return a response within 5s")?;
    client_conn
        .sender
        .send(Message::Notification(Notification {
            method: "initialized".to_string(),
            params: serde_json::json!({}),
        }))
        .context("replay: send initialized")?;

    // Re-open every captured buffer in the recorded order.
    for buf in &artifact.open_buffers {
        let did_open = lsp_types::DidOpenTextDocumentParams {
            text_document: lsp_types::TextDocumentItem {
                uri: buf
                    .uri
                    .parse()
                    .with_context(|| format!("replay: parse open-buffer URI {}", buf.uri))?,
                language_id: "gdscript".to_string(),
                version: buf.version,
                text: buf.text.clone(),
            },
        };
        client_conn
            .sender
            .send(Message::Notification(Notification {
                method: "textDocument/didOpen".to_string(),
                params: serde_json::to_value(did_open)?,
            }))
            .context("replay: send didOpen for captured buffer")?;
    }

    let mut metrics = Vec::with_capacity(artifact.trace.len());
    let mut next_id: i32 = init_id + 1;
    for entry in artifact.trace {
        match entry {
            TraceEntry::Notification { method, params } => {
                let start = Instant::now();
                client_conn
                    .sender
                    .send(Message::Notification(Notification {
                        method: method.clone(),
                        params,
                    }))
                    .context("replay: send notification")?;
                metrics.push(ReplayMetric {
                    method,
                    request_id: 0,
                    elapsed_us: start.elapsed().as_micros(),
                    notification: true,
                    timed_out: false,
                });
            }
            TraceEntry::Request { method, params, .. } => {
                let req_id = next_id;
                next_id = next_id.wrapping_add(1);
                let start = Instant::now();
                client_conn
                    .sender
                    .send(Message::Request(Request {
                        id: RequestId::from(req_id),
                        method: method.clone(),
                        params,
                    }))
                    .context("replay: send request")?;
                match drain_until_response(&client_conn, req_id, Duration::from_secs(5)) {
                    Ok(_resp) => metrics.push(ReplayMetric {
                        method,
                        request_id: req_id as i64,
                        elapsed_us: start.elapsed().as_micros(),
                        notification: false,
                        timed_out: false,
                    }),
                    Err(e) => {
                        log::warn!("replay: request id={req_id} method={method} timed out: {e}");
                        metrics.push(ReplayMetric {
                            method,
                            request_id: req_id as i64,
                            elapsed_us: start.elapsed().as_micros(),
                            notification: false,
                            timed_out: true,
                        });
                    }
                }
            }
        }
    }

    // Tear the server down cleanly so the spawned thread joins.
    let shutdown_id = next_id;
    let _ = client_conn.sender.send(Message::Request(Request {
        id: RequestId::from(shutdown_id),
        method: "shutdown".to_string(),
        params: serde_json::Value::Null,
    }));
    let _ = drain_until_response(&client_conn, shutdown_id, Duration::from_secs(5));
    let _ = client_conn.sender.send(Message::Notification(Notification {
        method: "exit".to_string(),
        params: serde_json::Value::Null,
    }));
    server_thread
        .join()
        .map_err(|_| anyhow::anyhow!("replay: server thread panicked"))?
        .context("replay: server returned an error")?;

    Ok(metrics)
}

/// Drive [`replay`] and stream the per-row CSV (`method,request_id,elapsed_us,timed_out`) into `writer`.
pub fn replay_to_csv<W: Write>(artifact_path: &Path, writer: &mut W) -> Result<Vec<ReplayMetric>> {
    let metrics = replay(artifact_path)?;
    writeln!(writer, "method,request_id,elapsed_us,timed_out").context("write CSV header")?;
    for m in &metrics {
        writeln!(
            writer,
            "{},{},{},{}",
            m.method, m.request_id, m.elapsed_us, m.timed_out
        )
        .context("write CSV row")?;
    }
    Ok(metrics)
}

/// Pull messages from `conn.receiver` until a `Response` matching `expected_id` arrives or
/// `deadline` elapses. Server-pushed notifications (e.g. `textDocument/publishDiagnostics`) are
/// observed and discarded.
fn drain_until_response(
    conn: &Connection,
    expected_id: i32,
    deadline: Duration,
) -> Result<Response> {
    let started = Instant::now();
    loop {
        let remaining = deadline.saturating_sub(started.elapsed());
        if remaining.is_zero() {
            anyhow::bail!("timed out waiting for response id={expected_id}");
        }
        match conn.receiver.recv_timeout(remaining) {
            Ok(Message::Response(resp)) if resp.id == RequestId::from(expected_id) => {
                return Ok(resp)
            }
            Ok(_) => continue, // diagnostic / unrelated response, keep draining
            Err(RecvTimeoutError::Timeout) => {
                anyhow::bail!("timed out waiting for response id={expected_id}");
            }
            Err(RecvTimeoutError::Disconnected) => {
                anyhow::bail!("server disconnected while waiting for response id={expected_id}");
            }
        }
    }
}
