//! The recursive-descent + Pratt parser — a faithful port of `GDScriptParser`
//! (`modules/gdscript/gdscript_parser.cpp`).
//!
//! WP-B (#13) landed the engine: the [`Parser`] state, the token-driving primitives, panic-mode error
//! recovery, extent tracking, the 24-level [`Precedence`] ladder, the 100-entry [`RULES`] table, the
//! [`Parser::parse_precedence`] driver, and every expression `parse_*` function plus `parse_type`.
//! WP-C (#14) adds the statement/declaration machinery — `parse_program`, classes, members, suites,
//! statements, control flow, `match`/patterns, and the real `parse_lambda` — wiring all of it into
//! [`crate::parse`] via [`Parser::parse_program`].
//!
//! Faithful-port notes: prefix and infix share one `fn`-pointer type ([`ParseFn`]) exactly as Godot's
//! `ParseFunction` does (prefix is called with `previous_operand = None`). The lexer is pull-based and
//! parser-driven, so multiline mode is toggled here on `()`/`[]`/`{}` and popped by each function that
//! consumes the matching close token. The one deliberate deviation from Godot is a recursion-depth
//! guard ([`MAX_PARSE_DEPTH`]): Godot relies on the native stack, but an overflow there is an
//! uncatchable abort, which would be a fuzz release-blocker — so we bail with an error instead.
//!
//! Completion contexts, documentation-comment parsing, and annotation *application* (`@export` type
//! resolution, `@tool`/`@icon` effects) are analyzer/Phase-2 concerns and are intentionally omitted:
//! they live behind `make_completion_context`/`#ifdef TOOLS_ENABLED`/`annotation->apply` in Godot
//! and never change parse-phase error output on valid input.

use std::collections::HashMap;

use crate::ast::*;
use crate::dialect::Dialect;
use crate::lexer::Lexer;
use crate::span::ByteSpan;
use crate::token::{Literal, Token, TokenKind};
use crate::warning_names::warning_name_is_valid;
use crate::{Diagnostic, DocumentSymbol, ParseOptions, SymbolKind};

/// Maximum expression/statement nesting before the parser bails with an error instead of risking a
/// native stack overflow (an uncatchable abort). Deliberate deviation from Godot for fuzz safety.
const MAX_PARSE_DEPTH: u32 = 256;

/// A prefix or infix parse function. Mirrors Godot's single `ParseFunction` pointer type: the
/// `previous_operand` is `None` for a prefix call and `Some(operand)` for an infix call.
type ParseFn = fn(&mut Parser, Option<NodeId>, bool) -> Option<NodeId>;

/// Precedence ladder, ascending (`PREC_NONE`..`PREC_PRIMARY`), ported from `gdscript_parser.h:1428`.
/// Declaration order is load-bearing: `derive(Ord)` makes a higher variant bind tighter.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
#[repr(u8)]
enum Precedence {
    None,
    Assignment,
    Cast,
    Ternary,
    LogicOr,
    LogicAnd,
    LogicNot,
    ContentTest,
    Comparison,
    BitOr,
    BitXor,
    BitAnd,
    BitShift,
    AdditionSubtraction,
    Factor,
    Sign,
    BitNot,
    Power,
    TypeTest,
    Await,
    Call,
    Attribute,
    Subscript,
    Primary,
}

impl Precedence {
    /// The next-tighter precedence (Godot's `(Precedence)(precedence + 1)`), saturating at `Primary`.
    fn higher(self) -> Precedence {
        use Precedence::*;
        match self {
            None => Assignment,
            Assignment => Cast,
            Cast => Ternary,
            Ternary => LogicOr,
            LogicOr => LogicAnd,
            LogicAnd => LogicNot,
            LogicNot => ContentTest,
            ContentTest => Comparison,
            Comparison => BitOr,
            BitOr => BitXor,
            BitXor => BitAnd,
            BitAnd => BitShift,
            BitShift => AdditionSubtraction,
            AdditionSubtraction => Factor,
            Factor => Sign,
            Sign => BitNot,
            BitNot => Power,
            Power => TypeTest,
            TypeTest => Await,
            Await => Call,
            Call => Attribute,
            Attribute => Subscript,
            Subscript | Primary => Primary,
        }
    }
}

struct ParseRule {
    prefix: Option<ParseFn>,
    infix: Option<ParseFn>,
    prec: Precedence,
}

const fn rule(prefix: Option<ParseFn>, infix: Option<ParseFn>, prec: Precedence) -> ParseRule {
    ParseRule {
        prefix,
        infix,
        prec,
    }
}

/// The parse-rule table, one row per [`TokenKind`] in declaration order — a row-for-row transcription
/// of `gdscript_parser.cpp:4233` (`rules[]`). Indexed by `kind as usize`.
#[rustfmt::skip]
static RULES: [ParseRule; crate::token::TOKEN_COUNT] = {
    use Precedence as P;
    [
        rule(None, None, P::None),                                                                              // Empty
        rule(None, None, P::None),                                                                              // Annotation
        rule(Some(Parser::parse_identifier as ParseFn), None, P::None),                                         // Identifier
        rule(Some(Parser::parse_literal as ParseFn), None, P::None),                                            // Literal
        rule(None, Some(Parser::parse_binary_operator as ParseFn), P::Comparison),                              // Less
        rule(None, Some(Parser::parse_binary_operator as ParseFn), P::Comparison),                              // LessEqual
        rule(None, Some(Parser::parse_binary_operator as ParseFn), P::Comparison),                              // Greater
        rule(None, Some(Parser::parse_binary_operator as ParseFn), P::Comparison),                              // GreaterEqual
        rule(None, Some(Parser::parse_binary_operator as ParseFn), P::Comparison),                              // EqualEqual
        rule(None, Some(Parser::parse_binary_operator as ParseFn), P::Comparison),                              // BangEqual
        rule(None, Some(Parser::parse_binary_operator as ParseFn), P::LogicAnd),                                // And
        rule(None, Some(Parser::parse_binary_operator as ParseFn), P::LogicOr),                                 // Or
        rule(Some(Parser::parse_unary_operator as ParseFn), Some(Parser::parse_binary_not_in_operator as ParseFn), P::ContentTest), // Not
        rule(None, Some(Parser::parse_binary_operator as ParseFn), P::LogicAnd),                                // AmpersandAmpersand
        rule(None, Some(Parser::parse_binary_operator as ParseFn), P::LogicOr),                                 // PipePipe
        rule(Some(Parser::parse_unary_operator as ParseFn), None, P::None),                                     // Bang
        rule(None, Some(Parser::parse_binary_operator as ParseFn), P::BitAnd),                                  // Ampersand
        rule(None, Some(Parser::parse_binary_operator as ParseFn), P::BitOr),                                   // Pipe
        rule(Some(Parser::parse_unary_operator as ParseFn), None, P::None),                                     // Tilde
        rule(None, Some(Parser::parse_binary_operator as ParseFn), P::BitXor),                                  // Caret
        rule(None, Some(Parser::parse_binary_operator as ParseFn), P::BitShift),                                // LessLess
        rule(None, Some(Parser::parse_binary_operator as ParseFn), P::BitShift),                                // GreaterGreater
        rule(Some(Parser::parse_unary_operator as ParseFn), Some(Parser::parse_binary_operator as ParseFn), P::AdditionSubtraction), // Plus
        rule(Some(Parser::parse_unary_operator as ParseFn), Some(Parser::parse_binary_operator as ParseFn), P::AdditionSubtraction), // Minus
        rule(None, Some(Parser::parse_binary_operator as ParseFn), P::Factor),                                  // Star
        rule(None, Some(Parser::parse_binary_operator as ParseFn), P::Power),                                   // StarStar
        rule(None, Some(Parser::parse_binary_operator as ParseFn), P::Factor),                                  // Slash
        rule(Some(Parser::parse_get_node as ParseFn), Some(Parser::parse_binary_operator as ParseFn), P::Factor), // Percent
        rule(None, Some(Parser::parse_assignment as ParseFn), P::Assignment),                                   // Equal
        rule(None, Some(Parser::parse_assignment as ParseFn), P::Assignment),                                   // PlusEqual
        rule(None, Some(Parser::parse_assignment as ParseFn), P::Assignment),                                   // MinusEqual
        rule(None, Some(Parser::parse_assignment as ParseFn), P::Assignment),                                   // StarEqual
        rule(None, Some(Parser::parse_assignment as ParseFn), P::Assignment),                                   // StarStarEqual
        rule(None, Some(Parser::parse_assignment as ParseFn), P::Assignment),                                   // SlashEqual
        rule(None, Some(Parser::parse_assignment as ParseFn), P::Assignment),                                   // PercentEqual
        rule(None, Some(Parser::parse_assignment as ParseFn), P::Assignment),                                   // LessLessEqual
        rule(None, Some(Parser::parse_assignment as ParseFn), P::Assignment),                                   // GreaterGreaterEqual
        rule(None, Some(Parser::parse_assignment as ParseFn), P::Assignment),                                   // AmpersandEqual
        rule(None, Some(Parser::parse_assignment as ParseFn), P::Assignment),                                   // PipeEqual
        rule(None, Some(Parser::parse_assignment as ParseFn), P::Assignment),                                   // CaretEqual
        rule(None, Some(Parser::parse_ternary_operator as ParseFn), P::Ternary),                                // If
        rule(None, None, P::None),                                                                              // Elif
        rule(None, None, P::None),                                                                              // Else
        rule(None, None, P::None),                                                                              // For
        rule(None, None, P::None),                                                                              // While
        rule(None, None, P::None),                                                                              // Break
        rule(None, None, P::None),                                                                              // Continue
        rule(None, None, P::None),                                                                              // Pass
        rule(None, None, P::None),                                                                              // Return
        rule(None, None, P::None),                                                                              // Match
        rule(None, None, P::None),                                                                              // When
        rule(None, Some(Parser::parse_cast as ParseFn), P::Cast),                                               // As
        rule(None, None, P::None),                                                                              // Assert
        rule(Some(Parser::parse_await as ParseFn), None, P::None),                                              // Await
        rule(None, None, P::None),                                                                              // Breakpoint
        rule(None, None, P::None),                                                                              // Class
        rule(None, None, P::None),                                                                              // ClassName
        rule(None, None, P::None),                                                                              // Const
        rule(None, None, P::None),                                                                              // Enum
        rule(None, None, P::None),                                                                              // Extends
        rule(Some(Parser::parse_lambda as ParseFn), None, P::None),                                             // Func
        rule(None, Some(Parser::parse_binary_operator as ParseFn), P::ContentTest),                             // In
        rule(None, Some(Parser::parse_type_test as ParseFn), P::TypeTest),                                      // Is
        rule(None, None, P::None),                                                                              // Namespace
        rule(Some(Parser::parse_preload as ParseFn), None, P::None),                                            // Preload
        rule(Some(Parser::parse_self as ParseFn), None, P::None),                                               // SelfKw
        rule(None, None, P::None),                                                                              // Signal
        rule(None, None, P::None),                                                                              // Static
        rule(Some(Parser::parse_call as ParseFn), None, P::None),                                               // Super
        rule(None, None, P::None),                                                                              // Trait
        rule(None, None, P::None),                                                                              // Var
        rule(None, None, P::None),                                                                              // Void
        rule(Some(Parser::parse_yield as ParseFn), None, P::None),                                              // Yield
        rule(Some(Parser::parse_array as ParseFn), Some(Parser::parse_subscript as ParseFn), P::Subscript),     // BracketOpen
        rule(None, None, P::None),                                                                              // BracketClose
        rule(Some(Parser::parse_dictionary as ParseFn), None, P::None),                                         // BraceOpen
        rule(None, None, P::None),                                                                              // BraceClose
        rule(Some(Parser::parse_grouping as ParseFn), Some(Parser::parse_call as ParseFn), P::Call),            // ParenthesisOpen
        rule(None, None, P::None),                                                                              // ParenthesisClose
        rule(None, None, P::None),                                                                              // Comma
        rule(None, None, P::None),                                                                              // Semicolon
        rule(None, Some(Parser::parse_attribute as ParseFn), P::Attribute),                                     // Period
        rule(None, None, P::None),                                                                              // PeriodPeriod
        rule(None, None, P::None),                                                                              // PeriodPeriodPeriod
        rule(None, None, P::None),                                                                              // Colon
        rule(Some(Parser::parse_get_node as ParseFn), None, P::None),                                           // Dollar
        rule(None, None, P::None),                                                                              // ForwardArrow
        rule(None, None, P::None),                                                                              // Underscore
        rule(None, None, P::None),                                                                              // Newline
        rule(None, None, P::None),                                                                              // Indent
        rule(None, None, P::None),                                                                              // Dedent
        rule(Some(Parser::parse_builtin_constant as ParseFn), None, P::None),                                   // ConstPi
        rule(Some(Parser::parse_builtin_constant as ParseFn), None, P::None),                                   // ConstTau
        rule(Some(Parser::parse_builtin_constant as ParseFn), None, P::None),                                   // ConstInf
        rule(Some(Parser::parse_builtin_constant as ParseFn), None, P::None),                                   // ConstNan
        rule(None, None, P::None),                                                                              // VcsConflictMarker
        rule(None, None, P::None),                                                                              // Backtick
        rule(None, Some(Parser::parse_invalid_token as ParseFn), P::Cast),                                      // QuestionMark
        rule(None, None, P::None),                                                                              // Error
        rule(None, None, P::None),                                                                              // Eof
    ]
};

// Mirrors Godot's `static_assert(std_size(rules) == TK_MAX)` (gdscript_parser.cpp:4351).
const _: () = assert!(RULES.len() == crate::token::TOKEN_COUNT);

fn get_rule(kind: TokenKind) -> &'static ParseRule {
    &RULES[kind as usize]
}

/// A class-member parse function (`parse_variable`/`parse_constant`/…), dispatched by
/// [`Parser::parse_class_member`]. Mirrors Godot's `T *(GDScriptParser::*)(bool)` member pointer.
type MemberParseFn = fn(&mut Parser, bool) -> Option<NodeId>;

/// Annotation target kinds (`AnnotationInfo::TargetKind`, `gdscript_parser.h:1407`) as bit flags.
mod annotation_target {
    pub const NONE: u32 = 0;
    pub const SCRIPT: u32 = 1 << 0;
    pub const CLASS: u32 = 1 << 1;
    pub const VARIABLE: u32 = 1 << 2;
    pub const CONSTANT: u32 = 1 << 3;
    pub const SIGNAL: u32 = 1 << 4;
    pub const FUNCTION: u32 = 1 << 5;
    pub const STATEMENT: u32 = 1 << 6;
    pub const STANDALONE: u32 = 1 << 7;
    pub const CLASS_LEVEL: u32 = CLASS | VARIABLE | CONSTANT | SIGNAL | FUNCTION;
}

/// The target kinds an annotation may apply to, or `None` if `name` is not a registered annotation.
/// Transcribed from the `register_annotation` calls in `GDScriptParser::GDScriptParser()`
/// (`gdscript_parser.cpp:149`). Only the target-kind mapping is modeled; each annotation's `apply`
/// callback (several of which run in the parser in Godot, e.g. `@tool`/`@icon`/`@abstract`) is not
/// modeled in M1 — see the module-level note.
fn annotation_target_kind(name: &str) -> Option<u32> {
    use annotation_target::*;
    Some(match name {
        "@tool" | "@icon" | "@static_unload" => SCRIPT,
        "@abstract" => SCRIPT | CLASS | FUNCTION,
        "@onready" => VARIABLE,
        "@export"
        | "@export_enum"
        | "@export_file"
        | "@export_file_path"
        | "@export_dir"
        | "@export_global_file"
        | "@export_global_dir"
        | "@export_multiline"
        | "@export_placeholder"
        | "@export_range"
        | "@export_exp_easing"
        | "@export_color_no_alpha"
        | "@export_node_path"
        | "@export_flags"
        | "@export_flags_2d_render"
        | "@export_flags_2d_physics"
        | "@export_flags_2d_navigation"
        | "@export_flags_3d_render"
        | "@export_flags_3d_physics"
        | "@export_flags_3d_navigation"
        | "@export_flags_avoidance"
        | "@export_storage"
        | "@export_custom"
        | "@export_tool_button" => VARIABLE,
        "@export_category" | "@export_group" | "@export_subgroup" => STANDALONE,
        "@warning_ignore" => CLASS_LEVEL | STATEMENT,
        "@warning_ignore_start" | "@warning_ignore_restore" => STANDALONE,
        "@rpc" => FUNCTION,
        _ => return None,
    })
}

/// Every registered annotation `(name_with_@, takes_arguments)`, transcribed from the
/// `register_annotation(MethodInfo("@…", …), …)` calls in `GDScriptParser::GDScriptParser()`
/// (`gdscript_parser.cpp:149-190`) — the **single source of truth** the M8 `ANNOTATION` completion
/// renders from (Godot's `get_annotation_list` iterates the same `valid_annotations` registry).
/// `takes_arguments` is `true` exactly when the registering `MethodInfo` carried one or more
/// `PropertyInfo` parameters (so completion appends `(`, matching `gdscript_editor.cpp:3473`). The
/// names are kept in Godot's registration order; the completion renderer sorts as needed.
pub const REGISTERED_ANNOTATIONS: &[(&str, bool)] = &[
    ("@tool", false),
    ("@icon", true),
    ("@static_unload", false),
    ("@abstract", false),
    ("@onready", false),
    ("@export", false),
    ("@export_enum", true),
    ("@export_file", true),
    ("@export_file_path", true),
    ("@export_dir", false),
    ("@export_global_file", true),
    ("@export_global_dir", false),
    ("@export_multiline", true),
    ("@export_placeholder", true),
    ("@export_range", true),
    ("@export_exp_easing", true),
    ("@export_color_no_alpha", false),
    ("@export_node_path", true),
    ("@export_flags", true),
    ("@export_flags_2d_render", false),
    ("@export_flags_2d_physics", false),
    ("@export_flags_2d_navigation", false),
    ("@export_flags_3d_render", false),
    ("@export_flags_3d_physics", false),
    ("@export_flags_3d_navigation", false),
    ("@export_flags_avoidance", false),
    ("@export_storage", false),
    ("@export_custom", true),
    ("@export_tool_button", true),
    ("@export_category", true),
    ("@export_group", true),
    ("@export_subgroup", true),
    ("@warning_ignore", true),
    ("@warning_ignore_start", true),
    ("@warning_ignore_restore", true),
    ("@rpc", true),
];

/// The lowercase noun Godot uses for a class member of this kind (`Member::get_type_name`).
fn member_type_name(member: &Member) -> &'static str {
    match member {
        Member::Class(_) => "class",
        Member::Constant(_) => "constant",
        Member::Function(_) => "function",
        Member::Signal(_) => "signal",
        Member::Variable(_) => "variable",
        Member::Enum(_) => "enum",
        Member::EnumValue(_) => "enum value",
        Member::Group(_) => "group",
    }
}

/// The noun Godot uses for a block-scoped local of this kind (`SuiteNode::Local::get_name`).
fn local_kind_name(kind: LocalKind) -> &'static str {
    match kind {
        LocalKind::Constant => "constant",
        LocalKind::Variable => "variable",
        LocalKind::Parameter => "parameter",
        LocalKind::ForVariable => "for loop iterator",
        LocalKind::PatternBind => "pattern bind",
    }
}

/// Godot's `Variant::get_type_name` for the value an unexpected literal carries (used only in the
/// `extends <non-string>` diagnostic).
fn literal_type_name(literal: &Option<Literal>) -> &'static str {
    match literal {
        Some(Literal::Int(_)) => "int",
        Some(Literal::Float(_)) => "float",
        Some(Literal::String(_)) => "String",
        Some(Literal::StringName(_)) => "StringName",
        Some(Literal::NodePath(_)) => "NodePath",
        Some(Literal::Bool(_)) => "bool",
        Some(Literal::Null) | None => "Nil",
    }
}

/// Capitalize the first ASCII letter — Godot's `String::capitalize()` on the single lowercase
/// member-kind nouns ("variable" → "Variable") used in the duplicate-member diagnostic.
fn capitalize_first(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}

/// The parser. Holds the lexer, single-token lookahead (`previous`/`current`), the node arena, and
/// the contextual stacks Godot's `GDScriptParser` keeps.
pub struct Parser {
    /// The Godot feature release whose parser semantics are in force. See [`Dialect`] for the
    /// `DIALECT(...)` guard convention.
    dialect: Dialect,
    /// The `res://` path of the script being parsed, or `""` when unknown. Godot's parser reads it
    /// only to reject `class_name` in a built-in (scene-embedded) script.
    script_path: String,
    lexer: Lexer,
    previous: Token,
    current: Token,
    panic_mode: bool,
    tree: ParseTree,
    errors: Vec<Diagnostic>,

    current_class: Option<NodeId>,
    current_function: Option<NodeId>,
    current_suite: Option<NodeId>,
    in_lambda: bool,
    lambda_ended: bool,
    can_break: bool,
    can_continue: bool,

    /// Annotations parsed but not yet attached to their target (Godot's `annotation_stack`).
    annotation_stack: Vec<NodeId>,
    multiline_stack: Vec<bool>,
    nodes_in_progress: Vec<NodeId>,
    depth: u32,
}

/// The default-constructed `Token` the parser's `previous` starts as.
///
/// DIALECT(4.7): gdscript_tokenizer.h `Token` — the four position fields default to `1` rather
/// than `0`. The parser positions context-free errors at `previous`, so anything raised before the
/// first `advance()` used to report line 0 / column 0; Godot's own comment calls that reading
/// uninitialized memory, and an LSP conversion turned it into a negative position. Observable here
/// through `reset_extents_from_previous` and `eof_line`, which stamp `previous.loc` onto nodes.
fn empty_token(dialect: Dialect) -> Token {
    let one = crate::span::LineCol::new(1, 1);
    let loc = if dialect < Dialect::Godot4_7 {
        crate::span::LineColRange::default()
    } else {
        crate::span::LineColRange {
            start: one,
            end: one,
        }
    };
    Token {
        kind: TokenKind::Empty,
        span: ByteSpan::default(),
        loc,
        literal: None,
        source: Box::from(""),
    }
}

impl Parser {
    /// A parser at [`Dialect::DEFAULT`] with no script path. Prefer [`Self::new_with_options`]
    /// anywhere the project's dialect is known.
    pub fn new(source: &str) -> Self {
        Self::new_with_options(source, &ParseOptions::default())
    }

    pub fn new_with_options(source: &str, options: &ParseOptions<'_>) -> Self {
        let mut lexer = Lexer::new_with_dialect(source, options.dialect);
        let current = lexer.scan();
        let mut parser = Parser {
            dialect: options.dialect,
            script_path: options.script_path.to_owned(),
            lexer,
            previous: empty_token(options.dialect),
            current,
            panic_mode: false,
            tree: ParseTree::new(),
            errors: Vec::new(),
            current_class: None,
            current_function: None,
            current_suite: None,
            in_lambda: false,
            lambda_ended: false,
            can_break: false,
            can_continue: false,
            annotation_stack: Vec::new(),
            multiline_stack: Vec::new(),
            nodes_in_progress: Vec::new(),
            depth: 0,
        };
        parser.prime_first_token();
        parser
    }

    /// The dialect this parser parses under.
    #[must_use]
    pub fn dialect(&self) -> Dialect {
        self.dialect
    }

    /// The `res://` path of the script being parsed, or `""` when unknown.
    #[must_use]
    pub fn script_path(&self) -> &str {
        &self.script_path
    }

    /// Load `current` for the first time, skipping leading `ERROR` and `NEWLINE` tokens
    /// (`gdscript_parser.cpp:483`). The newline skip is what lets a file made up entirely of comments
    /// and blank lines parse as an empty class instead of "Unexpected newline in class body".
    fn prime_first_token(&mut self) {
        while matches!(self.current.kind, TokenKind::Error | TokenKind::Newline) {
            if self.current.kind == TokenKind::Error {
                self.panic_mode = true;
                self.errors.push(Diagnostic {
                    span: self.current.span,
                    message: self.current.source.to_string(),
                });
            }
            self.current = self.lexer.scan();
        }
        // gdscript_parser.cpp:482-489 — EOF right after the leading skip means an empty script
        // file. Godot pushes the EMPTY_FILE warning here; gdls records the signal on the tree
        // and `gd_analyze` (owner of the warning set) emits it.
        self.tree.starts_at_eof = self.current.kind == TokenKind::Eof;
    }

    // ----- node allocation & extent tracking (gdscript_parser.h:1467, cpp:5614) -----

    fn alloc(&mut self, kind: NodeKind) -> NodeId {
        let id = self.tree.push(Node::new(kind));
        let (span, loc) = (self.previous.span, self.previous.loc);
        let n = self.tree.get_mut(id);
        n.span = span;
        n.loc = loc;
        self.nodes_in_progress.push(id);
        id
    }

    /// A recovery node: not extent-tracked and not on the in-progress stack (Godot's
    /// `alloc_recovery_node`), used for synthesized placeholders during error recovery.
    fn alloc_recovery(&mut self, kind: NodeKind) -> NodeId {
        self.tree.push(Node::new(kind))
    }

    fn reset_extents_from_node(&mut self, id: NodeId, from: Option<NodeId>) {
        if let Some(from) = from {
            let (span, loc) = {
                let f = self.tree.get(from);
                (f.span, f.loc)
            };
            let n = self.tree.get_mut(id);
            n.span = span;
            n.loc = loc;
        }
    }

    /// Reset a node's extents to span a single token (Godot's `reset_extents(node, token)`), used to
    /// re-anchor a node's start at a keyword the caller already looked past.
    fn reset_extents_from_current(&mut self, id: NodeId) {
        let (span, loc) = (self.current.span, self.current.loc);
        let n = self.tree.get_mut(id);
        n.span = span;
        n.loc = loc;
    }

    fn reset_extents_from_previous(&mut self, id: NodeId) {
        let (span, loc) = (self.previous.span, self.previous.loc);
        let n = self.tree.get_mut(id);
        n.span = span;
        n.loc = loc;
    }

    fn update_extents(&mut self, id: NodeId) {
        let (span_end, loc_end) = (self.previous.span.end, self.previous.loc.end);
        let n = self.tree.get_mut(id);
        n.span.end = span_end;
        n.loc.end = loc_end;
    }

    fn complete_extents(&mut self, id: NodeId) {
        while self.nodes_in_progress.last().is_some_and(|&n| n != id) {
            // Parser bug: mismatch in the extents stack. Recover by popping.
            self.nodes_in_progress.pop();
        }
        self.nodes_in_progress.pop();
    }

    // ----- token driving (gdscript_parser.cpp:570) -----

    fn drain_current_errors(&mut self) {
        while self.current.kind == TokenKind::Error {
            self.panic_mode = true;
            self.errors.push(Diagnostic {
                span: self.current.span,
                message: self.current.source.to_string(),
            });
            self.current = self.lexer.scan();
        }
    }

    fn advance(&mut self) {
        self.lambda_ended = false;
        if self.current.kind == TokenKind::Eof {
            return; // Never advance past EOF (Godot ERR_FAILs; we degrade to a no-op).
        }
        self.previous = std::mem::replace(&mut self.current, self.lexer.scan());
        self.drain_current_errors();
        if self.previous.kind != TokenKind::Dedent {
            // `DEDENT` belongs to the next non-empty line, so don't stretch nodes over it.
            let (span_end, loc_end) = (self.previous.span.end, self.previous.loc.end);
            for i in 0..self.nodes_in_progress.len() {
                let id = self.nodes_in_progress[i];
                let n = self.tree.get_mut(id);
                n.span.end = span_end;
                n.loc.end = loc_end;
            }
        }
    }

    fn match_token(&mut self, kind: TokenKind) -> bool {
        if !self.check(kind) {
            return false;
        }
        self.advance();
        true
    }

    fn check(&self, kind: TokenKind) -> bool {
        if kind == TokenKind::Identifier {
            return self.current.kind.is_identifier();
        }
        self.current.kind == kind
    }

    fn consume(&mut self, kind: TokenKind, message: impl Into<String>) -> bool {
        if self.match_token(kind) {
            return true;
        }
        self.push_error(message);
        false
    }

    fn is_at_end(&self) -> bool {
        self.current.kind == TokenKind::Eof
    }

    fn push_error(&mut self, message: impl Into<String>) {
        self.panic_mode = true;
        self.errors.push(Diagnostic {
            span: self.previous.span,
            message: message.into(),
        });
    }

    fn synchronize(&mut self) {
        self.panic_mode = false;
        while !self.is_at_end() {
            if matches!(
                self.previous.kind,
                TokenKind::Newline | TokenKind::Semicolon
            ) {
                return;
            }
            match self.current.kind {
                TokenKind::Class
                | TokenKind::Func
                | TokenKind::Static
                | TokenKind::Var
                | TokenKind::Const
                | TokenKind::Signal
                | TokenKind::For
                | TokenKind::While
                | TokenKind::Match
                | TokenKind::Return
                | TokenKind::Annotation => return,
                _ => {}
            }
            self.advance();
        }
    }

    fn push_multiline(&mut self, state: bool) {
        self.multiline_stack.push(state);
        self.lexer.set_multiline_mode(state);
        if state {
            // Consume whitespace tokens already queued (don't use advance: keep `previous`).
            while matches!(
                self.current.kind,
                TokenKind::Newline | TokenKind::Indent | TokenKind::Dedent
            ) {
                self.current = self.lexer.scan();
            }
            self.drain_current_errors();
        }
    }

    fn pop_multiline(&mut self) {
        self.multiline_stack.pop();
        let state = self.multiline_stack.last().copied().unwrap_or(false);
        self.lexer.set_multiline_mode(state);
    }

    fn is_statement_end_token(&self) -> bool {
        matches!(
            self.current.kind,
            TokenKind::Newline | TokenKind::Semicolon | TokenKind::Eof
        )
    }

    fn is_statement_end(&self) -> bool {
        self.lambda_ended || self.in_lambda || self.is_statement_end_token()
    }

    // ----- Pratt expression engine (gdscript_parser.cpp:2734) -----

    fn parse_expression(&mut self, can_assign: bool, stop_on_assign: bool) -> Option<NodeId> {
        self.parse_precedence(Precedence::Assignment, can_assign, stop_on_assign)
    }

    fn parse_precedence(
        &mut self,
        precedence: Precedence,
        can_assign: bool,
        stop_on_assign: bool,
    ) -> Option<NodeId> {
        if self.depth >= MAX_PARSE_DEPTH {
            self.push_error("Expression is too deeply nested.");
            return None;
        }
        self.depth += 1;
        let result = self.parse_precedence_impl(precedence, can_assign, stop_on_assign);
        self.depth -= 1;
        result
    }

    fn parse_precedence_impl(
        &mut self,
        precedence: Precedence,
        can_assign: bool,
        stop_on_assign: bool,
    ) -> Option<NodeId> {
        // Switch multiline on for grouping tokens before the tokenizer makes whitespace.
        match self.current.kind {
            TokenKind::ParenthesisOpen | TokenKind::BraceOpen | TokenKind::BracketOpen => {
                self.push_multiline(true)
            }
            _ => {}
        }

        let mut token_type = self.current.kind;
        if token_type.is_identifier() {
            token_type = TokenKind::Identifier;
        }
        let Some(prefix_rule) = get_rule(token_type).prefix else {
            // Expected expression. The caller emits the proper error message.
            return None;
        };

        self.advance(); // Only consume the token if there's a valid rule.
        let mut previous_operand = prefix_rule(self, None, can_assign);

        while precedence <= get_rule(self.current.kind).prec {
            if previous_operand.is_none()
                || (stop_on_assign && self.current.kind == TokenKind::Equal)
                || self.lambda_ended
            {
                return previous_operand;
            }
            match self.current.kind {
                TokenKind::ParenthesisOpen | TokenKind::BracketOpen => self.push_multiline(true),
                _ => {}
            }
            self.advance();
            let Some(infix_rule) = get_rule(self.previous.kind).infix else {
                return previous_operand;
            };
            previous_operand = infix_rule(self, previous_operand, can_assign);
        }

        previous_operand
    }

    // ----- prefix / infix parse functions -----

    /// Decl-position identifier (Godot's zero-arg `parse_identifier()`): used by type/attribute/super.
    fn parse_identifier_node(&mut self) -> Option<NodeId> {
        self.parse_identifier(None, false)
    }

    fn parse_identifier(&mut self, _prev: Option<NodeId>, _can_assign: bool) -> Option<NodeId> {
        if !self.previous.kind.is_identifier() {
            self.push_error("Parser bug: parsing identifier node without identifier token.");
            return None;
        }
        let name = self.previous.source.to_string();
        let id = self.alloc(NodeKind::Identifier(IdentifierNode { name }));
        self.complete_extents(id);
        Some(id)
    }

    fn parse_literal(&mut self, prev: Option<NodeId>, _can_assign: bool) -> Option<NodeId> {
        if self.previous.kind != TokenKind::Literal {
            self.push_error("Parser bug: parsing literal node without literal token.");
            return None;
        }
        let value = self.previous.literal.clone().unwrap_or(Literal::Null);
        let id = self.alloc(NodeKind::Literal(LiteralNode { value }));
        self.reset_extents_from_node(id, prev);
        self.update_extents(id);
        self.complete_extents(id);
        Some(id)
    }

    fn parse_self(&mut self, _prev: Option<NodeId>, _can_assign: bool) -> Option<NodeId> {
        // gdscript_parser.cpp:2900-2902 — a static function has no instance to name.
        if self.current_function.is_some_and(
            |f| matches!(&self.tree.get(f).kind, NodeKind::Function(fc) if fc.is_static),
        ) {
            self.push_error(r#"Cannot use "self" inside a static function."#);
        }
        let id = self.alloc(NodeKind::SelfExpr);
        self.complete_extents(id);
        Some(id)
    }

    fn parse_builtin_constant(
        &mut self,
        _prev: Option<NodeId>,
        _can_assign: bool,
    ) -> Option<NodeId> {
        let value = match self.previous.kind {
            TokenKind::ConstPi => Literal::Float(std::f64::consts::PI),
            TokenKind::ConstTau => Literal::Float(std::f64::consts::TAU),
            TokenKind::ConstInf => Literal::Float(f64::INFINITY),
            TokenKind::ConstNan => Literal::Float(f64::NAN),
            _ => return None, // Unreachable.
        };
        let id = self.alloc(NodeKind::Literal(LiteralNode { value }));
        self.complete_extents(id);
        Some(id)
    }

    fn parse_unary_operator(&mut self, _prev: Option<NodeId>, _can_assign: bool) -> Option<NodeId> {
        let op_type = self.previous.kind;
        let id = self.alloc(NodeKind::UnaryOp(UnaryOpNode {
            operation: UnaryOp::Positive,
            operand: None,
        }));
        let (operation, operand, message) = match op_type {
            TokenKind::Minus => (
                UnaryOp::Negative,
                self.parse_precedence(Precedence::Sign, false, false),
                r#"Expected expression after "-" operator."#,
            ),
            TokenKind::Plus => (
                UnaryOp::Positive,
                self.parse_precedence(Precedence::Sign, false, false),
                r#"Expected expression after "+" operator."#,
            ),
            TokenKind::Tilde => (
                UnaryOp::Complement,
                self.parse_precedence(Precedence::BitNot, false, false),
                r#"Expected expression after "~" operator."#,
            ),
            TokenKind::Not => (
                UnaryOp::LogicNot,
                self.parse_precedence(Precedence::LogicNot, false, false),
                r#"Expected expression after "not" operator."#,
            ),
            TokenKind::Bang => (
                UnaryOp::LogicNot,
                self.parse_precedence(Precedence::LogicNot, false, false),
                r#"Expected expression after "!" operator."#,
            ),
            _ => {
                self.complete_extents(id);
                return None; // Unreachable.
            }
        };
        if operand.is_none() {
            self.push_error(message);
        }
        self.complete_extents(id);
        if let NodeKind::UnaryOp(n) = &mut self.tree.get_mut(id).kind {
            n.operation = operation;
            n.operand = operand;
        }
        Some(id)
    }

    fn parse_binary_not_in_operator(
        &mut self,
        prev: Option<NodeId>,
        can_assign: bool,
    ) -> Option<NodeId> {
        // `not in`: consume the `in` then parse a plain content-test, wrapping it in a logic-not.
        let id = self.alloc(NodeKind::UnaryOp(UnaryOpNode {
            operation: UnaryOp::LogicNot,
            operand: None,
        }));
        self.reset_extents_from_node(id, prev);
        self.update_extents(id);
        self.consume(
            TokenKind::In,
            r#"Expected "in" after "not" in content-test operator."#,
        );
        let in_operation = self.parse_binary_operator(prev, can_assign);
        self.complete_extents(id);
        if let NodeKind::UnaryOp(n) = &mut self.tree.get_mut(id).kind {
            n.operand = in_operation;
        }
        Some(id)
    }

    fn parse_binary_operator(&mut self, prev: Option<NodeId>, _can_assign: bool) -> Option<NodeId> {
        let op = self.previous.kind;
        let op_name = op.name();
        let id = self.alloc(NodeKind::BinaryOp(BinaryOpNode {
            operation: BinaryOp::Addition,
            left_operand: prev,
            right_operand: None,
        }));
        self.reset_extents_from_node(id, prev);
        self.update_extents(id);

        let precedence = get_rule(op).prec.higher();
        let right = self.parse_precedence(precedence, false, false);
        self.complete_extents(id);

        if right.is_none() {
            self.push_error(format!(
                r#"Expected expression after "{op_name}" operator."#
            ));
        }

        let operation = match op {
            TokenKind::Plus => BinaryOp::Addition,
            TokenKind::Minus => BinaryOp::Subtraction,
            TokenKind::Star => BinaryOp::Multiplication,
            TokenKind::Slash => BinaryOp::Division,
            TokenKind::Percent => BinaryOp::Modulo,
            TokenKind::StarStar => BinaryOp::Power,
            TokenKind::LessLess => BinaryOp::BitLeftShift,
            TokenKind::GreaterGreater => BinaryOp::BitRightShift,
            TokenKind::Ampersand => BinaryOp::BitAnd,
            TokenKind::Pipe => BinaryOp::BitOr,
            TokenKind::Caret => BinaryOp::BitXor,
            TokenKind::And | TokenKind::AmpersandAmpersand => BinaryOp::LogicAnd,
            TokenKind::Or | TokenKind::PipePipe => BinaryOp::LogicOr,
            TokenKind::In => BinaryOp::ContentTest,
            TokenKind::EqualEqual => BinaryOp::CompEqual,
            TokenKind::BangEqual => BinaryOp::CompNotEqual,
            TokenKind::Less => BinaryOp::CompLess,
            TokenKind::LessEqual => BinaryOp::CompLessEqual,
            TokenKind::Greater => BinaryOp::CompGreater,
            TokenKind::GreaterEqual => BinaryOp::CompGreaterEqual,
            _ => return None, // Unreachable.
        };
        if let NodeKind::BinaryOp(n) = &mut self.tree.get_mut(id).kind {
            n.operation = operation;
            n.right_operand = right;
        }
        Some(id)
    }

    fn parse_ternary_operator(
        &mut self,
        prev: Option<NodeId>,
        _can_assign: bool,
    ) -> Option<NodeId> {
        let id = self.alloc(NodeKind::TernaryOp(TernaryOpNode {
            condition: None,
            true_expr: prev,
            false_expr: None,
        }));
        self.reset_extents_from_node(id, prev);
        self.update_extents(id);

        let condition = self.parse_precedence(Precedence::Ternary, false, false);
        if condition.is_none() {
            self.push_error(r#"Expected expression as ternary condition after "if"."#);
        }
        self.consume(
            TokenKind::Else,
            r#"Expected "else" after ternary operator condition."#,
        );
        let false_expr = self.parse_precedence(Precedence::Ternary, false, false);
        if false_expr.is_none() {
            self.push_error(r#"Expected expression after "else"."#);
        }
        self.complete_extents(id);
        if let NodeKind::TernaryOp(n) = &mut self.tree.get_mut(id).kind {
            n.condition = condition;
            n.false_expr = false_expr;
        }
        Some(id)
    }

    fn parse_assignment(&mut self, prev: Option<NodeId>, can_assign: bool) -> Option<NodeId> {
        if !can_assign {
            self.push_error("Assignment is not allowed inside an expression.");
            return self.parse_expression(false, false);
        }
        let Some(target) = prev else {
            return self.parse_expression(false, false);
        };
        let valid_target = matches!(
            self.tree.get(target).kind,
            NodeKind::Identifier(_) | NodeKind::Subscript(_)
        );
        if !valid_target {
            self.push_error(
                "Only identifier, attribute access, and subscription access can be used as assignment target.",
            );
            return self.parse_expression(false, false);
        }

        let id = self.alloc(NodeKind::Assignment(AssignmentNode {
            operation: AssignOp::None,
            assignee: prev,
            assigned_value: None,
        }));
        self.reset_extents_from_node(id, prev);
        self.update_extents(id);

        let operation = match self.previous.kind {
            TokenKind::Equal => AssignOp::None,
            TokenKind::PlusEqual => AssignOp::Addition,
            TokenKind::MinusEqual => AssignOp::Subtraction,
            TokenKind::StarEqual => AssignOp::Multiplication,
            TokenKind::StarStarEqual => AssignOp::Power,
            TokenKind::SlashEqual => AssignOp::Division,
            TokenKind::PercentEqual => AssignOp::Modulo,
            TokenKind::LessLessEqual => AssignOp::BitShiftLeft,
            TokenKind::GreaterGreaterEqual => AssignOp::BitShiftRight,
            TokenKind::AmpersandEqual => AssignOp::BitAnd,
            TokenKind::PipeEqual => AssignOp::BitOr,
            TokenKind::CaretEqual => AssignOp::BitXor,
            _ => AssignOp::None, // Unreachable.
        };
        let assigned_value = self.parse_expression(false, false);
        if assigned_value.is_none() {
            self.push_error(r#"Expected an expression after "="."#);
        }
        self.complete_extents(id);
        if let NodeKind::Assignment(n) = &mut self.tree.get_mut(id).kind {
            n.operation = operation;
            n.assigned_value = assigned_value;
        }
        Some(id)
    }

    fn parse_await(&mut self, _prev: Option<NodeId>, _can_assign: bool) -> Option<NodeId> {
        let id = self.alloc(NodeKind::Await(AwaitNode { to_await: None }));
        let element = self.parse_precedence(Precedence::Await, false, false);
        if element.is_none() {
            self.push_error(r#"Expected signal or coroutine after "await"."#);
        }
        self.complete_extents(id);
        if let NodeKind::Await(n) = &mut self.tree.get_mut(id).kind {
            n.to_await = element;
        }
        // gdscript_parser.cpp:3232-3234 — Godot sets `current_function->is_coroutine = true`
        // here so the analyzer can later propagate the flag to the call's return type for the
        // MISSING_AWAIT warning. Mirrored exactly; the comment "might be null in a getter or
        // setter" matches our `current_function.is_some()` gate.
        if let Some(func_id) = self.current_function {
            if let NodeKind::Function(f) = &mut self.tree.get_mut(func_id).kind {
                f.is_coroutine = true;
            }
        }
        Some(id)
    }

    fn parse_array(&mut self, _prev: Option<NodeId>, _can_assign: bool) -> Option<NodeId> {
        let id = self.alloc(NodeKind::Array(ArrayNode::default()));
        let mut elements = Vec::new();
        if !self.check(TokenKind::BracketClose) {
            loop {
                if self.check(TokenKind::BracketClose) {
                    break; // Trailing comma.
                }
                if let Some(element) = self.parse_expression(false, false) {
                    elements.push(element);
                } else {
                    self.push_error(r#"Expected expression as array element."#);
                }
                if !self.match_token(TokenKind::Comma) || self.is_at_end() {
                    break;
                }
            }
        }
        self.pop_multiline();
        self.consume(
            TokenKind::BracketClose,
            r#"Expected closing "]" after array elements."#,
        );
        self.complete_extents(id);
        if let NodeKind::Array(n) = &mut self.tree.get_mut(id).kind {
            n.elements = elements;
        }
        Some(id)
    }

    fn parse_dictionary(&mut self, _prev: Option<NodeId>, _can_assign: bool) -> Option<NodeId> {
        let id = self.alloc(NodeKind::Dictionary(DictionaryNode::default()));
        let mut elements = Vec::new();
        let mut style: Option<DictStyle> = None;
        if !self.check(TokenKind::BraceClose) {
            loop {
                if self.check(TokenKind::BraceClose) {
                    break; // Trailing comma.
                }
                let key = self.parse_expression(false, true); // Stop on "=" for Lua-table check.
                if key.is_none() {
                    self.push_error(r#"Expected expression as dictionary key."#);
                }
                if style.is_none() {
                    style = match self.current.kind {
                        TokenKind::Colon => Some(DictStyle::PythonDict),
                        TokenKind::Equal => Some(DictStyle::LuaTable),
                        _ => {
                            self.push_error(r#"Expected ":" or "=" after dictionary key."#);
                            Some(DictStyle::PythonDict)
                        }
                    };
                }
                match style {
                    Some(DictStyle::LuaTable) => {
                        let key_ok = key.is_some_and(|k| {
                            matches!(
                                self.tree.get(k).kind,
                                NodeKind::Identifier(_) | NodeKind::Literal(_)
                            )
                        });
                        if key.is_some() && !key_ok {
                            self.push_error(
                                r#"Expected identifier or string as Lua-style dictionary key (e.g "{ key = value }")."#,
                            );
                        }
                        if !self.match_token(TokenKind::Equal) {
                            if self.match_token(TokenKind::Colon) {
                                self.push_error(
                                    r#"Expected "=" after dictionary key. Mixing dictionary styles is not allowed."#,
                                );
                                self.advance();
                            } else {
                                self.push_error(r#"Expected "=" after dictionary key."#);
                            }
                        }
                    }
                    _ => {
                        if !self.match_token(TokenKind::Colon) {
                            if self.match_token(TokenKind::Equal) {
                                self.push_error(
                                    r#"Expected ":" after dictionary key. Mixing dictionary styles is not allowed."#,
                                );
                                self.advance();
                            } else {
                                self.push_error(r#"Expected ":" after dictionary key."#);
                            }
                        }
                    }
                }
                let value = self.parse_expression(false, false);
                if value.is_none() {
                    self.push_error(r#"Expected expression as dictionary value."#);
                }
                // Phrase-level recovery: insert a dummy literal for a missing key or value.
                match (key, value) {
                    (Some(k), Some(v)) => elements.push(KeyValue {
                        key: Some(k),
                        value: Some(v),
                    }),
                    (Some(k), None) => {
                        let dummy = self.alloc_recovery(NodeKind::Literal(LiteralNode {
                            value: Literal::Null,
                        }));
                        elements.push(KeyValue {
                            key: Some(k),
                            value: Some(dummy),
                        });
                    }
                    (None, Some(v)) => {
                        let dummy = self.alloc_recovery(NodeKind::Literal(LiteralNode {
                            value: Literal::Null,
                        }));
                        elements.push(KeyValue {
                            key: Some(dummy),
                            value: Some(v),
                        });
                    }
                    (None, None) => {}
                }
                if !self.match_token(TokenKind::Comma) || self.is_at_end() {
                    break;
                }
            }
        }
        self.pop_multiline();
        self.consume(
            TokenKind::BraceClose,
            r#"Expected closing "}" after dictionary elements."#,
        );
        self.complete_extents(id);
        if let NodeKind::Dictionary(n) = &mut self.tree.get_mut(id).kind {
            n.elements = elements;
            n.style = style;
        }
        Some(id)
    }

    fn parse_grouping(&mut self, _prev: Option<NodeId>, _can_assign: bool) -> Option<NodeId> {
        let grouped = self.parse_expression(false, false);
        self.pop_multiline();
        if grouped.is_none() {
            self.push_error(r#"Expected grouping expression."#);
        } else {
            self.consume(
                TokenKind::ParenthesisClose,
                r#"Expected closing ")" after grouping expression."#,
            );
        }
        grouped
    }

    fn parse_attribute(&mut self, prev: Option<NodeId>, _can_assign: bool) -> Option<NodeId> {
        let id = self.alloc(NodeKind::Subscript(SubscriptNode {
            base: prev,
            access: Some(SubscriptAccess::Attribute(None)),
        }));
        self.reset_extents_from_node(id, prev);
        self.update_extents(id);

        if self.current.kind.is_node_name() {
            self.current.kind = TokenKind::Identifier;
        }
        if !self.consume(
            TokenKind::Identifier,
            r#"Expected identifier after "." for attribute access."#,
        ) {
            self.complete_extents(id);
            return Some(id);
        }
        let attribute = self.parse_identifier_node();
        self.complete_extents(id);
        if let NodeKind::Subscript(n) = &mut self.tree.get_mut(id).kind {
            n.access = Some(SubscriptAccess::Attribute(attribute));
        }
        Some(id)
    }

    fn parse_subscript(&mut self, prev: Option<NodeId>, _can_assign: bool) -> Option<NodeId> {
        let id = self.alloc(NodeKind::Subscript(SubscriptNode {
            base: prev,
            access: Some(SubscriptAccess::Index(None)),
        }));
        self.reset_extents_from_node(id, prev);
        self.update_extents(id);

        let index = self.parse_expression(false, false);
        if index.is_none() {
            self.push_error(r#"Expected expression after "["."#);
        }
        self.pop_multiline();
        self.consume(
            TokenKind::BracketClose,
            r#"Expected "]" after subscription index."#,
        );
        self.complete_extents(id);
        if let NodeKind::Subscript(n) = &mut self.tree.get_mut(id).kind {
            n.access = Some(SubscriptAccess::Index(index));
        }
        Some(id)
    }

    fn parse_cast(&mut self, prev: Option<NodeId>, _can_assign: bool) -> Option<NodeId> {
        let id = self.alloc(NodeKind::Cast(CastNode {
            operand: prev,
            cast_type: None,
        }));
        self.reset_extents_from_node(id, prev);
        self.update_extents(id);

        let cast_type = self.parse_type(false);
        self.complete_extents(id);
        if cast_type.is_none() {
            self.push_error(r#"Expected type specifier after "as"."#);
            return prev;
        }
        if let NodeKind::Cast(n) = &mut self.tree.get_mut(id).kind {
            n.cast_type = cast_type;
        }
        Some(id)
    }

    fn parse_type_test(&mut self, prev: Option<NodeId>, _can_assign: bool) -> Option<NodeId> {
        // `x is T` / `x is not T`: wrap the type-test in a logic-not for the negated form.
        let not_node = if self.match_token(TokenKind::Not) {
            let n = self.alloc(NodeKind::UnaryOp(UnaryOpNode {
                operation: UnaryOp::LogicNot,
                operand: None,
            }));
            self.reset_extents_from_node(n, prev);
            self.update_extents(n);
            Some(n)
        } else {
            None
        };

        let id = self.alloc(NodeKind::TypeTest(TypeTestNode {
            operand: prev,
            test_type: None,
        }));
        self.reset_extents_from_node(id, prev);
        self.update_extents(id);

        let test_type = self.parse_type(false);
        self.complete_extents(id);
        if let NodeKind::TypeTest(n) = &mut self.tree.get_mut(id).kind {
            n.test_type = test_type;
        }

        if let Some(n) = not_node {
            self.complete_extents(n);
            if let NodeKind::UnaryOp(u) = &mut self.tree.get_mut(n).kind {
                u.operand = Some(id);
            }
        }

        if test_type.is_none() {
            if not_node.is_none() {
                self.push_error(r#"Expected type specifier after "is"."#);
            } else {
                self.push_error(r#"Expected type specifier after "is not"."#);
            }
        }

        Some(not_node.unwrap_or(id))
    }

    fn parse_call(&mut self, prev: Option<NodeId>, _can_assign: bool) -> Option<NodeId> {
        let id = self.alloc(NodeKind::Call(CallNode::default()));
        self.reset_extents_from_node(id, prev);

        let mut callee = prev;
        let mut function_name = String::new();
        let mut is_super = false;

        if self.previous.kind == TokenKind::Super {
            is_super = true;
            // DIALECT(4.7): gdscript_parser.cpp parse_call() — 4.6 entered multiline mode on
            // seeing `super` at all, so a malformed `super` (one with no parentheses) left the
            // tokenizer swallowing NEWLINE/INDENT/DEDENT and cascaded junk errors across the
            // lines that followed. 4.7 enters it only once the `(` is really there. Every message
            // and position is identical either way; what differs is the follow-on error set after
            // a bad `super`.
            let eager_multiline = self.dialect < Dialect::Godot4_7;
            if eager_multiline {
                self.push_multiline(true);
            }
            if self.check(TokenKind::ParenthesisOpen) {
                if !eager_multiline {
                    self.push_multiline(true);
                }
                self.advance();
                match self.current_function {
                    None => {
                        self.push_error(
                            r#"Cannot use implicit "super" call outside of a function."#,
                        );
                        self.pop_multiline();
                        self.complete_extents(id);
                        return None;
                    }
                    Some(func) => {
                        function_name = self.function_name_of(func);
                    }
                }
            } else {
                self.consume(TokenKind::Period, r#"Expected "." or "(" after "super"."#);
                if !self.consume(
                    TokenKind::Identifier,
                    r#"Expected function name after "."."#,
                ) {
                    if eager_multiline {
                        self.pop_multiline();
                    }
                    self.complete_extents(id);
                    return None;
                }
                let identifier = self.parse_identifier_node();
                callee = identifier;
                function_name = identifier
                    .map(|i| self.identifier_name(i))
                    .unwrap_or_default();
                if self.check(TokenKind::ParenthesisOpen) {
                    if !eager_multiline {
                        self.push_multiline(true);
                    }
                    self.advance();
                } else {
                    // 4.6 raised this through `consume`, which positions at `previous` exactly as
                    // this bare `push_error` does — the text and span are the same.
                    self.push_error(r#"Expected "(" after function name."#);
                    if eager_multiline {
                        self.pop_multiline();
                    }
                    self.complete_extents(id);
                    return None;
                }
            }
        } else {
            callee = prev;
            match prev.map(|c| &self.tree.get(c).kind) {
                None => {
                    self.push_error(
                        r#"Cannot call on an expression. Use ".call()" if it's a Callable."#,
                    );
                }
                Some(NodeKind::Identifier(ident)) => {
                    function_name = ident.name.clone();
                }
                Some(NodeKind::Subscript(sub)) => match sub.access {
                    Some(SubscriptAccess::Attribute(Some(attr))) => {
                        function_name = self.identifier_name(attr);
                    }
                    Some(SubscriptAccess::Attribute(None)) => {}
                    _ => {
                        self.push_error(
                            r#"Cannot call on an expression. Use ".call()" if it's a Callable."#,
                        );
                    }
                },
                Some(_) => {
                    self.push_error(
                        r#"Cannot call on an expression. Use ".call()" if it's a Callable."#,
                    );
                }
            }
        }

        // Arguments.
        let mut arguments = Vec::new();
        loop {
            if self.check(TokenKind::ParenthesisClose) {
                break; // Trailing comma.
            }
            if let Some(argument) = self.parse_expression(false, false) {
                arguments.push(argument);
            } else {
                self.push_error(r#"Expected expression as the function argument."#);
            }
            if !self.match_token(TokenKind::Comma) {
                break;
            }
        }
        self.pop_multiline();
        self.consume(
            TokenKind::ParenthesisClose,
            r#"Expected closing ")" after call arguments."#,
        );
        self.complete_extents(id);
        if let NodeKind::Call(n) = &mut self.tree.get_mut(id).kind {
            n.callee = callee;
            n.arguments = arguments;
            n.function_name = function_name;
            n.is_super = is_super;
        }
        Some(id)
    }

    fn parse_get_node(&mut self, _prev: Option<NodeId>, _can_assign: bool) -> Option<NodeId> {
        if !self.current.kind.is_node_name()
            && !self.check(TokenKind::Literal)
            && !self.check(TokenKind::Slash)
            && !self.check(TokenKind::Percent)
        {
            self.push_error(format!(
                r#"Expected node path as string or identifier after "{}"."#,
                self.previous.name()
            ));
            return None;
        }
        if self.check(TokenKind::Literal)
            && !matches!(self.current.literal, Some(Literal::String(_)))
        {
            self.push_error(format!(
                r#"Expected node path as string or identifier after "{}"."#,
                self.previous.name()
            ));
            return None;
        }

        let id = self.alloc(NodeKind::GetNode(GetNodeNode {
            full_path: String::new(),
            use_dollar: true,
        }));
        let mut full_path = String::new();
        let mut use_dollar = true;

        #[derive(PartialEq)]
        enum PathState {
            Start,
            Slash,
            Percent,
            NodeName,
        }
        let mut path_state = PathState::Start;

        if self.previous.kind == TokenKind::Dollar {
            self.match_token(TokenKind::Slash); // Optional initial slash.
        } else {
            use_dollar = false;
        }

        loop {
            if self.previous.kind == TokenKind::Percent {
                if path_state != PathState::Start && path_state != PathState::Slash {
                    self.push_error(
                        r#""%" is only valid in the beginning of a node name (either after "$" or after "/")"#,
                    );
                    self.complete_extents(id);
                    return None;
                }
                full_path.push('%');
                path_state = PathState::Percent;
            } else if self.previous.kind == TokenKind::Slash {
                if path_state != PathState::Start && path_state != PathState::NodeName {
                    self.push_error(
                        r#""/" is only valid at the beginning of the path or after a node name."#,
                    );
                    self.complete_extents(id);
                    return None;
                }
                full_path.push('/');
                path_state = PathState::Slash;
            }

            if self.match_token(TokenKind::Literal) {
                match &self.previous.literal {
                    Some(Literal::String(s)) => {
                        full_path.push_str(s);
                        path_state = PathState::NodeName;
                    }
                    _ => {
                        let prev_token = match path_state {
                            PathState::Start => "$",
                            PathState::Percent => "%",
                            PathState::Slash => "/",
                            PathState::NodeName => "",
                        };
                        self.push_error(format!(
                            r#"Expected node path as string or identifier after "{prev_token}"."#
                        ));
                        self.complete_extents(id);
                        return None;
                    }
                }
            } else if self.current.kind.is_node_name() {
                self.advance();
                full_path.push_str(&self.previous.source);
                path_state = PathState::NodeName;
            } else if !self.check(TokenKind::Slash) && !self.check(TokenKind::Percent) {
                self.push_error(format!(
                    r#"Unexpected "{}" in node path."#,
                    self.current.name()
                ));
                self.complete_extents(id);
                return None;
            }

            if !(self.match_token(TokenKind::Slash) || self.match_token(TokenKind::Percent)) {
                break;
            }
        }

        self.complete_extents(id);
        if let NodeKind::GetNode(n) = &mut self.tree.get_mut(id).kind {
            n.full_path = full_path;
            n.use_dollar = use_dollar;
        }
        Some(id)
    }

    fn parse_preload(&mut self, _prev: Option<NodeId>, _can_assign: bool) -> Option<NodeId> {
        let id = self.alloc(NodeKind::Preload(PreloadNode { path: None }));
        self.push_multiline(true);
        self.consume(
            TokenKind::ParenthesisOpen,
            r#"Expected "(" after "preload"."#,
        );
        let path = self.parse_expression(false, false);
        if path.is_none() {
            self.push_error(r#"Expected resource path after "("."#);
        }
        self.match_token(TokenKind::Comma); // Trailing comma.
        self.pop_multiline();
        self.consume(
            TokenKind::ParenthesisClose,
            r#"Expected ")" after preload path."#,
        );
        self.complete_extents(id);
        if let NodeKind::Preload(n) = &mut self.tree.get_mut(id).kind {
            n.path = path;
        }
        Some(id)
    }

    fn parse_lambda(&mut self, _prev: Option<NodeId>, _can_assign: bool) -> Option<NodeId> {
        let lambda = self.alloc(NodeKind::Lambda(LambdaNode::default()));
        let function = self.alloc(NodeKind::Function(FunctionNode::default()));

        // A lambda inherits its enclosing function's static-ness (`cpp:3712`).
        let is_static = self.current_function.is_some_and(
            |f| matches!(&self.tree.get(f).kind, NodeKind::Function(fc) if fc.is_static),
        );
        let mut identifier = None;
        if self.match_token(TokenKind::Identifier) {
            identifier = self.parse_identifier_node();
        }

        let multiline_context = self.multiline_stack.last().copied().unwrap_or(false);

        // Reset the multiline stack since we don't want the enclosing multiline mode in the body.
        self.push_multiline(false);
        if multiline_context {
            self.lexer.push_expression_indented_block();
        }

        self.push_multiline(true); // For the parameters.
        if identifier.is_some() {
            self.consume(
                TokenKind::ParenthesisOpen,
                r#"Expected opening "(" after lambda name."#,
            );
        } else {
            self.consume(
                TokenKind::ParenthesisOpen,
                r#"Expected opening "(" after "func"."#,
            );
        }

        let previous_function = self.current_function;
        self.current_function = Some(function);
        if let NodeKind::Function(f) = &mut self.tree.get_mut(function).kind {
            f.identifier = identifier;
            f.is_static = is_static;
        }

        let body = self.alloc(NodeKind::Suite(SuiteNode {
            parent_block: self.current_suite,
            ..SuiteNode::default()
        }));

        let previous_suite = self.current_suite;
        self.current_suite = Some(body);

        self.parse_function_signature(function, body, "lambda");

        self.current_suite = previous_suite;

        let previous_in_lambda = self.in_lambda;
        self.in_lambda = true;

        let could_break = self.can_break;
        let could_continue = self.can_continue;
        self.can_break = false;
        self.can_continue = false;

        let body = self.parse_suite("lambda declaration", Some(body), true);
        self.complete_extents(function);
        self.complete_extents(lambda);

        self.pop_multiline();

        if multiline_context {
            // Skip the spurious DEDENT/INDENT/NEWLINE tokens left by the indented-block context.
            while matches!(
                self.current.kind,
                TokenKind::Dedent | TokenKind::Indent | TokenKind::Newline
            ) {
                self.current = self.lexer.scan(); // Not advance(): keep `previous`.
            }
            self.drain_current_errors();
            self.lexer.pop_expression_indented_block();
        }

        self.current_function = previous_function;
        self.in_lambda = previous_in_lambda;
        self.can_break = could_break;
        self.can_continue = could_continue;

        if let NodeKind::Function(f) = &mut self.tree.get_mut(function).kind {
            f.body = Some(body);
        }
        if let NodeKind::Lambda(l) = &mut self.tree.get_mut(lambda).kind {
            l.function = Some(function);
        }
        Some(lambda)
    }

    fn parse_yield(&mut self, _prev: Option<NodeId>, _can_assign: bool) -> Option<NodeId> {
        self.push_error(r#""yield" was removed in Godot 4. Use "await" instead."#);
        None
    }

    fn parse_invalid_token(&mut self, prev: Option<NodeId>, _can_assign: bool) -> Option<NodeId> {
        match self.previous.kind {
            TokenKind::QuestionMark => self.push_error(
                r#"Unexpected "?" in source. If you want a ternary operator, use "truthy_value if true_condition else falsy_value"."#,
            ),
            _ => return None, // Unreachable.
        }
        prev
    }

    fn parse_type(&mut self, allow_void: bool) -> Option<NodeId> {
        // Depth-guard nested container types (`Array[Array[Array[…]]]`). See [`MAX_PARSE_DEPTH`].
        if self.depth >= MAX_PARSE_DEPTH {
            self.push_error("Type is too deeply nested.");
            return None;
        }
        self.depth += 1;
        let result = self.parse_type_inner(allow_void);
        self.depth -= 1;
        result
    }

    fn parse_type_inner(&mut self, allow_void: bool) -> Option<NodeId> {
        let id = self.alloc(NodeKind::Type(TypeNode::default()));
        if !self.match_token(TokenKind::Identifier) {
            if self.match_token(TokenKind::Void) {
                if allow_void {
                    self.complete_extents(id);
                    return Some(id);
                } else {
                    self.push_error(r#""void" is only allowed for a function return type."#);
                }
            }
            self.complete_extents(id);
            return None;
        }

        let mut type_chain = Vec::new();
        let mut container_types = Vec::new();
        if let Some(element) = self.parse_identifier_node() {
            type_chain.push(element);
        }

        // Typed collection (`Array[int]`, `Dictionary[String, int]`). Godot (`parse_type`,
        // cpp:3876) checks `[` *before* the attribute chain and returns immediately, so a typed
        // collection and an attribute chain are mutually exclusive: `A.B[int]` parses the type as
        // `A.B` and leaves `[int]` for the caller.
        if self.match_token(TokenKind::BracketOpen) {
            let mut ok = true;
            let mut first_pass = true;
            loop {
                if let Some(container_type) = self.parse_type(false) {
                    // Nested typed collections (`Array[Array[int]]`) are rejected (cpp:3886).
                    let nested = matches!(
                        &self.tree.get(container_type).kind,
                        NodeKind::Type(t) if !t.container_types.is_empty()
                    );
                    if nested {
                        self.push_error("Nested typed collections are not supported.");
                    } else {
                        container_types.push(container_type);
                    }
                } else {
                    self.push_error(format!(
                        r#"Expected type for collection after "{}"."#,
                        if first_pass { "[" } else { "," }
                    ));
                    ok = false;
                    break;
                }
                first_pass = false;
                if !self.match_token(TokenKind::Comma) {
                    break;
                }
            }
            if ok {
                self.consume(
                    TokenKind::BracketClose,
                    r#"Expected closing "]" after collection type."#,
                );
            }
            self.complete_extents(id);
            if !ok {
                return None;
            }
            if let NodeKind::Type(n) = &mut self.tree.get_mut(id).kind {
                n.type_chain = type_chain;
                n.container_types = container_types;
            }
            return Some(id);
        }

        // Attribute chain (`A.B.C`) — reached only when there was no `[` (cpp:3900). Unlike
        // `parse_attribute`, Godot does *not* coerce node-name keywords to identifiers here.
        while self.match_token(TokenKind::Period) {
            if self.consume(
                TokenKind::Identifier,
                "Expected inner type name after \".\".",
            ) {
                if let Some(element) = self.parse_identifier_node() {
                    type_chain.push(element);
                }
            }
        }

        self.complete_extents(id);
        if let NodeKind::Type(n) = &mut self.tree.get_mut(id).kind {
            n.type_chain = type_chain;
            n.container_types = container_types;
        }
        Some(id)
    }

    // ===== WP-C: statements, declarations, and the program entry (gdscript_parser.cpp) =====

    /// The top-level entry: parse a whole `.gd` file into the implicit head class (`cpp:698`).
    pub fn parse_program(&mut self) {
        let head = self.alloc(NodeKind::Class(ClassNode::default()));
        self.current_class = Some(head);
        self.tree.root = head;
        let mut can_have_class_or_extends = true;

        // Script-level annotations, `class_name`, and `extends` may only appear at the very top.
        while !self.check(TokenKind::Eof) {
            if self.match_token(TokenKind::Annotation) {
                let Some(annotation) = self.parse_annotation(
                    annotation_target::SCRIPT
                        | annotation_target::CLASS_LEVEL
                        | annotation_target::STANDALONE,
                ) else {
                    continue;
                };
                if self.annotation_applies_to(annotation, annotation_target::CLASS) {
                    // Might apply to `head` or a following inner class; defer.
                    self.annotation_stack.push(annotation);
                } else if self.annotation_applies_to(annotation, annotation_target::SCRIPT) {
                    self.push_pending_annotations_to_head(head);
                    // `@tool`/`@icon`/`@static_unload`/`@abstract` apply in the parser; we only record.
                    // WP-F1/F2: duplicate `@icon` / `@tool` / `@static_unload` is a parser-level error
                    // in Godot (`GDScriptParser::{icon,tool,static_unload}_annotation`,
                    // `gdscript_parser.cpp:4403-4454`).
                    if self.check_class_singleton_annotation(head, annotation) {
                        self.tree.get_mut(head).annotations.push(annotation);
                    }
                } else if self.annotation_applies_to(annotation, annotation_target::STANDALONE) {
                    if self.previous.kind != TokenKind::Newline {
                        self.push_error("Expected newline after a standalone annotation.");
                    }
                    let name = self.annotation_name(annotation);
                    if matches!(
                        name.as_str(),
                        "@export_category" | "@export_group" | "@export_subgroup"
                    ) {
                        self.class_add_member_group(head, annotation);
                        can_have_class_or_extends = false;
                        break;
                    } else if !matches!(
                        name.as_str(),
                        "@warning_ignore_start" | "@warning_ignore_restore"
                    ) {
                        self.push_error("Unexpected standalone annotation.");
                    }
                } else {
                    self.annotation_stack.push(annotation);
                    can_have_class_or_extends = false;
                    break;
                }
            } else if self.check(TokenKind::Literal)
                && matches!(self.current.literal, Some(Literal::String(_)))
            {
                // Allow strings in the class body as multiline comments.
                self.advance();
                if !self.match_token(TokenKind::Newline) {
                    self.push_error("Expected newline after comment string.");
                }
            } else {
                break;
            }
        }

        if matches!(self.current.kind, TokenKind::ClassName | TokenKind::Extends) {
            self.reset_extents_from_current(head);
        }

        while can_have_class_or_extends {
            match self.current.kind {
                TokenKind::ClassName => {
                    self.push_pending_annotations_to_head(head);
                    self.advance();
                    if self.class_has_identifier(head) {
                        self.push_error(r#""class_name" can only be used once."#);
                    } else {
                        self.parse_class_name();
                    }
                }
                TokenKind::Extends => {
                    self.push_pending_annotations_to_head(head);
                    self.advance();
                    if self.class_extends_used(head) {
                        self.push_error(r#""extends" can only be used once."#);
                    } else {
                        self.parse_extends();
                        self.end_statement("superclass");
                    }
                }
                TokenKind::Eof => {
                    self.push_pending_annotations_to_head(head);
                    can_have_class_or_extends = false;
                }
                TokenKind::Literal if matches!(self.current.literal, Some(Literal::String(_))) => {
                    self.advance();
                    if !self.match_token(TokenKind::Newline) {
                        self.push_error("Expected newline after comment string.");
                    }
                }
                _ => can_have_class_or_extends = false,
            }
            if self.panic_mode {
                self.synchronize();
            }
        }

        self.parse_class_body(true);
        self.complete_extents(head);

        if !self.check(TokenKind::Eof) {
            self.push_error("Expected end of file.");
        }
        self.clear_unused_annotations();
        // WP-F3: parser-side `@warning_ignore_start` / `@warning_ignore_restore` pair-balance pass
        // (mirrors Godot's `GDScriptParser::warning_ignore_region_annotations`,
        // `gdscript_parser.cpp:5182-5219`).
        self.check_warning_ignore_region_balance();
    }

    fn parse_class_name(&mut self) {
        if self.consume(
            TokenKind::Identifier,
            r#"Expected identifier for the global class name after "class_name"."#,
        ) {
            let ident = self.parse_identifier_node();
            if let Some(class_id) = self.current_class {
                if let NodeKind::Class(c) = &mut self.tree.get_mut(class_id).kind {
                    c.identifier = ident;
                }
            }
        }

        // DIALECT(4.7): gdscript_parser.cpp parse_class_name() — a script embedded in a scene or
        // resource cannot declare a global class name. The check is purely lexical on the script
        // path: `res://` prefix plus a `::` marker, which is how Godot spells a built-in script
        // (`res://main.tscn::GDScript_abc12`). Raised after the identifier has already been
        // recorded, so the AST still carries the name.
        //
        // gdls is handed a real file path for every buffer it serves, so this is dormant in
        // practice; it is ported because a caller may pass a `res://` path, and because a silent
        // gap is worse than an unused branch.
        if self.dialect >= Dialect::Godot4_7
            && self.script_path.starts_with("res://")
            && self.script_path.contains("::")
        {
            self.push_error(r#""class_name" isn't allowed in built-in scripts."#);
        }

        if self.match_token(TokenKind::Extends) {
            self.parse_extends();
            self.end_statement("superclass");
        } else {
            self.end_statement("class_name statement");
        }
    }

    fn parse_extends(&mut self) {
        let class_id = self.current_class;
        if let Some(class_id) = class_id {
            if let NodeKind::Class(c) = &mut self.tree.get_mut(class_id).kind {
                c.extends_used = true;
            }
        }

        if self.match_token(TokenKind::Literal) {
            match self.previous.literal.clone() {
                Some(Literal::String(s)) => {
                    if let Some(class_id) = class_id {
                        if let NodeKind::Class(c) = &mut self.tree.get_mut(class_id).kind {
                            c.extends_path = Some(s);
                        }
                    }
                }
                other => {
                    let tn = literal_type_name(&other);
                    self.push_error(format!(
                        r#"Only strings or identifiers can be used after "extends", found "{tn}" instead."#
                    ));
                }
            }
            if !self.match_token(TokenKind::Period) {
                return;
            }
        }

        if !self.consume(
            TokenKind::Identifier,
            r#"Expected superclass name after "extends"."#,
        ) {
            return;
        }
        if let Some(id) = self.parse_identifier_node() {
            self.class_push_extends(class_id, id);
        }
        while self.match_token(TokenKind::Period) {
            if !self.consume(
                TokenKind::Identifier,
                r#"Expected superclass name after "."."#,
            ) {
                return;
            }
            if let Some(id) = self.parse_identifier_node() {
                self.class_push_extends(class_id, id);
            }
        }
    }

    /// Inner class member: `class Name [extends X]:` (`cpp:937`). The static flag is unused (mirrors
    /// Godot's signature for the member-pointer dispatch).
    fn parse_class(&mut self, _is_static: bool) -> Option<NodeId> {
        let n_class = self.alloc(NodeKind::Class(ClassNode::default()));
        let previous_class = self.current_class;
        self.current_class = Some(n_class);
        if let NodeKind::Class(c) = &mut self.tree.get_mut(n_class).kind {
            c.outer = previous_class;
        }

        if self.consume(
            TokenKind::Identifier,
            r#"Expected identifier for the class name after "class"."#,
        ) {
            let ident = self.parse_identifier_node();
            if let NodeKind::Class(c) = &mut self.tree.get_mut(n_class).kind {
                c.identifier = ident;
            }
        }

        if self.match_token(TokenKind::Extends) {
            self.parse_extends();
        }

        self.consume(TokenKind::Colon, r#"Expected ":" after class declaration."#);

        let multiline = self.match_token(TokenKind::Newline);
        if multiline
            && !self.consume(
                TokenKind::Indent,
                r#"Expected indented block after class declaration."#,
            )
        {
            self.current_class = previous_class;
            self.complete_extents(n_class);
            return Some(n_class);
        }

        if self.match_token(TokenKind::Extends) {
            if self.class_extends_used(n_class) {
                self.push_error(r#"Cannot use "extends" more than once in the same class."#);
            }
            self.parse_extends();
            self.end_statement("superclass");
        }

        self.parse_class_body(multiline);
        self.complete_extents(n_class);

        if multiline {
            self.consume(
                TokenKind::Dedent,
                r#"Missing unindent at the end of the class body."#,
            );
        }

        self.current_class = previous_class;
        Some(n_class)
    }

    fn parse_class_body(&mut self, p_is_multiline: bool) {
        let mut class_end = false;
        let mut next_is_static = false;
        while !class_end && !self.is_at_end() {
            let token = self.current.kind;
            match token {
                TokenKind::Var => self.parse_class_member(
                    Parser::parse_variable_member as MemberParseFn,
                    annotation_target::VARIABLE,
                    "variable",
                    next_is_static,
                ),
                TokenKind::Const => self.parse_class_member(
                    Parser::parse_constant as MemberParseFn,
                    annotation_target::CONSTANT,
                    "constant",
                    false,
                ),
                TokenKind::Signal => self.parse_class_member(
                    Parser::parse_signal as MemberParseFn,
                    annotation_target::SIGNAL,
                    "signal",
                    false,
                ),
                TokenKind::Func => self.parse_class_member(
                    Parser::parse_function as MemberParseFn,
                    annotation_target::FUNCTION,
                    "function",
                    next_is_static,
                ),
                TokenKind::Class => self.parse_class_member(
                    Parser::parse_class as MemberParseFn,
                    annotation_target::CLASS,
                    "class",
                    false,
                ),
                TokenKind::Enum => self.parse_class_member(
                    Parser::parse_enum as MemberParseFn,
                    annotation_target::NONE,
                    "enum",
                    false,
                ),
                TokenKind::Static => {
                    self.advance();
                    next_is_static = true;
                    if !self.check(TokenKind::Func) && !self.check(TokenKind::Var) {
                        self.push_error(r#"Expected "func" or "var" after "static"."#);
                    }
                }
                TokenKind::Annotation => {
                    self.advance();
                    if let Some(annotation) = self.parse_annotation(
                        annotation_target::CLASS_LEVEL | annotation_target::STANDALONE,
                    ) {
                        if self.annotation_applies_to(annotation, annotation_target::STANDALONE) {
                            if self.previous.kind != TokenKind::Newline {
                                self.push_error("Expected newline after a standalone annotation.");
                            }
                            let name = self.annotation_name(annotation);
                            if matches!(
                                name.as_str(),
                                "@export_category" | "@export_group" | "@export_subgroup"
                            ) {
                                if let Some(cc) = self.current_class {
                                    self.class_add_member_group(cc, annotation);
                                }
                            } else if !matches!(
                                name.as_str(),
                                "@warning_ignore_start" | "@warning_ignore_restore"
                            ) {
                                self.push_error("Unexpected standalone annotation.");
                            }
                        } else {
                            self.annotation_stack.push(annotation);
                        }
                    }
                }
                TokenKind::Pass => {
                    self.advance();
                    self.end_statement(r#""pass""#);
                }
                TokenKind::Dedent => class_end = true,
                TokenKind::Literal if matches!(self.current.literal, Some(Literal::String(_))) => {
                    self.advance();
                    if !self.match_token(TokenKind::Newline) {
                        self.push_error("Expected newline after comment string.");
                    }
                }
                _ => {
                    self.advance();
                    let ident = self.previous.source.to_string();
                    let msg = match ident.as_str() {
                        "export" => r#"The "export" keyword was removed in Godot 4. Use an export annotation ("@export", "@export_range", etc.) instead."#.to_string(),
                        "tool" => r#"The "tool" keyword was removed in Godot 4. Use the "@tool" annotation instead."#.to_string(),
                        "onready" => r#"The "onready" keyword was removed in Godot 4. Use the "@onready" annotation instead."#.to_string(),
                        "remote" => r#"The "remote" keyword was removed in Godot 4. Use the "@rpc" annotation with "any_peer" instead."#.to_string(),
                        "remotesync" => r#"The "remotesync" keyword was removed in Godot 4. Use the "@rpc" annotation with "any_peer" and "call_local" instead."#.to_string(),
                        "puppet" => r#"The "puppet" keyword was removed in Godot 4. Use the "@rpc" annotation with "authority" instead."#.to_string(),
                        "puppetsync" => r#"The "puppetsync" keyword was removed in Godot 4. Use the "@rpc" annotation with "authority" and "call_local" instead."#.to_string(),
                        "master" => r#"The "master" keyword was removed in Godot 4. Use the "@rpc" annotation with "any_peer" and perform a check inside the function instead."#.to_string(),
                        "mastersync" => r#"The "mastersync" keyword was removed in Godot 4. Use the "@rpc" annotation with "any_peer" and "call_local", and perform a check inside the function instead."#.to_string(),
                        _ => format!("Unexpected {} in class body.", self.previous.debug_name()),
                    };
                    self.push_error(msg);
                }
            }
            if token != TokenKind::Static {
                next_is_static = false;
            }
            if self.panic_mode {
                self.synchronize();
            }
            if !p_is_multiline {
                class_end = true;
            }
        }
    }

    /// The class-member dispatcher (`cpp:1038`): consume matching pending annotations, parse the
    /// member, attach the annotations, then register it with a duplicate-name check.
    fn parse_class_member(
        &mut self,
        parse_fn: MemberParseFn,
        target: u32,
        member_kind: &str,
        is_static: bool,
    ) {
        self.advance();

        let mut annotations: Vec<NodeId> = Vec::new();
        while let Some(&last) = self.annotation_stack.last() {
            if self.annotation_applies_to(last, target) {
                annotations.push(last);
                self.annotation_stack.pop();
            } else {
                let name = self.annotation_name(last);
                self.push_error(format!(
                    r#"Annotation "{name}" cannot be applied to a {member_kind}."#
                ));
                self.clear_unused_annotations();
            }
        }
        annotations.reverse(); // Restore declaration order.

        let Some(member) = parse_fn(self, is_static) else {
            return;
        };

        if !annotations.is_empty() {
            self.tree.get_mut(member).annotations.extend(annotations);
        }

        let Some(ident) = self.node_identifier(member) else {
            return;
        };
        let name = self.identifier_name(ident);
        let Some(class_id) = self.current_class else {
            return;
        };
        // Enums may be unnamed; those register their values directly and have an empty name here.
        if !name.is_empty() && self.class_has_member(class_id, &name) {
            let existing = self.class_member_type_name(class_id, &name);
            let cap = capitalize_first(member_kind);
            self.push_error_at(
                ident,
                format!(r#"{cap} "{name}" has the same name as a previously declared {existing}."#),
            );
        } else {
            let m = self.member_for(member);
            self.class_add_member(class_id, m);
        }
    }

    /// `parse_variable` for class members — always allows a property body (`cpp:1222`).
    fn parse_variable_member(&mut self, is_static: bool) -> Option<NodeId> {
        self.parse_variable(is_static, true)
    }

    fn parse_variable(&mut self, is_static: bool, allow_property: bool) -> Option<NodeId> {
        let variable = self.alloc(NodeKind::Variable(VariableNode {
            is_static,
            ..VariableNode::default()
        }));

        if !self.consume(
            TokenKind::Identifier,
            r#"Expected variable name after "var"."#,
        ) {
            self.complete_extents(variable);
            return None;
        }
        let ident = self.parse_identifier_node();
        if let NodeKind::Variable(v) = &mut self.tree.get_mut(variable).kind {
            v.identifier = ident;
        }

        if self.match_token(TokenKind::Colon) {
            if self.check(TokenKind::Newline) {
                if allow_property {
                    self.advance();
                    return self.parse_property(variable, true);
                } else {
                    self.push_error(r#"Expected type after ":""#);
                    self.complete_extents(variable);
                    return None;
                }
            } else if self.check(TokenKind::Equal) {
                if let NodeKind::Variable(v) = &mut self.tree.get_mut(variable).kind {
                    v.infer_datatype = true;
                }
            } else {
                if allow_property
                    && self.check(TokenKind::Identifier)
                    && (self.current_identifier_is("get") || self.current_identifier_is("set"))
                {
                    return self.parse_property(variable, false);
                }
                let ty = self.parse_type(false);
                if let NodeKind::Variable(v) = &mut self.tree.get_mut(variable).kind {
                    v.datatype_specifier = ty;
                }
            }
        }

        if self.match_token(TokenKind::Equal) {
            let init = self.parse_expression(false, false);
            if init.is_none() {
                self.push_error(r#"Expected expression for variable initial value after "="."#);
            }
            if let NodeKind::Variable(v) = &mut self.tree.get_mut(variable).kind {
                v.initializer = init;
            }
        }

        if allow_property && self.match_token(TokenKind::Colon) {
            if self.match_token(TokenKind::Newline) {
                return self.parse_property(variable, true);
            } else {
                return self.parse_property(variable, false);
            }
        }

        self.complete_extents(variable);
        self.end_statement("variable declaration");
        Some(variable)
    }

    fn parse_property(&mut self, variable: NodeId, need_indent: bool) -> Option<NodeId> {
        if need_indent
            && !self.consume(
                TokenKind::Indent,
                r#"Expected indented block for property after ":"."#,
            )
        {
            self.complete_extents(variable);
            return None;
        }

        if !self.consume(
            TokenKind::Identifier,
            r#"Expected "get" or "set" for property declaration."#,
        ) {
            self.complete_extents(variable);
            return None;
        }
        let mut function = self.parse_identifier_node();

        let style = if self.check(TokenKind::Equal) {
            PropertyStyle::SetGet
        } else {
            if !need_indent {
                self.push_error("Property with inline code must go to an indented block.");
            }
            PropertyStyle::Inline
        };
        if let NodeKind::Variable(v) = &mut self.tree.get_mut(variable).kind {
            v.property = style;
        }

        let mut getter_used = false;
        let mut setter_used = false;

        // Order doesn't matter, so loop at most twice (set then get, or vice versa).
        for i in 0..2 {
            match function.map(|f| self.identifier_name(f)).as_deref() {
                Some("set") => {
                    if setter_used {
                        self.push_error("Properties can only have one setter.");
                    } else {
                        self.parse_property_setter(variable);
                        setter_used = true;
                    }
                }
                Some("get") => {
                    if getter_used {
                        self.push_error("Properties can only have one getter.");
                    } else {
                        self.parse_property_getter(variable);
                        getter_used = true;
                    }
                }
                _ => self.push_error(r#"Expected "get" or "set" for property declaration."#),
            }

            if i == 0 && style == PropertyStyle::SetGet {
                if self.match_token(TokenKind::Comma) {
                    if self.match_token(TokenKind::Newline) && !need_indent {
                        self.push_error(
                            r#"Inline setter/getter setting cannot span across multiple lines (use "\" if needed)."#,
                        );
                    }
                } else {
                    break;
                }
            }

            if !self.match_token(TokenKind::Identifier) {
                break;
            }
            function = self.parse_identifier_node();
        }
        self.complete_extents(variable);

        if style == PropertyStyle::SetGet {
            self.end_statement("property declaration");
        }
        if need_indent {
            self.consume(
                TokenKind::Dedent,
                r#"Expected end of indented block for property."#,
            );
        }
        Some(variable)
    }

    fn parse_property_setter(&mut self, variable: NodeId) {
        let style = self.variable_property_style(variable);
        match style {
            PropertyStyle::Inline => {
                // gdscript_parser.cpp:1376-1378 — synthesize an identifier named
                // `@<varname>_setter` and attach it to the synthetic FunctionNode. The
                // analyzer's static-context error templates read this name via
                // `current_function->identifier->name`, which surfaces in messages like
                // `Cannot access non-static X from the static function "@my_var_setter()".`.
                let var_name = match &self.tree.get(variable).kind {
                    NodeKind::Variable(v) => v
                        .identifier
                        .map(|i| self.identifier_name(i))
                        .unwrap_or_default(),
                    _ => String::new(),
                };
                let synthesized = self.alloc(NodeKind::Identifier(IdentifierNode {
                    name: format!("@{var_name}_setter"),
                }));
                let function = self.alloc(NodeKind::Function(FunctionNode {
                    is_static: self.variable_is_static(variable),
                    identifier: Some(synthesized),
                    ..FunctionNode::default()
                }));
                self.consume(TokenKind::ParenthesisOpen, r#"Expected "(" after "set"."#);

                let parameter = self.alloc(NodeKind::Parameter(ParameterNode::default()));
                let mut setter_parameter = None;
                if self.consume(
                    TokenKind::Identifier,
                    r#"Expected parameter name after "("."#,
                ) {
                    self.reset_extents_from_previous(parameter);
                    let pid = self.parse_identifier_node();
                    setter_parameter = pid;
                    if let NodeKind::Parameter(p) = &mut self.tree.get_mut(parameter).kind {
                        p.identifier = pid;
                    }
                    let pname = pid.map(|i| self.identifier_name(i)).unwrap_or_default();
                    if let NodeKind::Function(f) = &mut self.tree.get_mut(function).kind {
                        f.parameters_indices.insert(pname, 0);
                        f.parameters.push(parameter);
                    }
                }
                self.complete_extents(parameter);

                self.consume(
                    TokenKind::ParenthesisClose,
                    r#"Expected ")" after parameter name."#,
                );
                self.consume(TokenKind::Colon, r#"Expected ":" after ")"."#);

                let previous_function = self.current_function;
                self.current_function = Some(function);
                if setter_parameter.is_some() {
                    let body = self.alloc(NodeKind::Suite(SuiteNode::default()));
                    self.suite_add_local(
                        body,
                        Local {
                            kind: LocalKind::Parameter,
                            name: setter_parameter
                                .map(|i| self.identifier_name(i))
                                .unwrap_or_default(),
                            source: parameter,
                        },
                    );
                    let body = self.parse_suite("setter declaration", Some(body), false);
                    if let NodeKind::Function(f) = &mut self.tree.get_mut(function).kind {
                        f.body = Some(body);
                    }
                    if let NodeKind::Variable(v) = &mut self.tree.get_mut(variable).kind {
                        v.setter = PropertyAccessor::Inline(function);
                        v.setter_parameter = setter_parameter;
                    }
                }
                self.current_function = previous_function;
                self.complete_extents(function);
            }
            PropertyStyle::SetGet => {
                self.consume(TokenKind::Equal, r#"Expected "=" after "set""#);
                if self.consume(
                    TokenKind::Identifier,
                    r#"Expected setter function name after "="."#,
                ) {
                    let ptr = self.parse_identifier_node();
                    if let NodeKind::Variable(v) = &mut self.tree.get_mut(variable).kind {
                        if let Some(ptr) = ptr {
                            v.setter = PropertyAccessor::Pointer(ptr);
                        }
                    }
                }
            }
            PropertyStyle::None => {}
        }
    }

    fn parse_property_getter(&mut self, variable: NodeId) {
        let style = self.variable_property_style(variable);
        match style {
            PropertyStyle::Inline => {
                // gdscript_parser.cpp:1433-1436 — synthesize an identifier named
                // `@<varname>_getter` on the FunctionNode so the analyzer's static-context
                // error templates can read it via `current_function->identifier->name`.
                let var_name = match &self.tree.get(variable).kind {
                    NodeKind::Variable(v) => v
                        .identifier
                        .map(|i| self.identifier_name(i))
                        .unwrap_or_default(),
                    _ => String::new(),
                };
                let synthesized = self.alloc(NodeKind::Identifier(IdentifierNode {
                    name: format!("@{var_name}_getter"),
                }));
                let function = self.alloc(NodeKind::Function(FunctionNode {
                    is_static: self.variable_is_static(variable),
                    identifier: Some(synthesized),
                    ..FunctionNode::default()
                }));
                if self.match_token(TokenKind::ParenthesisOpen) {
                    self.consume(TokenKind::ParenthesisClose, r#"Expected ")" after "get("."#);
                    self.consume(TokenKind::Colon, r#"Expected ":" after "get()"."#);
                } else {
                    self.consume(TokenKind::Colon, r#"Expected ":" or "(" after "get"."#);
                }

                let previous_function = self.current_function;
                self.current_function = Some(function);
                let body = self.alloc(NodeKind::Suite(SuiteNode::default()));
                let body = self.parse_suite("getter declaration", Some(body), false);
                if let NodeKind::Function(f) = &mut self.tree.get_mut(function).kind {
                    f.body = Some(body);
                }
                if let NodeKind::Variable(v) = &mut self.tree.get_mut(variable).kind {
                    v.getter = PropertyAccessor::Inline(function);
                }
                self.current_function = previous_function;
                self.complete_extents(function);
            }
            PropertyStyle::SetGet => {
                self.consume(TokenKind::Equal, r#"Expected "=" after "get""#);
                if self.consume(
                    TokenKind::Identifier,
                    r#"Expected getter function name after "="."#,
                ) {
                    let ptr = self.parse_identifier_node();
                    if let NodeKind::Variable(v) = &mut self.tree.get_mut(variable).kind {
                        if let Some(ptr) = ptr {
                            v.getter = PropertyAccessor::Pointer(ptr);
                        }
                    }
                }
            }
            PropertyStyle::None => {}
        }
    }

    fn parse_constant(&mut self, _is_static: bool) -> Option<NodeId> {
        let constant = self.alloc(NodeKind::Constant(ConstantNode::default()));

        if !self.consume(
            TokenKind::Identifier,
            r#"Expected constant name after "const"."#,
        ) {
            self.complete_extents(constant);
            return None;
        }
        let ident = self.parse_identifier_node();
        if let NodeKind::Constant(c) = &mut self.tree.get_mut(constant).kind {
            c.identifier = ident;
        }

        if self.match_token(TokenKind::Colon) {
            if self.check(TokenKind::Equal) {
                if let NodeKind::Constant(c) = &mut self.tree.get_mut(constant).kind {
                    c.infer_datatype = true;
                }
            } else {
                let ty = self.parse_type(false);
                if let NodeKind::Constant(c) = &mut self.tree.get_mut(constant).kind {
                    c.datatype_specifier = ty;
                }
            }
        }

        if self.consume(
            TokenKind::Equal,
            r#"Expected initializer after constant name."#,
        ) {
            let init = self.parse_expression(false, false);
            if init.is_none() {
                self.push_error("Expected initializer expression for constant.");
                self.complete_extents(constant);
                return None;
            }
            if let NodeKind::Constant(c) = &mut self.tree.get_mut(constant).kind {
                c.initializer = init;
            }
        } else {
            self.complete_extents(constant);
            return None;
        }

        self.complete_extents(constant);
        self.end_statement("constant declaration");
        Some(constant)
    }

    fn parse_parameter(&mut self) -> Option<NodeId> {
        if !self.consume(TokenKind::Identifier, "Expected parameter name.") {
            return None;
        }
        let parameter = self.alloc(NodeKind::Parameter(ParameterNode::default()));
        let ident = self.parse_identifier_node();
        if let NodeKind::Parameter(p) = &mut self.tree.get_mut(parameter).kind {
            p.identifier = ident;
        }

        if self.match_token(TokenKind::Colon) {
            if self.check(TokenKind::Equal) {
                if let NodeKind::Parameter(p) = &mut self.tree.get_mut(parameter).kind {
                    p.infer_datatype = true;
                }
            } else {
                let ty = self.parse_type(false);
                if let NodeKind::Parameter(p) = &mut self.tree.get_mut(parameter).kind {
                    p.datatype_specifier = ty;
                }
            }
        }

        if self.match_token(TokenKind::Equal) {
            let init = self.parse_expression(false, false);
            if let NodeKind::Parameter(p) = &mut self.tree.get_mut(parameter).kind {
                p.initializer = init;
            }
        }

        self.complete_extents(parameter);
        Some(parameter)
    }

    fn parse_signal(&mut self, _is_static: bool) -> Option<NodeId> {
        let signal = self.alloc(NodeKind::Signal(SignalNode::default()));

        if !self.consume(
            TokenKind::Identifier,
            r#"Expected signal name after "signal"."#,
        ) {
            self.complete_extents(signal);
            return None;
        }
        let ident = self.parse_identifier_node();
        if let NodeKind::Signal(s) = &mut self.tree.get_mut(signal).kind {
            s.identifier = ident;
        }

        if self.check(TokenKind::ParenthesisOpen) {
            self.push_multiline(true);
            self.advance();
            loop {
                if self.check(TokenKind::ParenthesisClose) {
                    break; // Trailing comma.
                }
                let Some(parameter) = self.parse_parameter() else {
                    self.push_error("Expected signal parameter name.");
                    break;
                };
                if self.parameter_has_initializer(parameter) {
                    self.push_error("Signal parameters cannot have a default value.");
                }
                let pname = self.parameter_name(parameter);
                if self.signal_has_parameter(signal, &pname) {
                    self.push_error(format!(
                        r#"Parameter with name "{pname}" was already declared for this signal."#
                    ));
                } else if let NodeKind::Signal(s) = &mut self.tree.get_mut(signal).kind {
                    s.parameters.push(parameter);
                }
                if !self.match_token(TokenKind::Comma) || self.is_at_end() {
                    break;
                }
            }
            self.pop_multiline();
            self.consume(
                TokenKind::ParenthesisClose,
                r#"Expected closing ")" after signal parameters."#,
            );
        }

        self.complete_extents(signal);
        self.end_statement("signal declaration");
        Some(signal)
    }

    fn parse_enum(&mut self, _is_static: bool) -> Option<NodeId> {
        let enum_node = self.alloc(NodeKind::Enum(EnumNode::default()));
        let mut named = false;

        if self.match_token(TokenKind::Identifier) {
            let ident = self.parse_identifier_node();
            if let NodeKind::Enum(e) = &mut self.tree.get_mut(enum_node).kind {
                e.identifier = ident;
            }
            named = true;
        }

        self.push_multiline(true);
        self.consume(
            TokenKind::BraceOpen,
            format!(
                r#"Expected "{{" after {}."#,
                if named { "enum name" } else { r#""enum""# }
            ),
        );

        // Names already seen in *this* enum, mapped to the line each was first declared on —
        // the duplicate-key diagnostic names that line (gdscript_parser.cpp:1629).
        let mut elements: HashMap<String, u32> = HashMap::new();

        loop {
            if self.check(TokenKind::BraceClose) {
                break; // Trailing comma.
            }
            if self.consume(TokenKind::Identifier, "Expected identifier for enum key.") {
                let ident = self.parse_identifier_node();
                let key = ident.map(|i| self.identifier_name(i)).unwrap_or_default();

                if let Some(&first_line) = elements.get(&key) {
                    self.push_error(format!(
                        r#"Name "{key}" was already in this enum (at line {first_line})."#
                    ));
                } else if !named {
                    if let Some(class_id) = self.current_class {
                        if self.class_has_member(class_id, &key) {
                            let existing = self.class_member_type_name(class_id, &key);
                            self.push_error(format!(
                                r#"Name "{key}" is already used as a class {existing}."#
                            ));
                        }
                    }
                }
                let key_line = ident
                    .map(|i| self.tree.get(i).loc.start.line)
                    .unwrap_or_else(|| self.previous.loc.start.line);
                elements.entry(key).or_insert(key_line);

                let mut custom_value = None;
                if self.match_token(TokenKind::Equal) {
                    custom_value = self.parse_expression(false, false);
                    if custom_value.is_none() {
                        self.push_error(r#"Expected expression value after "="."#);
                    }
                }

                let mut value = EnumValue {
                    identifier: ident,
                    custom_value,
                    parent_enum: Some(enum_node),
                    ..EnumValue::default()
                };
                if let NodeKind::Enum(e) = &mut self.tree.get_mut(enum_node).kind {
                    // parser.cpp:1646 — the index is the position this value is about to take.
                    value.index = e.values.len() as i32;
                    e.values.push(value.clone());
                }
                if !named {
                    if let Some(class_id) = self.current_class {
                        self.class_add_member(class_id, Member::EnumValue(value));
                    }
                }
            }
            if !self.match_token(TokenKind::Comma) {
                break;
            }
        }

        self.pop_multiline();
        self.consume(TokenKind::BraceClose, r#"Expected closing "}" for enum."#);
        self.complete_extents(enum_node);
        self.end_statement("enum");
        Some(enum_node)
    }

    /// Parse a function/lambda parameter list and (optional) return type after the opening `(`.
    /// Returns whether a body follows (`true`) — abstract functions have none (`cpp:1673`).
    fn parse_function_signature(&mut self, function: NodeId, body: NodeId, p_type: &str) -> bool {
        if !self.check(TokenKind::ParenthesisClose) && !self.is_at_end() {
            let mut default_used = false;
            loop {
                if self.check(TokenKind::ParenthesisClose) {
                    break; // Trailing comma.
                }
                let is_rest = self.match_token(TokenKind::PeriodPeriodPeriod);

                let Some(parameter) = self.parse_parameter() else {
                    break;
                };

                if self.function_is_vararg(function) {
                    self.push_error("Cannot have parameters after the rest parameter.");
                    continue;
                }

                if self.parameter_has_initializer(parameter) {
                    if is_rest {
                        self.push_error("The rest parameter cannot have a default value.");
                        continue;
                    }
                    default_used = true;
                } else if default_used && !is_rest {
                    self.push_error("Cannot have mandatory parameters after optional parameters.");
                    continue;
                }

                let pname = self.parameter_name(parameter);
                if self.function_has_parameter(function, &pname) {
                    self.push_error(format!(
                        r#"Parameter with name "{pname}" was already declared for this {p_type}."#
                    ));
                } else if is_rest {
                    if let NodeKind::Function(f) = &mut self.tree.get_mut(function).kind {
                        f.rest_parameter = Some(parameter);
                    }
                    self.suite_add_local(
                        body,
                        Local {
                            kind: LocalKind::Parameter,
                            name: pname,
                            source: parameter,
                        },
                    );
                } else {
                    if let NodeKind::Function(f) = &mut self.tree.get_mut(function).kind {
                        let idx = f.parameters.len();
                        f.parameters_indices.insert(pname.clone(), idx);
                        f.parameters.push(parameter);
                    }
                    self.suite_add_local(
                        body,
                        Local {
                            kind: LocalKind::Parameter,
                            name: pname,
                            source: parameter,
                        },
                    );
                }

                if !self.match_token(TokenKind::Comma) {
                    break;
                }
            }
        }

        self.pop_multiline();
        self.consume(
            TokenKind::ParenthesisClose,
            format!(r#"Expected closing ")" after {p_type} parameters."#),
        );

        if self.match_token(TokenKind::ForwardArrow) {
            let return_type = self.parse_type(true);
            if return_type.is_none() {
                self.push_error(r#"Expected return type or "void" after "->"."#);
            }
            if let NodeKind::Function(f) = &mut self.tree.get_mut(function).kind {
                f.return_type = return_type;
            }
        }

        // Static-constructor checks (`_static_init`). Godot gates these on `!source_lambda`; the
        // non-lambda calls are exactly those with `p_type != "lambda"`.
        if p_type != "lambda" {
            if let Some(name) = self.function_name_opt(function) {
                if name == "_static_init" {
                    if !self.function_is_static(function) {
                        self.push_error("Static constructor must be declared static.");
                    }
                    if self.function_param_count(function) != 0 || self.function_is_vararg(function)
                    {
                        self.push_error("Static constructor cannot have parameters.");
                    }
                }
            }
        }

        if p_type == "lambda" {
            return self.consume(
                TokenKind::Colon,
                r#"Expected ":" after lambda declaration."#,
            );
        }
        // The colon may be absent for abstract functions.
        self.match_token(TokenKind::Colon)
    }

    fn parse_function(&mut self, is_static: bool) -> Option<NodeId> {
        let function = self.alloc(NodeKind::Function(FunctionNode {
            is_static,
            ..FunctionNode::default()
        }));

        if !self.consume(
            TokenKind::Identifier,
            r#"Expected function name after "func"."#,
        ) {
            self.complete_extents(function);
            return None;
        }

        let previous_function = self.current_function;
        self.current_function = Some(function);

        let ident = self.parse_identifier_node();
        if let NodeKind::Function(f) = &mut self.tree.get_mut(function).kind {
            f.identifier = ident;
        }

        let body = self.alloc(NodeKind::Suite(SuiteNode::default()));
        let previous_suite = self.current_suite;
        self.current_suite = Some(body);

        self.push_multiline(true);
        self.consume(
            TokenKind::ParenthesisOpen,
            r#"Expected opening "(" after function name."#,
        );

        let has_body = self.parse_function_signature(function, body, "function");

        self.current_suite = previous_suite;

        let body = if !has_body {
            // Abstract functions have no body.
            self.end_statement("bodyless function declaration");
            self.reset_extents_from_current(body);
            self.complete_extents(body);
            body
        } else {
            self.parse_suite("function declaration", Some(body), false)
        };
        if let NodeKind::Function(f) = &mut self.tree.get_mut(function).kind {
            f.body = Some(body);
        }

        self.current_function = previous_function;
        self.complete_extents(function);
        Some(function)
    }

    /// Parse `@annotation[(args)]` (`cpp:1817`). Annotation argument *validation* and *application*
    /// are deferred to the analyzer; we keep the name/target/argument structure and the parser-level
    /// diagnostics (unrecognized name, wrong level).
    fn parse_annotation(&mut self, valid_targets: u32) -> Option<NodeId> {
        let annotation = self.alloc(NodeKind::Annotation(AnnotationNode::default()));
        let name = self.previous.literal.clone();
        let name = match name {
            Some(Literal::String(s)) => s,
            _ => self.previous.source.to_string(),
        };
        if let NodeKind::Annotation(a) = &mut self.tree.get_mut(annotation).kind {
            a.name = name.clone();
        }

        let mut valid = true;
        let target_kind = annotation_target_kind(&name);
        if target_kind.is_none() {
            match name.as_str() {
                // Normal (escaped) strings: the messages contain `"##`, which would prematurely
                // close any `r#"…"#`/`r##"…"##` raw string.
                "@deprecated" => self.push_error(
                    "\"@deprecated\" annotation does not exist. Use \"## @deprecated: Reason here.\" instead.",
                ),
                "@experimental" => self.push_error(
                    "\"@experimental\" annotation does not exist. Use \"## @experimental: Reason here.\" instead.",
                ),
                "@tutorial" => self.push_error(
                    "\"@tutorial\" annotation does not exist. Use \"## @tutorial(Title): https://example.com\" instead.",
                ),
                other => self.push_error(format!(r#"Unrecognized annotation: "{other}"."#)),
            }
            valid = false;
        }

        if let Some(kind) = target_kind {
            if (kind & valid_targets) == 0 {
                if (kind & annotation_target::SCRIPT) != 0 {
                    self.push_error(format!(
                        r#"Annotation "{name}" must be at the top of the script, before "extends" and "class_name"."#
                    ));
                } else {
                    self.push_error(format!(
                        r#"Annotation "{name}" is not allowed in this level."#
                    ));
                }
                valid = false;
            }
        }

        if self.check(TokenKind::ParenthesisOpen) {
            self.push_multiline(true);
            self.advance();
            loop {
                if self.check(TokenKind::ParenthesisClose) {
                    break; // Trailing comma.
                }
                let argument = self.parse_expression(false, false);
                if let Some(argument) = argument {
                    if let NodeKind::Annotation(a) = &mut self.tree.get_mut(annotation).kind {
                        a.arguments.push(argument);
                    }
                } else {
                    self.push_error("Expected expression as the annotation argument.");
                    valid = false;
                }
                if !self.match_token(TokenKind::Comma) {
                    break;
                }
            }
            self.pop_multiline();
            self.consume(
                TokenKind::ParenthesisClose,
                r#"Expected ")" after annotation arguments."#,
            );
        }
        if name == "@warning_ignore" {
            self.check_warning_ignore_annotation_args(annotation);
        }
        self.complete_extents(annotation);

        self.match_token(TokenKind::Newline); // Newline after annotation is optional.

        if valid {
            Some(annotation)
        } else {
            None
        }
    }

    /// Godot `warning_ignore_annotation` (`gdscript_parser.cpp:5105-5179`) validates warning names
    /// during parser-side annotation application. Unknown names emit an error, but the annotation is
    /// still represented in the tree; the analyzer-side suppression builder ignores unknown names.
    fn check_warning_ignore_annotation_args(&mut self, annotation: NodeId) {
        let args = match &self.tree.get(annotation).kind {
            NodeKind::Annotation(a) => a.arguments.clone(),
            _ => return,
        };
        for arg in args {
            let Some(raw) = self.literal_string_value(arg) else {
                continue;
            };
            if !warning_name_is_valid(&raw.to_uppercase(), self.dialect) {
                self.push_error_at(annotation, format!(r#"Invalid warning name: "{raw}"."#));
            }
        }
    }

    fn clear_unused_annotations(&mut self) {
        let stack = std::mem::take(&mut self.annotation_stack);
        for annotation in stack {
            let name = self.annotation_name(annotation);
            self.push_error_at(
                annotation,
                format!(
                    r#"Annotation "{name}" does not precede a valid target, so it will have no effect."#
                ),
            );
        }
    }

    /// WP-F3: walk every `@warning_ignore_start(...)` / `@warning_ignore_restore(...)` annotation in
    /// source order and emit Godot's two pair-balance diagnostics (`gdscript_parser.cpp:5182-5219`'s
    /// `warning_ignore_region_annotations`):
    ///
    /// * **Extra start**: a second `_start` for an already-open warning code →
    ///   `Warning "%s" is already being ignored by "@warning_ignore_start" at line %d.`
    /// * **Restore without start**: a `_restore` with no matching open code →
    ///   `Warning "%s" is not being ignored by "@warning_ignore_start".`
    ///
    /// Tracks Godot's `warning_ignore_region_annotations`:
    ///
    /// * (a) names are uppercased before comparison (Godot's `String(warning_name).to_upper()`);
    /// * no end-of-file leftover sweep — an unbalanced `_start` is silently absorbed by the
    ///   analyzer-side region builder, matching Godot's `INT_MAX` terminal state.
    /// * unknown codes are rejected before pair-balance bookkeeping, with Godot's exact
    ///   `Invalid warning name: "%s".` diagnostic (`gdscript_parser.cpp:5187-5192`).
    fn check_warning_ignore_region_balance(&mut self) {
        // (annotation NodeId, source line) — collected then sorted by line so the diagnostic's
        // "at line N" reports the prior `_start`'s line in source order, not allocation order.
        let mut anns: Vec<(NodeId, u32)> = self
            .tree
            .iter_ids()
            .filter_map(|id| match &self.tree.get(id).kind {
                NodeKind::Annotation(a)
                    if a.name == "@warning_ignore_start" || a.name == "@warning_ignore_restore" =>
                {
                    Some((id, self.tree.get(id).loc.start.line))
                }
                _ => None,
            })
            .collect();
        anns.sort_by_key(|&(_, line)| line);

        let mut open: std::collections::HashMap<String, u32> = std::collections::HashMap::new();
        for (ann_id, line) in anns {
            let (is_start, args): (bool, Vec<NodeId>) = match &self.tree.get(ann_id).kind {
                NodeKind::Annotation(a) => (a.name == "@warning_ignore_start", a.arguments.clone()),
                _ => continue,
            };

            for arg_id in args {
                // Godot's `validate_annotation_arguments` (gdscript_parser.cpp:4377-4391) requires
                // every `@warning_ignore_start`/`_restore` argument to be a `Variant::STRING`
                // literal — a `StringName` (`&"…"`) or `NodePath` (`^"…"`) is rejected before it
                // reaches `resolved_arguments`, so the region bookkeeping never sees it. (gdls does
                // not yet emit Godot's "Argument N … must be a string literal." error for those —
                // a deferred `validate_annotation_arguments` port — but it must at least not run
                // region bookkeeping on an argument Godot discards.)
                let Some(raw) = self.string_literal_value(arg_id) else {
                    continue;
                };
                let code_name = raw.to_uppercase();

                if !warning_name_is_valid(&code_name, self.dialect) {
                    self.push_error_at(ann_id, format!(r#"Invalid warning name: "{raw}"."#));
                    continue;
                }

                if is_start {
                    if let Some(&prior_line) = open.get(&code_name) {
                        self.push_error_at(
                            ann_id,
                            format!(
                                "Warning \"{code_name}\" is already being ignored by \
                                 \"@warning_ignore_start\" at line {prior_line}."
                            ),
                        );
                    } else {
                        open.insert(code_name, line);
                    }
                } else if open.remove(&code_name).is_none() {
                    self.push_error_at(
                        ann_id,
                        format!(
                            "Warning \"{code_name}\" is not being ignored by \
                             \"@warning_ignore_start\"."
                        ),
                    );
                }
            }
        }
        // Deliberate non-emit: Godot has no leftover-at-EOF sweep, so neither does gdls.
    }

    /// Resolve `arg` to the underlying string when it's a string-like literal (`String`,
    /// `StringName`, or `NodePath`). Used for the analyzer-resolved `@warning_ignore`
    /// ([`Self::check_warning_ignore_annotation_args`]): its `warning_ignore_annotation` callback
    /// coerces every argument through `String(warning_name)` (gdscript_parser.cpp), so a
    /// `&"…"`/`^"…"` argument is accepted there. The *region* annotations
    /// (`@warning_ignore_start`/`_restore`) are stricter — they pass through
    /// `validate_annotation_arguments`, which requires a plain `String` — so they use the
    /// String-only [`Self::string_literal_value`] instead.
    fn literal_string_value(&self, arg: NodeId) -> Option<String> {
        if let NodeKind::Literal(lit) = &self.tree.get(arg).kind {
            match &lit.value {
                Literal::String(s) | Literal::StringName(s) | Literal::NodePath(s) => {
                    return Some(s.clone());
                }
                _ => {}
            }
        }
        None
    }

    /// Resolve `arg` to the underlying string ONLY when it's a plain `String` literal — the strict
    /// form Godot's `validate_annotation_arguments` (gdscript_parser.cpp:4377-4391) demands for the
    /// `@warning_ignore_start`/`_restore` region annotations (a `StringName`/`NodePath` literal is
    /// rejected there). Contrast [`Self::literal_string_value`], the looser variant for the
    /// analyzer-coerced `@warning_ignore`.
    fn string_literal_value(&self, arg: NodeId) -> Option<String> {
        if let NodeKind::Literal(lit) = &self.tree.get(arg).kind {
            if let Literal::String(s) = &lit.value {
                return Some(s.clone());
            }
        }
        None
    }

    fn parse_suite(
        &mut self,
        p_context: &str,
        p_suite: Option<NodeId>,
        p_for_lambda: bool,
    ) -> NodeId {
        let suite = match p_suite {
            Some(s) => s,
            None => self.alloc(NodeKind::Suite(SuiteNode::default())),
        };
        let parent_block = self.current_suite;
        if let NodeKind::Suite(s) = &mut self.tree.get_mut(suite).kind {
            s.parent_block = parent_block;
        }
        self.current_suite = Some(suite);

        let multiline = self.match_token(TokenKind::Newline);

        if multiline
            && !self.consume(
                TokenKind::Indent,
                format!(r#"Expected indented block after {p_context}."#),
            )
        {
            self.current_suite = parent_block;
            self.complete_extents(suite);
            return suite;
        }
        self.reset_extents_from_current(suite);

        let mut error_count = 0;
        loop {
            if self.is_at_end()
                || (!multiline
                    && self.previous.kind == TokenKind::Semicolon
                    && self.check(TokenKind::Newline))
            {
                break;
            }
            let statement = self.parse_statement();
            match statement {
                None => {
                    error_count += 1;
                    if error_count > 100 {
                        self.push_error_at(suite, "Too many statement errors.");
                        break;
                    }
                }
                Some(statement) => {
                    if let NodeKind::Suite(s) = &mut self.tree.get_mut(suite).kind {
                        s.statements.push(statement);
                    }
                    // Register block locals for `var`/`const` (`cpp:1964`).
                    self.register_suite_local(suite, statement);
                }
            }

            let continue_loop = (multiline || self.previous.kind == TokenKind::Semicolon)
                && !self.check(TokenKind::Dedent)
                && !self.lambda_ended
                && !self.is_at_end();
            if !continue_loop {
                break;
            }
        }

        self.complete_extents(suite);

        if multiline {
            if !self.lambda_ended {
                self.consume(
                    TokenKind::Dedent,
                    format!(r#"Missing unindent at the end of {p_context}."#),
                );
            } else {
                self.match_token(TokenKind::Dedent);
            }
        } else if self.previous.kind == TokenKind::Semicolon {
            self.consume(
                TokenKind::Newline,
                format!(r#"Expected newline after ";" at the end of {p_context}."#),
            );
        }

        if p_for_lambda {
            self.lambda_ended = true;
        }
        self.current_suite = parent_block;
        suite
    }

    fn parse_statement(&mut self) -> Option<NodeId> {
        // Depth-guard the statement/block recursion cycle (parse_statement → if/for/while/match →
        // parse_suite → parse_statement). See [`MAX_PARSE_DEPTH`].
        if self.depth >= MAX_PARSE_DEPTH {
            self.push_error("Statement is too deeply nested.");
            return None;
        }
        self.depth += 1;
        let result = self.parse_statement_inner();
        self.depth -= 1;
        result
    }

    fn parse_statement_inner(&mut self) -> Option<NodeId> {
        // Collect statement-level annotations unless the current token is itself an annotation.
        let mut annotations: Vec<NodeId> = Vec::new();
        if self.current.kind != TokenKind::Annotation {
            while let Some(&last) = self.annotation_stack.last() {
                if self.annotation_applies_to(last, annotation_target::STATEMENT) {
                    annotations.push(last);
                    self.annotation_stack.pop();
                } else {
                    let name = self.annotation_name(last);
                    self.push_error(format!(
                        r#"Annotation "{name}" cannot be applied to a statement."#
                    ));
                    self.clear_unused_annotations();
                }
            }
            annotations.reverse();
        }

        let mut result: Option<NodeId> = None;
        match self.current.kind {
            TokenKind::Pass => {
                self.advance();
                let id = self.alloc(NodeKind::Pass);
                self.complete_extents(id);
                self.end_statement(r#""pass""#);
                result = Some(id);
            }
            TokenKind::Var => {
                self.advance();
                result = self.parse_variable(false, false);
            }
            TokenKind::Const => {
                self.advance();
                result = self.parse_constant(false);
            }
            TokenKind::If => {
                self.advance();
                result = self.parse_if("if");
            }
            TokenKind::For => {
                self.advance();
                result = self.parse_for();
            }
            TokenKind::While => {
                self.advance();
                result = self.parse_while();
            }
            TokenKind::Match => {
                self.advance();
                result = self.parse_match();
            }
            TokenKind::Break => {
                self.advance();
                result = self.parse_break();
            }
            TokenKind::Continue => {
                self.advance();
                result = self.parse_continue();
            }
            TokenKind::Return => {
                self.advance();
                let n_return = self.alloc(NodeKind::Return(ReturnNode::default()));
                let mut return_value = None;
                if !self.is_statement_end() {
                    if self.current_function_is_constructor() {
                        self.push_error("Constructor cannot return a value.");
                    }
                    return_value = self.parse_expression(false, false);
                } else if self.in_lambda && !self.is_statement_end_token() {
                    // Might not be the statement end inside a lambda; try anyway.
                    return_value = self.parse_expression(false, false);
                }
                self.complete_extents(n_return);
                if let NodeKind::Return(r) = &mut self.tree.get_mut(n_return).kind {
                    r.return_value = return_value;
                    r.void_return = return_value.is_none();
                }
                if let Some(suite_id) = self.current_suite {
                    if let NodeKind::Suite(s) = &mut self.tree.get_mut(suite_id).kind {
                        s.has_return = true;
                    }
                }
                self.end_statement("return statement");
                result = Some(n_return);
            }
            TokenKind::Breakpoint => {
                self.advance();
                let id = self.alloc(NodeKind::Breakpoint);
                self.complete_extents(id);
                self.end_statement(r#""breakpoint""#);
                result = Some(id);
            }
            TokenKind::Assert => {
                self.advance();
                result = self.parse_assert();
            }
            TokenKind::Annotation => {
                self.advance();
                if let Some(annotation) = self
                    .parse_annotation(annotation_target::STATEMENT | annotation_target::STANDALONE)
                {
                    if self.annotation_applies_to(annotation, annotation_target::STANDALONE) {
                        if self.previous.kind != TokenKind::Newline {
                            self.push_error("Expected newline after a standalone annotation.");
                        }
                        let name = self.annotation_name(annotation);
                        if !matches!(
                            name.as_str(),
                            "@warning_ignore_start" | "@warning_ignore_restore"
                        ) {
                            self.push_error("Unexpected standalone annotation.");
                        }
                    } else {
                        self.annotation_stack.push(annotation);
                    }
                }
            }
            _ => {
                // Expression statement (assignment allowed).
                let expression = self.parse_expression(true, false);
                let mut has_ended_lambda = false;
                if expression.is_none() {
                    if self.in_lambda {
                        // Might be the continuation of the outer expression containing this lambda.
                        self.lambda_ended = true;
                        has_ended_lambda = true;
                    } else {
                        self.advance();
                        let found = self.previous.name();
                        self.push_error(format!(r#"Expected statement, found "{found}" instead."#));
                    }
                } else {
                    self.end_statement("expression");
                }
                // A standalone lambda is useless and so an error (the rest of Godot's
                // `DEBUG_ENABLED` block here — `STANDALONE_EXPRESSION`/`STANDALONE_TERNARY`/
                // `RETURN_VALUE_DISCARDED` — is a set of M3 warnings, not parse errors).
                if let Some(expr) = expression {
                    if matches!(self.tree.get(expr).kind, NodeKind::Lambda(_)) {
                        self.push_error_at(
                            expr,
                            "Standalone lambdas cannot be accessed. Consider assigning it to a variable.",
                        );
                    }
                }
                self.lambda_ended = self.lambda_ended || has_ended_lambda;
                result = expression;
            }
        }

        if let Some(result) = result {
            if !annotations.is_empty() {
                self.tree.get_mut(result).annotations.extend(annotations);
            }
        }

        if self.panic_mode {
            self.synchronize();
        }
        result
    }

    fn parse_assert(&mut self) -> Option<NodeId> {
        let assert = self.alloc(NodeKind::Assert(AssertNode::default()));

        self.push_multiline(true);
        self.consume(
            TokenKind::ParenthesisOpen,
            r#"Expected "(" after "assert"."#,
        );

        let condition = self.parse_expression(false, false);
        if condition.is_none() {
            self.push_error("Expected expression to assert.");
            self.pop_multiline();
            self.complete_extents(assert);
            return None;
        }

        let mut message = None;
        if self.match_token(TokenKind::Comma) && !self.check(TokenKind::ParenthesisClose) {
            message = self.parse_expression(false, false);
            if message.is_none() {
                self.push_error(r#"Expected error message for assert after ","."#);
                self.pop_multiline();
                self.complete_extents(assert);
                return None;
            }
            self.match_token(TokenKind::Comma);
        }

        self.pop_multiline();
        self.consume(
            TokenKind::ParenthesisClose,
            r#"Expected ")" after assert expression."#,
        );

        self.complete_extents(assert);
        self.end_statement(r#""assert""#);
        if let NodeKind::Assert(a) = &mut self.tree.get_mut(assert).kind {
            a.condition = condition;
            a.message = message;
        }
        Some(assert)
    }

    fn parse_break(&mut self) -> Option<NodeId> {
        if !self.can_break {
            self.push_error(r#"Cannot use "break" outside of a loop."#);
        }
        let id = self.alloc(NodeKind::Break);
        self.complete_extents(id);
        self.end_statement(r#""break""#);
        Some(id)
    }

    fn parse_continue(&mut self) -> Option<NodeId> {
        if !self.can_continue {
            self.push_error(r#"Cannot use "continue" outside of a loop."#);
        }
        let id = self.alloc(NodeKind::Continue);
        self.complete_extents(id);
        self.end_statement(r#""continue""#);
        Some(id)
    }

    fn parse_for(&mut self) -> Option<NodeId> {
        let n_for = self.alloc(NodeKind::For(ForNode::default()));

        let mut variable = None;
        if self.consume(
            TokenKind::Identifier,
            r#"Expected loop variable name after "for"."#,
        ) {
            variable = self.parse_identifier_node();
            if let NodeKind::For(f) = &mut self.tree.get_mut(n_for).kind {
                f.variable = variable;
            }
        }

        let mut has_type = false;
        if self.match_token(TokenKind::Colon) {
            let ty = self.parse_type(false);
            if ty.is_none() {
                self.push_error(r#"Expected type specifier after ":"."#);
            }
            has_type = ty.is_some();
            if let NodeKind::For(f) = &mut self.tree.get_mut(n_for).kind {
                f.datatype_specifier = ty;
            }
        }

        if has_type {
            self.consume(
                TokenKind::In,
                r#"Expected "in" after "for" variable type specifier."#,
            );
        } else {
            self.consume(
                TokenKind::In,
                r#"Expected "in" or ":" after "for" variable name."#,
            );
        }

        let list = self.parse_expression(false, false);
        if list.is_none() {
            self.push_error(r#"Expected iterable after "in"."#);
        }
        if let NodeKind::For(f) = &mut self.tree.get_mut(n_for).kind {
            f.list = list;
        }

        self.consume(TokenKind::Colon, r#"Expected ":" after "for" condition."#);

        let could_break = self.can_break;
        let could_continue = self.can_continue;
        self.can_break = true;
        self.can_continue = true;

        let suite = self.alloc(NodeKind::Suite(SuiteNode::default()));
        if let Some(var) = variable {
            let name = self.identifier_name(var);
            if let Some(kind) = self.suite_lookup_local(self.current_suite, &name) {
                let existing = local_kind_name(kind);
                self.push_error_at(
                    var,
                    format!(
                        r#"There is already a {existing} named "{name}" declared in this scope."#
                    ),
                );
            }
            self.suite_add_local(
                suite,
                Local {
                    kind: LocalKind::ForVariable,
                    name,
                    source: var,
                },
            );
        }
        let loop_body = self.parse_suite(r#""for" block"#, Some(suite), false);
        if let NodeKind::For(f) = &mut self.tree.get_mut(n_for).kind {
            f.loop_body = Some(loop_body);
        }
        self.complete_extents(n_for);

        self.can_break = could_break;
        self.can_continue = could_continue;
        Some(n_for)
    }

    fn parse_if(&mut self, p_token: &str) -> Option<NodeId> {
        let n_if = self.alloc(NodeKind::If(IfNode::default()));

        let condition = self.parse_expression(false, false);
        if condition.is_none() {
            self.push_error(format!(
                r#"Expected conditional expression after "{p_token}"."#
            ));
        }
        if let NodeKind::If(n) = &mut self.tree.get_mut(n_if).kind {
            n.condition = condition;
        }

        self.consume(
            TokenKind::Colon,
            format!(r#"Expected ":" after "{p_token}" condition."#),
        );

        let true_block = self.parse_suite(&format!(r#""{p_token}" block"#), None, false);
        if let NodeKind::If(n) = &mut self.tree.get_mut(n_if).kind {
            n.true_block = Some(true_block);
        }

        if self.match_token(TokenKind::Elif) {
            let else_block = self.alloc(NodeKind::Suite(SuiteNode {
                parent_block: self.current_suite,
                ..SuiteNode::default()
            }));
            let previous_suite = self.current_suite;
            self.current_suite = Some(else_block);

            let elif = self.parse_if("elif");
            if let (NodeKind::Suite(s), Some(elif)) =
                (&mut self.tree.get_mut(else_block).kind, elif)
            {
                s.statements.push(elif);
            }
            self.complete_extents(else_block);
            if let NodeKind::If(n) = &mut self.tree.get_mut(n_if).kind {
                n.false_block = Some(else_block);
            }
            self.current_suite = previous_suite;
        } else if self.match_token(TokenKind::Else) {
            self.consume(TokenKind::Colon, r#"Expected ":" after "else"."#);
            let false_block = self.parse_suite(r#""else" block"#, None, false);
            if let NodeKind::If(n) = &mut self.tree.get_mut(n_if).kind {
                n.false_block = Some(false_block);
            }
        }
        self.complete_extents(n_if);

        // gdscript_parser.cpp:2383-2385 — both `true_block` and `false_block` having `has_return`
        // propagates return-coverage to the containing suite.
        let (true_returns, false_returns) = if let NodeKind::If(n) = &self.tree.get(n_if).kind {
            let tr = n
                .true_block
                .and_then(|b| match &self.tree.get(b).kind {
                    NodeKind::Suite(s) => Some(s.has_return),
                    _ => None,
                })
                .unwrap_or(false);
            let fr = n
                .false_block
                .and_then(|b| match &self.tree.get(b).kind {
                    NodeKind::Suite(s) => Some(s.has_return),
                    _ => None,
                })
                .unwrap_or(false);
            (tr, fr)
        } else {
            (false, false)
        };
        if true_returns && false_returns {
            if let Some(suite_id) = self.current_suite {
                if let NodeKind::Suite(s) = &mut self.tree.get_mut(suite_id).kind {
                    s.has_return = true;
                }
            }
        }
        Some(n_if)
    }

    fn parse_while(&mut self) -> Option<NodeId> {
        let n_while = self.alloc(NodeKind::While(WhileNode::default()));

        let condition = self.parse_expression(false, false);
        if condition.is_none() {
            self.push_error(r#"Expected conditional expression after "while"."#);
        }
        if let NodeKind::While(n) = &mut self.tree.get_mut(n_while).kind {
            n.condition = condition;
        }

        self.consume(TokenKind::Colon, r#"Expected ":" after "while" condition."#);

        let could_break = self.can_break;
        let could_continue = self.can_continue;
        self.can_break = true;
        self.can_continue = true;

        let suite = self.alloc(NodeKind::Suite(SuiteNode::default()));
        let loop_body = self.parse_suite(r#""while" block"#, Some(suite), false);
        if let NodeKind::While(n) = &mut self.tree.get_mut(n_while).kind {
            n.loop_body = Some(loop_body);
        }
        self.complete_extents(n_while);

        self.can_break = could_break;
        self.can_continue = could_continue;
        Some(n_while)
    }

    fn parse_match(&mut self) -> Option<NodeId> {
        let match_node = self.alloc(NodeKind::Match(MatchNode::default()));

        let test = self.parse_expression(false, false);
        if test.is_none() {
            self.push_error(r#"Expected expression to test after "match"."#);
        }
        if let NodeKind::Match(m) = &mut self.tree.get_mut(match_node).kind {
            m.test = test;
        }

        self.consume(
            TokenKind::Colon,
            r#"Expected ":" after "match" expression."#,
        );
        self.consume(
            TokenKind::Newline,
            r#"Expected a newline after "match" statement."#,
        );

        if !self.consume(
            TokenKind::Indent,
            r#"Expected an indented block after "match" statement."#,
        ) {
            self.complete_extents(match_node);
            return Some(match_node);
        }

        let mut branch_annotations: Vec<NodeId> = Vec::new();
        // gdscript_parser.cpp:2409-2459 — track `all_have_return && have_wildcard` across branches.
        let mut all_have_return = true;
        let mut have_wildcard = false;
        while !self.check(TokenKind::Dedent) && !self.is_at_end() {
            if self.match_token(TokenKind::Pass) {
                self.consume(TokenKind::Newline, r#"Expected newline after "pass"."#);
                continue;
            }
            if self.match_token(TokenKind::Annotation) {
                let Some(annotation) = self.parse_annotation(annotation_target::STATEMENT) else {
                    continue;
                };
                if self.annotation_name(annotation) != "@warning_ignore" {
                    let name = self.annotation_name(annotation);
                    self.push_error_at(
                        annotation,
                        format!(r#"Annotation "{name}" is not allowed in this level."#),
                    );
                    continue;
                }
                branch_annotations.push(annotation);
                continue;
            }

            let Some(branch) = self.parse_match_branch() else {
                self.advance();
                continue;
            };
            if !branch_annotations.is_empty() {
                self.tree
                    .get_mut(branch)
                    .annotations
                    .append(&mut branch_annotations);
            }
            // gdscript_parser.cpp:2450-2451 — `all_have_return &&= branch->block->has_return`
            // and `have_wildcard ||= branch->has_wildcard`.
            if let NodeKind::MatchBranch(b) = &self.tree.get(branch).kind {
                have_wildcard = have_wildcard || b.has_wildcard;
                let branch_returns = b
                    .block
                    .and_then(|blk| match &self.tree.get(blk).kind {
                        NodeKind::Suite(s) => Some(s.has_return),
                        _ => None,
                    })
                    .unwrap_or(false);
                all_have_return = all_have_return && branch_returns;
            }
            if let NodeKind::Match(m) = &mut self.tree.get_mut(match_node).kind {
                m.branches.push(branch);
            }
        }
        self.complete_extents(match_node);

        self.consume(
            TokenKind::Dedent,
            r#"Expected an indented block after "match" statement."#,
        );

        // gdscript_parser.cpp:2458-2460 — every branch returns AND a wildcard pattern is present
        // (otherwise the match is non-exhaustive at runtime, so return coverage is not guaranteed).
        if all_have_return && have_wildcard {
            if let Some(suite_id) = self.current_suite {
                if let NodeKind::Suite(s) = &mut self.tree.get_mut(suite_id).kind {
                    s.has_return = true;
                }
            }
        }

        for annotation in branch_annotations {
            let name = self.annotation_name(annotation);
            self.push_error_at(
                annotation,
                format!(
                    r#"Annotation "{name}" does not precede a valid target, so it will have no effect."#
                ),
            );
        }
        Some(match_node)
    }

    fn parse_match_branch(&mut self) -> Option<NodeId> {
        let branch = self.alloc(NodeKind::MatchBranch(MatchBranchNode::default()));
        self.reset_extents_from_current(branch);

        let mut has_bind = false;
        loop {
            let pattern = self.parse_match_pattern(None);
            if let Some(pattern) = pattern {
                if self.pattern_bind_count(pattern) > 0 {
                    has_bind = true;
                }
                if self.branch_pattern_count(branch) > 0 && has_bind {
                    self.push_error("Cannot use a variable bind with multiple patterns.");
                }
                let pt = self.pattern_kind(pattern);
                if matches!(pt, PatternKind::Rest) {
                    self.push_error(
                        "Rest pattern can only be used inside array and dictionary patterns.",
                    );
                } else if matches!(pt, PatternKind::Bind(_) | PatternKind::Wildcard) {
                    if let NodeKind::MatchBranch(b) = &mut self.tree.get_mut(branch).kind {
                        b.has_wildcard = true;
                    }
                }
                if let NodeKind::MatchBranch(b) = &mut self.tree.get_mut(branch).kind {
                    b.patterns.push(pattern);
                }
            }
            if !self.match_token(TokenKind::Comma) {
                break;
            }
        }

        if self.branch_pattern_count(branch) == 0 {
            self.push_error(r#"No pattern found for "match" branch."#);
        }

        let mut has_guard = false;
        if self.match_token(TokenKind::When) {
            // The guard gets its own block so pattern binds are visible without leaking outward.
            let guard_body = self.alloc(NodeKind::Suite(SuiteNode::default()));
            self.add_branch_binds_to_suite(branch, guard_body);

            let parent_block = self.current_suite;
            if let NodeKind::Suite(s) = &mut self.tree.get_mut(guard_body).kind {
                s.parent_block = parent_block;
            }
            self.current_suite = Some(guard_body);

            let guard = self.parse_expression(false, false);
            if let Some(guard) = guard {
                if let NodeKind::Suite(s) = &mut self.tree.get_mut(guard_body).kind {
                    s.statements.push(guard);
                }
            } else {
                self.push_error(r#"Expected expression for pattern guard after "when"."#);
            }
            self.current_suite = parent_block;
            self.complete_extents(guard_body);

            has_guard = true;
            if let NodeKind::MatchBranch(b) = &mut self.tree.get_mut(branch).kind {
                b.guard_body = Some(guard_body);
                b.has_wildcard = false; // A guard might still not match.
            }
        }

        let colon_msg = if has_guard {
            r#"Expected ":" after "match" pattern guard."#.to_string()
        } else {
            r#"Expected ":" or "when" after "match" patterns."#.to_string()
        };
        if !self.consume(TokenKind::Colon, colon_msg) {
            let recovery = self.alloc_recovery(NodeKind::Suite(SuiteNode::default()));
            if let NodeKind::MatchBranch(b) = &mut self.tree.get_mut(branch).kind {
                b.block = Some(recovery);
            }
            self.complete_extents(branch);
            // Consume the rest of the line; treat the next as a new branch.
            while self.current.kind != TokenKind::Newline && !self.is_at_end() {
                self.advance();
            }
            if !self.is_at_end() {
                self.advance();
            }
            return Some(branch);
        }

        let suite = self.alloc(NodeKind::Suite(SuiteNode::default()));
        self.add_branch_binds_to_suite(branch, suite);
        let block = self.parse_suite("match pattern block", Some(suite), false);
        if let NodeKind::MatchBranch(b) = &mut self.tree.get_mut(branch).kind {
            b.block = Some(block);
        }
        self.complete_extents(branch);
        Some(branch)
    }

    fn parse_match_pattern(&mut self, p_root_pattern: Option<NodeId>) -> Option<NodeId> {
        // Depth-guard nested array/dictionary patterns (`[[[…]]]`). See [`MAX_PARSE_DEPTH`].
        if self.depth >= MAX_PARSE_DEPTH {
            self.push_error("Pattern is too deeply nested.");
            return None;
        }
        self.depth += 1;
        let result = self.parse_match_pattern_inner(p_root_pattern);
        self.depth -= 1;
        result
    }

    fn parse_match_pattern_inner(&mut self, p_root_pattern: Option<NodeId>) -> Option<NodeId> {
        let pattern = self.alloc(NodeKind::Pattern(PatternNode::default()));
        self.reset_extents_from_current(pattern);

        match self.current.kind {
            TokenKind::Var => {
                self.advance();
                if !self.consume(TokenKind::Identifier, r#"Expected bind name after "var"."#) {
                    self.complete_extents(pattern);
                    return None;
                }
                let bind = self.parse_identifier_node();
                if let NodeKind::Pattern(p) = &mut self.tree.get_mut(pattern).kind {
                    p.pattern_type = PatternKind::Bind(bind);
                }
                let bind_name = bind.map(|i| self.identifier_name(i)).unwrap_or_default();
                let root = p_root_pattern.unwrap_or(pattern);

                if p_root_pattern.is_some() && self.pattern_has_bind(root, &bind_name) {
                    self.push_error(format!(
                        r#"Bind variable name "{bind_name}" was already used in this pattern."#
                    ));
                    self.complete_extents(pattern);
                    return None;
                }
                if let Some(kind) = self.suite_lookup_local(self.current_suite, &bind_name) {
                    let existing = local_kind_name(kind);
                    self.push_error(format!(
                        r#"There's already a {existing} named "{bind_name}" in this scope."#
                    ));
                    self.complete_extents(pattern);
                    return None;
                }
                if let (Some(bind), NodeKind::Pattern(rp)) =
                    (bind, &mut self.tree.get_mut(root).kind)
                {
                    rp.binds.insert(bind_name, bind);
                }
            }
            TokenKind::Underscore => {
                self.advance();
                if let NodeKind::Pattern(p) = &mut self.tree.get_mut(pattern).kind {
                    p.pattern_type = PatternKind::Wildcard;
                }
            }
            TokenKind::PeriodPeriod => {
                self.advance();
                if let NodeKind::Pattern(p) = &mut self.tree.get_mut(pattern).kind {
                    p.pattern_type = PatternKind::Rest;
                }
            }
            TokenKind::BracketOpen => {
                self.push_multiline(true);
                self.advance();
                if let NodeKind::Pattern(p) = &mut self.tree.get_mut(pattern).kind {
                    p.pattern_type = PatternKind::Array;
                }
                let root = p_root_pattern.or(Some(pattern));
                loop {
                    if self.is_at_end() || self.check(TokenKind::BracketClose) {
                        break;
                    }
                    if let Some(sub) = self.parse_match_pattern(root) {
                        if self.pattern_rest_used(pattern) {
                            self.push_error(
                                r#"The ".." pattern must be the last element in the pattern array."#,
                            );
                        } else if matches!(self.pattern_kind(sub), PatternKind::Rest) {
                            if let NodeKind::Pattern(p) = &mut self.tree.get_mut(pattern).kind {
                                p.rest_used = true;
                            }
                        }
                        if let NodeKind::Pattern(p) = &mut self.tree.get_mut(pattern).kind {
                            p.array.push(sub);
                        }
                    }
                    if !self.match_token(TokenKind::Comma) {
                        break;
                    }
                }
                self.consume(
                    TokenKind::BracketClose,
                    r#"Expected "]" to close the array pattern."#,
                );
                self.pop_multiline();
            }
            TokenKind::BraceOpen => {
                self.push_multiline(true);
                self.advance();
                if let NodeKind::Pattern(p) = &mut self.tree.get_mut(pattern).kind {
                    p.pattern_type = PatternKind::Dictionary;
                }
                let root = p_root_pattern.or(Some(pattern));
                loop {
                    if self.check(TokenKind::BraceClose) || self.is_at_end() {
                        break;
                    }
                    if self.match_token(TokenKind::PeriodPeriod) {
                        if self.pattern_rest_used(pattern) {
                            self.push_error(
                                r#"The ".." pattern must be the last element in the pattern dictionary."#,
                            );
                        } else {
                            let sub = self.alloc(NodeKind::Pattern(PatternNode {
                                pattern_type: PatternKind::Rest,
                                ..PatternNode::default()
                            }));
                            self.complete_extents(sub);
                            if let NodeKind::Pattern(p) = &mut self.tree.get_mut(pattern).kind {
                                p.dictionary.push(KeyValue {
                                    key: None,
                                    value: Some(sub),
                                });
                                p.rest_used = true;
                            }
                        }
                    } else {
                        let key = self.parse_expression(false, false);
                        if key.is_none() {
                            self.push_error("Expected expression as key for dictionary pattern.");
                        }
                        if self.match_token(TokenKind::Colon) {
                            if let Some(sub) = self.parse_match_pattern(root) {
                                if self.pattern_rest_used(pattern) {
                                    self.push_error(
                                        r#"The ".." pattern must be the last element in the pattern dictionary."#,
                                    );
                                } else if matches!(self.pattern_kind(sub), PatternKind::Rest) {
                                    self.push_error(
                                        r#"The ".." pattern cannot be used as a value."#,
                                    );
                                } else if let NodeKind::Pattern(p) =
                                    &mut self.tree.get_mut(pattern).kind
                                {
                                    p.dictionary.push(KeyValue {
                                        key,
                                        value: Some(sub),
                                    });
                                }
                            }
                        } else if let NodeKind::Pattern(p) = &mut self.tree.get_mut(pattern).kind {
                            p.dictionary.push(KeyValue { key, value: None });
                        }
                    }
                    if !self.match_token(TokenKind::Comma) {
                        break;
                    }
                }
                self.consume(
                    TokenKind::BraceClose,
                    r#"Expected "}" to close the dictionary pattern."#,
                );
                self.pop_multiline();
            }
            _ => {
                let expression = self.parse_expression(false, false);
                if expression.is_none() {
                    self.push_error("Expected expression for match pattern.");
                    self.complete_extents(pattern);
                    return None;
                }
                let is_literal = expression
                    .is_some_and(|e| matches!(self.tree.get(e).kind, NodeKind::Literal(_)));
                if let NodeKind::Pattern(p) = &mut self.tree.get_mut(pattern).kind {
                    p.pattern_type = if is_literal {
                        PatternKind::Literal(expression)
                    } else {
                        PatternKind::Expression(expression)
                    };
                }
            }
        }
        self.complete_extents(pattern);
        Some(pattern)
    }

    // ----- small helpers reading node payloads -----

    fn identifier_name(&self, id: NodeId) -> String {
        match &self.tree.get(id).kind {
            NodeKind::Identifier(ident) => ident.name.clone(),
            _ => String::new(),
        }
    }

    fn function_name_of(&self, func: NodeId) -> String {
        if let NodeKind::Function(f) = &self.tree.get(func).kind {
            if let Some(ident) = f.identifier {
                return self.identifier_name(ident);
            }
        }
        "<anonymous>".to_string()
    }

    // ----- statement termination & positioned errors (cpp:672, 234) -----

    /// Consume the token(s) that end a statement, erroring if none is found (`cpp:672`).
    fn end_statement(&mut self, p_context: &str) {
        let mut found = false;
        while self.is_statement_end() && !self.is_at_end() {
            if self.is_statement_end_token() {
                self.advance();
            } else if self.lambda_ended {
                self.lambda_ended = false; // Consume this synthetic "token".
                found = true;
                break;
            } else {
                if !found {
                    self.lambda_ended = true; // Found something else that ends the statement.
                    found = true;
                }
                break;
            }
            found = true;
        }
        if !found && !self.is_at_end() {
            let cur = self.current.name();
            self.push_error(format!(
                r#"Expected end of statement after {p_context}, found "{cur}" instead."#
            ));
        }
    }

    /// Like [`Parser::push_error`] but anchored at a node's extents (Godot's `push_error(msg, node)`).
    fn push_error_at(&mut self, node: NodeId, message: impl Into<String>) {
        self.panic_mode = true;
        let span = self.tree.get(node).span;
        self.errors.push(Diagnostic {
            span,
            message: message.into(),
        });
    }

    /// Whether the current token is an identifier with exactly this spelling (Godot's
    /// `current.get_identifier() == "…"`), used to spot the soft keywords `get`/`set`.
    fn current_identifier_is(&self, name: &str) -> bool {
        self.current.kind.is_identifier() && &*self.current.source == name
    }

    // ----- annotation helpers -----

    fn annotation_name(&self, id: NodeId) -> String {
        match &self.tree.get(id).kind {
            NodeKind::Annotation(a) => a.name.clone(),
            _ => String::new(),
        }
    }

    fn annotation_applies_to(&self, id: NodeId, targets: u32) -> bool {
        let kind = annotation_target_kind(&self.annotation_name(id)).unwrap_or(0);
        (kind & targets) != 0
    }

    fn push_pending_annotations_to_head(&mut self, head: NodeId) {
        if !self.annotation_stack.is_empty() {
            let annots = std::mem::take(&mut self.annotation_stack);
            self.tree.get_mut(head).annotations.extend(annots);
        }
    }

    /// WP-F1/F2: enforce Godot's "this annotation can only be used once" rule for `@icon` and
    /// `@tool` (`gdscript_parser.cpp:4430-4470`'s `tool_annotation` / `icon_annotation`). Returns
    /// `true` if the caller should attach `annotation` to `head`; `false` if a duplicate diagnostic
    /// was emitted and the annotation must be dropped. As a free side-effect (per plan §2.4.5 and
    /// the otherwise-dead `ClassNode::icon_path` field at `ast.rs:244`), the first `@icon` with a
    /// resolvable string argument also populates `head.icon_path`.
    fn check_class_singleton_annotation(&mut self, head: NodeId, annotation: NodeId) -> bool {
        let name = match &self.tree.get(annotation).kind {
            NodeKind::Annotation(a) => a.name.clone(),
            _ => return true,
        };
        match name.as_str() {
            // Godot `tool_annotation` (gdscript_parser.cpp:4403-4406): a second `@tool` is the
            // error. `@tool` takes no argument and always succeeds, so the presence of any prior
            // `@tool` on the script class is the duplicate signal (mirrors the parser-wide
            // `_is_tool` flag — one script, one flag).
            "@tool" => {
                for prior in self.tree.get(head).annotations.clone() {
                    if let NodeKind::Annotation(p) = &self.tree.get(prior).kind {
                        if p.name == "@tool" {
                            self.push_error_at(
                                annotation,
                                r#""@tool" annotation can only be used once."#.to_string(),
                            );
                            return false;
                        }
                    }
                }
                true
            }
            // Godot `icon_annotation` (gdscript_parser.cpp:4414-4443). The old node-presence check
            // got two malformed-input cases wrong, both of which produced a false positive:
            //   * `ERR_FAIL_COND_V(resolved_arguments.is_empty())` (4416) runs BEFORE the
            //     duplicate check, so an `@icon` with no string argument records nothing and
            //     raises no "used once" error — it is not a duplicate of anything, nor does it
            //     make a later `@icon` one.
            //   * the duplicate is keyed on the class ALREADY holding a non-empty `icon_path`
            //     (4422) — a prior `@icon` that recorded one — NOT on the presence of a prior
            //     `@icon` node. A prior `@icon` with an empty/absent path leaves `icon_path`
            //     empty, so a following `@icon` is not a duplicate.
            "@icon" => {
                // Godot anchors the empty-path error on the argument node
                // (`p_annotation->arguments[0]`, gdscript_parser.cpp:4427), not the whole
                // annotation — unlike the duplicate error, which uses `p_annotation`.
                let arg_id = match &self.tree.get(annotation).kind {
                    NodeKind::Annotation(a) => a.arguments.first().copied(),
                    _ => None,
                };
                let Some(path) = self.first_string_argument(annotation) else {
                    return true;
                };
                let has_icon_path = matches!(
                    &self.tree.get(head).kind,
                    NodeKind::Class(c) if c.icon_path.as_deref().is_some_and(|p| !p.is_empty())
                );
                if has_icon_path {
                    self.push_error_at(
                        annotation,
                        r#""@icon" annotation can only be used once."#.to_string(),
                    );
                    return false;
                }
                if path.is_empty() {
                    self.push_error_at(
                        arg_id.unwrap_or(annotation),
                        r#""@icon" annotation argument must contain the path to the icon."#
                            .to_string(),
                    );
                    return false;
                }
                if let NodeKind::Class(c) = &mut self.tree.get_mut(head).kind {
                    c.icon_path = Some(path);
                }
                true
            }
            // Godot `static_unload_annotation` (gdscript_parser.cpp:4445-4454): a second
            // `@static_unload` is the error. Unlike `@tool`/`@icon` this check is NOT behind
            // `#ifdef DEBUG_ENABLED`, but gdls runs `check_class_singleton_annotation`
            // unconditionally so it fires regardless. A prior `@static_unload` already recorded on
            // the head is the duplicate signal (same shape as `@tool`).
            "@static_unload" => {
                for prior in self.tree.get(head).annotations.clone() {
                    if let NodeKind::Annotation(p) = &self.tree.get(prior).kind {
                        if p.name == "@static_unload" {
                            self.push_error_at(
                                annotation,
                                r#""@static_unload" annotation can only be used once per script."#
                                    .to_string(),
                            );
                            return false;
                        }
                    }
                }
                true
            }
            _ => true,
        }
    }

    /// Resolve the first argument of `annotation` as a string literal, if any. Used by
    /// [`Self::check_class_singleton_annotation`] to populate `ClassNode::icon_path`. Returns
    /// `None` when the annotation has no arguments or the first one is not a string literal.
    fn first_string_argument(&self, annotation: NodeId) -> Option<String> {
        let arg = match &self.tree.get(annotation).kind {
            NodeKind::Annotation(a) => *a.arguments.first()?,
            _ => return None,
        };
        if let NodeKind::Literal(lit) = &self.tree.get(arg).kind {
            if let Literal::String(s) = &lit.value {
                return Some(s.clone());
            }
        }
        None
    }

    // ----- class member helpers (gdscript_parser.h:779) -----

    fn class_has_identifier(&self, class_id: NodeId) -> bool {
        matches!(&self.tree.get(class_id).kind, NodeKind::Class(c) if c.identifier.is_some())
    }

    fn class_extends_used(&self, class_id: NodeId) -> bool {
        matches!(&self.tree.get(class_id).kind, NodeKind::Class(c) if c.extends_used)
    }

    fn class_push_extends(&mut self, class_id: Option<NodeId>, ident: NodeId) {
        if let Some(class_id) = class_id {
            if let NodeKind::Class(c) = &mut self.tree.get_mut(class_id).kind {
                c.extends.push(ident);
            }
        }
    }

    fn class_has_member(&self, class_id: NodeId, name: &str) -> bool {
        matches!(&self.tree.get(class_id).kind, NodeKind::Class(c) if c.members_indices.contains_key(name))
    }

    fn class_member_type_name(&self, class_id: NodeId, name: &str) -> &'static str {
        if let NodeKind::Class(c) = &self.tree.get(class_id).kind {
            if let Some(&idx) = c.members_indices.get(name) {
                return member_type_name(&c.members[idx]);
            }
        }
        "???"
    }

    fn class_add_member(&mut self, class_id: NodeId, member: Member) {
        let name = self.member_name(&member);
        if let NodeKind::Class(c) = &mut self.tree.get_mut(class_id).kind {
            let idx = c.members.len();
            c.members_indices.insert(name, idx);
            c.members.push(member);
        }
    }

    fn class_add_member_group(&mut self, class_id: NodeId, annotation: NodeId) {
        if let NodeKind::Class(c) = &mut self.tree.get_mut(class_id).kind {
            let idx = c.members.len();
            // A synthetic, collision-free key (the export prefix is an analyzer concern).
            c.members_indices.insert(format!("@group_{idx}"), idx);
            c.members.push(Member::Group(annotation));
        }
    }

    /// Wrap a parsed member node in its [`Member`] variant for class registration.
    fn member_for(&self, node: NodeId) -> Member {
        match &self.tree.get(node).kind {
            NodeKind::Variable(_) => Member::Variable(node),
            NodeKind::Constant(_) => Member::Constant(node),
            NodeKind::Function(_) => Member::Function(node),
            NodeKind::Signal(_) => Member::Signal(node),
            NodeKind::Class(_) => Member::Class(node),
            NodeKind::Enum(_) => Member::Enum(node),
            _ => Member::Variable(node), // Unreachable: dispatch only yields the kinds above.
        }
    }

    fn member_name(&self, member: &Member) -> String {
        let ident = match member {
            Member::Class(id)
            | Member::Constant(id)
            | Member::Function(id)
            | Member::Signal(id)
            | Member::Variable(id)
            | Member::Enum(id) => self.node_identifier(*id),
            Member::EnumValue(v) => v.identifier,
            Member::Group(_) => None,
        };
        ident.map(|i| self.identifier_name(i)).unwrap_or_default()
    }

    /// The `identifier` child of a declaration node, if it has one.
    fn node_identifier(&self, node: NodeId) -> Option<NodeId> {
        match &self.tree.get(node).kind {
            NodeKind::Variable(v) => v.identifier,
            NodeKind::Constant(c) => c.identifier,
            NodeKind::Function(f) => f.identifier,
            NodeKind::Signal(s) => s.identifier,
            NodeKind::Class(c) => c.identifier,
            NodeKind::Enum(e) => e.identifier,
            NodeKind::Parameter(p) => p.identifier,
            _ => None,
        }
    }

    // ----- variable / function / parameter / signal field reads -----

    fn variable_property_style(&self, variable: NodeId) -> PropertyStyle {
        match &self.tree.get(variable).kind {
            NodeKind::Variable(v) => v.property,
            _ => PropertyStyle::None,
        }
    }

    fn variable_is_static(&self, variable: NodeId) -> bool {
        matches!(&self.tree.get(variable).kind, NodeKind::Variable(v) if v.is_static)
    }

    fn function_is_vararg(&self, function: NodeId) -> bool {
        matches!(&self.tree.get(function).kind, NodeKind::Function(f) if f.rest_parameter.is_some())
    }

    fn function_has_parameter(&self, function: NodeId, name: &str) -> bool {
        matches!(&self.tree.get(function).kind, NodeKind::Function(f) if f.parameters_indices.contains_key(name))
    }

    fn function_param_count(&self, function: NodeId) -> usize {
        match &self.tree.get(function).kind {
            NodeKind::Function(f) => f.parameters.len(),
            _ => 0,
        }
    }

    fn function_is_static(&self, function: NodeId) -> bool {
        matches!(&self.tree.get(function).kind, NodeKind::Function(f) if f.is_static)
    }

    fn function_name_opt(&self, function: NodeId) -> Option<String> {
        match &self.tree.get(function).kind {
            NodeKind::Function(f) => f.identifier.map(|i| self.identifier_name(i)),
            _ => None,
        }
    }

    /// Whether the enclosing function is a constructor (`_init`/`_static_init`); a constructor may
    /// not `return` a value (`cpp:2078`).
    fn current_function_is_constructor(&self) -> bool {
        self.current_function
            .and_then(|f| self.function_name_opt(f))
            .is_some_and(|n| n == "_init" || n == "_static_init")
    }

    fn parameter_has_initializer(&self, parameter: NodeId) -> bool {
        matches!(&self.tree.get(parameter).kind, NodeKind::Parameter(p) if p.initializer.is_some())
    }

    fn parameter_name(&self, parameter: NodeId) -> String {
        match &self.tree.get(parameter).kind {
            NodeKind::Parameter(p) => p
                .identifier
                .map(|i| self.identifier_name(i))
                .unwrap_or_default(),
            _ => String::new(),
        }
    }

    fn signal_has_parameter(&self, signal: NodeId, name: &str) -> bool {
        match &self.tree.get(signal).kind {
            NodeKind::Signal(s) => s.parameters.iter().any(|&p| self.parameter_name(p) == name),
            _ => false,
        }
    }

    // ----- suite local tracking (gdscript_parser.h:1182) -----

    fn suite_add_local(&mut self, suite: NodeId, local: Local) {
        if let NodeKind::Suite(s) = &mut self.tree.get_mut(suite).kind {
            let idx = s.locals.len();
            s.locals_indices.insert(local.name.clone(), idx);
            s.locals.push(local);
        }
    }

    /// Look up a local by name, walking up the `parent_block` chain (Godot's `get_local`); returns
    /// its kind, or `None` if undefined in any enclosing scope.
    fn suite_lookup_local(&self, suite: Option<NodeId>, name: &str) -> Option<LocalKind> {
        let mut cur = suite;
        while let Some(id) = cur {
            let NodeKind::Suite(s) = &self.tree.get(id).kind else {
                break;
            };
            if let Some(&idx) = s.locals_indices.get(name) {
                return Some(s.locals[idx].kind);
            }
            cur = s.parent_block;
        }
        None
    }

    /// Register a `var`/`const` statement as a block local, erroring on redefinition (`cpp:1964`).
    fn register_suite_local(&mut self, suite: NodeId, statement: NodeId) {
        match &self.tree.get(statement).kind {
            NodeKind::Variable(v) => {
                let Some(ident) = v.identifier else { return };
                let name = self.identifier_name(ident);
                if let Some(kind) = self.suite_lookup_local(Some(suite), &name) {
                    let existing = local_kind_name(kind);
                    self.push_error_at(
                        ident,
                        format!(
                            r#"There is already a {existing} named "{name}" declared in this scope."#
                        ),
                    );
                }
                self.suite_add_local(
                    suite,
                    Local {
                        kind: LocalKind::Variable,
                        name,
                        source: statement,
                    },
                );
            }
            NodeKind::Constant(c) => {
                let Some(ident) = c.identifier else { return };
                let name = self.identifier_name(ident);
                if let Some(kind) = self.suite_lookup_local(Some(suite), &name) {
                    // Constants report only "constant"/"variable" (Godot collapses the rest).
                    let existing = if matches!(kind, LocalKind::Constant) {
                        "constant"
                    } else {
                        "variable"
                    };
                    self.push_error_at(
                        ident,
                        format!(
                            r#"There is already a {existing} named "{name}" declared in this scope."#
                        ),
                    );
                }
                self.suite_add_local(
                    suite,
                    Local {
                        kind: LocalKind::Constant,
                        name,
                        source: statement,
                    },
                );
            }
            _ => {}
        }
    }

    // ----- match pattern / branch field reads -----

    fn pattern_kind(&self, pattern: NodeId) -> PatternKind {
        match &self.tree.get(pattern).kind {
            NodeKind::Pattern(p) => p.pattern_type,
            _ => PatternKind::Wildcard,
        }
    }

    fn pattern_rest_used(&self, pattern: NodeId) -> bool {
        matches!(&self.tree.get(pattern).kind, NodeKind::Pattern(p) if p.rest_used)
    }

    fn pattern_bind_count(&self, pattern: NodeId) -> usize {
        match &self.tree.get(pattern).kind {
            NodeKind::Pattern(p) => p.binds.len(),
            _ => 0,
        }
    }

    fn pattern_has_bind(&self, pattern: NodeId, name: &str) -> bool {
        matches!(&self.tree.get(pattern).kind, NodeKind::Pattern(p) if p.binds.contains_key(name))
    }

    fn branch_pattern_count(&self, branch: NodeId) -> usize {
        match &self.tree.get(branch).kind {
            NodeKind::MatchBranch(b) => b.patterns.len(),
            _ => 0,
        }
    }

    /// Copy the binds accumulated on a branch's first pattern into a suite as `PATTERN_BIND` locals
    /// (`cpp:2543`), so a guard/body can resolve them.
    fn add_branch_binds_to_suite(&mut self, branch: NodeId, suite: NodeId) {
        let first_pattern = match &self.tree.get(branch).kind {
            NodeKind::MatchBranch(b) => b.patterns.first().copied(),
            _ => None,
        };
        let Some(first_pattern) = first_pattern else {
            return;
        };
        let binds: Vec<(String, NodeId)> = match &self.tree.get(first_pattern).kind {
            NodeKind::Pattern(p) => p.binds.iter().map(|(k, v)| (k.clone(), *v)).collect(),
            _ => Vec::new(),
        };
        for (name, source) in binds {
            self.suite_add_local(
                suite,
                Local {
                    kind: LocalKind::PatternBind,
                    name,
                    source,
                },
            );
        }
    }

    // ----- accessors for tests / future wiring -----

    pub fn tree(&self) -> &ParseTree {
        &self.tree
    }

    pub fn errors(&self) -> &[Diagnostic] {
        &self.errors
    }

    /// Consume the parser, yielding the owned tree and collected diagnostics (used by [`crate::parse`]).
    /// M7 (#62): hand the lexer's recorded comments to the post-parse doc association.
    pub fn take_comments(&mut self) -> std::collections::HashMap<u32, crate::lexer::CommentData> {
        std::mem::take(&mut self.lexer.comments)
    }

    pub fn into_parts(mut self) -> (ParseTree, Vec<Diagnostic>) {
        // WP-R3: stamp the lexer's final line counter onto the tree so analyzer emissions
        // anchored on the parser's `previous` token at end-of-parse can render at the
        // Godot's synthetic post-EOF line. By the time `parse_program` finishes,
        // `self.current` is the EOF token; `loc.end.line` carries the lexer's `line`
        // counter after the EOF `newline(true)` bump (lexer.rs:214). For empty/partial
        // parses, fall back to `previous.loc.end.line` (also the lexer's view of "end").
        let eof_line = self.current.loc.end.line.max(self.previous.loc.end.line);
        self.tree.eof_line = eof_line;
        (self.tree, self.errors)
    }
}

// ===== documentSymbol projection (walks the parsed head class) =====

/// Project the parse tree's top-level class into a nested [`DocumentSymbol`] outline, mirroring
/// Godot's `parse_class_symbol` (`gdscript_extend_parser.cpp:240-252`). Always wraps the script in
/// one root `Class` symbol (named by `class_name` if present, else empty name — the server handler
/// fills the file basename for unnamed scripts). Inner classes recurse; unnamed members (e.g. a
/// malformed declaration with no identifier) are skipped.
pub fn document_symbols(tree: &ParseTree) -> Vec<DocumentSymbol> {
    if tree.is_empty() {
        return Vec::new();
    }
    let NodeKind::Class(class) = &tree.get(tree.root).kind else {
        return Vec::new();
    };

    let children = class_member_symbols(tree, tree.root);

    // name + selectionRange from the class_name identifier if present, else empty/zero-width
    // (the handler fills the file basename for an unnamed script — the parser has no path).
    let (name, selection_span) = match class.identifier {
        Some(id) => match &tree.get(id).kind {
            NodeKind::Identifier(ident) => (ident.name.clone(), tree.get(id).span),
            _ => (String::new(), ByteSpan { start: 0, end: 0 }),
        },
        None => (String::new(), ByteSpan { start: 0, end: 0 }),
    };

    vec![DocumentSymbol {
        name,
        kind: SymbolKind::Class,
        span: tree.get(tree.root).span, // whole-script range of the root class node
        selection_span,
        children,
    }]
}

fn class_member_symbols(tree: &ParseTree, class_id: NodeId) -> Vec<DocumentSymbol> {
    let NodeKind::Class(class) = &tree.get(class_id).kind else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for member in &class.members {
        if let Some(symbol) = member_symbol(tree, member) {
            out.push(symbol);
        }
    }
    out
}

fn member_symbol(tree: &ParseTree, member: &Member) -> Option<DocumentSymbol> {
    match member {
        Member::Class(id) => decl_symbol(
            tree,
            *id,
            SymbolKind::Class,
            class_member_symbols(tree, *id),
        ),
        Member::Constant(id) => decl_symbol(tree, *id, SymbolKind::Constant, Vec::new()),
        Member::Function(id) => decl_symbol(tree, *id, SymbolKind::Function, Vec::new()),
        Member::Signal(id) => decl_symbol(tree, *id, SymbolKind::Signal, Vec::new()),
        Member::Variable(id) => {
            let kind = match &tree.get(*id).kind {
                NodeKind::Variable(v) if v.property != PropertyStyle::None => SymbolKind::Property,
                _ => SymbolKind::Variable,
            };
            decl_symbol(tree, *id, kind, Vec::new())
        }
        Member::Enum(id) => {
            let children = match &tree.get(*id).kind {
                NodeKind::Enum(e) => e
                    .values
                    .iter()
                    .filter_map(|v| enum_value_symbol(tree, v))
                    .collect(),
                _ => Vec::new(),
            };
            decl_symbol(tree, *id, SymbolKind::Enum, children)
        }
        Member::EnumValue(value) => enum_value_symbol(tree, value),
        Member::Group(_) => None,
    }
}

/// Build a symbol for a declaration node, using its identifier child for the name + selection range.
fn decl_symbol(
    tree: &ParseTree,
    node: NodeId,
    kind: SymbolKind,
    children: Vec<DocumentSymbol>,
) -> Option<DocumentSymbol> {
    let identifier = match &tree.get(node).kind {
        NodeKind::Variable(v) => v.identifier,
        NodeKind::Constant(c) => c.identifier,
        NodeKind::Function(f) => f.identifier,
        NodeKind::Signal(s) => s.identifier,
        NodeKind::Class(c) => c.identifier,
        NodeKind::Enum(e) => e.identifier,
        _ => None,
    }?;
    let NodeKind::Identifier(ident) = &tree.get(identifier).kind else {
        return None;
    };
    Some(DocumentSymbol {
        name: ident.name.clone(),
        kind,
        span: tree.get(node).span,
        selection_span: tree.get(identifier).span,
        children,
    })
}

fn enum_value_symbol(tree: &ParseTree, value: &EnumValue) -> Option<DocumentSymbol> {
    let identifier = value.identifier?;
    let NodeKind::Identifier(ident) = &tree.get(identifier).kind else {
        return None;
    };
    Some(DocumentSymbol {
        name: ident.name.clone(),
        kind: SymbolKind::EnumMember,
        span: tree.get(identifier).span,
        selection_span: tree.get(identifier).span,
        children: Vec::new(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Parse `src` as a single expression, returning the tree, the root expression id, and errors.
    fn expr(src: &str) -> (ParseTree, Option<NodeId>, Vec<Diagnostic>) {
        let mut p = Parser::new(src);
        let root = p.parse_expression(true, false);
        (p.tree, root, p.errors)
    }

    fn kind_of(tree: &ParseTree, id: Option<NodeId>) -> Option<&NodeKind> {
        id.map(|i| &tree.get(i).kind)
    }

    /// M11 scene-typing relies on `GetNodeNode::full_path` faithfully carrying the path text so the
    /// analyzer can tell an ABSOLUTE path (leading `/`) and a UNIQUE name (leading `%`) apart from a
    /// plain relative path — a misread is a false-positive vector (an absolute `$/abs/x` read as
    /// relative would resolve against the wrong node). Pin the reconstructed `full_path`/`use_dollar`
    /// for each shape.
    #[test]
    fn get_node_full_path_preserves_absolute_and_unique_markers() {
        let path_of = |src: &str| -> (String, bool) {
            let (tree, root, errs) = expr(src);
            assert!(errs.is_empty(), "{src:?} parsed with errors: {errs:?}");
            match kind_of(&tree, root) {
                Some(NodeKind::GetNode(n)) => (n.full_path.clone(), n.use_dollar),
                other => panic!("{src:?} did not parse to a GetNode: {other:?}"),
            }
        };
        // Relative path: no leading slash, no `%`.
        assert_eq!(path_of("$Health"), ("Health".to_owned(), true));
        assert_eq!(path_of("$A/B"), ("A/B".to_owned(), true));
        // Absolute path: the leading `/` MUST be preserved (the parser consumes one optional initial
        // slash after `$`, then re-emits it into full_path — `$/root/x` → `/root/x`).
        assert_eq!(path_of("$/root/Child"), ("/root/Child".to_owned(), true));
        // Unique name: the `%` is preserved at the front.
        assert_eq!(path_of("%Special"), ("%Special".to_owned(), false));
        assert_eq!(path_of("$%Special"), ("%Special".to_owned(), true));
    }

    /// WP-F1/F2 fidelity (`gdscript_parser.cpp:4441-4459`): `@icon` duplicate detection is keyed on
    /// the class already holding a NON-EMPTY `icon_path`, not on the mere presence of a prior
    /// `@icon` node. These malformed-input cases have no `.out` corpus fixture (the corpus only
    /// covers the well-formed double-`@icon`), so they are pinned here to guard against regressing
    /// to the node-presence check, which false-fired on a no-argument or empty-path `@icon`.
    #[test]
    fn icon_duplicate_detection_matches_godot() {
        const DUP: &str = r#""@icon" annotation can only be used once."#;
        const EMPTY: &str = r#""@icon" annotation argument must contain the path to the icon."#;
        let dup_count = |src: &str| {
            crate::parse(src)
                .diagnostics
                .iter()
                .filter(|d| d.message == DUP)
                .count()
        };
        let messages = |src: &str| {
            crate::parse(src)
                .diagnostics
                .into_iter()
                .map(|d| d.message)
                .collect::<Vec<_>>()
        };

        // Well-formed: two `@icon` with real paths IS a duplicate (the corpus case, preserved).
        assert_eq!(
            dup_count("@icon(\"res://1.png\")\n@icon(\"res://2.png\")\n\nfunc test():\n\tpass\n"),
            1,
            "two resolved-path @icon must still report exactly one duplicate"
        );
        // A no-argument `@icon` after a valid one is NOT a duplicate in Godot: its empty
        // `resolved_arguments` trips `ERR_FAIL_COND_V` before the duplicate check ever runs.
        assert_eq!(
            dup_count("@icon(\"res://1.png\")\n@icon\n\nfunc test():\n\tpass\n"),
            0,
            "a no-argument @icon after a valid one must not be flagged as a duplicate"
        );
        // A valid `@icon` after a path-less `@icon` is NOT a duplicate: the first left `icon_path`
        // empty, so the second is the first one to actually record a path.
        assert_eq!(
            dup_count("@icon\n@icon(\"res://1.png\")\n\nfunc test():\n\tpass\n"),
            0,
            "a valid @icon after a path-less @icon must not be flagged as a duplicate"
        );
        let msgs = messages("@icon(\"\")\n@icon(\"res://1.png\")\n\nfunc test():\n\tpass\n");
        assert!(
            msgs.iter().any(|m| m == EMPTY),
            "empty @icon path must report the Godot diagnostic, got {msgs:?}"
        );
        assert!(
            !msgs.iter().any(|m| m == DUP),
            "empty @icon path must not poison duplicate detection, got {msgs:?}"
        );
    }

    #[test]
    fn static_unload_duplicate_detection_matches_godot() {
        // Godot `static_unload_annotation` (gdscript_parser.cpp:4445-4454): a second
        // `@static_unload` per script is the error below — and, unlike `@tool`/`@icon`, this check
        // is NOT behind `#ifdef DEBUG_ENABLED`. A single `@static_unload` is valid.
        const DUP: &str = r#""@static_unload" annotation can only be used once per script."#;
        let dup_count = |src: &str| {
            crate::parse(src)
                .diagnostics
                .iter()
                .filter(|d| d.message == DUP)
                .count()
        };
        assert_eq!(
            dup_count("@static_unload\n\nfunc test():\n\tpass\n"),
            0,
            "a single @static_unload must not be flagged as a duplicate"
        );
        assert_eq!(
            dup_count("@static_unload\n@static_unload\n\nfunc test():\n\tpass\n"),
            1,
            "a second @static_unload must report exactly one duplicate"
        );
    }

    #[test]
    fn warning_ignore_invalid_names_report_parser_errors() {
        const INVALID: &str = r#"Invalid warning name: "not_a_warning"."#;
        let msgs = |src: &str| {
            crate::parse(src)
                .diagnostics
                .into_iter()
                .map(|d| d.message)
                .collect::<Vec<_>>()
        };

        let line_msgs = msgs("@warning_ignore(\"not_a_warning\")\nvar x\n");
        assert!(
            line_msgs.iter().any(|m| m == INVALID),
            "@warning_ignore invalid name must report parser error, got {line_msgs:?}"
        );

        let region_msgs = msgs("@warning_ignore_restore(\"not_a_warning\")\n");
        assert!(
            region_msgs.iter().any(|m| m == INVALID),
            "@warning_ignore_restore invalid name must report parser error, got {region_msgs:?}"
        );
        assert!(
            !region_msgs
                .iter()
                .any(|m| m.contains("is not being ignored by")),
            "invalid region name must not also run pair-balance bookkeeping, got {region_msgs:?}"
        );
    }

    #[test]
    fn literal_and_identifier() {
        let (tree, root, errs) = expr("42");
        assert!(errs.is_empty());
        assert!(matches!(kind_of(&tree, root), Some(NodeKind::Literal(_))));

        let (tree, root, errs) = expr("foo");
        assert!(errs.is_empty());
        assert!(matches!(
            kind_of(&tree, root),
            Some(NodeKind::Identifier(_))
        ));
    }

    #[test]
    fn precedence_multiplication_binds_tighter_than_addition() {
        // 1 + 2 * 3  ->  (+ 1 (* 2 3))
        let (tree, root, errs) = expr("1 + 2 * 3");
        assert!(errs.is_empty(), "errors: {errs:?}");
        let NodeKind::BinaryOp(add) = kind_of(&tree, root).unwrap() else {
            panic!("root is not a binary op");
        };
        assert_eq!(add.operation, BinaryOp::Addition);
        let right = add.right_operand;
        let NodeKind::BinaryOp(mul) = kind_of(&tree, right).unwrap() else {
            panic!("rhs is not a binary op");
        };
        assert_eq!(mul.operation, BinaryOp::Multiplication);
    }

    #[test]
    fn unary_minus() {
        // `-5` at expression start lexes as a negative-number *literal* (faithful to Godot's
        // `can_precede_bin_op`); unary minus needs a non-digit operand like an identifier.
        let (tree, root, errs) = expr("-x");
        assert!(errs.is_empty(), "errors: {errs:?}");
        let NodeKind::UnaryOp(op) = kind_of(&tree, root).unwrap() else {
            panic!("not unary");
        };
        assert_eq!(op.operation, UnaryOp::Negative);

        // Sanity: the negative literal really is a literal, not a unary op.
        let (tree, root, _errs) = expr("-5");
        assert!(matches!(kind_of(&tree, root), Some(NodeKind::Literal(_))));
    }

    #[test]
    fn attribute_chain_and_call() {
        let (tree, root, errs) = expr("a.b.c");
        assert!(errs.is_empty(), "errors: {errs:?}");
        assert!(matches!(kind_of(&tree, root), Some(NodeKind::Subscript(_))));

        let (tree, root, errs) = expr("foo(1, 2)");
        assert!(errs.is_empty(), "errors: {errs:?}");
        let NodeKind::Call(call) = kind_of(&tree, root).unwrap() else {
            panic!("not a call");
        };
        assert_eq!(call.arguments.len(), 2);
        assert_eq!(call.function_name, "foo");
    }

    #[test]
    fn array_and_dictionary() {
        let (tree, root, errs) = expr("[1, 2, 3]");
        assert!(errs.is_empty(), "errors: {errs:?}");
        let NodeKind::Array(arr) = kind_of(&tree, root).unwrap() else {
            panic!("not an array");
        };
        assert_eq!(arr.elements.len(), 3);

        let (tree, root, errs) = expr("{ \"a\": 1, \"b\": 2 }");
        assert!(errs.is_empty(), "errors: {errs:?}");
        let NodeKind::Dictionary(dict) = kind_of(&tree, root).unwrap() else {
            panic!("not a dict");
        };
        assert_eq!(dict.elements.len(), 2);
        assert_eq!(dict.style, Some(DictStyle::PythonDict));
    }

    #[test]
    fn ternary_and_cast_and_type_test() {
        let (_t, root, errs) = expr("a if b else c");
        assert!(errs.is_empty(), "errors: {errs:?}");
        assert!(root.is_some());

        let (tree, root, errs) = expr("x as int");
        assert!(errs.is_empty(), "errors: {errs:?}");
        assert!(matches!(kind_of(&tree, root), Some(NodeKind::Cast(_))));

        let (tree, root, errs) = expr("x is int");
        assert!(errs.is_empty(), "errors: {errs:?}");
        assert!(matches!(kind_of(&tree, root), Some(NodeKind::TypeTest(_))));
    }

    #[test]
    fn typed_array_type_in_cast() {
        let (_t, root, errs) = expr("x as Array[int]");
        assert!(errs.is_empty(), "errors: {errs:?}");
        assert!(root.is_some());
    }

    #[test]
    fn qualified_type_does_not_absorb_following_subscript() {
        // Godot `parse_type` (cpp:3876) checks `[` *before* the attribute chain and returns
        // immediately, so the two are mutually exclusive: `A.B[int]` parses the type as `A.B` and
        // leaves `[int]` to the caller. Hence `x as A.B[int]` is `(x as A.B)[int]` — a subscript on
        // the cast, not a cast to a typed collection.
        let (tree, root, errs) = expr("x as A.B[int]");
        assert!(errs.is_empty(), "errors: {errs:?}");
        assert!(
            matches!(kind_of(&tree, root), Some(NodeKind::Subscript(_))),
            "expected trailing [int] to parse as a subscript on the cast, got {:?}",
            kind_of(&tree, root)
        );
    }

    #[test]
    fn type_attribute_chain_uses_godot_message_on_missing_name() {
        // Missing identifier after `.` in a type uses Godot's exact string (cpp:3903).
        let (_t, _root, errs) = expr("x as A.");
        assert!(
            errs.iter()
                .any(|d| d.message == "Expected inner type name after \".\"."),
            "errors: {errs:?}"
        );
    }

    #[test]
    fn multi_error_single_scan_reports_in_godot_emission_order() {
        // `0x_` makes one number scan emit two errors: it push_errors the underscore complaint onto
        // the LIFO error_stack, then directly returns the "expected hex digit" error. Godot reads
        // each message off its own token, so the directly-returned error is reported first. (A FIFO
        // cursor over the creation-ordered error list — the old bug — flipped the order.)
        let (_t, _root, errs) = expr("0x_");
        let msgs: Vec<&str> = errs.iter().map(|d| d.message.as_str()).collect();
        assert_eq!(
            msgs,
            vec![
                r#"Expected hexadecimal digit after "0x"."#,
                r#"Unexpected underscore after "0x"."#,
            ]
        );
    }

    #[test]
    fn error_recovery_returns_partial_and_records_error() {
        // Unterminated grouping: must record an error and not panic.
        let (_t, _root, errs) = expr("(1 +");
        assert!(!errs.is_empty());
    }

    #[test]
    fn never_panics_on_arbitrary_expression_input() {
        for src in [
            "",
            "(",
            "[",
            "{",
            "1 +",
            "a.",
            "foo(",
            "not",
            "- - -",
            "1 if",
            "x as",
            "$",
            "1 ** 2 ** 3",
            "{1=2}",
            "a[",
            "((((((((((",
            "@",
            "?",
            "is",
            "await",
        ] {
            let mut p = Parser::new(src);
            let _ = p.parse_expression(true, false);
        }
    }

    // --- documentSymbol projection tests ---

    #[test]
    fn document_symbols_wraps_named_script_in_root_class() {
        let src = "class_name Foo\nvar x := 1\nfunc bar() -> void:\n\tpass\n";
        let tree = crate::parse(src).tree;
        let syms = document_symbols(&tree);
        assert_eq!(syms.len(), 1, "expected a single root Class symbol");
        let root = &syms[0];
        assert_eq!(root.kind, SymbolKind::Class);
        assert_eq!(root.name, "Foo");
        // members live UNDER the root, not at top level
        let names: Vec<_> = root.children.iter().map(|c| c.name.as_str()).collect();
        assert!(names.contains(&"x") && names.contains(&"bar"));
        // selection_span points at the class_name identifier (non-zero width)
        assert_ne!(root.selection_span.start, root.selection_span.end);
    }

    #[test]
    fn document_symbols_wraps_unnamed_script_with_empty_name_and_zero_width_span() {
        let src = "var x := 1\nclass Inner:\n\tvar y := 2\n";
        let tree = crate::parse(src).tree;
        let syms = document_symbols(&tree);
        assert_eq!(syms.len(), 1);
        let root = &syms[0];
        assert_eq!(root.kind, SymbolKind::Class);
        // unnamed -> parser leaves name empty; the HANDLER (A3) fills the file basename.
        assert_eq!(root.name, "");
        // zero-width span at file start
        assert_eq!(root.selection_span, ByteSpan { start: 0, end: 0 });
        // inner class nests under the root
        assert!(root
            .children
            .iter()
            .any(|c| c.kind == SymbolKind::Class && c.name == "Inner"));
    }

    /// `Nested typed collections are not supported.` (`gdscript_parser.cpp:3904`). Godot's own
    /// corpus never covers this message, so it is pinned here rather than in the vendored tree.
    #[test]
    fn nested_typed_collections_are_rejected() {
        for src in [
            "var x: Array[Array[int]]",
            "var d: Dictionary[int, Array[int]]",
        ] {
            let errs = crate::parse(src).diagnostics;
            assert_eq!(
                errs.first().map(|d| d.message.as_str()),
                Some("Nested typed collections are not supported."),
                "{src:?} produced {errs:?}"
            );
        }
    }

    /// #372 — `self` names an instance, so a static function has none to name
    /// (gdscript_parser.cpp:2900-2902). Godot's own corpus never writes this shape, so nothing in
    /// the conformance tree pins it.
    #[test]
    fn self_inside_a_static_function_is_an_error() {
        let messages = |src: &str| -> Vec<String> {
            crate::parse(src)
                .diagnostics
                .into_iter()
                .map(|d| d.message)
                .collect()
        };
        assert_eq!(
            messages("extends Node\n\nstatic func h() -> void:\n\tprint(self)\n"),
            vec![r#"Cannot use "self" inside a static function."#.to_owned()]
        );
        // A non-static function, and a static one that never says `self`, both stay silent.
        assert!(messages("extends Node\n\nfunc h() -> void:\n\tprint(self)\n").is_empty());
        assert!(messages("extends Node\n\nstatic func h() -> int:\n\treturn 1\n").is_empty());
        // A lambda inherits its enclosing function's static-ness (cpp:3712), so it inherits the
        // restriction too.
        assert_eq!(
            messages("extends Node\n\nstatic func h() -> void:\n\tvar f := func(): return self\n\tf.call()\n"),
            vec![r#"Cannot use "self" inside a static function."#.to_owned()]
        );
    }

    /// #373 — the duplicate-enum-name error names the line the first declaration is on
    /// (gdscript_parser.cpp:1629). With several enums in a file, that suffix is what says which
    /// declaration the duplicate collides with.
    #[test]
    fn a_duplicate_enum_name_names_the_line_it_collides_with() {
        let messages = |src: &str| -> Vec<String> {
            crate::parse(src)
                .diagnostics
                .into_iter()
                .map(|d| d.message)
                .collect()
        };
        assert_eq!(
            messages("extends Node\nenum Kind { A, B, A }\n"),
            vec![r#"Name "A" was already in this enum (at line 2)."#.to_owned()]
        );
        // Across lines, the suffix points at the FIRST one, not the previous one.
        assert_eq!(
            messages("extends Node\nenum Kind {\n\tA,\n\tB,\n\tA,\n\tA,\n}\n"),
            vec![
                r#"Name "A" was already in this enum (at line 3)."#.to_owned(),
                r#"Name "A" was already in this enum (at line 3)."#.to_owned(),
            ]
        );
    }
}
