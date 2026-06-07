//! Tokens — a 1:1 port of `GDScriptTokenizer::Token` from Godot
//! (`modules/gdscript/gdscript_tokenizer.h`, `.cpp`).
//!
//! [`TokenKind`] mirrors `Token::Type` variant-for-variant and in the same order, so the discriminant
//! doubles as the index into [`TOKEN_NAMES`] (the port of Godot's `token_names[]`). Parser error
//! messages embed these names verbatim, so they must match byte-for-byte.

use crate::span::{ByteSpan, LineColRange};

/// The kind of a token. Ordered identically to Godot's `GDScriptTokenizer::Token::Type`
/// (`EMPTY`..`TK_EOF`); Godot's trailing `TK_MAX` sentinel is represented by [`TOKEN_COUNT`].
///
/// Only three names differ from the C++ to avoid Rust keywords/reserved words: `TK_CONST` →
/// [`TokenKind::Const`], `TK_IN` → [`TokenKind::In`], `TK_VOID` → [`TokenKind::Void`] (these were
/// already `TK_`-prefixed upstream to avoid WinAPI clashes), and `SELF` → [`TokenKind::SelfKw`]
/// (`Self` is reserved in Rust).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum TokenKind {
    Empty,
    // Basic
    Annotation,
    Identifier,
    Literal,
    // Comparison
    Less,
    LessEqual,
    Greater,
    GreaterEqual,
    EqualEqual,
    BangEqual,
    // Logical
    And,
    Or,
    Not,
    AmpersandAmpersand,
    PipePipe,
    Bang,
    // Bitwise
    Ampersand,
    Pipe,
    Tilde,
    Caret,
    LessLess,
    GreaterGreater,
    // Math
    Plus,
    Minus,
    Star,
    StarStar,
    Slash,
    Percent,
    // Assignment
    Equal,
    PlusEqual,
    MinusEqual,
    StarEqual,
    StarStarEqual,
    SlashEqual,
    PercentEqual,
    LessLessEqual,
    GreaterGreaterEqual,
    AmpersandEqual,
    PipeEqual,
    CaretEqual,
    // Control flow
    If,
    Elif,
    Else,
    For,
    While,
    Break,
    Continue,
    Pass,
    Return,
    Match,
    When,
    // Keywords
    As,
    Assert,
    Await,
    Breakpoint,
    Class,
    ClassName,
    Const, // TK_CONST
    Enum,
    Extends,
    Func,
    In, // TK_IN
    Is,
    Namespace,
    Preload,
    SelfKw, // SELF
    Signal,
    Static,
    Super,
    Trait,
    Var,
    Void, // TK_VOID
    Yield,
    // Punctuation
    BracketOpen,
    BracketClose,
    BraceOpen,
    BraceClose,
    ParenthesisOpen,
    ParenthesisClose,
    Comma,
    Semicolon,
    Period,
    PeriodPeriod,
    PeriodPeriodPeriod,
    Colon,
    Dollar,
    ForwardArrow,
    Underscore,
    // Whitespace
    Newline,
    Indent,
    Dedent,
    // Constants
    ConstPi,
    ConstTau,
    ConstInf,
    ConstNan,
    // Error message improvement
    VcsConflictMarker,
    Backtick,
    QuestionMark,
    // Special
    Error,
    Eof, // TK_EOF
}

/// Number of real token kinds (`EMPTY`..`TK_EOF`), i.e. Godot's `TK_MAX`.
pub const TOKEN_COUNT: usize = 100;

// The enum must stay dense `0..TOKEN_COUNT` so `kind as usize` indexes [`TOKEN_NAMES`].
const _: () = assert!(TokenKind::Eof as usize == TOKEN_COUNT - 1);

/// Display names, ported verbatim from `token_names[]`. Indexed by `TokenKind as usize`.
pub const TOKEN_NAMES: [&str; TOKEN_COUNT] = [
    "Empty",
    // Basic
    "Annotation",
    "Identifier",
    "Literal",
    // Comparison
    "<",
    "<=",
    ">",
    ">=",
    "==",
    "!=",
    // Logical
    "and",
    "or",
    "not",
    "&&",
    "||",
    "!",
    // Bitwise
    "&",
    "|",
    "~",
    "^",
    "<<",
    ">>",
    // Math
    "+",
    "-",
    "*",
    "**",
    "/",
    "%",
    // Assignment
    "=",
    "+=",
    "-=",
    "*=",
    "**=",
    "/=",
    "%=",
    "<<=",
    ">>=",
    "&=",
    "|=",
    "^=",
    // Control flow
    "if",
    "elif",
    "else",
    "for",
    "while",
    "break",
    "continue",
    "pass",
    "return",
    "match",
    "when",
    // Keywords
    "as",
    "assert",
    "await",
    "breakpoint",
    "class",
    "class_name",
    "const",
    "enum",
    "extends",
    "func",
    "in",
    "is",
    "namespace",
    "preload",
    "self",
    "signal",
    "static",
    "super",
    "trait",
    "var",
    "void",
    "yield",
    // Punctuation
    "[",
    "]",
    "{",
    "}",
    "(",
    ")",
    ",",
    ";",
    ".",
    "..",
    "...",
    ":",
    "$",
    "->",
    "_",
    // Whitespace
    "Newline",
    "Indent",
    "Dedent",
    // Constants
    "PI",
    "TAU",
    "INF",
    "NaN",
    // Error message improvement
    "VCS conflict marker",
    "`",
    "?",
    // Special
    "Error",
    "End of file",
];

impl TokenKind {
    /// The Godot display name for this token (`Token::get_name()` / `token_names[]`).
    pub fn name(self) -> &'static str {
        TOKEN_NAMES[self as usize]
    }

    /// Whether this token can appear immediately before a binary operator. Ported from
    /// `Token::can_precede_bin_op()`; the lexer uses it to decide whether `+`/`-` before a digit
    /// starts a number (e.g. `= -3`) or is a binary operator (e.g. `x - 3`).
    pub fn can_precede_bin_op(self) -> bool {
        use TokenKind::*;
        matches!(
            self,
            Identifier
                | Literal
                | SelfKw
                | BracketClose
                | BraceClose
                | ParenthesisClose
                | ConstPi
                | ConstTau
                | ConstInf
                | ConstNan
        )
    }

    /// Whether this token may be treated as a regular identifier. Ported from
    /// `Token::is_identifier()` — a few keywords already on the engine API are allowed.
    pub fn is_identifier(self) -> bool {
        use TokenKind::*;
        matches!(
            self,
            Identifier | Match | When | ConstPi | ConstInf | ConstNan | ConstTau
        )
    }

    /// Whether this token may follow `$`/`%` as a node-path segment. Ported from
    /// `Token::is_node_name()` — allows most keywords in the `$` notation but not as general
    /// identifiers.
    pub fn is_node_name(self) -> bool {
        use TokenKind::*;
        matches!(
            self,
            Identifier
                | And
                | As
                | Assert
                | Await
                | Break
                | Breakpoint
                | ClassName
                | Class
                | Const
                | ConstPi
                | ConstInf
                | ConstNan
                | ConstTau
                | Continue
                | Elif
                | Else
                | Enum
                | Extends
                | For
                | Func
                | If
                | In
                | Is
                | Match
                | Namespace
                | Not
                | Or
                | Pass
                | Preload
                | Return
                | SelfKw
                | Signal
                | Static
                | Super
                | Trait
                | Underscore
                | Var
                | Void
                | While
                | When
                | Yield
        )
    }
}

/// The decoded value of a `LITERAL` token. Godot stores this as a `Variant`; the frontend only ever
/// produces these constant kinds from the lexer. The three string flavors mirror Godot's
/// `String` / `StringName` (`&"…"`) / `NodePath` (`^"…"`) literal types.
#[derive(Clone, Debug, PartialEq)]
pub enum Literal {
    Int(i64),
    Float(f64),
    String(String),
    StringName(String),
    NodePath(String),
    Bool(bool),
    Null,
}

/// A single token. Adds a [`ByteSpan`] (for LSP range mapping) alongside Godot's `(line, column)`
/// extents and the raw lexeme.
#[derive(Clone, Debug)]
pub struct Token {
    pub kind: TokenKind,
    /// Byte offsets into the source (gdls addition, for LSP positions).
    pub span: ByteSpan,
    /// Godot-faithful 1-based line/column extents (for `.out` diagnostic fidelity).
    pub loc: LineColRange,
    /// The decoded literal value, for `LITERAL` tokens.
    pub literal: Option<Literal>,
    /// The exact source text of the token (Godot's `Token::source`). For `ERROR` tokens this instead
    /// carries the diagnostic message (Godot keeps it in `Token::literal`), so the parser pairs each
    /// emitted error token with its own message.
    pub source: Box<str>,
}

impl Token {
    pub fn name(&self) -> &'static str {
        self.kind.name()
    }

    /// Human-readable name for diagnostics (`Token::get_debug_name`): an identifier shows its source
    /// text, every other token shows its quoted name.
    pub fn debug_name(&self) -> String {
        if self.kind == TokenKind::Identifier {
            format!(r#"identifier "{}""#, self.source)
        } else {
            format!(r#""{}""#, self.kind.name())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_count_matches_godot() {
        // EMPTY..TK_EOF == 100 kinds (Godot's TK_MAX), and the names table is parallel.
        assert_eq!(TOKEN_COUNT, 100);
        assert_eq!(TOKEN_NAMES.len(), TOKEN_COUNT);
        assert_eq!(TokenKind::Empty as usize, 0);
        assert_eq!(TokenKind::Eof as usize, 99);
    }

    #[test]
    fn names_match_the_godot_table() {
        assert_eq!(TokenKind::Less.name(), "<");
        assert_eq!(TokenKind::StarStarEqual.name(), "**=");
        assert_eq!(TokenKind::Const.name(), "const");
        assert_eq!(TokenKind::SelfKw.name(), "self");
        assert_eq!(TokenKind::Underscore.name(), "_");
        assert_eq!(TokenKind::VcsConflictMarker.name(), "VCS conflict marker");
        assert_eq!(TokenKind::Eof.name(), "End of file");
    }
}
