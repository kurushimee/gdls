//! Conversion between gdls's internal **byte offsets** and LSP `Position`s.
//!
//! Three coordinate spaces exist in this project and must never be confused:
//!   * **byte offsets** — what the frontend (`gd_syntax`) uses internally;
//!   * Godot's tab-expanded UTF-32 `(line, column)` — used only for `.out` message fidelity;
//!   * LSP `Position` — UTF-16 by default, or UTF-8/UTF-32 if negotiated.
//!
//! This module owns the byte ↔ LSP conversion at the protocol boundary. Every conversion is
//! clamped so out-of-range client input can never panic.

use gd_syntax::ByteSpan;
use lsp_types::{ClientCapabilities, Position, PositionEncodingKind, Range};
use ropey::Rope;

/// The position encoding negotiated with the client.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PositionEncoding {
    Utf8,
    Utf16,
    Utf32,
}

impl PositionEncoding {
    /// Pick an encoding from the client's offered list. Preference order is UTF-8 (cheapest, since
    /// our internals are byte-based) → UTF-16 (the LSP default every server must support) → UTF-32.
    /// If the client offers nothing, UTF-16 is assumed per the LSP spec.
    pub fn negotiate(caps: &ClientCapabilities) -> Self {
        let offered = caps
            .general
            .as_ref()
            .and_then(|g| g.position_encodings.as_ref());
        if let Some(list) = offered {
            if list.contains(&PositionEncodingKind::UTF8) {
                return Self::Utf8;
            }
            if list.contains(&PositionEncodingKind::UTF16) {
                return Self::Utf16;
            }
            if list.contains(&PositionEncodingKind::UTF32) {
                return Self::Utf32;
            }
        }
        Self::Utf16
    }

    /// The LSP `PositionEncodingKind` to advertise back in `ServerCapabilities`.
    pub fn to_kind(self) -> PositionEncodingKind {
        match self {
            Self::Utf8 => PositionEncodingKind::UTF8,
            Self::Utf16 => PositionEncodingKind::UTF16,
            Self::Utf32 => PositionEncodingKind::UTF32,
        }
    }
}

/// Converts byte offsets to/from LSP positions against a specific document rope + encoding.
///
/// Construct one per request from the *current* rope so it never holds stale state.
pub struct PositionMapper<'a> {
    rope: &'a Rope,
    enc: PositionEncoding,
}

impl<'a> PositionMapper<'a> {
    pub fn new(rope: &'a Rope, enc: PositionEncoding) -> Self {
        Self { rope, enc }
    }

    /// Byte offset → LSP `Position`. Clamps to the end of the document.
    pub fn byte_to_position(&self, byte: usize) -> Position {
        let byte = byte.min(self.rope.len_bytes());
        let line = self.rope.byte_to_line(byte);
        let line_start_byte = self.rope.line_to_byte(line);
        let line_slice = self.rope.line(line);
        let character = match self.enc {
            PositionEncoding::Utf8 => byte - line_start_byte,
            PositionEncoding::Utf16 => {
                let char_in_line =
                    self.rope.byte_to_char(byte) - self.rope.byte_to_char(line_start_byte);
                line_slice.char_to_utf16_cu(char_in_line)
            }
            PositionEncoding::Utf32 => {
                self.rope.byte_to_char(byte) - self.rope.byte_to_char(line_start_byte)
            }
        };
        Position {
            line: line as u32,
            character: character as u32,
        }
    }

    /// LSP `Position` → byte offset. Clamps every component so malformed client positions are safe.
    pub fn position_to_byte(&self, pos: Position) -> usize {
        let line = (pos.line as usize).min(self.rope.len_lines().saturating_sub(1));
        let line_start_byte = self.rope.line_to_byte(line);
        let line_slice = self.rope.line(line);
        let char_in_line = match self.enc {
            PositionEncoding::Utf8 => {
                let off = (pos.character as usize).min(line_slice.len_bytes());
                return (line_start_byte + off).min(self.rope.len_bytes());
            }
            PositionEncoding::Utf16 => {
                let cu = (pos.character as usize).min(line_slice.len_utf16_cu());
                line_slice.utf16_cu_to_char(cu)
            }
            PositionEncoding::Utf32 => (pos.character as usize).min(line_slice.len_chars()),
        };
        let line_start_char = self.rope.byte_to_char(line_start_byte);
        self.rope.char_to_byte(line_start_char + char_in_line)
    }

    /// Byte span → LSP `Range`.
    pub fn span_to_range(&self, span: ByteSpan) -> Range {
        Range {
            start: self.byte_to_position(span.start),
            end: self.byte_to_position(span.end),
        }
    }

    /// Byte span → LSP `Range` for a **diagnostic**, where a range ending on a line break stays on
    /// the line that break closes.
    ///
    /// Godot's tokenizer gives a NEWLINE token the columns `column - 1 .. column` *after* `column`
    /// has moved past the `\n` (`gdscript_tokenizer.cpp`'s `newline`), so a diagnostic that ends
    /// on one ends at the character just past the line's last one — one column beyond the text,
    /// which every client clamps back to the line end. gdls carries the same token as the byte
    /// span `[offset_of_newline, offset_of_newline + 1)`, and that end byte is the *first byte of
    /// the next line*, so the plain conversion reports `(line + 1, 0)` and the editor highlights a
    /// whole extra line of leading whitespace. Only a token that spans the break can land there,
    /// so the rewrite touches nothing else.
    pub fn diagnostic_span_to_range(&self, span: ByteSpan) -> Range {
        Range {
            start: self.byte_to_position(span.start),
            end: self.diagnostic_end_position(span),
        }
    }

    fn diagnostic_end_position(&self, span: ByteSpan) -> Position {
        let end = span.end;
        if end <= span.start || end > self.rope.len_bytes() || self.rope.byte(end - 1) != b'\n' {
            return self.byte_to_position(end);
        }
        let line = self.rope.byte_to_line(end - 1);
        let line_slice = self.rope.line(line);
        // The line slice carries its own terminator, so its length in client units is already the
        // "one past the last character" column Godot reports.
        let character = match self.enc {
            PositionEncoding::Utf8 => line_slice.len_bytes(),
            PositionEncoding::Utf16 => line_slice.len_utf16_cu(),
            PositionEncoding::Utf32 => line_slice.len_chars(),
        };
        Position {
            line: line as u32,
            character: character as u32,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn round_trip(text: &str, enc: PositionEncoding) {
        let rope = Rope::from_str(text);
        let mapper = PositionMapper::new(&rope, enc);
        // Iterate genuine char boundaries (LSP positions can't address mid-codepoint, and the
        // tokenizer only ever emits spans on boundaries). `char_to_byte` guarantees a boundary;
        // note `try_byte_to_char` does NOT — it rounds a mid-codepoint byte down to its char.
        for char_idx in 0..=rope.len_chars() {
            let byte = rope.char_to_byte(char_idx);
            let pos = mapper.byte_to_position(byte);
            let back = mapper.position_to_byte(pos);
            assert_eq!(
                byte, back,
                "round-trip failed at char {char_idx} (byte {byte}) in {enc:?}"
            );
        }
    }

    #[test]
    fn a_diagnostic_ending_on_a_newline_stays_on_that_line() {
        // #595. `\tpass\n` is 5 characters; Godot reports the end one past them, at 6.
        let text = "func f() -> int:\n\tpass\nfunc g():\n\tpass\n";
        let rope = Rope::from_str(text);
        let mapper = PositionMapper::new(&rope, PositionEncoding::Utf16);
        let end_of_body = text.find("func g").expect("fixture has a second function");
        let span = ByteSpan::new(0, end_of_body);
        assert_eq!(
            mapper.span_to_range(span).end,
            Position {
                line: 2,
                character: 0
            }
        );
        assert_eq!(
            mapper.diagnostic_span_to_range(span).end,
            Position {
                line: 1,
                character: 6
            }
        );
    }

    #[test]
    fn the_newline_rule_counts_in_the_client_encoding() {
        let text = "var s := \"café 🎮\"\nvar t := 1\n";
        let rope = Rope::from_str(text);
        let end = text.find("var t").expect("fixture has a second line");
        let span = ByteSpan::new(0, end);
        let ends = |enc| {
            PositionMapper::new(&rope, enc)
                .diagnostic_span_to_range(span)
                .end
        };
        // 17 characters on line 0, so 18 in UTF-32 and 19 in UTF-16 (the emoji is a surrogate
        // pair), each counting the line break as the one past the last character.
        assert_eq!(ends(PositionEncoding::Utf32).character, 18);
        assert_eq!(ends(PositionEncoding::Utf16).character, 19);
        assert_eq!(ends(PositionEncoding::Utf8).character, 22);
        assert!(ends(PositionEncoding::Utf16).line == 0);
    }

    #[test]
    fn a_body_ending_on_the_last_newline_of_the_file_stays_on_its_line() {
        let text = "extends Node\nfunc f() -> int:\n\tpass\n";
        let rope = Rope::from_str(text);
        let mapper = PositionMapper::new(&rope, PositionEncoding::Utf16);
        let span = ByteSpan::new(
            text.find("func").expect("fixture has a function"),
            text.len(),
        );
        assert_eq!(
            mapper.diagnostic_span_to_range(span).end,
            Position {
                line: 2,
                character: 6
            }
        );
    }

    #[test]
    fn an_ordinary_diagnostic_range_is_untouched() {
        let text = "func f() -> int:
	pass
";
        let rope = Rope::from_str(text);
        let mapper = PositionMapper::new(&rope, PositionEncoding::Utf16);
        let span = ByteSpan::new(5, 6);
        assert_eq!(
            mapper.diagnostic_span_to_range(span),
            mapper.span_to_range(span)
        );
        // An empty span sitting at a line start belongs to that line, not the one before it.
        let at_line_start = ByteSpan::new(17, 17);
        assert_eq!(
            mapper.diagnostic_span_to_range(at_line_start),
            mapper.span_to_range(at_line_start)
        );
    }

    #[test]
    fn ascii_round_trips_in_all_encodings() {
        let text = "extends Node\nfunc _ready():\n\tprint(1)\n";
        round_trip(text, PositionEncoding::Utf8);
        round_trip(text, PositionEncoding::Utf16);
        round_trip(text, PositionEncoding::Utf32);
    }

    #[test]
    fn multibyte_round_trips_in_all_encodings() {
        // "café" (é = 2 UTF-8 bytes, 1 UTF-16 unit) and an emoji (4 UTF-8 bytes, 2 UTF-16 units).
        let text = "var s := \"café 🎮\"\n";
        round_trip(text, PositionEncoding::Utf8);
        round_trip(text, PositionEncoding::Utf16);
        round_trip(text, PositionEncoding::Utf32);
    }

    #[test]
    fn out_of_range_position_is_clamped_not_panicked() {
        let rope = Rope::from_str("ab\n");
        let mapper = PositionMapper::new(&rope, PositionEncoding::Utf16);
        let b = mapper.position_to_byte(Position {
            line: 999,
            character: 999,
        });
        assert!(b <= rope.len_bytes());
    }
}
