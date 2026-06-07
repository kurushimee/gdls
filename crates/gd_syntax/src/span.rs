//! Source positions. Two coordinate spaces live here, deliberately kept distinct:
//!   * [`ByteSpan`] — byte offsets into the source UTF-8 text. This is what the rest of the
//!     frontend uses; the LSP layer converts it to UTF-16/UTF-8 positions at the protocol boundary.
//!   * [`LineColRange`] — Godot's tokenizer-faithful 1-based `(line, column)`, where `column`
//!     counts **tab-expanded UTF-32** units (Godot widens a tab to `tab_size` columns). This exists
//!     only to reproduce Godot's diagnostic positions for `.out` conformance.

/// A half-open byte range `[start, end)` into the source text.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct ByteSpan {
    pub start: usize,
    pub end: usize,
}

impl ByteSpan {
    pub fn new(start: usize, end: usize) -> Self {
        Self { start, end }
    }

    pub fn len(self) -> usize {
        self.end.saturating_sub(self.start)
    }

    pub fn is_empty(self) -> bool {
        self.start >= self.end
    }
}

/// A 1-based `(line, column)` position, matching Godot's tokenizer. `column` counts tab-expanded
/// UTF-32 code points.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LineCol {
    pub line: u32,
    pub column: u32,
}

impl LineCol {
    pub fn new(line: u32, column: u32) -> Self {
        Self { line, column }
    }
}

/// A `(start, end)` pair of [`LineCol`] positions — the Godot-faithful span of a token or diagnostic.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LineColRange {
    pub start: LineCol,
    pub end: LineCol,
}
