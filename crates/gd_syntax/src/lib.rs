//! `gd_syntax` — a faithful Rust port of Godot 4.6.3-stable's GDScript front-of-frontend: the tokenizer, the
//! recursive-descent + Pratt parser, and the AST.
//!
//! This crate has **no engine knowledge** (no type system, no native classes) so it can be fuzzed
//! and unit-tested in isolation. M1 lands the tokenizer ([`token`] + the lexer), the [`ast`] arena,
//! and the [`parser`] (recursive-descent + Pratt); [`parse`] runs the whole pipeline, always
//! returning a (possibly partial) tree, its syntax diagnostics, and a symbol outline
//! (`docs/02-frontend-port.md`). Type checking and warnings arrive later in `gd_analyze` (M3).

pub mod ast;
pub mod dialect;
pub mod doc_comments;
pub mod lexer;
pub mod parser;
pub mod span;
pub mod token;
pub mod warning_names;

pub use ast::ParseTree;
pub use dialect::Dialect;
pub use doc_comments::{ClassDoc, DocTable, MemberDoc};
pub use lexer::{tokenize, tokenize_with_dialect, CommentData, LexError, Lexer};
pub use span::{ByteSpan, LineCol, LineColRange};
pub use token::{Literal, Token, TokenKind};
pub use warning_names::{warning_name_is_valid, WARNINGS, WARNING_COUNT};

/// A frontend diagnostic. In M1 this carries syntax errors; the analyzer reuses the same shape for
/// type errors and warnings later.
#[derive(Clone, Debug)]
pub struct Diagnostic {
    pub span: ByteSpan,
    pub message: String,
}

/// The kind of a [`DocumentSymbol`], mapped to `lsp_types::SymbolKind` at the protocol boundary.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SymbolKind {
    Class,
    Function,
    Variable,
    /// A `var` with a getter/setter (`var x: int: get: …`).
    Property,
    Constant,
    Signal,
    Enum,
    /// A value inside an `enum { … }`.
    EnumMember,
}

/// A document symbol projected from the parse tree (classes, functions, vars, consts, signals,
/// enums, inner classes), shaped for LSP's nested `documentSymbol` response.
#[derive(Clone, Debug)]
pub struct DocumentSymbol {
    pub name: String,
    pub kind: SymbolKind,
    /// Full source range of the declaration.
    pub span: ByteSpan,
    /// Range of just the declared name (LSP's `selectionRange`).
    pub selection_span: ByteSpan,
    pub children: Vec<DocumentSymbol>,
}

/// The output of parsing one `.gd` source. The parser always returns a (possibly partial) result —
/// it never fails to produce one — so the server can always respond.
#[derive(Clone, Debug, Default)]
pub struct ParseResult {
    /// The parse tree (arena). Empty (sentinel root) only when nothing was parsed.
    pub tree: ParseTree,
    pub diagnostics: Vec<Diagnostic>,
    pub symbols: Vec<DocumentSymbol>,
    /// M9 (#70): the lexer's recorded comments, keyed by 1-based line — the same
    /// [`CommentData`] side-channel `doc_comments::associate` consumes (the M7 #62 mirror of
    /// Godot's `GDScriptTokenizer::CommentData`; the token stream never sees them, so both
    /// fidelity ratchets are unaffected). Surfaced here, **additively**, so `gd_server`'s
    /// `foldingRange` can fold comment runs and `#region`/`#endregion` pairs without re-lexing.
    /// Empty for sources without comments.
    pub comments: std::collections::HashMap<u32, CommentData>,
}

/// Per-parse knobs. Kept as a struct, mirroring `gd_analyze::AnalyzeOptions`, so every new knob
/// lands here without touching the signatures of [`parse`]'s many test and fuzz call sites.
#[derive(Clone, Copy, Debug, Default)]
pub struct ParseOptions<'a> {
    /// The Godot feature release whose frontend semantics apply. Defaults to [`Dialect::DEFAULT`].
    pub dialect: Dialect,
    /// The `res://` path of the script, or `""` when unknown (an untitled buffer, a fuzz input, a
    /// `.gd` outside the project). Godot's parser reads it only to reject `class_name` in a
    /// built-in script, so an empty path simply never triggers that check.
    pub script_path: &'a str,
}

/// Parse a GDScript source file into a (possibly partial) tree, its syntax diagnostics, and a
/// projected symbol outline. Mirrors `GDScriptParser::parse` + `parse_program`
/// (`docs/02-frontend-port.md`); never fails to return a result.
///
/// Parses at [`Dialect::DEFAULT`]; use [`parse_with_options`] where the project's dialect is known.
pub fn parse(source: &str) -> ParseResult {
    parse_with_options(source, &ParseOptions::default())
}

/// [`parse`] with per-parse knobs — see [`ParseOptions`]. The single source of truth for the parse
/// pipeline; the bare [`parse`] wrapper is the test and fuzz default.
pub fn parse_with_options(source: &str, options: &ParseOptions<'_>) -> ParseResult {
    let mut parser = parser::Parser::new_with_options(source, options);
    parser.parse_program();
    let comments = parser.take_comments();
    let (mut tree, diagnostics) = parser.into_parts();
    // M7 (#62): associate `##` doc comments post-parse — a read-only pass over the finished
    // tree + the lexer's comment side-channel, so the ported grammar (and both conformance
    // ratchets) never sees them.
    tree.docs = doc_comments::associate_with_dialect(source, &tree, &comments, options.dialect);
    let symbols = parser::document_symbols(&tree);
    // M9 (#70): hand the same comment side-channel through to the result so `foldingRange` can
    // see comment runs / `#region` markers. `associate` borrowed `&comments`, so it is intact to
    // move here — purely additive, the parser/tokenizer never observe it.
    ParseResult {
        tree,
        diagnostics,
        symbols,
        comments,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_extends_is_clean() {
        let r = parse("extends Node\n");
        assert!(r.diagnostics.is_empty(), "diagnostics: {:?}", r.diagnostics);
        // Unconditional root Class wrapper (Godot parse_class_symbol parity): one symbol,
        // unnamed (no class_name), no children (no members).
        assert_eq!(r.symbols.len(), 1, "expected one root Class symbol");
        let root = &r.symbols[0];
        assert_eq!(root.kind, SymbolKind::Class);
        assert_eq!(root.name, "");
        assert!(root.children.is_empty());
    }

    #[test]
    fn projects_top_level_symbols() {
        let r = parse("extends Node\n\nvar speed := 1.0\n\nfunc move():\n\tpass\n");
        assert!(r.diagnostics.is_empty(), "diagnostics: {:?}", r.diagnostics);
        // Root Class wraps the members (Godot parse_class_symbol parity).
        assert_eq!(r.symbols.len(), 1, "expected one root Class symbol");
        let root = &r.symbols[0];
        assert_eq!(root.kind, SymbolKind::Class);
        assert_eq!(root.name, "");
        let names: Vec<_> = root
            .children
            .iter()
            .map(|s| (s.name.as_str(), s.kind))
            .collect();
        assert_eq!(
            names,
            vec![
                ("speed", SymbolKind::Variable),
                ("move", SymbolKind::Function)
            ]
        );
    }

    /// Deeply nested input must hit the recursion-depth guard and return (with errors), never
    /// overflow the native stack — that would be an uncatchable abort, a fuzz release-blocker.
    #[test]
    fn pathological_nesting_does_not_overflow() {
        let n = 50_000;
        for src in [
            format!("var x = {}", "(".repeat(n)), // expression nesting
            format!("var x = {}", "[".repeat(n)), // array nesting
            format!("var x: {}int", "Array[".repeat(n)), // type nesting
            format!("match 0:\n\t{}0", "[".repeat(n)), // pattern nesting
            "\tif 0:\n".repeat(n),                // block / indentation nesting
        ] {
            let r = parse(&src);
            assert!(!r.diagnostics.is_empty(), "expected errors, got none");
        }
    }
}
