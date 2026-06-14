//! M10 (#74): `textDocument/documentColor` + `textDocument/colorPresentation` for GDScript `Color`
//! literals.
//!
//! Both requests are **parse-priced** — they scan the lexer's token stream (never the fragile
//! mid-edit AST) for `Color` literal expressions and emit / round-trip color swatches. No analyzer,
//! no cross-file fan-out; served even at Hard memory pressure (like `foldingRange`).
//!
//! Three literal forms produce a swatch ([`document_color`]):
//!   * **`Color(r, g, b)` / `Color(r, g, b, a)`** with *constant numeric* args — components are
//!     0.0–1.0 floats (Godot's `Color(float, float, float[, float])` ctor; the 0–255 form is the
//!     separate `Color8` static, out of scope). Int and Float literal tokens both read straight to
//!     `f32`; a 3-arg call defaults `a = 1`.
//!   * **`Color.CONSTANT`** named colors (`Color.RED`, `Color.CORNFLOWER_BLUE`) — RGBA resolved from
//!     the native DB ([`gd_types::NativeDb::builtin_color_constant`]), which decodes it from
//!     `extension_api.json` at ingest. No server-side color table.
//!   * **`Color("#hex")` / `Color("name")`** string forms — parsed *exactly* as Godot's
//!     `Color(String)` ctor ([`parse_color_string`]): a valid 3/4/6/8-digit hex (`#` optional) goes
//!     through `html`, else a name goes through `find_named_color`'s normalization. A malformed
//!     string yields **no** swatch (never a false black swatch).
//!
//! Discrimination (no false swatch): a `Color` token preceded by `.` (a member access like
//! `foo.Color`) is skipped, and a bare `Color` that is neither called nor `.`-accessed (a variable
//! or type *use*) produces nothing. Any non-literal constructor argument (an identifier, a nested
//! call, a unary minus) bails the whole literal.
//!
//! [`color_presentation`] offers constructor form(s) for a (possibly user-edited) color whose
//! `textEdit` replaces the **whole** literal, lossless on round-trip: the float `Color(...)` form is
//! always offered (Rust's shortest-round-trip `f32` formatting guarantees re-parse equality), and a
//! `Color.NAME` form is offered only on an exact (bitwise) match against a DB constant.

use gd_syntax::{Literal, Token, TokenKind};
use lsp_types::{
    Color, ColorInformation, ColorPresentation, ColorPresentationParams, DocumentColorParams,
    Range, TextEdit,
};

use crate::position::PositionMapper;
use crate::server::ServerState;

/// `textDocument/documentColor`: a `ColorInformation` for each `Color` literal in the document.
///
/// Returns `Some(vec)` (possibly empty); never `None`, never an error — an unparseable buffer simply
/// yields whatever literals the tokenizer recovered. Parse-priced (no analyzer), so it is served at
/// Hard memory pressure.
pub fn document_color(
    state: &mut ServerState,
    params: DocumentColorParams,
) -> Option<Vec<ColorInformation>> {
    let uri = params.text_document.uri;
    let doc = state.vfs.get(uri.as_str())?;
    let text = doc.text();
    let mapper = PositionMapper::new(&doc.rope, state.encoding);

    // Token-primary scan: strings are single tokens, so hex/name scanning is string-safe and a
    // mid-edit partial AST can't desync us. The shared cached parse isn't needed (we never touch the
    // tree), but tokenizing directly keeps this independent of analysis.
    let (tokens, _errors) = gd_syntax::tokenize(&text);
    let mut out: Vec<ColorInformation> = Vec::new();
    for lit in scan_color_literals(&tokens, &state.workspace.native) {
        out.push(ColorInformation {
            range: mapper.span_to_range(gd_syntax::ByteSpan {
                start: lit.start,
                end: lit.end,
            }),
            color: lit.color,
        });
    }
    Some(out)
}

/// `textDocument/colorPresentation`: constructor form(s) for `params.color`, each replacing the
/// whole literal at `params.range`.
///
/// Always offers the float `Color(r, g, b)` (alpha == 1) or `Color(r, g, b, a)` form, which
/// re-parses to the identical color. When the color exactly matches a DB `Color` constant, also
/// offers the `Color.NAME` form (the named form is listed first so a picker that highlights the
/// first match prefers it). Never `None` — a color always has at least the float presentation.
pub fn color_presentation(
    state: &mut ServerState,
    params: ColorPresentationParams,
) -> Option<Vec<ColorPresentation>> {
    let c = params.color;
    let rgba = [c.red, c.green, c.blue, c.alpha];
    let mut presentations: Vec<ColorPresentation> = Vec::new();

    // Named form first, only on an exact (bitwise) match — lossless by construction (the constant's
    // own RGBA is what the editor picked).
    if let Some(name) = state.workspace.native.color_constant_named_exact(rgba) {
        let label = format!("Color.{name}");
        presentations.push(presentation(label, params.range));
    }

    // The float constructor — always lossless (shortest-round-trip f32 formatting).
    let label = format_color_ctor(rgba);
    presentations.push(presentation(label, params.range));

    Some(presentations)
}

/// A [`ColorPresentation`] whose `text_edit` replaces `range` with `label` (the label doubles as the
/// inserted text — the LSP default).
fn presentation(label: String, range: Range) -> ColorPresentation {
    ColorPresentation {
        text_edit: Some(TextEdit {
            range,
            new_text: label.clone(),
        }),
        label,
        additional_text_edits: None,
    }
}

/// Render `Color(...)` source for `rgba`, dropping the alpha component when it is exactly `1.0` (the
/// 3-arg ctor's default). Each component is formatted with `{}`, which prints the **shortest** string
/// that round-trips that `f32` — the property that makes [`color_presentation`] lossless. Never fixed
/// precision (which would lose bits) and never widened to `f64` first.
fn format_color_ctor(rgba: [f32; 4]) -> String {
    let [r, g, b, a] = rgba;
    if a.to_bits() == 1.0f32.to_bits() {
        format!("Color({r}, {g}, {b})")
    } else {
        format!("Color({r}, {g}, {b}, {a})")
    }
}

/// One detected `Color` literal: its byte extent (the whole expression, for the swatch `range`) and
/// decoded RGBA.
struct ColorLiteral {
    start: usize,
    end: usize,
    color: Color,
}

/// Scan the token stream for `Color` literal expressions. Pull-based over a flat token slice — the
/// only structure needed is matching the `(`…`)` of a constructor (the lexer suppresses newlines
/// inside parens, so the call's tokens are contiguous between the bracket pair on the token stream).
fn scan_color_literals(tokens: &[Token], db: &gd_types::NativeDb) -> Vec<ColorLiteral> {
    let mut out: Vec<ColorLiteral> = Vec::new();
    let mut i = 0;
    while i < tokens.len() {
        let tok = &tokens[i];
        // Anchor on the identifier `Color`. A preceding `.` makes it a member access (`foo.Color`),
        // not the global builtin — skip it.
        if tok.kind != TokenKind::Identifier || &*tok.source != "Color" {
            i += 1;
            continue;
        }
        let preceded_by_dot = i > 0 && tokens[i - 1].kind == TokenKind::Period;
        if preceded_by_dot {
            i += 1;
            continue;
        }

        match tokens.get(i + 1).map(|t| t.kind) {
            // `Color(` — a constructor call.
            Some(TokenKind::ParenthesisOpen) => {
                if let Some((lit, next)) = parse_color_ctor(tokens, i, db) {
                    out.push(lit);
                    i = next;
                    continue;
                }
            }
            // `Color.NAME` — a named constant.
            Some(TokenKind::Period) => {
                if let Some(name_tok) = tokens.get(i + 2) {
                    if name_tok.kind == TokenKind::Identifier {
                        if let Some(rgba) = db.builtin_color_constant(&name_tok.source) {
                            out.push(ColorLiteral {
                                start: tok.span.start,
                                end: name_tok.span.end,
                                color: rgba_to_color(rgba),
                            });
                            i += 3;
                            continue;
                        }
                    }
                }
            }
            _ => {}
        }
        i += 1;
    }
    out
}

/// Parse a `Color(...)` constructor starting at the `Color` identifier token index `start`
/// (`tokens[start + 1]` is `(`). Returns the detected literal and the index just past the closing
/// `)`, or `None` when the args aren't a recognizable color (then the caller advances by one and
/// keeps scanning). The whole-expression span runs from `Color` to the closing `)`.
fn parse_color_ctor(
    tokens: &[Token],
    start: usize,
    db: &gd_types::NativeDb,
) -> Option<(ColorLiteral, usize)> {
    let open = start + 1;
    // Collect the argument tokens between the matched `(` and `)`. Bail on any nested bracket — a
    // nested call/array means a non-constant argument we won't evaluate.
    let mut args: Vec<&Token> = Vec::new();
    let mut close = None;
    let mut j = open + 1;
    while j < tokens.len() {
        match tokens[j].kind {
            TokenKind::ParenthesisClose => {
                close = Some(j);
                break;
            }
            // Skip separators; a nested opener means non-constant args → bail.
            TokenKind::Comma => {}
            TokenKind::ParenthesisOpen
            | TokenKind::BracketOpen
            | TokenKind::BraceOpen
            | TokenKind::BracketClose
            | TokenKind::BraceClose => return None,
            _ => args.push(&tokens[j]),
        }
        j += 1;
    }
    let close = close?;
    let end = tokens[close].span.end;
    let span_start = tokens[start].span.start;

    let color = interpret_color_args(&args, db)?;
    Some((
        ColorLiteral {
            start: span_start,
            end,
            color,
        },
        close + 1,
    ))
}

/// Interpret the (separator-stripped) argument tokens of a `Color(...)` call as a color.
///
/// Two shapes are accepted, matching Godot's overloads:
///   * a single **string** literal → `Color(String)` (hex or name), via [`parse_color_string`];
///   * **three or four numeric** literals (Int or Float) → `Color(float, float, float[, float])`,
///     components read straight to `f32` (no 0–255 division), 3-arg defaults `a = 1`.
///
/// Anything else (an identifier arg, a `PI`/`TAU` constant, a unary-minus expression that arrives as
/// two tokens, the wrong arity) → `None`, so no swatch is emitted.
fn interpret_color_args(args: &[&Token], db: &gd_types::NativeDb) -> Option<Color> {
    // `Color(code)` / `Color(code, alpha)` string form. The single-string case is the common one;
    // `Color("#fff", 0.5)` (string + numeric alpha) is also a real overload.
    if let Some(first) = args.first() {
        if let Some(Literal::String(s)) = &first.literal {
            return match args.len() {
                1 => parse_color_string(s, db),
                2 => {
                    let a = numeric_component(args[1])?;
                    let base = parse_color_string(s, db)?;
                    Some(Color { alpha: a, ..base })
                }
                _ => None,
            };
        }
    }

    // Numeric form: 3 or 4 float/int components.
    if args.len() == 3 || args.len() == 4 {
        let r = numeric_component(args[0])?;
        let g = numeric_component(args[1])?;
        let b = numeric_component(args[2])?;
        let a = match args.get(3) {
            Some(t) => numeric_component(t)?,
            None => 1.0,
        };
        return Some(Color {
            red: r,
            green: g,
            blue: b,
            alpha: a,
        });
    }

    None
}

/// A single numeric color component from one token: an `Int` or `Float` literal, cast to `f32`. Any
/// other token (identifier, string, `PI`, …) → `None`. No 0–255 scaling — `Color(1, 0, 0)` is full
/// red, matching the `Color(float, …)` ctor.
fn numeric_component(tok: &Token) -> Option<f32> {
    match &tok.literal {
        Some(Literal::Int(n)) => Some(*n as f32),
        Some(Literal::Float(f)) => Some(*f as f32),
        _ => None,
    }
}

/// Parse a `Color(String)` argument exactly as Godot's `Color(const String &)` ctor does
/// (`core/math/color.cpp`): if the string is a valid HTML color it goes through `html`, otherwise it
/// is looked up as a named color. A string that is neither → `None` (Godot returns black + logs an
/// error at runtime; the LSP surface emits no swatch instead of a false one).
fn parse_color_string(s: &str, db: &gd_types::NativeDb) -> Option<Color> {
    if let Some(rgba) = html_color(s) {
        return Some(rgba_to_color(rgba));
    }
    // Named form: resolved against the DB's `Color` constants with Godot's name normalization, so
    // the values flow from `extension_api.json` — no server-side color table.
    db.color_constant_by_display_name(s).map(rgba_to_color)
}

/// Faithful port of `Color::html` + `Color::html_is_valid` (`core/math/color.cpp`): a `#`-optional
/// string of **3, 4, 6, or 8** hex digits → RGBA, where 3/4 digits are `#rgb[a]` (each nibble /15)
/// and 6/8 digits are `#rrggbb[aa]` (each byte /255). Any other length, or a non-hex digit, → `None`
/// (Godot's `ERR_FAIL`). Alpha defaults to `1.0` for the 3- and 6-digit forms.
fn html_color(s: &str) -> Option<[f32; 4]> {
    let bytes = s.as_bytes();
    let pos = usize::from(bytes.first() == Some(&b'#'));
    let digits = &bytes[pos..];
    let n = digits.len();
    if !matches!(n, 3 | 4 | 6 | 8) {
        return None;
    }
    // Every remaining char must be a hex digit.
    let nib = |b: u8| -> Option<f32> {
        match b {
            b'0'..=b'9' => Some((b - b'0') as f32),
            b'a'..=b'f' => Some((b - b'a' + 10) as f32),
            b'A'..=b'F' => Some((b - b'A' + 10) as f32),
            _ => None,
        }
    };
    match n {
        3 | 4 => {
            let r = nib(digits[0])? / 15.0;
            let g = nib(digits[1])? / 15.0;
            let b = nib(digits[2])? / 15.0;
            let a = if n == 4 { nib(digits[3])? / 15.0 } else { 1.0 };
            Some([r, g, b, a])
        }
        6 | 8 => {
            let byte =
                |hi: u8, lo: u8| -> Option<f32> { Some((nib(hi)? * 16.0 + nib(lo)?) / 255.0) };
            let r = byte(digits[0], digits[1])?;
            let g = byte(digits[2], digits[3])?;
            let b = byte(digits[4], digits[5])?;
            let a = if n == 8 {
                byte(digits[6], digits[7])?
            } else {
                1.0
            };
            Some([r, g, b, a])
        }
        _ => unreachable!("length pre-checked to be 3/4/6/8"),
    }
}

/// RGBA array → an LSP [`Color`].
fn rgba_to_color(rgba: [f32; 4]) -> Color {
    Color {
        red: rgba[0],
        green: rgba[1],
        blue: rgba[2],
        alpha: rgba[3],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bits(c: [f32; 4]) -> [u32; 4] {
        c.map(f32::to_bits)
    }

    #[test]
    fn html_six_and_eight_digit_with_and_without_hash() {
        // #ff8800 → (1, 136/255, 0, 1).
        let want6 = [1.0, 136.0 / 255.0, 0.0, 1.0];
        assert_eq!(bits(html_color("#ff8800").unwrap()), bits(want6));
        assert_eq!(
            bits(html_color("ff8800").unwrap()),
            bits(want6),
            "the leading # is optional"
        );
        // #ff8800cc → adds alpha 204/255.
        let want8 = [1.0, 136.0 / 255.0, 0.0, 204.0 / 255.0];
        assert_eq!(bits(html_color("#ff8800cc").unwrap()), bits(want8));
        assert_eq!(
            bits(html_color("FF8800CC").unwrap()),
            bits(want8),
            "case-insensitive"
        );
    }

    #[test]
    fn html_three_and_four_digit_short_forms() {
        // #f80 → each nibble /15.
        let want3 = [15.0 / 15.0, 8.0 / 15.0, 0.0, 1.0];
        assert_eq!(bits(html_color("#f80").unwrap()), bits(want3));
        // #f80c → adds alpha c/15.
        let want4 = [15.0 / 15.0, 8.0 / 15.0, 0.0, 12.0 / 15.0];
        assert_eq!(bits(html_color("f80c").unwrap()), bits(want4));
    }

    #[test]
    fn html_rejects_malformed() {
        assert_eq!(html_color(""), None);
        assert_eq!(html_color("#"), None);
        assert_eq!(html_color("#ff"), None, "2 digits is not a valid length");
        assert_eq!(html_color("#ff880"), None, "5 digits is not a valid length");
        assert_eq!(
            html_color("#ff8800c"),
            None,
            "7 digits is not a valid length"
        );
        assert_eq!(html_color("#gggggg"), None, "non-hex digit");
        assert_eq!(html_color("notacolor"), None);
        assert_eq!(
            html_color("#ff88zz"),
            None,
            "non-hex in a valid-length string"
        );
    }

    #[test]
    fn format_ctor_drops_alpha_when_one_and_round_trips() {
        assert_eq!(
            format_color_ctor([0.2, 0.4, 0.6, 1.0]),
            "Color(0.2, 0.4, 0.6)"
        );
        assert_eq!(
            format_color_ctor([1.0, 0.0, 0.0, 0.5]),
            "Color(1, 0, 0, 0.5)",
            "alpha != 1 keeps the 4-arg form"
        );
        // Shortest-round-trip: the formatted text parses back to the identical f32.
        let c = 136.0f32 / 255.0;
        let s = format_color_ctor([c, c, c, 1.0]);
        let inner = s.strip_prefix("Color(").unwrap().strip_suffix(')').unwrap();
        let first: f32 = inner.split(',').next().unwrap().trim().parse().unwrap();
        assert_eq!(
            first.to_bits(),
            c.to_bits(),
            "f32 round-trips through the format"
        );
    }
}
