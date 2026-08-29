//! Shared bits for the fuzz targets. Included with `#[path]` rather than pulled from a library,
//! because a cargo-fuzz crate is a set of `[[bin]]`s with no lib target of its own.

use gd_syntax::Dialect;

/// Pick the Godot dialect the input should be read as.
///
/// gdls serves several feature releases and they do not tokenize, parse, or analyze identically, so
/// fuzzing only the default would leave every older-dialect guard uncovered. The byte comes out of
/// the fuzz input, which lets libFuzzer's coverage feedback steer it the same way it steers the
/// source text — a crash that only reproduces at one tag shrinks to a minimal input that names it.
#[allow(dead_code)]
pub fn dialect_from(byte: u8) -> Dialect {
    // Ordered oldest to newest, so a new variant widens the space without renumbering the old ones.
    const ALL: &[Dialect] = &[Dialect::Godot4_6, Dialect::Godot4_7];
    ALL[byte as usize % ALL.len()]
}
