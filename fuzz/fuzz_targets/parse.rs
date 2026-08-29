#![no_main]

use libfuzzer_sys::fuzz_target;

use gd_syntax::ParseOptions;

#[path = "common.rs"]
mod common;

// Fuzz the whole frontend pipeline: tokenizer → parser → AST → symbol projection. Taking `&str`
// makes libfuzzer-sys feed a valid-UTF-8 view of the raw input (via the `arbitrary` crate), matching
// `gd_syntax::parse`'s signature. The leading byte picks the dialect (`common::dialect_from`), since
// the two supported releases do not tokenize or parse identically and fuzzing only the default would
// leave every older-dialect guard uncovered. The contract under test is "never crash": for ANY input
// at ANY dialect the parser must return a (possibly partial) result without panicking or overflowing
// the stack (`CLAUDE.md`).
fuzz_target!(|input: (u8, &str)| {
    let (tag, data) = input;
    let _ = gd_syntax::parse_with_options(
        data,
        &ParseOptions {
            dialect: common::dialect_from(tag),
            script_path: "",
        },
    );
});
