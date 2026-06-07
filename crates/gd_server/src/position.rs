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
