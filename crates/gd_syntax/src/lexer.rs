//! The lexer — a faithful port of `GDScriptTokenizerText` (`modules/gdscript/gdscript_tokenizer.cpp`).
//!
//! It is **pull-based** like Godot's: the parser calls [`Lexer::scan`] one token at a time and toggles
//! [`Lexer::set_multiline_mode`] / [`Lexer::push_expression_indented_block`] as it enters and leaves
//! `()[]{}` and lambda bodies. This coupling is load-bearing — newline/indent suppression inside
//! brackets is driven by the parser, so the lexer cannot be a standalone pre-pass.
//!
//! Godot operates on UTF-32 (`char32_t`); we operate on a `Vec<char>` (one entry per code point) and
//! keep a parallel byte-offset table so every token also carries a [`ByteSpan`] for LSP ranges.
//! `column` is 1-based and matches Godot's tokenizer for `(line, column)` fidelity. Under 4.6 a
//! tab widens it to `TAB_SIZE` columns; under 4.7 a tab is one column (see the `DIALECT(4.7)`
//! notes in `check_indent` / `skip_whitespace`). Either way this space exists only for `.out`
//! message fidelity — LSP positions are derived from byte spans, never from here.

use crate::dialect::Dialect;
use crate::span::{ByteSpan, LineCol, LineColRange};
use crate::token::{Literal, Token, TokenKind};

/// Godot's `tab_size`. Under both dialects a tab is worth this many *indent* units; under 4.6 it
/// additionally widens the reported `column` by this much.
const TAB_SIZE: u32 = 4;

// --- Character classification, ported from `core/string/char_utils.h`. ---

fn is_digit(c: char) -> bool {
    c.is_ascii_digit()
}
fn is_hex_digit(c: char) -> bool {
    c.is_ascii_hexdigit()
}
fn is_binary_digit(c: char) -> bool {
    c == '0' || c == '1'
}
fn is_underscore(c: char) -> bool {
    c == '_'
}

/// `is_unicode_identifier_start` — XID_Start plus `_` (Godot's `xid_start` table includes the
/// underscore; the standard property does not).
fn is_identifier_start(c: char) -> bool {
    c == '_' || unicode_ident::is_xid_start(c)
}
/// `is_unicode_identifier_continue` — XID_Continue (already includes `_`).
fn is_identifier_continue(c: char) -> bool {
    unicode_ident::is_xid_continue(c)
}

/// `is_whitespace` from `char_utils.h` (the Unicode whitespace set Godot recognizes).
fn is_whitespace(c: char) -> bool {
    matches!(c as u32,
        0x09..=0x0d | 0x20 | 0x85 | 0xa0 | 0x1680 | 0x2000..=0x200b | 0x2028 | 0x2029 | 0x202f | 0x205f | 0x3000)
}

/// WP-F4: the keyword set Godot's tokenizer scans when checking a non-ASCII identifier for
/// visual similarity to a reserved word (`gdscript_tokenizer.cpp:585-602`'s `potential_identifier`
/// confusable branch passes `keyword_list` to `TS->is_confusable`). Godot's `keyword_list` is
/// built by `make_keyword_list` (`gdscript_tokenizer.cpp:552`) **exclusively** from the `KEYWORDS`
/// macro (`:486`, ending at `TAU`); `true`/`false`/`null` are special literals handled *after* the
/// keyword switch (`:628-639`) and are deliberately NOT in `keyword_list`, so they must not appear
/// here — including them would over-report a confusable where Godot stays silent. Kept as a
/// single source-order list so adding a future keyword only requires one edit here.
const KEYWORDS_FOR_SIMILARITY: &[&str] = &[
    "as",
    "and",
    "assert",
    "await",
    "break",
    "breakpoint",
    "class",
    "class_name",
    "const",
    "continue",
    "elif",
    "else",
    "enum",
    "extends",
    "for",
    "func",
    "if",
    "in",
    "is",
    "match",
    "namespace",
    "not",
    "or",
    "pass",
    "preload",
    "return",
    "self",
    "signal",
    "static",
    "super",
    "trait",
    "var",
    "void",
    "while",
    "when",
    "yield",
    "INF",
    "NAN",
    "PI",
    "TAU",
];

/// Keyword length bounds, mirroring Godot's `MIN_KEYWORD_LENGTH` / `MAX_KEYWORD_LENGTH`
/// (`gdscript_tokenizer.cpp:551-552`). An identifier whose character length is outside this range
/// cannot match any keyword, so Godot returns it as a plain identifier *before* the keyword
/// switch and the confusable check (the `len < MIN || len > MAX` gate at
/// `gdscript_tokenizer.cpp:585`, ahead of the `is_confusable` check at :597).
const MIN_KEYWORD_LENGTH: usize = 2;
const MAX_KEYWORD_LENGTH: usize = 10;

/// Look up the GDScript keyword that `name` is visually confusable with, or `None` if no keyword
/// shares its UTS #39 skeleton. Computed once per process (and amortized across thousands of files
/// in a long-running LSP session) via `OnceLock` — the per-call cost is just one
/// `skeleton(name)` plus a linear scan of the ~43-entry pre-skeletoned keyword set.
fn keyword_skeleton_lookup(name: &str) -> Option<&'static str> {
    use std::sync::OnceLock;
    static TABLE: OnceLock<Vec<(String, &'static str)>> = OnceLock::new();
    let table = TABLE.get_or_init(|| {
        KEYWORDS_FOR_SIMILARITY
            .iter()
            .map(|&kw| (unicode_security::skeleton(kw).collect::<String>(), kw))
            .collect()
    });
    let name_skel: String = unicode_security::skeleton(name).collect();
    table
        .iter()
        .find(|(skel, _)| skel == &name_skel)
        .map(|(_, kw)| *kw)
}

/// Maps a keyword string to its token kind (the `KEYWORDS` macro). `true`/`false`/`null` are
/// intentionally absent — they are handled as literals after this lookup fails.
fn keyword_kind(name: &str) -> Option<TokenKind> {
    use TokenKind::*;
    Some(match name {
        "as" => As,
        "and" => And,
        "assert" => Assert,
        "await" => Await,
        "break" => Break,
        "breakpoint" => Breakpoint,
        "class" => Class,
        "class_name" => ClassName,
        "const" => Const,
        "continue" => Continue,
        "elif" => Elif,
        "else" => Else,
        "enum" => Enum,
        "extends" => Extends,
        "for" => For,
        "func" => Func,
        "if" => If,
        "in" => In,
        "is" => Is,
        "match" => Match,
        "namespace" => Namespace,
        "not" => Not,
        "or" => Or,
        "pass" => Pass,
        "preload" => Preload,
        "return" => Return,
        "self" => SelfKw,
        "signal" => Signal,
        "static" => Static,
        "super" => Super,
        "trait" => Trait,
        "var" => Var,
        "void" => Void,
        "while" => While,
        "when" => When,
        "yield" => Yield,
        "INF" => ConstInf,
        "NAN" => ConstNan,
        "PI" => ConstPi,
        "TAU" => ConstTau,
        _ => return None,
    })
}

/// A lexical error: a Godot tokenizer error message with its source span. Collected in detection
/// order (so `errors[0]` is the first error, matching Godot's "first parser error").
#[derive(Clone, Debug)]
pub struct LexError {
    pub span: ByteSpan,
    pub loc: LineColRange,
    pub message: String,
}

/// One recorded comment (M7 #62) — the side-channel mirror of Godot's
/// `GDScriptTokenizer::CommentData` (`gdscript_tokenizer.h:188`, recorded at
/// `gdscript_tokenizer.cpp:1208` / `:1339` under `TOOLS_ENABLED`). The token stream is
/// untouched: comments stay invisible to the ported grammar (both conformance ratchets see
/// identical tokens), and the text is sliced from the source by span on demand instead of
/// being built char-by-char in the hot loop.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CommentData {
    /// Byte range of the comment, from the `#` to (exclusive) the line's `\n`/EOF.
    pub span: ByteSpan,
    /// `true`: the comment starts at the beginning of the line or after indentation only.
    /// `false`: inline (after some code) — Godot's `CommentData::new_line`.
    pub new_line: bool,
}

/// The GDScript lexer. Drive it with [`Lexer::scan`] until it returns [`TokenKind::Eof`].
pub struct Lexer {
    /// The Godot feature release whose tokenizer semantics are in force. See [`Dialect`] for the
    /// `DIALECT(...)` guard convention.
    dialect: Dialect,
    chars: Vec<char>,
    /// `byte_offsets[i]` is the byte offset of `chars[i]`; `byte_offsets[len]` is the total length.
    byte_offsets: Vec<usize>,
    pos: usize,
    line: u32,
    column: u32,

    // Start markers for the in-progress token.
    start_pos: usize,
    start_line: u32,
    start_column: u32,

    line_continuation: bool,
    multiline_mode: bool,
    pending_newline: bool,
    last_newline: Option<Token>,
    last_token: TokenKind,
    pending_indents: i32,
    indent_stack: Vec<i32>,
    indent_stack_stack: Vec<Vec<i32>>,
    paren_stack: Vec<char>,
    indent_char: char, // '\0' until the first indentation character is seen.

    error_stack: Vec<Token>,
    pub errors: Vec<LexError>,
    /// M7 (#62): comments keyed by 1-based line, mirroring Godot's `HashMap<int, CommentData>`
    /// (one comment per line — a later comment on the same line overwrites, as upstream's
    /// `comments[line] =` does). Consumed post-parse by `doc_comments::associate`.
    pub comments: std::collections::HashMap<u32, CommentData>,
}

impl Lexer {
    /// A lexer at [`Dialect::DEFAULT`]. Prefer [`Self::new_with_dialect`] anywhere the project's
    /// dialect is known.
    pub fn new(source: &str) -> Self {
        Self::new_with_dialect(source, Dialect::DEFAULT)
    }

    pub fn new_with_dialect(source: &str, dialect: Dialect) -> Self {
        let chars: Vec<char> = source.chars().collect();
        let mut byte_offsets = Vec::with_capacity(chars.len() + 1);
        let mut acc = 0usize;
        for &c in &chars {
            byte_offsets.push(acc);
            acc += c.len_utf8();
        }
        byte_offsets.push(acc);
        Self {
            dialect,
            chars,
            byte_offsets,
            pos: 0,
            line: 1,
            column: 1,
            start_pos: 0,
            start_line: 1,
            start_column: 1,
            line_continuation: false,
            multiline_mode: false,
            pending_newline: false,
            last_newline: None,
            last_token: TokenKind::Empty,
            pending_indents: 0,
            indent_stack: Vec::new(),
            indent_stack_stack: Vec::new(),
            paren_stack: Vec::new(),
            indent_char: '\0',
            error_stack: Vec::new(),
            errors: Vec::new(),
            comments: std::collections::HashMap::new(),
        }
    }

    /// The dialect this lexer tokenizes under.
    #[must_use]
    pub fn dialect(&self) -> Dialect {
        self.dialect
    }

    /// Suppress newlines/indentation while parsing inside `()[]{}` or a lambda (parser-driven).
    pub fn set_multiline_mode(&mut self, state: bool) {
        self.multiline_mode = state;
    }

    /// Save the current indentation context (entering a lambda body inside an expression).
    pub fn push_expression_indented_block(&mut self) {
        self.indent_stack_stack.push(self.indent_stack.clone());
    }

    /// Restore the indentation context saved by [`Self::push_expression_indented_block`].
    pub fn pop_expression_indented_block(&mut self) {
        if let Some(stack) = self.indent_stack_stack.pop() {
            self.indent_stack = stack;
        }
    }

    fn is_at_end(&self) -> bool {
        self.pos >= self.chars.len()
    }

    /// `_peek(offset)` — the code point at `pos + offset`, or `'\0'` out of range. Mirrors Godot,
    /// where `_peek(-1)` is the character just consumed.
    fn peek(&self, offset: isize) -> char {
        let idx = self.pos as isize + offset;
        if idx < 0 || idx as usize >= self.chars.len() {
            '\0'
        } else {
            self.chars[idx as usize]
        }
    }

    fn byte_at(&self, char_index: usize) -> usize {
        self.byte_offsets[char_index.min(self.byte_offsets.len() - 1)]
    }

    /// `_advance()` — consume one code point. On reaching the end it synthesizes a trailing newline
    /// plus the dedents the parser needs, exactly as Godot does. Returns the consumed character.
    fn advance(&mut self) -> char {
        if self.is_at_end() {
            return '\0';
        }
        self.pos += 1;
        self.column = self.column.saturating_add(1);
        if self.is_at_end() {
            self.newline(true);
            self.check_indent();
        }
        self.peek(-1)
    }

    fn push_paren(&mut self, c: char) {
        self.paren_stack.push(c);
    }

    fn pop_paren(&mut self, expected: char) -> bool {
        match self.paren_stack.pop() {
            Some(actual) => actual == expected,
            None => false,
        }
    }

    fn lexeme(&self) -> Box<str> {
        self.chars[self.start_pos..self.pos]
            .iter()
            .collect::<String>()
            .into()
    }

    fn make_token(&mut self, kind: TokenKind) -> Token {
        let token = Token {
            kind,
            span: ByteSpan::new(self.byte_at(self.start_pos), self.byte_at(self.pos)),
            loc: LineColRange {
                start: LineCol::new(self.start_line, self.start_column),
                end: LineCol::new(self.line, self.column),
            },
            literal: None,
            source: self.lexeme(),
        };
        self.last_token = kind;
        token
    }

    fn make_literal(&mut self, literal: Literal) -> Token {
        let mut token = self.make_token(TokenKind::Literal);
        token.literal = Some(literal);
        token
    }

    fn make_identifier(&mut self) -> Token {
        self.make_token(TokenKind::Identifier)
    }

    /// `make_error` — records a [`LexError`] and returns an `ERROR` token carrying the message in its
    /// `source` field (mirroring Godot's `Token::literal`). The parser reads the message off the
    /// token, so diagnostics come out in token-emission order. [`Self::errors`] keeps a creation-order
    /// copy for the standalone lexer tests.
    fn make_error(&mut self, message: impl Into<String>) -> Token {
        let mut token = self.make_token(TokenKind::Error);
        // One scan can push an error onto the LIFO `error_stack` *and* directly return another (e.g.
        // `0x_`), so the emission order differs from the creation order. Pairing each token with its
        // own message (as Godot does) is what reproduces Godot's "first error" exactly; a FIFO cursor
        // over `errors` would report them swapped.
        token.source = message.into().into_boxed_str();
        self.errors.push(LexError {
            span: token.span,
            loc: token.loc,
            message: token.source.to_string(),
        });
        token
    }

    fn push_error(&mut self, message: impl Into<String>) {
        let token = self.make_error(message);
        self.error_stack.push(token);
    }

    fn has_error(&self) -> bool {
        !self.error_stack.is_empty()
    }

    fn pop_error(&mut self) -> Token {
        self.error_stack.pop().expect("pop_error with empty stack")
    }

    fn make_paren_error(&mut self, paren: char) -> Token {
        if let Some(&open) = self.paren_stack.last() {
            let msg = format!("Closing \"{paren}\" doesn't match the opening \"{open}\".");
            self.paren_stack.pop(); // Remove opening one anyway.
            self.make_error(msg)
        } else {
            self.make_error(format!(
                "Closing \"{paren}\" doesn't have an opening counterpart."
            ))
        }
    }

    fn check_vcs_marker(&mut self, test: char, double_type: TokenKind) -> Token {
        // Count repeated `test` characters ahead (two already matched by scan).
        let mut chars = 2;
        let mut offset = 1isize;
        while self.peek(offset) == test {
            chars += 1;
            offset += 1;
        }
        if chars >= 7 {
            // VCS conflict marker: consume all (the first was already consumed by scan).
            while chars > 1 {
                self.advance();
                chars -= 1;
            }
            self.make_token(TokenKind::VcsConflictMarker)
        } else {
            // Regular double-character token: consume the second character.
            self.advance();
            self.make_token(double_type)
        }
    }

    /// `newline()` — queue a `NEWLINE` token (unless suppressed) and advance the line counter.
    fn newline(&mut self, make_token: bool) {
        if make_token && !self.pending_newline && !self.line_continuation {
            let nl = Token {
                kind: TokenKind::Newline,
                span: ByteSpan::new(
                    self.byte_at(self.pos.saturating_sub(1)),
                    self.byte_at(self.pos),
                ),
                loc: LineColRange {
                    start: LineCol::new(self.line, self.column.saturating_sub(1)),
                    end: LineCol::new(self.line, self.column),
                },
                literal: None,
                source: Box::from(""),
            };
            self.pending_newline = true;
            self.last_token = TokenKind::Newline;
            self.last_newline = Some(nl);
        }
        self.line += 1;
        self.column = 1;
    }

    fn indent_char_name(c: char) -> &'static str {
        if c == ' ' {
            "space"
        } else {
            "tab"
        }
    }

    fn annotation(&mut self) -> Token {
        if is_identifier_start(self.peek(0)) {
            self.advance();
        } else {
            self.push_error("Expected annotation identifier after \"@\".");
        }
        while is_identifier_continue(self.peek(0)) {
            self.advance();
        }
        self.make_token(TokenKind::Annotation)
    }

    fn potential_identifier(&mut self) -> Token {
        while is_identifier_continue(self.peek(0)) {
            self.advance();
        }
        let len = self.pos - self.start_pos;

        if len == 1 && self.peek(-1) == '_' {
            return self.make_token(TokenKind::Underscore);
        }

        // Length gate (Godot: `gdscript_tokenizer.cpp:581-585`): an identifier shorter than
        // `MIN_KEYWORD_LENGTH` or longer than `MAX_KEYWORD_LENGTH` can't be a keyword or a special
        // literal, so Godot returns it as a plain identifier before the keyword switch — which
        // also skips the confusable check. `len` is a `char` count (matching Godot's `_current
        // - _start` over UTF-32), so this is independent of UTF-8 byte width.
        if !(MIN_KEYWORD_LENGTH..=MAX_KEYWORD_LENGTH).contains(&len) {
            return self.make_identifier();
        }

        let name: String = self.chars[self.start_pos..self.pos].iter().collect();
        let only_ascii = name.bytes().all(|b| b < 0x80);

        // WP-F4: visually-similar-to-keyword check via the UTS #39 confusable-skeleton algorithm
        // (mirrors Godot's `TextServer::is_confusable` call in `gdscript_tokenizer.cpp:585-602`,
        // gated on `!only_ascii` because pure-ASCII identifiers either are keywords already or
        // can't be confused with one). Records the diagnostic on the lexer's error stack — the
        // identifier token still flows through to the parser so error recovery can keep going.
        if !only_ascii {
            if let Some(kw) = keyword_skeleton_lookup(&name) {
                self.push_error(format!(
                    "Identifier \"{name}\" is visually similar to the GDScript keyword \"{kw}\" \
                     and thus not allowed."
                ));
            }
        }

        if let Some(kind) = keyword_kind(&name) {
            return self.make_token(kind);
        }

        // Special literals (checked after keywords, exactly as Godot orders it).
        match name.as_str() {
            "true" => return self.make_literal(Literal::Bool(true)),
            "false" => return self.make_literal(Literal::Bool(false)),
            "null" => return self.make_literal(Literal::Null),
            _ => {}
        }

        self.make_identifier()
    }

    fn number(&mut self) -> Token {
        let mut base = 10;
        let mut has_decimal = false;
        let mut has_exponent = false;
        let mut has_error = false;
        let mut need_digits = false;
        let digit_check: fn(char) -> bool;

        // Sign before hexadecimal or binary (e.g. `-0x...`).
        if (self.peek(-1) == '+' || self.peek(-1) == '-') && self.peek(0) == '0' {
            self.advance();
        }

        if self.peek(-1) == '.' {
            has_decimal = true;
            digit_check = is_digit;
        } else if self.peek(-1) == '0' && (self.peek(0) == 'x' || self.peek(0) == 'X') {
            base = 16;
            need_digits = true;
            self.advance();
            digit_check = is_hex_digit;
        } else if self.peek(-1) == '0' && (self.peek(0) == 'b' || self.peek(0) == 'B') {
            base = 2;
            need_digits = true;
            self.advance();
            digit_check = is_binary_digit;
        } else {
            digit_check = is_digit;
        }

        if base != 10 && is_underscore(self.peek(0)) {
            // Disallow `0x_` and `0b_`.
            self.push_error(format!(
                "Unexpected underscore after \"0{}\".",
                self.peek(-1)
            ));
            has_error = true;
        }
        let mut previous_was_underscore = false;
        while digit_check(self.peek(0)) || is_underscore(self.peek(0)) {
            if is_underscore(self.peek(0)) {
                if previous_was_underscore {
                    self.push_error(
                        "Multiple underscores cannot be adjacent in a numeric literal.",
                    );
                }
                previous_was_underscore = true;
            } else {
                need_digits = false;
                previous_was_underscore = false;
            }
            self.advance();
        }

        // A `.` here is a decimal point only if it is not the `..` range token.
        if self.peek(0) == '.' && self.peek(1) != '.' {
            if base == 10 && !has_decimal {
                has_decimal = true;
            } else if base == 10 {
                self.push_error("Cannot use a decimal point twice in a number.");
                has_error = true;
            } else if base == 16 {
                self.push_error("Cannot use a decimal point in a hexadecimal number.");
                has_error = true;
            } else {
                self.push_error("Cannot use a decimal point in a binary number.");
                has_error = true;
            }
            if !has_error {
                self.advance();
                if is_underscore(self.peek(0)) {
                    // Disallow `10._`, but allow `10.`.
                    self.push_error("Unexpected underscore after decimal point.");
                    has_error = true;
                }
                previous_was_underscore = false;
                while is_digit(self.peek(0)) || is_underscore(self.peek(0)) {
                    if is_underscore(self.peek(0)) {
                        if previous_was_underscore {
                            self.push_error(
                                "Multiple underscores cannot be adjacent in a numeric literal.",
                            );
                        }
                        previous_was_underscore = true;
                    } else {
                        previous_was_underscore = false;
                    }
                    self.advance();
                }
            }
        }

        if base == 10 && (self.peek(0) == 'e' || self.peek(0) == 'E') {
            has_exponent = true;
            self.advance();
            if self.peek(0) == '+' || self.peek(0) == '-' {
                self.advance();
            }
            if !is_digit(self.peek(0)) {
                self.push_error("Expected exponent value after \"e\".");
            }
            previous_was_underscore = false;
            while is_digit(self.peek(0)) || is_underscore(self.peek(0)) {
                if is_underscore(self.peek(0)) {
                    if previous_was_underscore {
                        self.push_error(
                            "Multiple underscores cannot be adjacent in a numeric literal.",
                        );
                    }
                    previous_was_underscore = true;
                } else {
                    previous_was_underscore = false;
                }
                self.advance();
            }
        }

        if need_digits {
            let (word, c) = if base == 16 {
                ("hexadecimal", 'x')
            } else {
                ("binary", 'b')
            };
            return self.make_error(format!("Expected {word} digit after \"0{c}\"."));
        }

        if !has_error && has_decimal && self.peek(0) == '.' && self.peek(1) != '.' {
            self.push_error("Cannot use a decimal point twice in a number.");
        } else if is_identifier_start(self.peek(0)) || is_identifier_continue(self.peek(0)) {
            // Letter at the end of the number.
            self.push_error("Invalid numeric notation.");
        }

        let text: String = self.chars[self.start_pos..self.pos]
            .iter()
            .filter(|&&c| c != '_')
            .collect();

        if base == 16 {
            let value =
                i64::from_str_radix(text.trim_start_matches(['0', 'x', 'X']), 16).unwrap_or(0);
            self.make_literal(Literal::Int(value))
        } else if base == 2 {
            let value =
                i64::from_str_radix(text.trim_start_matches(['0', 'b', 'B']), 2).unwrap_or(0);
            self.make_literal(Literal::Int(value))
        } else if has_decimal || has_exponent {
            self.make_literal(Literal::Float(text.parse::<f64>().unwrap_or(0.0)))
        } else {
            self.make_literal(Literal::Int(text.parse::<i64>().unwrap_or(0)))
        }
    }

    fn string(&mut self) -> Token {
        #[derive(PartialEq)]
        enum StringType {
            Regular,
            Name,
            NodePath,
        }
        let mut is_raw = false;
        let mut is_multiline = false;
        let mut ty = StringType::Regular;

        match self.peek(-1) {
            'r' => {
                is_raw = true;
                self.advance();
            }
            '&' => {
                ty = StringType::Name;
                self.advance();
            }
            '^' => {
                ty = StringType::NodePath;
                self.advance();
            }
            _ => {}
        }

        let quote = self.peek(-1);

        if self.peek(0) == quote && self.peek(1) == quote {
            is_multiline = true;
            self.advance();
            self.advance();
        }

        let mut result = String::new();
        let mut prev: u32 = 0; // Pending UTF-16 lead surrogate from an escape, or 0.

        loop {
            if self.is_at_end() {
                return self.make_error("Unterminated string.");
            }
            let ch = self.peek(0);

            // Invisible text-direction control characters.
            if ch == '\u{200E}'
                || ch == '\u{200F}'
                || ('\u{202A}'..='\u{202E}').contains(&ch)
                || ('\u{2066}'..='\u{2069}').contains(&ch)
            {
                if is_raw {
                    self.push_error("Invisible text direction control character present in the string, use regular string literal instead of r-string.");
                } else {
                    self.push_error(format!("Invisible text direction control character present in the string, escape it (\"\\u{:x}\") to avoid confusion.", ch as u32));
                }
            }

            if ch == '\\' {
                self.advance();
                if self.is_at_end() {
                    return self.make_error("Unterminated string.");
                }
                if is_raw {
                    if self.peek(0) == quote {
                        self.advance();
                        if self.is_at_end() {
                            return self.make_error("Unterminated string.");
                        }
                        result.push('\\');
                        result.push(quote);
                    } else if self.peek(0) == '\\' {
                        self.advance();
                        if self.is_at_end() {
                            return self.make_error("Unterminated string.");
                        }
                        result.push('\\');
                        result.push('\\');
                    } else {
                        result.push('\\');
                    }
                    continue;
                }

                let code = self.peek(0);
                self.advance();
                if self.is_at_end() {
                    return self.make_error("Unterminated string.");
                }
                let mut escaped: u32 = 0;
                let mut valid_escape = true;
                match code {
                    'a' => escaped = 0x07,
                    'b' => escaped = 0x08,
                    'f' => escaped = 0x0c,
                    'n' => escaped = '\n' as u32,
                    'r' => escaped = '\r' as u32,
                    't' => escaped = '\t' as u32,
                    'v' => escaped = 0x0b,
                    '\'' => escaped = '\'' as u32,
                    '"' => escaped = '"' as u32,
                    '\\' => escaped = '\\' as u32,
                    'U' | 'u' => {
                        let hex_len = if code == 'U' { 6 } else { 4 };
                        for _ in 0..hex_len {
                            if self.is_at_end() {
                                return self.make_error("Unterminated string.");
                            }
                            let digit = self.peek(0);
                            let value = match digit {
                                '0'..='9' => digit as u32 - '0' as u32,
                                'a'..='f' => digit as u32 - 'a' as u32 + 10,
                                'A'..='F' => digit as u32 - 'A' as u32 + 10,
                                _ => {
                                    self.push_error(
                                        "Invalid hexadecimal digit in unicode escape sequence.",
                                    );
                                    valid_escape = false;
                                    break;
                                }
                            };
                            escaped = (escaped << 4) | value;
                            self.advance();
                        }
                    }
                    '\r' => {
                        if self.peek(0) != '\n' {
                            result.push(ch);
                            self.advance();
                            continue;
                        }
                        // fallthrough to '\n'
                        self.newline(false);
                        valid_escape = false;
                    }
                    '\n' => {
                        self.newline(false);
                        valid_escape = false;
                    }
                    _ => {
                        self.push_error("Invalid escape in string.");
                        valid_escape = false;
                    }
                }

                if valid_escape {
                    if escaped & 0xffff_fc00 == 0xd800 {
                        // Lead surrogate.
                        if prev == 0 {
                            prev = escaped;
                            continue;
                        }
                        self.push_error(
                            "Invalid UTF-16 sequence in string, unpaired lead surrogate.",
                        );
                        valid_escape = false;
                        prev = 0;
                    } else if escaped & 0xffff_fc00 == 0xdc00 {
                        // Trail surrogate.
                        if prev == 0 {
                            self.push_error(
                                "Invalid UTF-16 sequence in string, unpaired trail surrogate.",
                            );
                            valid_escape = false;
                        } else {
                            escaped =
                                (prev << 10) + escaped - ((0xd800u32 << 10) + 0xdc00 - 0x10000);
                            prev = 0;
                        }
                    }
                    if prev != 0 {
                        self.push_error(
                            "Invalid UTF-16 sequence in string, unpaired lead surrogate.",
                        );
                        prev = 0;
                    }
                    if valid_escape {
                        result.push(char::from_u32(escaped).unwrap_or('\u{fffd}'));
                    }
                }
            } else if ch == quote {
                if prev != 0 {
                    self.push_error("Invalid UTF-16 sequence in string, unpaired lead surrogate");
                    prev = 0;
                }
                self.advance();
                if is_multiline {
                    if self.peek(0) == quote && self.peek(1) == quote {
                        self.advance();
                        self.advance();
                        break;
                    }
                    result.push(quote);
                } else {
                    break;
                }
            } else {
                if prev != 0 {
                    self.push_error("Invalid UTF-16 sequence in string, unpaired lead surrogate");
                    prev = 0;
                }
                result.push(ch);
                self.advance();
                if ch == '\n' {
                    self.newline(false);
                }
            }
        }
        if prev != 0 {
            self.push_error("Invalid UTF-16 sequence in string, unpaired lead surrogate");
        }

        let literal = match ty {
            StringType::Regular => Literal::String(result),
            StringType::Name => Literal::StringName(result),
            StringType::NodePath => Literal::NodePath(result),
        };
        self.make_literal(literal)
    }

    fn check_indent(&mut self) {
        debug_assert!(self.column == 1, "checking indentation mid-line");

        if self.is_at_end() {
            self.pending_indents -= self.indent_stack.len() as i32;
            self.indent_stack.clear();
            return;
        }

        loop {
            let current_indent_char = self.peek(0);
            let mut indent_count: i32 = 0;

            if current_indent_char != ' '
                && current_indent_char != '\t'
                && current_indent_char != '\r'
                && current_indent_char != '\n'
                && current_indent_char != '#'
            {
                if self.line_continuation || self.multiline_mode {
                    return;
                }
                self.pending_indents -= self.indent_stack.len() as i32;
                self.indent_stack.clear();
                return;
            }

            if self.peek(0) == '\r' {
                self.advance();
                if self.peek(0) != '\n' {
                    self.push_error("Stray carriage return character in source code.");
                }
            }
            if self.peek(0) == '\n' {
                self.advance();
                self.newline(false);
                continue;
            }

            // Count indentation.
            let mut mixed = false;
            while !self.is_at_end() {
                let space = self.peek(0);
                if space == '\t' {
                    // DIALECT(4.7): gdscript_tokenizer.cpp check_indent() — a tab advances
                    // `column` by one, not by `tab_size`. Indent DEPTH is unchanged: a tab is
                    // still worth `TAB_SIZE` indent units, which is what keeps mixed
                    // tabs-and-spaces resolving the same way. Only the reported column moved, and
                    // with it Godot's own LSP dropped all its tab-expansion machinery.
                    if self.dialect < Dialect::Godot4_7 {
                        self.column = self.column.saturating_add(TAB_SIZE - 1);
                    }
                    indent_count = indent_count.saturating_add(TAB_SIZE as i32);
                } else if space == ' ' {
                    indent_count = indent_count.saturating_add(1);
                } else {
                    break;
                }
                mixed = mixed || space != current_indent_char;
                self.advance();
            }

            if self.is_at_end() {
                self.pending_indents -= self.indent_stack.len() as i32;
                self.indent_stack.clear();
                return;
            }

            if self.peek(0) == '\r' {
                self.advance();
                if self.peek(0) != '\n' {
                    self.push_error("Stray carriage return character in source code.");
                }
            }
            if self.peek(0) == '\n' {
                self.advance();
                self.newline(false);
                continue;
            }
            if self.peek(0) == '#' {
                // M7 (#62): record the comment (Godot: gdscript_tokenizer.cpp:1208 — always
                // `new_line: true` on this whole-line path) before consuming it.
                let comment_start = self.byte_offsets[self.pos];
                while self.peek(0) != '\n' && !self.is_at_end() {
                    self.advance();
                }
                self.comments.insert(
                    self.line,
                    CommentData {
                        span: ByteSpan::new(comment_start, self.byte_offsets[self.pos]),
                        new_line: true,
                    },
                );
                if self.is_at_end() {
                    self.pending_indents -= self.indent_stack.len() as i32;
                    self.indent_stack.clear();
                    return;
                }
                self.advance(); // Consume '\n'.
                self.newline(false);
                continue;
            }

            if mixed && !self.line_continuation && !self.multiline_mode {
                self.push_error("Mixed use of tabs and spaces for indentation.");
            }

            if self.line_continuation || self.multiline_mode {
                return;
            }

            // Consistent indentation character.
            if self.indent_char == '\0' {
                self.indent_char = current_indent_char;
            } else if current_indent_char != self.indent_char {
                let msg = format!(
                    "Used {} character for indentation instead of {} as used before in the file.",
                    Self::indent_char_name(current_indent_char),
                    Self::indent_char_name(self.indent_char)
                );
                self.push_error(msg);
            }

            // Apply the indentation change.
            let previous_indent = self.indent_stack.last().copied().unwrap_or(0);
            if indent_count == previous_indent {
                return;
            }
            if indent_count > previous_indent {
                self.indent_stack.push(indent_count);
                self.pending_indents += 1;
            } else {
                if self.indent_stack.is_empty() {
                    self.push_error("Tokenizer bug: trying to dedent without previous indent.");
                    return;
                }
                while self
                    .indent_stack
                    .last()
                    .is_some_and(|&top| top > indent_count)
                {
                    self.indent_stack.pop();
                    self.pending_indents -= 1;
                }
                let mismatched = match self.indent_stack.last() {
                    Some(&top) => top != indent_count,
                    None => indent_count != 0,
                };
                if mismatched {
                    self.push_error("Unindent doesn't match the previous indentation level.");
                    // Lenient: keep this level on the stack and continue.
                    self.indent_stack.push(indent_count);
                }
            }
            break;
        }
    }

    fn skip_whitespace(&mut self) {
        if self.pending_indents != 0 {
            return;
        }
        let is_bol = self.column == 1;
        if is_bol {
            self.check_indent();
            return;
        }
        loop {
            match self.peek(0) {
                ' ' => {
                    self.advance();
                }
                '\t' => {
                    self.advance();
                    // DIALECT(4.7): gdscript_tokenizer.cpp _skip_whitespace() — see the
                    // matching note in `check_indent`. 4.6 widened a tab to `tab_size` columns
                    // (editor-configurable, so column numbers depended on a user setting); 4.7
                    // counts it as the single character it is.
                    if self.dialect < Dialect::Godot4_7 {
                        self.column = self.column.saturating_add(TAB_SIZE - 1);
                    }
                }
                '\r' => {
                    self.advance();
                    if self.peek(0) != '\n' {
                        self.push_error("Stray carriage return character in source code.");
                        return;
                    }
                }
                '\n' => {
                    self.advance();
                    self.newline(!is_bol);
                    self.check_indent();
                }
                '#' => {
                    // M7 (#62): record the comment (Godot: gdscript_tokenizer.cpp:1339 —
                    // `new_line` is whether the line held no code before it).
                    let comment_start = self.byte_offsets[self.pos];
                    while self.peek(0) != '\n' && !self.is_at_end() {
                        self.advance();
                    }
                    self.comments.insert(
                        self.line,
                        CommentData {
                            span: ByteSpan::new(comment_start, self.byte_offsets[self.pos]),
                            new_line: is_bol,
                        },
                    );
                    if self.is_at_end() {
                        return;
                    }
                    self.advance(); // Consume '\n'.
                    self.newline(!is_bol);
                    self.check_indent();
                }
                _ => return,
            }
        }
    }

    /// Scan and return the next token (`GDScriptTokenizerText::scan`).
    pub fn scan(&mut self) -> Token {
        // Godot scans the next token after a `\` line continuation by *recursing* into `scan()`. We
        // loop instead: a long run of continuations would otherwise overflow the native stack — an
        // uncatchable abort and fuzz release-blocker. Deliberate deviation, like the parser's
        // recursion-depth guard. Each `continue` is exactly Godot's `return scan();`.
        let c = loop {
            if self.has_error() {
                return self.pop_error();
            }

            self.skip_whitespace();

            if self.pending_newline {
                self.pending_newline = false;
                if !self.multiline_mode {
                    return self
                        .last_newline
                        .clone()
                        .expect("pending newline without token");
                }
            }

            if self.has_error() {
                return self.pop_error();
            }

            self.start_pos = self.pos;
            self.start_line = self.line;
            self.start_column = self.column;

            if self.pending_indents != 0 {
                // Re-anchor the token to the start of the line.
                self.start_pos = self
                    .start_pos
                    .saturating_sub(self.start_column.saturating_sub(1) as usize);
                self.start_column = 1;
                if self.pending_indents > 0 {
                    self.pending_indents -= 1;
                    return self.make_token(TokenKind::Indent);
                }
                self.pending_indents += 1;
                let mut dedent = self.make_token(TokenKind::Dedent);
                dedent.loc.end.column = dedent.loc.end.column.saturating_add(1);
                return dedent;
            }

            if self.is_at_end() {
                return self.make_token(TokenKind::Eof);
            }

            let c = self.advance();

            if c == '\\' {
                // Backslash line continuation.
                if self.peek(0) == '\r' {
                    if self.peek(1) != '\n' {
                        return self.make_error("Unexpected carriage return character.");
                    }
                    self.advance();
                }
                if self.peek(0) != '\n' {
                    return self.make_error("Expected new line after \"\\\".");
                }
                self.advance();
                self.newline(false);
                self.line_continuation = true;
                self.skip_whitespace(); // Skip whitespace/comment lines after `\` (GH-89403).
                continue;
            }

            break c;
        };

        self.line_continuation = false;

        if is_digit(c) {
            return self.number();
        }
        if c == 'r' && (self.peek(0) == '"' || self.peek(0) == '\'') {
            return self.string();
        }
        if is_identifier_start(c) {
            return self.potential_identifier();
        }

        use TokenKind::*;
        match c {
            '"' | '\'' => self.string(),
            '@' => self.annotation(),
            '~' => self.make_token(Tilde),
            ',' => self.make_token(Comma),
            ':' => self.make_token(Colon),
            ';' => self.make_token(Semicolon),
            '$' => self.make_token(Dollar),
            '?' => self.make_token(QuestionMark),
            '`' => self.make_token(Backtick),
            '(' => {
                self.push_paren('(');
                self.make_token(ParenthesisOpen)
            }
            '[' => {
                self.push_paren('[');
                self.make_token(BracketOpen)
            }
            '{' => {
                self.push_paren('{');
                self.make_token(BraceOpen)
            }
            ')' => {
                if !self.pop_paren('(') {
                    return self.make_paren_error(c);
                }
                self.make_token(ParenthesisClose)
            }
            ']' => {
                if !self.pop_paren('[') {
                    return self.make_paren_error(c);
                }
                self.make_token(BracketClose)
            }
            '}' => {
                if !self.pop_paren('{') {
                    return self.make_paren_error(c);
                }
                self.make_token(BraceClose)
            }
            '!' => {
                if self.peek(0) == '=' {
                    self.advance();
                    self.make_token(BangEqual)
                } else {
                    self.make_token(Bang)
                }
            }
            '.' => {
                if self.peek(0) == '.' {
                    self.advance();
                    if self.peek(0) == '.' {
                        self.advance();
                        self.make_token(PeriodPeriodPeriod)
                    } else {
                        self.make_token(PeriodPeriod)
                    }
                } else if is_digit(self.peek(0)) {
                    self.number()
                } else {
                    self.make_token(Period)
                }
            }
            '+' => {
                if self.peek(0) == '=' {
                    self.advance();
                    self.make_token(PlusEqual)
                } else if is_digit(self.peek(0)) && !self.last_token.can_precede_bin_op() {
                    self.number()
                } else {
                    self.make_token(Plus)
                }
            }
            '-' => {
                if self.peek(0) == '=' {
                    self.advance();
                    self.make_token(MinusEqual)
                } else if is_digit(self.peek(0)) && !self.last_token.can_precede_bin_op() {
                    self.number()
                } else if self.peek(0) == '>' {
                    self.advance();
                    self.make_token(ForwardArrow)
                } else {
                    self.make_token(Minus)
                }
            }
            '*' => {
                if self.peek(0) == '=' {
                    self.advance();
                    self.make_token(StarEqual)
                } else if self.peek(0) == '*' {
                    if self.peek(1) == '=' {
                        self.advance();
                        self.advance();
                        self.make_token(StarStarEqual)
                    } else {
                        self.advance();
                        self.make_token(StarStar)
                    }
                } else {
                    self.make_token(Star)
                }
            }
            '/' => {
                if self.peek(0) == '=' {
                    self.advance();
                    self.make_token(SlashEqual)
                } else {
                    self.make_token(Slash)
                }
            }
            '%' => {
                if self.peek(0) == '=' {
                    self.advance();
                    self.make_token(PercentEqual)
                } else {
                    self.make_token(Percent)
                }
            }
            '^' => {
                if self.peek(0) == '=' {
                    self.advance();
                    self.make_token(CaretEqual)
                } else if self.peek(0) == '"' || self.peek(0) == '\'' {
                    self.string() // NodePath literal.
                } else {
                    self.make_token(Caret)
                }
            }
            '&' => {
                if self.peek(0) == '&' {
                    self.advance();
                    self.make_token(AmpersandAmpersand)
                } else if self.peek(0) == '=' {
                    self.advance();
                    self.make_token(AmpersandEqual)
                } else if self.peek(0) == '"' || self.peek(0) == '\'' {
                    self.string() // StringName literal.
                } else {
                    self.make_token(Ampersand)
                }
            }
            '|' => {
                if self.peek(0) == '|' {
                    self.advance();
                    self.make_token(PipePipe)
                } else if self.peek(0) == '=' {
                    self.advance();
                    self.make_token(PipeEqual)
                } else {
                    self.make_token(Pipe)
                }
            }
            '=' => {
                if self.peek(0) == '=' {
                    self.check_vcs_marker('=', EqualEqual)
                } else {
                    self.make_token(Equal)
                }
            }
            '<' => {
                if self.peek(0) == '=' {
                    self.advance();
                    self.make_token(LessEqual)
                } else if self.peek(0) == '<' {
                    if self.peek(1) == '=' {
                        self.advance();
                        self.advance();
                        self.make_token(LessLessEqual)
                    } else {
                        self.check_vcs_marker('<', LessLess)
                    }
                } else {
                    self.make_token(Less)
                }
            }
            '>' => {
                if self.peek(0) == '=' {
                    self.advance();
                    self.make_token(GreaterEqual)
                } else if self.peek(0) == '>' {
                    if self.peek(1) == '=' {
                        self.advance();
                        self.advance();
                        self.make_token(GreaterGreaterEqual)
                    } else {
                        self.check_vcs_marker('>', GreaterGreater)
                    }
                } else {
                    self.make_token(Greater)
                }
            }
            _ => {
                if is_whitespace(c) {
                    self.make_error(format!("Invalid white space character U+{:04X}.", c as u32))
                } else {
                    self.make_error(format!("Invalid character \"{}\" (U+{:04X}).", c, c as u32))
                }
            }
        }
    }
}

/// Drive the lexer standalone to a flat token list, for callers that need the raw token stream
/// without the parser (the lexer is otherwise parser-driven — `docs/01`). Runs with
/// `multiline_mode = false`, so newline/indent/dedent tokens are **emitted** (not suppressed); a
/// consumer doing a bracket-depth scan simply skips them. Every `LITERAL` token (including string
/// literals) is a single token carrying its decoded value, so a downstream scan never breaks on a
/// `)`/`,` inside a string.
///
/// Always terminates: [`Lexer::scan`] returns [`TokenKind::Eof`] at end-of-input (and on a long
/// `\`-continuation run it loops rather than recurses), so the returned vector always ends with
/// exactly one `Eof`. The companion [`Vec<LexError>`] mirrors what the parser would have collected.
///
/// This is **additive** — it does not touch the ported scan loop; M8 completion-context detection
/// (`gd_server`) is its first consumer.
#[must_use]
pub fn tokenize(source: &str) -> (Vec<Token>, Vec<LexError>) {
    tokenize_with_dialect(source, Dialect::DEFAULT)
}

/// [`tokenize`] under an explicit dialect.
pub fn tokenize_with_dialect(source: &str, dialect: Dialect) -> (Vec<Token>, Vec<LexError>) {
    let mut lx = Lexer::new_with_dialect(source, dialect);
    let mut tokens = Vec::new();
    loop {
        let t = lx.scan();
        let is_eof = t.kind == TokenKind::Eof;
        tokens.push(t);
        if is_eof {
            break;
        }
    }
    (tokens, lx.errors)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Drive the lexer to completion (standalone, `multiline_mode = false`). Suitable for code with
    /// no parser-driven multiline constructs. Thin wrapper over the public [`tokenize`] so the
    /// existing token-shape tests below also exercise the exported entry point.
    fn lex(src: &str) -> (Vec<Token>, Vec<LexError>) {
        tokenize(src)
    }

    fn kinds(src: &str) -> Vec<TokenKind> {
        lex(src).0.into_iter().map(|t| t.kind).collect()
    }

    #[test]
    fn simple_var_declaration() {
        use TokenKind::*;
        assert_eq!(
            kinds("var x = 1\n"),
            vec![Var, Identifier, Equal, Literal, Newline, Eof]
        );
    }

    #[test]
    fn confusable_keyword_lookup_excludes_special_literals() {
        // Godot's `keyword_list` (`make_keyword_list`, gdscript_tokenizer.cpp:552) is built from
        // the `KEYWORDS` macro ONLY; `true`/`false`/`null` are special literals, never keywords, so
        // `is_confusable` can never match them. Matching them here would over-report vs Godot.
        assert_eq!(keyword_skeleton_lookup("true"), None);
        assert_eq!(keyword_skeleton_lookup("false"), None);
        assert_eq!(keyword_skeleton_lookup("null"), None);
        // The lookup mechanism itself still finds a genuine keyword (skeleton of an ASCII keyword
        // is itself, so it matches its own table entry).
        assert_eq!(keyword_skeleton_lookup("func"), Some("func"));
        assert_eq!(keyword_skeleton_lookup("class"), Some("class"));
        // A non-keyword identifier never matches.
        assert_eq!(keyword_skeleton_lookup("foobar"), None);
    }

    #[test]
    fn backslash_continuation_joins_lines_and_never_overflows() {
        use TokenKind::*;
        // A `\`-continuation suppresses the newline, joining the two physical lines into one.
        assert_eq!(
            kinds("1 + \\\n2\n"),
            vec![Literal, Plus, Literal, Newline, Eof]
        );
        // A long run of continuations is scanned iteratively (Godot recurses), so it must not
        // overflow the stack — run it on a small-stack thread to be sure.
        let src = "\\\n".repeat(200_000);
        std::thread::Builder::new()
            .stack_size(256 * 1024)
            .spawn(move || {
                let (toks, _errs) = lex(&src);
                // Continuations carry no tokens of their own; only EOF remains.
                assert_eq!(toks.last().map(|t| t.kind), Some(TokenKind::Eof));
            })
            .unwrap()
            .join()
            .expect("scanning many line-continuations overflowed the stack");
    }

    #[test]
    fn indent_and_dedent_around_function_body() {
        use TokenKind::*;
        // func f():\n\tpass\n  → INDENT before `pass`, DEDENT after.
        assert_eq!(
            kinds("func f():\n\tpass\n"),
            vec![
                Func,
                Identifier,
                ParenthesisOpen,
                ParenthesisClose,
                Colon,
                Newline,
                Indent,
                Pass,
                Newline,
                Dedent,
                Eof
            ]
        );
    }

    #[test]
    fn keywords_constants_and_literals() {
        use TokenKind::*;
        let (toks, errs) = lex("const PI_VALUE = PI\nreturn true\n");
        assert!(errs.is_empty());
        let ks: Vec<_> = toks.iter().map(|t| t.kind).collect();
        assert_eq!(
            ks,
            vec![Const, Identifier, Equal, ConstPi, Newline, Return, Literal, Newline, Eof]
        );
        // `true` decodes to a bool literal. (Fully qualified: `use TokenKind::*` above shadows the
        // bare `Literal` enum name with the `TokenKind::Literal` variant.)
        assert_eq!(toks[6].literal, Some(super::Literal::Bool(true)));
    }

    #[test]
    fn numbers_decode_to_int_and_float() {
        let (toks, errs) = lex("0xFF\n1_000\n2.5\n1e3\n0b101\n");
        assert!(errs.is_empty(), "{errs:?}");
        let lits: Vec<_> = toks
            .iter()
            .filter(|t| t.kind == TokenKind::Literal)
            .map(|t| t.literal.clone().unwrap())
            .collect();
        assert_eq!(
            lits,
            vec![
                Literal::Int(255),
                Literal::Int(1000),
                Literal::Float(2.5),
                Literal::Float(1000.0),
                Literal::Int(5),
            ]
        );
    }

    #[test]
    fn strings_regular_name_and_nodepath() {
        let (toks, errs) = lex("\"hi\\n\"\n&\"sn\"\n^\"np\"\n");
        assert!(errs.is_empty(), "{errs:?}");
        let lits: Vec<_> = toks
            .iter()
            .filter(|t| t.kind == TokenKind::Literal)
            .map(|t| t.literal.clone().unwrap())
            .collect();
        assert_eq!(
            lits,
            vec![
                Literal::String("hi\n".to_string()),
                Literal::StringName("sn".to_string()),
                Literal::NodePath("np".to_string()),
            ]
        );
    }

    #[test]
    fn mixed_tabs_and_spaces_is_an_error() {
        // Indent with a space after the file established tabs.
        let (_toks, errs) = lex("func f():\n\tif x:\n\t pass\n");
        assert!(
            errs.iter()
                .any(|e| e.message == "Mixed use of tabs and spaces for indentation."),
            "expected mixed tabs/spaces error, got {errs:?}"
        );
    }

    #[test]
    fn unterminated_string_is_an_error() {
        let (_toks, errs) = lex("var s = \"oops\n");
        assert!(
            errs.iter().any(|e| e.message == "Unterminated string."),
            "{errs:?}"
        );
    }

    #[test]
    fn operators_and_byte_spans() {
        use TokenKind::*;
        let (toks, errs) = lex("a += 2\n");
        assert!(errs.is_empty());
        assert_eq!(
            toks.iter().map(|t| t.kind).collect::<Vec<_>>(),
            vec![Identifier, PlusEqual, Literal, Newline, Eof]
        );
        // `+=` spans bytes 2..4 of "a += 2\n".
        assert_eq!(toks[1].span, ByteSpan::new(2, 4));
        assert_eq!(toks[1].source.as_ref(), "+=");
    }

    #[test]
    fn negative_number_vs_subtraction() {
        use TokenKind::*;
        // After `=`, `-3` is a negative number literal.
        assert_eq!(
            kinds("x = -3\n"),
            vec![Identifier, Equal, Literal, Newline, Eof]
        );
        // After an identifier, `- 3` is subtraction.
        assert_eq!(
            kinds("x - 3\n"),
            vec![Identifier, Minus, Literal, Newline, Eof]
        );
    }

    #[test]
    fn never_panics_on_arbitrary_input() {
        for src in [
            "", "\u{0}", "\"", "0x", "@", "\\", "\t\t\t", "🎮", "((((", "1.2.3", "&", "^",
        ] {
            let _ = lex(src);
        }
    }

    // --- The public `tokenize` entry (M8 completion consumer). ---

    #[test]
    fn tokenize_always_ends_in_exactly_one_eof() {
        for src in [
            "",
            "var x = 1",
            "print(",
            "func f(\n\t",
            "((((",
            "\"unterminated",
        ] {
            let (toks, _errs) = tokenize(src);
            assert_eq!(
                toks.last().map(|t| t.kind),
                Some(TokenKind::Eof),
                "tokenize({src:?}) must end in Eof"
            );
            // Exactly one Eof, and it is last.
            let eofs = toks.iter().filter(|t| t.kind == TokenKind::Eof).count();
            assert_eq!(eofs, 1, "tokenize({src:?}) emitted {eofs} Eof tokens");
        }
    }

    #[test]
    fn tokenize_keeps_string_literal_as_one_token() {
        // A `)`/`,` inside a string must NOT surface as punctuation — the whole quote is one token,
        // so a downstream bracket-depth scan over `tokenize` output never breaks on in-string
        // brackets (the #65 mandate that M8 completion relies on).
        let (toks, _errs) = tokenize("print(\"a, b)c\", 1)");
        let kinds: Vec<_> = toks.iter().map(|t| t.kind).collect();
        use TokenKind::*;
        assert_eq!(
            kinds,
            vec![
                Identifier,
                ParenthesisOpen,
                Literal, // "a, b)c" — single token, the `,`/`)` inside are invisible
                Comma,
                Literal, // 1
                ParenthesisClose,
                Newline,
                Eof
            ]
        );
    }

    #[test]
    fn tokenize_emits_newlines_not_suppressed() {
        // `multiline_mode = false`: a newline inside parens is emitted (the parser would suppress
        // it, but the standalone consumer wants it and skips it). Pins that contract.
        let (toks, _errs) = tokenize("max(\n1)");
        assert!(
            toks.iter().any(|t| t.kind == TokenKind::Newline),
            "newline inside parens should be emitted in standalone mode"
        );
    }
}
