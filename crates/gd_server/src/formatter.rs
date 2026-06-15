//! M11 (#80) — the external-formatter bridge (`textDocument/formatting`).
//!
//! ## Why a bridge, not a port
//!
//! Godot ships no GDScript formatter to port (the engine reformats nothing — there is no canonical
//! style in the frontend gdls mirrors), so gdls cannot *be* a formatter. Instead it bridges to a
//! user-configured external command (the rust-analyzer `rustfmt.overrideCommand` pattern): the
//! document's current text is piped to the child's STDIN, and the child's STDOUT is the formatted
//! document. The community tool is `gdformat` (the `gdtoolkit` package), but gdls is agnostic — any
//! stdin→stdout filter works.
//!
//! ## Advertise only when configured (anti-catalog W15)
//!
//! `documentFormattingProvider` is advertised **iff** `formatter.command` is set
//! ([`FormatterConfig::is_configured`]). With no command configured, the capability is absent and a
//! stray `textDocument/formatting` request (a non-conforming client) returns no edits rather than an
//! error — gdls never tells a client it formats when it can't.
//!
//! `rangeFormatting` is intentionally NOT advertised: GDScript's real-world formatters (`gdformat`)
//! are whole-file only, and the spec says to advertise range formatting only when the tool supports
//! it. A future `formatter.rangeArgs`-style flag could add it; until then offering it would route a
//! partial-range request to a whole-file tool and silently reformat the whole buffer.
//!
//! ## Subprocess safety is the bar
//!
//! The command and its output are BOTH untrusted, and the child runs on every save in a live editor.
//! The contract — never corrupt the buffer, never hang the session — is enforced four ways:
//!
//!   1. **No shell.** [`std::process::Command`] is invoked with the configured executable + an
//!      explicit `argv` vector; gdls never spawns `sh -c` / `cmd /c`. A `command`/`args` value can
//!      therefore never be interpreted as a shell expression (no injection, no glob / `$VAR`
//!      expansion). A user wanting a pipeline points `command` at a wrapper script.
//!   2. **Bounded timeout, no pipe deadlock.** stdin is written on its own thread while stdout is
//!      drained CONCURRENTLY (so a formatter emitting a large result before draining its stdin can't
//!      deadlock against a sequential write-then-read); the main thread blocks on a channel with
//!      [`FORMAT_TIMEOUT`]. If the child hasn't produced its output by then it is KILLED and the
//!      request is a timeout failure. A wedged formatter can never hang the worker.
//!   3. **Output is validated.** A non-zero exit, non-UTF-8 stdout, or a spawn error is a failure
//!      that yields NO edits. Only a clean (exit 0, valid UTF-8) run produces edits.
//!   4. **Minimal-diff edits, never a blind replace.** On success the original and formatted texts
//!      are line-diffed (common prefix/suffix) into a single [`TextEdit`] over only the changed
//!      region, so an unchanged head/tail — and the user's cursor in it — survives. A whole-document
//!      replace would jump the cursor to the top on every save.
//!
//! Every failure class additionally emits a `window/showMessage(Warning)` **once per session per
//! class** ([`ServerState::formatter_warned`]) so the user learns the formatter is misconfigured
//! without being spammed on every keystroke-save. The buffer is NEVER mutated on a failure.

use std::io::{Read, Write};
use std::process::{Command, Stdio};
use std::time::Duration;

use lsp_types::{DocumentFormattingParams, MessageType, OneOf, TextEdit};

use crate::config::FormatterConfig;
use crate::position::PositionMapper;
use crate::server::{show_message, ServerState};

/// How long the external formatter may run before gdls kills it and treats the format as failed.
/// A formatter is an interactive-latency operation (it runs on save); a few seconds is generous for
/// any real file while still bounding a wedged child so the worker can never hang. Not yet
/// configurable — a `formatter.timeoutMs` knob can be added if a project needs a longer bound.
const FORMAT_TIMEOUT: Duration = Duration::from_secs(5);

/// Upper bound on the bytes gdls will buffer from the formatter's STDOUT. A formatted GDScript file
/// is at most a small multiple of its source; 64 MiB is orders of magnitude above any real `.gd`
/// while still bounding a runaway / malicious formatter that streams without end, so it can never
/// OOM the server. Output that would exceed this is a failure ([`FormatterFailure::OversizedOutput`])
/// — no edits, the buffer untouched. (The timeout is the other half of the backstop: a formatter
/// that emits slowly forever is killed at [`FORMAT_TIMEOUT`] regardless of byte count.)
const MAX_OUTPUT_BYTES: u64 = 64 * 1024 * 1024;

/// The distinct ways a format attempt can fail. Each is warned about AT MOST ONCE PER SESSION (the
/// dedupe key in [`ServerState::formatter_warned`]) so a persistently-misconfigured formatter does
/// not spam `window/showMessage` on every save, while a user still gets one actionable message per
/// distinct problem.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum FormatterFailure {
    /// The command could not be spawned at all (not on `PATH`, not executable, …).
    Spawn,
    /// The child exited with a non-zero status (the formatter rejected the input / errored).
    NonZeroExit,
    /// The child did not finish within [`FORMAT_TIMEOUT`] and was killed.
    Timeout,
    /// The child's STDOUT was not valid UTF-8 (a formatter must emit text; binary output is a bug).
    NonUtf8Output,
    /// The child's STDOUT exceeded [`MAX_OUTPUT_BYTES`] — a runaway/streaming formatter. Bounded so a
    /// misbehaving child can never OOM the server.
    OversizedOutput,
}

impl FormatterFailure {
    /// A short, user-actionable `window/showMessage` line. Names the configured command so the user
    /// knows which tool is at fault. Deliberately terse (one line, no stack/exit-code dump — the full
    /// detail goes to the stderr log).
    fn message(self, command: &str) -> String {
        match self {
            FormatterFailure::Spawn => format!(
                "gdls: could not run the configured GDScript formatter ({command:?}). Check that \
                 `formatter.command` names an executable on PATH (or an absolute path). The buffer \
                 was left unchanged."
            ),
            FormatterFailure::NonZeroExit => format!(
                "gdls: the GDScript formatter ({command:?}) exited with an error and produced no \
                 formatting. The buffer was left unchanged."
            ),
            FormatterFailure::Timeout => format!(
                "gdls: the GDScript formatter ({command:?}) did not finish within {}s and was \
                 stopped. The buffer was left unchanged.",
                FORMAT_TIMEOUT.as_secs()
            ),
            FormatterFailure::NonUtf8Output => format!(
                "gdls: the GDScript formatter ({command:?}) wrote non-text output, so its result \
                 was discarded. The buffer was left unchanged."
            ),
            FormatterFailure::OversizedOutput => format!(
                "gdls: the GDScript formatter ({command:?}) produced an unreasonably large amount \
                 of output, so its result was discarded. The buffer was left unchanged."
            ),
        }
    }
}

/// Build the `documentFormattingProvider` server capability — `Some` iff a formatter command is
/// configured (anti-catalog W15: never advertise an unconfigured surface). `rangeFormatting` is
/// deliberately omitted (see the module docs: GDScript formatters are whole-file).
#[must_use]
pub(crate) fn document_formatting_provider(
    formatter: &FormatterConfig,
) -> Option<OneOf<bool, lsp_types::DocumentFormattingOptions>> {
    formatter.is_configured().then_some(OneOf::Left(true))
}

/// `textDocument/formatting`: pipe the document's current text through the configured external
/// formatter and return minimal-diff [`TextEdit`]s for the changed region, or `None` (LSP `null`) on
/// any failure. NEVER mutates the buffer on failure; emits a deduped `window/showMessage(Warning)`
/// per failure class.
///
/// Returns `None` (no edits) when:
///   * no formatter is configured (defensive — the capability isn't advertised, so a conforming
///     client never sends this), or the buffer isn't open;
///   * the format fails (spawn error / non-zero exit / timeout / non-UTF-8 output) — plus a warning;
///   * the formatter's output is byte-identical to the input (nothing to change — not a failure).
#[must_use]
pub(crate) fn formatting(
    state: &mut ServerState,
    params: DocumentFormattingParams,
) -> Option<Vec<TextEdit>> {
    // Defensive: the capability is gated on this, so a conforming client never reaches here without
    // a command — but a non-conforming one might, and we must answer with no edits, never spawn a
    // bogus command nor panic.
    let formatter = state.options.formatter.clone();
    let command = formatter.command.clone()?;

    let uri = params.text_document.uri;
    // The current buffer is the source of truth (docs/01 vfs.rs). No open buffer ⇒ nothing to format
    // (we never read disk here — formatting acts on what the user sees).
    let doc = state.vfs.get(uri.as_str())?;
    let original = doc.text();
    let mapper = PositionMapper::new(&doc.rope, state.encoding);

    match run_formatter(&command, &formatter.args, &original) {
        Ok(formatted) => minimal_edits(&original, &formatted, &mapper),
        Err(failure) => {
            // Log the full detail to stderr always; the user-facing showMessage is deduped.
            log::warn!(
                "textDocument/formatting: formatter {command:?} failed ({failure:?}); returning no \
                 edits (buffer unchanged)"
            );
            warn_once(state, failure, &command);
            None
        }
    }
}

/// Emit the `window/showMessage(Warning)` for a failure class AT MOST ONCE PER SESSION. The
/// per-class dedupe lives in [`ServerState::formatter_warned`]; a class already present is a no-op
/// (the user was told once; every later save stays silent).
fn warn_once(state: &mut ServerState, failure: FormatterFailure, command: &str) {
    if state.formatter_warned.insert(failure) {
        show_message(state, MessageType::WARNING, &failure.message(command));
    }
}

/// Spawn `command args…` with NO shell, write `input` to its STDIN, and read its STDOUT under a
/// bounded timeout. Returns the formatted text on a clean run (exit 0 + valid UTF-8 stdout), or the
/// matching [`FormatterFailure`] otherwise. The child is killed on timeout. Never panics.
///
/// The stdin-write and stdout-read run CONCURRENTLY — stdin on its own thread while the worker
/// drains stdout — so a formatter that writes a large result (filling the OS pipe buffer) before
/// draining its stdin can't deadlock against us: neither side blocks the other, and the main thread
/// only blocks on a channel `recv_timeout`. On timeout the child handle (kept on the main thread) is
/// killed, which breaks both pipes and unblocks both I/O threads so they exit.
fn run_formatter(command: &str, args: &[String], input: &str) -> Result<String, FormatterFailure> {
    let mut child = Command::new(command)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        // Inherit stderr is wrong (it would mingle with gdls's own stderr log stream); discard the
        // formatter's diagnostics — a non-zero exit already tells us it failed, and capturing
        // stderr would add another pipe to drain. The user sees the deduped showMessage.
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| {
            log::warn!("formatter spawn failed for {command:?}: {e}");
            FormatterFailure::Spawn
        })?;

    // Take the pipe ends to move onto the worker thread; `spawn()` with `piped()` always populates
    // them, so the `expect` documents an invariant rather than a reachable panic.
    let mut stdin = child
        .stdin
        .take()
        .expect("invariant: stdin is piped, so Child::stdin is Some right after spawn");
    let mut stdout = child
        .stdout
        .take()
        .expect("invariant: stdout is piped, so Child::stdout is Some right after spawn");

    let (tx, rx) = std::sync::mpsc::channel::<std::io::Result<Vec<u8>>>();
    let input_bytes = input.as_bytes().to_vec();
    let worker = std::thread::spawn(move || {
        // Write stdin on a DEDICATED thread so we drain stdout CONCURRENTLY. A sequential
        // write-all-then-read deadlocks any formatter that emits a large result (filling the OS
        // stdout pipe buffer) before it finishes draining its stdin: the child blocks writing its
        // stdout while we block writing its stdin, and neither side advances. Concurrent write+read
        // removes that window — large documents (gdls's stated target scale) format correctly.
        let writer = std::thread::spawn(move || {
            // A broken-pipe write (the child died early / never reads all of stdin) is NOT fatal:
            // the concurrent read surfaces whatever the child wrote and the exit-status check
            // classifies the failure. Drop stdin to send EOF (a filter blocks on EOF before its tail).
            let _ = stdin.write_all(&input_bytes).and_then(|()| stdin.flush());
            drop(stdin);
        });
        // Read at most MAX_OUTPUT_BYTES + 1: the extra byte makes "exceeded the cap" detectable on
        // the main thread (len > MAX) without ever buffering an unbounded stream — a runaway
        // formatter is capped, never an OOM.
        let result = read_capped(&mut stdout);
        // Reap the writer (it completes once stdin is fully written or the pipe breaks). On the
        // timeout path the main thread's `kill` breaks both pipes, unblocking both this read and the
        // writer — so neither thread leaks even when we've already returned.
        let _ = writer.join();
        // The receiver may be gone (we timed out and returned); ignore the send error.
        let _ = tx.send(result);
    });

    // Block on the worker for at most FORMAT_TIMEOUT. On timeout, KILL the child (which unblocks the
    // worker's I/O) and report a timeout — never wait on `worker.join()` here (that could re-block
    // for as long as the OS takes to tear the child down; the kill is enough and the detached worker
    // exits on its own once its pipes break).
    let stdout_bytes = match rx.recv_timeout(FORMAT_TIMEOUT) {
        Ok(Ok(bytes)) => bytes,
        Ok(Err(e)) => {
            log::warn!("formatter I/O error for {command:?}: {e}");
            let _ = child.kill();
            let _ = child.wait();
            // An I/O error talking to the child is, from the buffer's perspective, the same as a
            // failed run: classify by exit status if the child already died, else treat as non-zero.
            return classify_io_failure(&mut child);
        }
        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
            log::warn!("formatter {command:?} timed out after {FORMAT_TIMEOUT:?}; killing it");
            let _ = child.kill();
            let _ = child.wait();
            drop(worker); // detached; it returns once its pipes break from the kill
            return Err(FormatterFailure::Timeout);
        }
        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
            // The worker dropped the sender without sending — only possible on a panic in the worker
            // (which our code doesn't do). Treat as a failed run, fail-closed.
            log::warn!("formatter worker for {command:?} disconnected unexpectedly");
            let _ = child.kill();
            let _ = child.wait();
            return Err(FormatterFailure::Spawn);
        }
    };

    // The output is in hand; reap the child for its exit status (it has already written EOF, so this
    // returns promptly). A non-zero status discards the output (a formatter that errored must not
    // have its partial output applied).
    let status = match child.wait() {
        Ok(s) => s,
        Err(e) => {
            log::warn!("formatter {command:?} wait() failed: {e}");
            return Err(FormatterFailure::NonZeroExit);
        }
    };
    // The worker has sent its result, so it's effectively done; join to avoid a leaked thread.
    let _ = worker.join();

    if !status.success() {
        return Err(FormatterFailure::NonZeroExit);
    }
    // Oversize is checked BEFORE the UTF-8 decode: an over-cap buffer is rejected as oversized
    // regardless of whether its (capped, possibly mid-codepoint) prefix happens to decode.
    if stdout_bytes.len() as u64 > MAX_OUTPUT_BYTES {
        log::warn!("formatter {command:?} output exceeded {MAX_OUTPUT_BYTES} bytes; discarding");
        return Err(FormatterFailure::OversizedOutput);
    }
    String::from_utf8(stdout_bytes).map_err(|_| FormatterFailure::NonUtf8Output)
}

/// Read up to [`MAX_OUTPUT_BYTES`] + 1 bytes from `reader`. The one extra byte over the cap lets the
/// caller detect "the formatter produced more than the cap" (`len > MAX_OUTPUT_BYTES`) while never
/// buffering an unbounded stream into memory. Returns the bytes read (which the caller bounds-checks)
/// or the underlying read error.
fn read_capped(reader: &mut impl Read) -> std::io::Result<Vec<u8>> {
    let mut buf = Vec::new();
    reader
        .take(MAX_OUTPUT_BYTES + 1)
        .read_to_end(&mut buf)
        .map(|_| buf)
}

/// Classify a child that hit an I/O error mid-talk: if it already exited, honor its status; otherwise
/// it's a failed run. Always returns an `Err` (an I/O failure never yields formatted output).
fn classify_io_failure(child: &mut std::process::Child) -> Result<String, FormatterFailure> {
    match child.try_wait() {
        Ok(Some(status)) if status.success() => {
            // Exited 0 but we couldn't read its output — discard, fail-closed (don't apply a
            // possibly-truncated result).
            Err(FormatterFailure::NonUtf8Output)
        }
        _ => Err(FormatterFailure::NonZeroExit),
    }
}

/// Diff `original` → `formatted` into AT MOST ONE [`TextEdit`] over the changed region, or `None`
/// when they are byte-identical (nothing to do). The edit's `range` is in `original`-document
/// coordinates (via `mapper`), and its `new_text` is the changed slice of `formatted`.
///
/// ## Why line-granular common prefix/suffix
///
/// A whole-document replace works but moves the cursor to the top of the file on every save. Editing
/// only the changed region keeps the cursor (and folds, and scroll) stable in the unchanged head and
/// tail — the rust-analyzer / gopls behavior. We compute the longest common *line* prefix and the
/// longest common *line* suffix (clamped so the two never overlap), then replace the byte range of
/// `original`'s differing middle with `formatted`'s differing middle. Line granularity (not byte) is
/// chosen because a reformat's changes are line-shaped (re-indent, blank-line normalization) and a
/// line-anchored edit produces a clean, reviewable diff. A single coalesced edit (rather than one
/// per changed line) is correct and avoids overlapping-range hazards entirely.
fn minimal_edits(
    original: &str,
    formatted: &str,
    mapper: &PositionMapper,
) -> Option<Vec<TextEdit>> {
    if original == formatted {
        return None; // already formatted — no edit (and no spurious cursor jump)
    }

    // Split into lines *including* their terminators so byte offsets reconstruct exactly. `\n` and
    // `\r\n` are both folded into the preceding line's bytes, so concatenating the pieces is the
    // original string verbatim.
    let orig_lines = split_inclusive_lines(original);
    let fmt_lines = split_inclusive_lines(formatted);

    // Longest common line PREFIX.
    let mut prefix = 0;
    while prefix < orig_lines.len()
        && prefix < fmt_lines.len()
        && orig_lines[prefix] == fmt_lines[prefix]
    {
        prefix += 1;
    }

    // Longest common line SUFFIX, not crossing into the prefix on either side.
    let max_suffix = orig_lines.len().min(fmt_lines.len()) - prefix;
    let mut suffix = 0;
    while suffix < max_suffix
        && orig_lines[orig_lines.len() - 1 - suffix] == fmt_lines[fmt_lines.len() - 1 - suffix]
    {
        suffix += 1;
    }

    // Byte offset where the differing middle starts (= total bytes of the common prefix lines) and
    // where it ends in `original` (= start of the common suffix lines).
    let start_byte: usize = orig_lines[..prefix].iter().map(|l| l.len()).sum();
    let end_byte: usize = original.len()
        - orig_lines[orig_lines.len() - suffix..]
            .iter()
            .map(|l| l.len())
            .sum::<usize>();

    // The replacement text = `formatted`'s differing middle (the same prefix/suffix lines removed).
    let fmt_start: usize = fmt_lines[..prefix].iter().map(|l| l.len()).sum();
    let fmt_end: usize = formatted.len()
        - fmt_lines[fmt_lines.len() - suffix..]
            .iter()
            .map(|l| l.len())
            .sum::<usize>();
    let new_text = formatted[fmt_start..fmt_end].to_string();

    let range = mapper.span_to_range(gd_syntax::ByteSpan::new(start_byte, end_byte));
    Some(vec![TextEdit { range, new_text }])
}

/// Split a string into lines, each INCLUDING its trailing `\n` / `\r\n` so the pieces concatenate
/// back to the input byte-for-byte. A trailing newline yields no empty final element (the last line
/// carries its own terminator); a string with no trailing newline yields a final element without
/// one. An empty string yields no lines.
fn split_inclusive_lines(s: &str) -> Vec<&str> {
    s.split_inclusive('\n').collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::position::PositionEncoding;
    use ropey::Rope;

    fn edits_of(original: &str, formatted: &str) -> Option<Vec<TextEdit>> {
        let rope = Rope::from_str(original);
        let mapper = PositionMapper::new(&rope, PositionEncoding::Utf16);
        minimal_edits(original, formatted, &mapper)
    }

    /// Applying an edit to `original` reproduces `formatted` — the round-trip every minimal-diff
    /// must satisfy regardless of where the change falls.
    fn apply(original: &str, edits: &[TextEdit]) -> String {
        let rope = Rope::from_str(original);
        let mapper = PositionMapper::new(&rope, PositionEncoding::Utf16);
        // Single edit by construction; apply via byte offsets recovered through the mapper.
        assert!(edits.len() <= 1, "minimal_edits emits at most one edit");
        let mut out = original.to_string();
        if let Some(e) = edits.first() {
            let start = mapper.position_to_byte(e.range.start);
            let end = mapper.position_to_byte(e.range.end);
            out.replace_range(start..end, &e.new_text);
        }
        out
    }

    #[test]
    fn identical_text_yields_no_edit() {
        assert!(edits_of("var x = 1\n", "var x = 1\n").is_none());
    }

    #[test]
    fn single_changed_line_touches_only_that_line() {
        let original = "var a = 1\nvar  b   =   2\nvar c = 3\n";
        let formatted = "var a = 1\nvar b = 2\nvar c = 3\n";
        let edits = edits_of(original, formatted).expect("a change → one edit");
        assert_eq!(edits.len(), 1);
        // The edit must NOT span the whole document: it starts on line 1 (0-based) and ends before
        // the last line, leaving the common head/tail untouched.
        assert_eq!(
            edits[0].range.start.line, 1,
            "edit starts at the changed line"
        );
        assert_eq!(edits[0].range.end.line, 2, "edit ends at the changed line");
        assert_eq!(apply(original, &edits), formatted);
    }

    #[test]
    fn change_at_start_keeps_tail() {
        let original = "x=1\nkeep\nkeep2\n";
        let formatted = "x = 1\nkeep\nkeep2\n";
        let edits = edits_of(original, formatted).expect("one edit");
        assert_eq!(edits[0].range.start.line, 0);
        assert_eq!(apply(original, &edits), formatted);
    }

    #[test]
    fn change_at_end_keeps_head() {
        let original = "keep\nkeep2\ny=2\n";
        let formatted = "keep\nkeep2\ny = 2\n";
        let edits = edits_of(original, formatted).expect("one edit");
        assert!(edits[0].range.start.line >= 2, "head is preserved");
        assert_eq!(apply(original, &edits), formatted);
    }

    #[test]
    fn whole_document_rewrite_round_trips() {
        let original = "a\nb\nc\n";
        let formatted = "1\n2\n3\n4\n";
        let edits = edits_of(original, formatted).expect("one edit");
        assert_eq!(apply(original, &edits), formatted);
    }

    #[test]
    fn added_lines_round_trip() {
        let original = "func f():\n\tpass\n";
        let formatted = "func f():\n\tpass\n\n\nfunc g():\n\tpass\n";
        let edits = edits_of(original, formatted).expect("one edit");
        assert_eq!(apply(original, &edits), formatted);
    }

    #[test]
    fn removed_lines_round_trip() {
        let original = "func f():\n\tpass\n\n\n\nfunc g():\n\tpass\n";
        let formatted = "func f():\n\tpass\n\nfunc g():\n\tpass\n";
        let edits = edits_of(original, formatted).expect("one edit");
        assert_eq!(apply(original, &edits), formatted);
    }

    #[test]
    fn crlf_change_round_trips() {
        let original = "var a=1\r\nvar b=2\r\n";
        let formatted = "var a = 1\r\nvar b=2\r\n";
        let edits = edits_of(original, formatted).expect("one edit");
        assert_eq!(apply(original, &edits), formatted);
    }

    #[test]
    fn no_trailing_newline_round_trips() {
        let original = "var a=1";
        let formatted = "var a = 1";
        let edits = edits_of(original, formatted).expect("one edit");
        assert_eq!(apply(original, &edits), formatted);
    }

    #[test]
    fn failure_messages_name_the_command() {
        for f in [
            FormatterFailure::Spawn,
            FormatterFailure::NonZeroExit,
            FormatterFailure::Timeout,
            FormatterFailure::NonUtf8Output,
            FormatterFailure::OversizedOutput,
        ] {
            assert!(f.message("gdformat").contains("gdformat"));
        }
    }
}
