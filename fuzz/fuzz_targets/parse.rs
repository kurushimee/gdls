#![no_main]

use libfuzzer_sys::fuzz_target;

// Fuzz the whole frontend pipeline: tokenizer → parser → AST → symbol projection. Taking `&str`
// makes libfuzzer-sys feed a valid-UTF-8 view of the raw input (via the `arbitrary` crate), matching
// `gd_syntax::parse`'s signature. The contract under test is "never crash": for ANY input the parser
// must return a (possibly partial) result without panicking or overflowing the stack (`CLAUDE.md`).
fuzz_target!(|data: &str| {
    let _ = gd_syntax::parse(data);
});
