//! M11 (#80) — a cross-platform STUB formatter for the `tests/formatting.rs` integration tests.
//!
//! gdls bridges `textDocument/formatting` to a user-configured external command (stdin → stdout). To
//! exercise that bridge on BOTH Linux and Windows CI we need a formatter whose behavior is identical
//! on every platform — platform commands (`cat`/`sed`/`sleep`) differ or don't exist on Windows. So
//! this tiny helper *is* the test formatter: a `[[bin]]` of the `gd_server` package, located by the
//! integration test via `env!("CARGO_BIN_EXE_gdls_format_stub")` (Cargo sets that for every bin of
//! the package under test) — never invoked through a shell, exactly as the real bridge spawns the
//! configured command.
//!
//! It is NOT part of the shipped server: release packaging builds `--bin gdls` explicitly
//! (`.claude/skills/release/SKILL.md`), so this second bin is compiled only by `cargo build`/`cargo
//! test`, never published.
//!
//! Behavior is selected by the FIRST CLI argument (the test passes it via `formatter.args`):
//!
//! * `squeeze` — collapse runs of spaces to one, exit 0 (the deterministic "successful format").
//! * `replace` — write a fixed canonical document, exit 0 (a whole-document rewrite round-trip).
//! * `passthru` — copy stdin to stdout verbatim, exit 0 (output == input → the handler emits no edit).
//! * `touchone` — collapse spaces only on lines containing `MARK`, exit 0 (one-line minimal-diff).
//! * `fail` — drain stdin, write nothing, exit 1 (a formatter that errors).
//! * `sleep` — drain stdin, sleep past the handler's timeout (exercises kill-on-timeout).
//! * `binary` — drain stdin, write invalid UTF-8 bytes to stdout, exit 0 (the non-UTF-8 path).
//! * (anything else / missing) — exit 2 without reading, an unknown-mode guard.
//!
//! Two parameterized modes exercise the off-worker scheduling (#135 head-of-line blocking) and the
//! cancel-driven child kill (#136). They take a SECOND CLI argument:
//!
//! * `delaysqueeze <ms>` — sleep `<ms>` milliseconds, THEN squeeze (a successful but SLOW format).
//!   Used by the HOL test: a long format must not stall an unrelated request — the slow `delaysqueeze`
//!   occupies the bridge while a fast request races past it.
//! * `markerafter <path>` — sleep a long time (past any cancel window), then create the file at
//!   `<path>` and squeeze. The marker file appears ONLY if the child ran to completion; a cancel that
//!   kills the child mid-sleep leaves no marker — that absence is the no-orphan / no-late-write proof.
//! * `markerafterms <ms> <path>` — same marker proof with a caller-chosen delay. Used by the
//!   shutdown/exit teardown test so an uncancelled child proves itself quickly.
//!
//! Every mode drains stdin first (a real filter does), so the parent's stdin write never blocks on a
//! full pipe.

use std::io::{Read, Write};

fn main() {
    let mut args = std::env::args().skip(1);
    let mode = args.next().unwrap_or_default();
    let arg2 = args.next();

    // Drain stdin first in every mode (a real stdin→stdout filter consumes its input). Using a byte
    // buffer so the `binary` mode can also read non-text input without a decode error.
    let mut input = Vec::new();
    let _ = std::io::stdin().read_to_end(&mut input);

    match mode.as_str() {
        "squeeze" => {
            let text = String::from_utf8_lossy(&input);
            let out = squeeze_spaces(&text);
            write_stdout(out.as_bytes());
        }
        "replace" => {
            // A fixed canonical document, unrelated to the input (whole-document rewrite).
            write_stdout(b"func canonical():\n\tpass\n");
        }
        "passthru" => {
            write_stdout(&input);
        }
        "touchone" => {
            let text = String::from_utf8_lossy(&input);
            let out: String = text
                .split_inclusive('\n')
                .map(|line| {
                    if line.contains("MARK") {
                        squeeze_spaces(line)
                    } else {
                        line.to_string()
                    }
                })
                .collect();
            write_stdout(out.as_bytes());
        }
        "fail" => {
            // Errored formatter: no output, non-zero exit.
            std::process::exit(1);
        }
        "sleep" => {
            // Far longer than the handler's bounded timeout; the parent kills us before this returns.
            std::thread::sleep(std::time::Duration::from_secs(60));
            std::process::exit(0);
        }
        "binary" => {
            // Invalid UTF-8 (a lone continuation byte / bare 0xFF) — the handler must reject it.
            write_stdout(&[0x66, 0x6f, 0x6f, 0xff, 0xfe, 0x00, 0x80]);
        }
        "delaysqueeze" => {
            // A SUCCESSFUL but slow format: sleep the requested ms, then squeeze. Drives the #135
            // head-of-line test — this format must run off the request worker so a concurrent
            // request answers without waiting out this sleep.
            let ms: u64 = arg2.as_deref().unwrap_or("0").parse().unwrap_or(0);
            std::thread::sleep(std::time::Duration::from_millis(ms));
            let text = String::from_utf8_lossy(&input);
            let out = squeeze_spaces(&text);
            write_stdout(out.as_bytes());
        }
        "markerafter" => {
            // No-orphan proof for #136: sleep well past the test's cancel window, then write a
            // marker file and squeeze. A cancel that kills this child mid-sleep leaves NO marker and
            // NO stdout — the test asserts the marker's absence (no orphaned process completed, no
            // late edit was produced). The path is the second arg.
            let marker = arg2.expect("markerafter requires a marker path");
            std::thread::sleep(std::time::Duration::from_secs(30));
            let _ = std::fs::write(&marker, b"done");
            let text = String::from_utf8_lossy(&input);
            let out = squeeze_spaces(&text);
            write_stdout(out.as_bytes());
        }
        "markerafterms" => {
            let ms: u64 = arg2
                .as_deref()
                .unwrap_or("0")
                .parse()
                .expect("markerafterms requires a millisecond delay");
            let marker = args.next().expect("markerafterms requires a marker path");
            std::thread::sleep(std::time::Duration::from_millis(ms));
            let _ = std::fs::write(&marker, b"done");
            let text = String::from_utf8_lossy(&input);
            let out = squeeze_spaces(&text);
            write_stdout(out.as_bytes());
        }
        _ => {
            // Unknown / missing mode — fail loudly so a mis-wired test is obvious.
            std::process::exit(2);
        }
    }
}

/// Collapse every run of one-or-more ASCII spaces into a single space. Tabs/newlines are untouched,
/// so a `\t`-indented GDScript body keeps its indentation while `var  x   =  1` → `var x = 1`.
fn squeeze_spaces(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut prev_space = false;
    for c in s.chars() {
        if c == ' ' {
            if !prev_space {
                out.push(' ');
            }
            prev_space = true;
        } else {
            out.push(c);
            prev_space = false;
        }
    }
    out
}

/// Write all bytes to stdout, ignoring a broken pipe (the parent may have stopped reading).
fn write_stdout(bytes: &[u8]) {
    let stdout = std::io::stdout();
    let mut lock = stdout.lock();
    let _ = lock.write_all(bytes);
    let _ = lock.flush();
}
