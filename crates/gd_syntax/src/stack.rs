//! Stack growth for recursion whose depth the source controls.
//!
//! The parser, the analyzer, and the server's tree walks all recurse once per nesting level, and a
//! file can nest as deep as [`crate::parser::MAX_PARSE_DEPTH`]. Calling [`ensure`] at each level
//! moves the work onto a fresh heap-allocated segment whenever the current stack runs low, so that
//! depth fits on any thread: the server's session thread, the `diagnose` CLI's main thread, a test
//! harness thread, or a fuzzer.

/// Space a recursion level may use before the next [`ensure`] call checks again. One level is a
/// few frames (`parse_precedence` → a prefix rule → `parse_expression`), well under this even in an
/// unoptimized or sanitizer build.
const RED_ZONE: usize = 256 * 1024;

/// Size of each segment [`ensure`] allocates when the current stack runs low.
const SEGMENT: usize = 4 * 1024 * 1024;

/// Run `f`, first moving to a new stack segment if fewer than [`RED_ZONE`] bytes are left.
#[inline]
pub fn ensure<R>(f: impl FnOnce() -> R) -> R {
    stacker::maybe_grow(RED_ZONE, SEGMENT, f)
}
