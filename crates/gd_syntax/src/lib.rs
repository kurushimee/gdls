//! `gd_syntax` — a faithful Rust port of Godot 4.6.3-stable's GDScript front-of-frontend: the tokenizer, the
//! recursive-descent + Pratt parser, and the AST.
//!
//! This crate has **no engine knowledge** (no type system, no native classes) so it can be fuzzed
//! and unit-tested in isolation. M1 lands the tokenizer ([`token`] + the lexer), the [`ast`] arena,
//! and the [`parser`] (recursive-descent + Pratt); [`parse`] runs the whole pipeline, always
//! returning a (possibly partial) tree, its syntax diagnostics, and a symbol outline
//! (`docs/02-frontend-port.md`). Type checking and warnings arrive later in `gd_analyze` (M3).

pub mod ast;
pub mod lexer;
pub mod parser;
pub mod span;
pub mod token;

pub use ast::ParseTree;
pub use lexer::{LexError, Lexer};
pub use span::{ByteSpan, LineCol, LineColRange};
pub use token::{Literal, Token, TokenKind};

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
}

/// Parse a GDScript source file into a (possibly partial) tree, its syntax diagnostics, and a
/// projected symbol outline. Mirrors `GDScriptParser::parse` + `parse_program`
/// (`docs/02-frontend-port.md`); never fails to return a result.
pub fn parse(source: &str) -> ParseResult {
    let mut parser = parser::Parser::new(source);
    parser.parse_program();
    let (tree, diagnostics) = parser.into_parts();
    let symbols = parser::document_symbols(&tree);
    ParseResult {
        tree,
        diagnostics,
        symbols,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_extends_is_clean() {
        let r = parse("extends Node\n");
        assert!(r.diagnostics.is_empty(), "diagnostics: {:?}", r.diagnostics);
        assert!(r.symbols.is_empty());
    }

    #[test]
    fn projects_top_level_symbols() {
        let r = parse("extends Node\n\nvar speed := 1.0\n\nfunc move():\n\tpass\n");
        assert!(r.diagnostics.is_empty(), "diagnostics: {:?}", r.diagnostics);
        let names: Vec<_> = r
            .symbols
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
