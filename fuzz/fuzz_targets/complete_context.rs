#![no_main]

use libfuzzer_sys::fuzz_target;

use gd_server::completion_context::classify;
use gd_syntax::ParseOptions;

#[path = "common.rs"]
mod common;

// M8 (#64) context engine, promoted to a coverage-guided fuzz target: drive the cursor →
// completion-context classifier at EVERY byte offset of arbitrary input. Taking `&str` makes
// libfuzzer-sys feed a valid-UTF-8 view of the raw bytes (via the `arbitrary` crate), matching the
// `gd_syntax::parse` / `gd_syntax::tokenize` signatures the classifier consumes.
//
// The contract under test is `classify`'s documented promise: "Pure and panic-free for any
// `(tree, tokens, byte)`" (`completion_context.rs`) — out-of-range offsets, partial/mid-edit token
// streams, and degenerate inputs (empty, lone surrogate-free multibyte chars, unbalanced brackets)
// must all resolve to a well-defined `CompletionContext` (worst case `CompletionKind::None`) rather
// than panic or overflow the stack. This mirrors the in-crate exhaustive test
// `classify_never_panics_at_every_offset_of_every_fixture`, but over libfuzzer's evolving corpus
// instead of a fixed fixture list; any panic here is a release blocker (CLAUDE.md "never crash").
//
// `tokens` must be the standalone tokenizer output for the same source AND the same dialect the
// tree was parsed at (the classifier's precondition), so the leading byte's dialect reaches both.
// The two supported releases do not tokenize identically — indentation columns differ — so a
// mismatched pair would be a token frame no session can produce. We assert nothing about the
// returned context — fidelity of the classification itself is covered by the in-crate unit tests
// and the differential oracle (`docs/06`); this target is solely about panic-freedom across every
// offset of the token-frame + AST-probe paths.
fuzz_target!(|input: (u8, &str)| {
    let (tag, data) = input;
    let dialect = common::dialect_from(tag);
    let tree = gd_syntax::parse_with_options(
        data,
        &ParseOptions {
            dialect,
            script_path: "",
        },
    )
    .tree;
    let (tokens, _errs) = gd_syntax::tokenize_with_dialect(data, dialect);
    // Inclusive of 0 and len, plus one past the end, plus a wildly-out-of-range offset — every
    // offset must clamp to a well-defined result rather than panic (half-open spans mean the AST
    // probe is `None` at end-of-input, so the boundary offsets are the interesting ones).
    for byte in 0..=data.len() + 1 {
        let _ = classify(&tree, &tokens, byte);
    }
    let _ = classify(&tree, &tokens, usize::MAX);
});
