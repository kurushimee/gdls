//! The parse tree — a faithful port of the `GDScriptParser::Node` hierarchy
//! (`modules/gdscript/gdscript_parser.h`).
//!
//! Godot allocates `Node*` subclasses and links them through a `next` pointer so the parser can free
//! them all in one pass. We mirror that with a flat **arena**: every node lives in [`ParseTree::nodes`]
//! and is referenced by a [`NodeId`] index. Children, and Godot's cyclic back-references
//! (`suite -> parent_block`, `identifier -> declaration`), are all just a `NodeId` — no `Rc<RefCell>`,
//! no borrow-checker fight — and the analyzer (M3) can reach any node by id and mutate its
//! [`Node::datatype`] in place. Cleanup is `Vec::drop`.
//!
//! This is a **parser-level** AST: only fields the parser itself populates are present. Engine- and
//! analyzer-typed state (`Variant` reduced values, `MethodInfo`, `PropertyInfo`, resolution flags,
//! identifier source links, the real type lattice) is intentionally absent so `gd_syntax` keeps zero
//! engine knowledge and stays fuzzable in isolation; M3 adds it via side tables / later fields.
//!
//! Every Godot `union` keyed by an adjacent tag becomes a Rust `enum`, so the wrong arm can never be
//! read. The `NodeKind` variants are kept in Godot's declaration order (`gdscript_parser.h:299`).

use std::collections::HashMap;

use crate::span::{ByteSpan, LineColRange};
use crate::token::Literal;

/// Index of a [`Node`] within [`ParseTree::nodes`]. Godot's `Node *`. The inner index is
/// `pub(crate)` so ids can only be minted in-crate (via [`ParseTree::push`]); other crates hold them
/// opaquely and read them through [`NodeId::index`], which keeps a forged id from reaching `get`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct NodeId(pub(crate) u32);

impl NodeId {
    #[inline]
    pub fn index(self) -> usize {
        self.0 as usize
    }
}

/// Placeholder for the analyzer's type lattice (M3, ported into `gd_types`). The parser leaves every
/// node's datatype at this default; `gd_analyze` fills it in during the reduce/resolve passes.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DataType {}

// ---------------------------------------------------------------------------------------------------
// Operator enums (ported verbatim, in Godot's declaration order).
// ---------------------------------------------------------------------------------------------------

/// `AssignmentNode::Operation` (`gdscript_parser.h:425`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AssignOp {
    None,
    Addition,
    Subtraction,
    Multiplication,
    Division,
    Modulo,
    Power,
    BitShiftLeft,
    BitShiftRight,
    BitAnd,
    BitOr,
    BitXor,
}

/// `BinaryOpNode::OpType` (`gdscript_parser.h:460`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BinaryOp {
    Addition,
    Subtraction,
    Multiplication,
    Division,
    Modulo,
    Power,
    BitLeftShift,
    BitRightShift,
    BitAnd,
    BitOr,
    BitXor,
    LogicAnd,
    LogicOr,
    ContentTest,
    CompEqual,
    CompNotEqual,
    CompLess,
    CompLessEqual,
    CompGreater,
    CompGreaterEqual,
}

/// `UnaryOpNode::OpType` (`gdscript_parser.h:1234`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UnaryOp {
    Positive,
    Negative,
    Complement,
    LogicNot,
}

/// `DictionaryNode::Style` (`gdscript_parser.h:832`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DictStyle {
    LuaTable,
    PythonDict,
}

/// `VariableNode::PropertyStyle` (`gdscript_parser.h:1251`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum PropertyStyle {
    #[default]
    None,
    Inline,
    SetGet,
}

// ---------------------------------------------------------------------------------------------------
// Tagged unions (Godot `union` + adjacent tag → Rust enum).
// ---------------------------------------------------------------------------------------------------

/// `SubscriptNode`'s `index | attribute` union + `is_attribute` flag (`gdscript_parser.h:1082`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SubscriptAccess {
    /// `base[index]`.
    Index(Option<NodeId>),
    /// `base.attribute`.
    Attribute(Option<NodeId>),
}

/// A `VariableNode` accessor (`gdscript_parser.h:1258`): inline `get:`/`set:` body, or a method-name
/// pointer (`get = m`, `set = m`), or none.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum PropertyAccessor {
    #[default]
    None,
    /// Inline `FunctionNode`.
    Inline(NodeId),
    /// Method-name `IdentifierNode` pointer.
    Pointer(NodeId),
}

/// A `ClassNode::Member` (`gdscript_parser.h:563`); each variant holds the member's node id.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Member {
    Class(NodeId),
    Constant(NodeId),
    Function(NodeId),
    Signal(NodeId),
    Variable(NodeId),
    Enum(NodeId),
    /// Value of an unnamed enum.
    EnumValue(EnumValue),
    /// `@export_group`/`@export_category`/`@export_subgroup` annotation node.
    Group(NodeId),
}

/// A `SuiteNode::Local` (`gdscript_parser.h:1097`): a name bound in a block, for redefinition checks
/// and resolution. Extents come from the source node.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Local {
    pub kind: LocalKind,
    pub name: String,
    /// The declaring node (`ConstantNode`/`VariableNode`/`ParameterNode`/`IdentifierNode`).
    pub source: NodeId,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LocalKind {
    Constant,
    Variable,
    Parameter,
    ForVariable,
    PatternBind,
}

/// An `EnumNode::Value` (`gdscript_parser.h:535`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EnumValue {
    pub identifier: Option<NodeId>,
    pub custom_value: Option<NodeId>,
}

/// A `DictionaryNode`/`PatternNode` key→value pair.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct KeyValue {
    pub key: Option<NodeId>,
    pub value: Option<NodeId>,
}

// ---------------------------------------------------------------------------------------------------
// Node payloads, alphabetical within Godot's grouping. Pointers → `NodeId`; optional pointers →
// `Option<NodeId>`; `Vector<T*>` → `Vec<NodeId>`.
// ---------------------------------------------------------------------------------------------------

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AnnotationNode {
    pub name: String,
    pub arguments: Vec<NodeId>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ArrayNode {
    pub elements: Vec<NodeId>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AssertNode {
    pub condition: Option<NodeId>,
    pub message: Option<NodeId>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AssignmentNode {
    pub operation: AssignOp,
    pub assignee: Option<NodeId>,
    pub assigned_value: Option<NodeId>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AwaitNode {
    pub to_await: Option<NodeId>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BinaryOpNode {
    pub operation: BinaryOp,
    pub left_operand: Option<NodeId>,
    pub right_operand: Option<NodeId>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CallNode {
    pub callee: Option<NodeId>,
    pub arguments: Vec<NodeId>,
    pub function_name: String,
    pub is_super: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CastNode {
    pub operand: Option<NodeId>,
    pub cast_type: Option<NodeId>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ClassNode {
    pub identifier: Option<NodeId>,
    pub icon_path: Option<String>,
    pub members: Vec<Member>,
    /// Name → index into `members`; the two are maintained in lockstep — do not mutate independently.
    pub members_indices: HashMap<String, usize>,
    pub outer: Option<NodeId>,
    pub extends_used: bool,
    pub is_abstract: bool,
    pub extends_path: Option<String>,
    /// `extends A.B.C` as an identifier chain.
    pub extends: Vec<NodeId>,
}

/// `ConstantNode` — an `AssignableNode` (`gdscript_parser.h:409`, `:809`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ConstantNode {
    pub identifier: Option<NodeId>,
    pub initializer: Option<NodeId>,
    pub datatype_specifier: Option<NodeId>,
    pub infer_datatype: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DictionaryNode {
    pub elements: Vec<KeyValue>,
    pub style: Option<DictStyle>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct EnumNode {
    pub identifier: Option<NodeId>,
    pub values: Vec<EnumValue>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ForNode {
    pub variable: Option<NodeId>,
    pub datatype_specifier: Option<NodeId>,
    pub list: Option<NodeId>,
    pub loop_body: Option<NodeId>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FunctionNode {
    pub identifier: Option<NodeId>,
    pub parameters: Vec<NodeId>,
    /// Name → index into `parameters`; the two are maintained in lockstep.
    pub parameters_indices: HashMap<String, usize>,
    pub rest_parameter: Option<NodeId>,
    pub return_type: Option<NodeId>,
    pub body: Option<NodeId>,
    pub is_abstract: bool,
    pub is_static: bool,
    pub is_coroutine: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GetNodeNode {
    pub full_path: String,
    pub use_dollar: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct IdentifierNode {
    pub name: String,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct IfNode {
    pub condition: Option<NodeId>,
    pub true_block: Option<NodeId>,
    pub false_block: Option<NodeId>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LambdaNode {
    pub function: Option<NodeId>,
    pub captures: Vec<NodeId>,
    pub use_self: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct LiteralNode {
    pub value: Literal,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MatchNode {
    pub test: Option<NodeId>,
    pub branches: Vec<NodeId>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MatchBranchNode {
    pub patterns: Vec<NodeId>,
    pub block: Option<NodeId>,
    pub has_wildcard: bool,
    pub guard_body: Option<NodeId>,
}

/// `ParameterNode` — an `AssignableNode` (`gdscript_parser.h:990`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ParameterNode {
    pub identifier: Option<NodeId>,
    pub initializer: Option<NodeId>,
    pub datatype_specifier: Option<NodeId>,
    pub infer_datatype: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PatternNode {
    pub pattern_type: PatternKind,
    /// `array` sub-patterns (`PT_ARRAY`).
    pub array: Vec<NodeId>,
    /// `dictionary` key→value-pattern pairs (`PT_DICTIONARY`).
    pub dictionary: Vec<KeyValue>,
    /// Bind names declared across the whole branch, accumulated on the *root* pattern
    /// (`gdscript_parser.h:1028`): name → the binding `IdentifierNode`.
    pub binds: HashMap<String, NodeId>,
    pub rest_used: bool,
}

/// `PatternNode`'s `literal | bind | expression` union + its `Type` tag (`gdscript_parser.h:1003`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum PatternKind {
    Literal(Option<NodeId>),
    Expression(Option<NodeId>),
    Bind(Option<NodeId>),
    Array,
    Dictionary,
    Rest,
    #[default]
    Wildcard,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PreloadNode {
    pub path: Option<NodeId>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ReturnNode {
    pub return_value: Option<NodeId>,
    pub void_return: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SignalNode {
    pub identifier: Option<NodeId>,
    pub parameters: Vec<NodeId>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SubscriptNode {
    pub base: Option<NodeId>,
    pub access: Option<SubscriptAccess>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SuiteNode {
    pub parent_block: Option<NodeId>,
    pub statements: Vec<NodeId>,
    pub locals: Vec<Local>,
    /// Name → index into `locals`; the two are maintained in lockstep.
    pub locals_indices: HashMap<String, usize>,
    /// Mirrors Godot's `SuiteNode::has_return` (gdscript_parser.h:1177). Set by the parser when this
    /// suite contains a `return` statement, or when every conditional path inside it has return
    /// coverage (if/else with both arms returning, match with all branches returning and a wildcard
    /// branch present). The analyzer consults this to emit `Not all code paths return a value.`
    /// at `gdscript_analyzer.cpp:2022-2024`.
    pub has_return: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TernaryOpNode {
    pub condition: Option<NodeId>,
    pub true_expr: Option<NodeId>,
    pub false_expr: Option<NodeId>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TypeNode {
    pub type_chain: Vec<NodeId>,
    pub container_types: Vec<NodeId>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TypeTestNode {
    pub operand: Option<NodeId>,
    pub test_type: Option<NodeId>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UnaryOpNode {
    pub operation: UnaryOp,
    pub operand: Option<NodeId>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct VariableNode {
    pub identifier: Option<NodeId>,
    pub initializer: Option<NodeId>,
    pub datatype_specifier: Option<NodeId>,
    pub infer_datatype: bool,
    pub property: PropertyStyle,
    pub setter: PropertyAccessor,
    pub getter: PropertyAccessor,
    pub setter_parameter: Option<NodeId>,
    pub exported: bool,
    pub onready: bool,
    pub is_static: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct WhileNode {
    pub condition: Option<NodeId>,
    pub loop_body: Option<NodeId>,
}

// ---------------------------------------------------------------------------------------------------
// The 40-variant node tag, in Godot's declaration order (`gdscript_parser.h:299-340`).
// ---------------------------------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq)]
pub enum NodeKind {
    /// `NONE` — the empty/recovery node.
    None,
    Annotation(AnnotationNode),
    Array(ArrayNode),
    Assert(AssertNode),
    Assignment(AssignmentNode),
    Await(AwaitNode),
    BinaryOp(BinaryOpNode),
    Break,
    Breakpoint,
    Call(CallNode),
    Cast(CastNode),
    Class(ClassNode),
    Constant(ConstantNode),
    Continue,
    Dictionary(DictionaryNode),
    Enum(EnumNode),
    For(ForNode),
    Function(FunctionNode),
    GetNode(GetNodeNode),
    Identifier(IdentifierNode),
    If(IfNode),
    Lambda(LambdaNode),
    Literal(LiteralNode),
    Match(MatchNode),
    MatchBranch(MatchBranchNode),
    Parameter(ParameterNode),
    Pass,
    Pattern(PatternNode),
    Preload(PreloadNode),
    Return(ReturnNode),
    SelfExpr,
    Signal(SignalNode),
    Subscript(SubscriptNode),
    Suite(SuiteNode),
    TernaryOp(TernaryOpNode),
    Type(TypeNode),
    TypeTest(TypeTestNode),
    UnaryOp(UnaryOpNode),
    Variable(VariableNode),
    While(WhileNode),
}

impl NodeKind {
    /// Whether this node is an expression (Godot's `Node::is_expression()`).
    pub fn is_expression(&self) -> bool {
        use NodeKind::*;
        matches!(
            self,
            Array(_)
                | Assignment(_)
                | Await(_)
                | BinaryOp(_)
                | Call(_)
                | Cast(_)
                | Dictionary(_)
                | GetNode(_)
                | Identifier(_)
                | Lambda(_)
                | Literal(_)
                | Preload(_)
                | SelfExpr
                | Subscript(_)
                | TernaryOp(_)
                | TypeTest(_)
                | UnaryOp(_)
        )
    }
}

/// One node: the kind-tagged payload plus the shared extents Godot's base `Node` carries.
#[derive(Clone, Debug, PartialEq)]
pub struct Node {
    pub kind: NodeKind,
    /// Byte range into the source (for LSP ranges).
    pub span: ByteSpan,
    /// Godot-faithful tab-expanded 1-based extents (for `.out` fidelity).
    pub loc: LineColRange,
    /// Annotations attached to this node (`@onready`, `@export`, …). Empty for most nodes.
    pub annotations: Vec<NodeId>,
    /// Filled by the analyzer (M3); left default by the parser.
    pub datatype: DataType,
}

impl Node {
    pub fn new(kind: NodeKind) -> Self {
        Node {
            kind,
            span: ByteSpan::default(),
            loc: LineColRange::default(),
            annotations: Vec::new(),
            datatype: DataType::default(),
        }
    }
}

/// The owned arena of nodes for one parsed source. The root is the head [`ClassNode`]
/// (Godot's implicit top-level class).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ParseTree {
    nodes: Vec<Node>,
    /// The head class node. Meaningful only when [`ParseTree::is_empty`] is false; `pub(crate)` so
    /// external callers go through the bounds-checked [`ParseTree::root`] accessor instead of feeding
    /// the empty-tree sentinel `NodeId(0)` to the panicking [`ParseTree::get`].
    pub(crate) root: NodeId,
    /// The lexer's `line` counter at end-of-parse (the EOF token's `loc.end.line`). For sources
    /// that end with a `\n`, Godot's tokenizer increments `line` once more inside
    /// `_advance()`'s EOF check (gdscript_tokenizer.cpp:327-332: `newline(true)`) — so a 5-line
    /// `.gd` file gets `eof_line = 7` (5 newlines → line 6, plus the synthetic EOF newline → 7).
    /// Diagnostics anchored on the parser's `previous` token at end-of-parse (Godot's null-
    /// source `push_error` path at gdscript_parser.cpp:241-244) inherit this line — used by
    /// `resolve_match_pattern`'s subscript-Index arm to mirror Godot's line 7 on
    /// `match_with_subscript.gd`.
    pub eof_line: u32,
    /// `true` when the token stream was already at EOF after the leading newline/error skip —
    /// i.e. the source held no meaningful tokens (empty, whitespace-only, or comment-only).
    /// This is the exact condition Godot's `parse()` checks for the `EMPTY_FILE` warning
    /// (gdscript_parser.cpp:482-489); the warning itself is emitted by `gd_analyze`, which owns
    /// the warning set — this crate stays engine-free and only records the signal.
    pub starts_at_eof: bool,
    /// M7 (#62): `##` doc-comment associations, populated by [`crate::parse`] after the parse
    /// completes (`doc_comments::associate`). Riding on the tree means every existing
    /// interface-extraction call site gets docs with zero signature churn, and the parse cache
    /// carries them for free. Empty for sources without doc comments.
    pub docs: crate::doc_comments::DocTable,
}

impl ParseTree {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    /// Append a node and return its id.
    pub fn push(&mut self, node: Node) -> NodeId {
        let id = NodeId(self.nodes.len() as u32);
        self.nodes.push(node);
        id
    }

    /// Iterate every `NodeId` allocated in this tree, in declaration order. Used by `gd_analyze`'s
    /// whole-tree warning sweeps (name-set construction for `UNUSED_*` warnings) and any other
    /// pass that needs to visit every node without keeping a parent-side child list.
    pub fn iter_ids(&self) -> impl Iterator<Item = NodeId> + '_ {
        (0..self.nodes.len() as u32).map(NodeId)
    }

    pub fn get(&self, id: NodeId) -> &Node {
        &self.nodes[id.index()]
    }

    pub fn get_mut(&mut self, id: NodeId) -> &mut Node {
        &mut self.nodes[id.index()]
    }

    /// The root class node, or `None` if nothing was parsed.
    pub fn root(&self) -> Option<&Node> {
        self.nodes.get(self.root.index())
    }

    /// The id of the root class node, or `None` if nothing was parsed. The analyzer needs the root
    /// `NodeId` (not just the node) to seed its `NodeId`-keyed side tables and to walk members; the
    /// `root` field is `pub(crate)`, so this is the sanctioned external accessor.
    pub fn root_id(&self) -> Option<NodeId> {
        (!self.nodes.is_empty()).then_some(self.root)
    }

    /// The id of the **innermost** node whose [`Node::span`] contains `byte`, or `None` if no node
    /// covers that offset. Linear over the arena — adequate for per-keystroke LSP queries (a 3k-line
    /// file is on the order of 10k nodes); a smarter pick (e.g. binary search by span) would only
    /// matter for `hover`/`definition` on enormous files, which is well outside v1's target scale.
    ///
    /// "Innermost" = smallest span containing the byte. Ties (zero-width spans, identical extents)
    /// resolve to the **latest-emitted** node, which mirrors the parser's emission order:
    /// children are pushed after their parents into the arena, so the deepest child wins. This is
    /// the seam `gd_server`'s [hover](`textDocument/hover`) and
    /// [definition](`textDocument/definition`) handlers need to map an LSP `Position` (converted
    /// to a byte through [`crate::ByteSpan`]) onto a specific node and read its analyzer side
    /// tables.
    pub fn innermost_node_at(&self, byte: usize) -> Option<NodeId> {
        let mut best: Option<(NodeId, u32)> = None;
        for (i, n) in self.nodes.iter().enumerate() {
            if n.span.start <= byte && byte < n.span.end {
                let width = (n.span.end - n.span.start) as u32;
                match best {
                    Some((_, best_width)) if width > best_width => {}
                    _ => best = Some((NodeId(i as u32), width)),
                }
            }
        }
        best.map(|(id, _)| id)
    }

    /// The innermost [`SuiteNode`] (block) whose span contains `byte` — the
    /// [`Self::innermost_node_at`] restriction to `NodeKind::Suite`, used by M8 completion to find
    /// the block a cursor sits in so it can enumerate the locals in scope there
    /// ([`Self::locals_in_scope_at`]). Same half-open `start <= byte < end` / smallest-span /
    /// latest-emitted-on-ties convention as [`Self::innermost_node_at`]. `None` when no block
    /// contains the byte (e.g. the cursor is at class scope, or past the last node at
    /// end-of-input — completion's cursor layer probes `byte` and `byte-1` to cover that edge).
    pub fn innermost_suite_at(&self, byte: usize) -> Option<NodeId> {
        let mut best: Option<(NodeId, u32)> = None;
        for (i, n) in self.nodes.iter().enumerate() {
            if matches!(n.kind, NodeKind::Suite(_)) && n.span.start <= byte && byte < n.span.end {
                let width = (n.span.end - n.span.start) as u32;
                match best {
                    Some((_, best_width)) if width > best_width => {}
                    _ => best = Some((NodeId(i as u32), width)),
                }
            }
        }
        best.map(|(id, _)| id)
    }

    /// Enumerate every local **in scope** at `byte`: the innermost enclosing block's locals plus
    /// all locals of its `parent_block` ancestors, innermost-first. Because the parser records a
    /// function's **parameters**, `for` loop variables, and `match` pattern binds as
    /// [`SuiteNode::locals`] too (each with its [`LocalKind`]), this single `parent_block` walk
    /// yields the *full* in-scope set — locals, params, for-vars, and pattern-binds — without a
    /// separate [`FunctionNode::parameters`] pass.
    ///
    /// Two scoping rules are applied so the result is what is actually reachable at the cursor,
    /// matching Godot's lexical scoping:
    /// - **Not yet declared:** a local is included only if its declaration **ends at or before**
    ///   `byte` (`source.span.end <= byte`). A `var later = …` further down the same block — or a
    ///   variable mid-way through its own initializer (`var x = <cursor>`) — is therefore excluded,
    ///   exactly as it is unreferenceable there. Parameters and outer-block locals always satisfy
    ///   this (they textually precede the inner block).
    /// - **Shadowing:** an inner binding shadows an outer one of the same name. The walk is
    ///   innermost-first and keeps the **first** occurrence of each name, so the inner binding
    ///   wins.
    ///
    /// Returns borrowed [`Local`]s in innermost-first, declaration order. Empty when `byte` is not
    /// inside any block. Read-only and allocation-light — the M8 completion handler reconstructs
    /// scope from the AST here because the analyzer's transient scope stack is discarded after a
    /// pass.
    pub fn locals_in_scope_at(&self, byte: usize) -> Vec<&Local> {
        let mut out: Vec<&Local> = Vec::new();
        let mut seen: std::collections::HashSet<&str> = std::collections::HashSet::new();
        let mut cur = self.innermost_suite_at(byte);
        while let Some(id) = cur {
            let NodeKind::Suite(s) = &self.get(id).kind else {
                break;
            };
            for local in &s.locals {
                // Skip not-yet-declared bindings (declaration must complete before the cursor).
                if self.get(local.source).span.end > byte {
                    continue;
                }
                // Inner shadows outer: keep the first (innermost) occurrence of each name.
                if seen.insert(local.name.as_str()) {
                    out.push(local);
                }
            }
            cur = s.parent_block;
        }
        out
    }

    /// The declaration **identifier** node of a [`Local`] — the token that names it. This is the
    /// stable identity key for a binding across the LSP local-resolution / rename path, because a
    /// `Local`'s `source` varies by kind: a `Variable`/`Constant` records the whole statement node
    /// (`var x = …`), while a `ForVariable`/`PatternBind` records the identifier directly. Returns
    /// `None` only for a malformed `var`/`const` with no identifier.
    fn local_decl_ident(&self, local: &Local) -> Option<NodeId> {
        match &self.get(local.source).kind {
            NodeKind::Variable(v) => v.identifier,
            NodeKind::Constant(c) => c.identifier,
            NodeKind::Parameter(p) => p.identifier,
            // ForVariable / PatternBind store the identifier node as the source itself.
            NodeKind::Identifier(_) => Some(local.source),
            _ => None,
        }
    }

    /// Whether the identifier node `ident_id` occupies a position that SHARES a local's name but is
    /// NOT a reference to it, so a consumer of local resolution must never treat it as the local. Two
    /// such positions are excluded. An ATTRIBUTE identifier (the trailing ident of `obj.x` / `self.x`)
    /// is a member access — a different symbol. A LUA-STYLE dictionary KEY (`x` in `{ x = value }`) is
    /// folded by the analyzer to a string literal recording no binding, so it is not a reference; a
    /// Python-style key (`x` in `{ x: value }`) IS a real expression and is kept, and the
    /// single-element ambiguous case (`style == None`) is parsed Lua-style, so it is excluded too.
    ///
    /// One arena pass. This is the SINGLE source of the two exclusions both the cursor-anchor
    /// ([`Self::resolve_local_binding_at`]) and the occurrence collector
    /// ([`Self::local_binding_occurrences`]) must apply — rewriting either position under a rename is
    /// silent corruption (a member access turned dangling, or a folded key string silently changed).
    fn ident_is_non_local_position(&self, ident_id: NodeId) -> bool {
        for id in self.iter_ids() {
            match &self.get(id).kind {
                NodeKind::Subscript(s) => {
                    if let Some(SubscriptAccess::Attribute(Some(aid))) = s.access {
                        if aid == ident_id {
                            return true;
                        }
                    }
                }
                NodeKind::Dictionary(d) => {
                    if matches!(d.style, Some(DictStyle::LuaTable) | None) {
                        for kv in &d.elements {
                            if kv.key == Some(ident_id) {
                                return true;
                            }
                        }
                    }
                }
                _ => {}
            }
        }
        false
    }

    /// Resolve the identifier at `byte` (textually `name`) to the **declaration identifier** of the
    /// local binding it refers to, respecting nested `var`/`for`/`match`-bind/`param`/lambda scopes —
    /// the precise (binding-based) analog of [`Self::locals_in_scope_at`]. `None` when `byte` is not
    /// inside any block, or `name` is not a local there (a member / global / unresolved identifier).
    ///
    /// Two cases, in order:
    /// - **Declaration click:** `byte` lands on a binding's own declaration identifier (which for a
    ///   `for`/`match` bind sits textually OUTSIDE the body block that owns the `Local`, so the suite
    ///   walk below would miss it) → that binding. The nearest enclosing block to the click is used
    ///   to disambiguate same-named bindings, so clicking an inner `var x` resolves to the inner one.
    /// - **Use:** walk the block chain innermost-first (so an inner binding shadows an outer one) and
    ///   return the first in-scope binding of `name` whose declaration **completes at or before**
    ///   `byte` (`source.span.end <= byte`) — the same not-yet-declared rule as `locals_in_scope_at`,
    ///   which also makes a use inside a binding's own initializer (`var x = x`) resolve outward.
    pub fn resolve_local_binding_at(&self, byte: usize, name: &str) -> Option<NodeId> {
        // Anchor-side exclusion (#181): if the cursor lands on an identifier that merely SHARES a
        // local's name but is not a reference to it — an attribute ident (`obj.x` / `self.x`) or a
        // Lua-style dict key (`{ x = v }`) — it must NOT resolve to that local. The occurrence
        // collector below already excludes these; without the same guard here the cursor-anchor (the
        // rename firewall's local check) admits an `obj.NAME` use that collides with a `var NAME` as
        // the local and renames the WRONG symbol. Returning None lets the member/enum classifier own
        // the cursor.
        if let Some(node_id) = self.innermost_node_at(byte) {
            if matches!(self.get(node_id).kind, NodeKind::Identifier(_))
                && self.ident_is_non_local_position(node_id)
            {
                return None;
            }
        }
        // Declaration click: the cursor is on a binding's own declaration identifier. A `for`/`match`
        // bind's declaration token sits textually OUTSIDE the body block that owns its `Local`, so
        // this scans every block's locals (not just the blocks containing `byte`) for the binding
        // whose declaration identifier covers the cursor. Declaration identifiers are unique extents,
        // so at most one matches — no innermost-first disambiguation is needed for this branch.
        for id in self.iter_ids() {
            let NodeKind::Suite(s) = &self.get(id).kind else {
                continue;
            };
            for local in &s.locals {
                if local.name != name {
                    continue;
                }
                if let Some(decl) = self.local_decl_ident(local) {
                    let dspan = self.get(decl).span;
                    if dspan.start <= byte && byte < dspan.end {
                        return Some(decl);
                    }
                }
            }
        }
        // Use: the first in-scope, already-declared binding of `name`.
        let mut cur = self.innermost_suite_at(byte);
        while let Some(id) = cur {
            let NodeKind::Suite(s) = &self.get(id).kind else {
                break;
            };
            for local in &s.locals {
                if local.name != name {
                    continue;
                }
                if self.get(local.source).span.end > byte {
                    continue; // not yet declared at the cursor (incl. inside its own initializer)
                }
                return self.local_decl_ident(local);
            }
            cur = s.parent_block;
        }
        None
    }

    /// Every identifier occurrence (declaration + uses) that resolves to the binding whose
    /// declaration identifier is `decl_ident`, searched within `scope` (pass the enclosing
    /// **function** span — a `for`/`match` declaration token sits outside the body block, so a
    /// block-scoped search would drop it). The precise occurrence set for a local rename /
    /// documentHighlight: distinct same-named siblings in inner/outer blocks are excluded by
    /// per-occurrence re-resolution ([`Self::resolve_local_binding_at`]).
    ///
    /// Non-reference identifiers that share the name are excluded so a rewrite never corrupts them:
    /// attribute positions (`obj.x` / `self.x` — that `x` is a member) and Lua-style dictionary keys
    /// (`{ x = value }` — a folded string literal, not a reference to the local). The returned spans
    /// are the identifier token extents, in arena (source) order.
    ///
    /// Cost: one arena pass to scan candidates; each candidate that matches the name is then checked
    /// for a non-local position ([`Self::ident_is_non_local_position`] — an arena pass) and, if a
    /// reference, re-resolved (a suite-chain walk). Both the position check and the re-resolve run
    /// only for identifiers that already share the target name, so the quadratic factor is bounded by
    /// that name's occurrence count (small in practice), not the node count — and this runs at most
    /// once per LSP request on a single file.
    pub fn local_binding_occurrences(&self, decl_ident: NodeId, scope: ByteSpan) -> Vec<ByteSpan> {
        // Identifier nodes that LOOK like a same-named local but are NOT a reference to one are
        // excluded so renaming/highlighting the local never rewrites them (a wrong-symbol/dangling
        // edit under rename) — attribute idents (`obj.x` / `self.x`) and Lua-style dict keys
        // (`{ x = value }`). The shared [`Self::ident_is_non_local_position`] predicate is the single
        // source of those two exclusions (also applied at the cursor anchor in
        // [`Self::resolve_local_binding_at`], #181); it is queried per candidate below. (The same
        // exclusion lives in the read-only semantic-tokens local-use fallback.)
        let target_name = match &self.get(decl_ident).kind {
            NodeKind::Identifier(i) => i.name.as_str(),
            _ => return Vec::new(),
        };
        let mut out = Vec::new();
        for id in self.iter_ids() {
            let node = self.get(id);
            if node.span.start < scope.start || node.span.end > scope.end {
                continue;
            }
            let NodeKind::Identifier(i) = &node.kind else {
                continue;
            };
            if i.name != target_name {
                continue;
            }
            if self.ident_is_non_local_position(id) {
                continue;
            }
            if self.resolve_local_binding_at(node.span.start, target_name) == Some(decl_ident) {
                out.push(node.span);
            }
        }
        out
    }

    /// `true` iff the class node `class_id` declares a member named `name` usable in a TYPE-annotation
    /// position — a nested `enum`, an inner `class`, or a `const` alias (`const Hero = preload(...)`).
    /// The COMPLETE type-position member set is `{Enum, Class, Constant}` (a `var`/`func`/`signal`
    /// cannot annotate a type), so a hit here is unambiguously a TYPE named `name` declared in that
    /// scope — never an unrelated value/member that happens to share the spelling. The shared building
    /// block of [`Self::type_name_shadowed_by_enclosing_scope`].
    fn class_declares_type_named(&self, class_id: NodeId, name: &str) -> bool {
        let NodeKind::Class(class) = &self.get(class_id).kind else {
            return false;
        };
        class.members.iter().any(|m| match m {
            Member::Enum(id) => {
                matches!(&self.get(*id).kind, NodeKind::Enum(en) if en.identifier.map(|i| self.ident_text(i)) == Some(name))
            }
            Member::Class(id) => {
                matches!(&self.get(*id).kind, NodeKind::Class(c) if c.identifier.map(|i| self.ident_text(i)) == Some(name))
            }
            Member::Constant(id) => {
                matches!(&self.get(*id).kind, NodeKind::Constant(c) if c.identifier.map(|i| self.ident_text(i)) == Some(name))
            }
            _ => false,
        })
    }

    /// The text of an identifier node, or `""` if `id` is not an [`NodeKind::Identifier`]. A small
    /// borrow helper for the member-name comparisons above (`Some(self.ident_text(i)) == Some(name)`
    /// stays a cheap `&str` compare).
    fn ident_text(&self, id: NodeId) -> &str {
        match &self.get(id).kind {
            NodeKind::Identifier(i) => i.name.as_str(),
            _ => "",
        }
    }

    /// **Per-occurrence scope-aware type-name resolution.** Given a TYPE-position identifier at `byte`
    /// (textually `name` — the base segment of an `extends`/`: T` chain, e.g. `extends Foo` / `: Foo` /
    /// `: Foo.Inner` on `Foo`), decide whether `name` resolves to a TYPE declared in a SCOPE lexically
    /// enclosing `byte` (a suite local, or a member of some enclosing CLASS) rather than to a global
    /// `class_name` of the same name.
    ///
    /// Returns `true` iff some such local declaration is in scope at `byte`. Two scopes are checked —
    /// and they suppress the occurrence for TWO DISTINCT reasons (both verified against the 4.6.3
    /// binary), which a maintainer must keep separate or risk reverting one in the corrupting
    /// direction:
    ///
    ///   1. **Suite-local `NAME` — a TRUE IDENTITY/ERROR SHADOW (faithful).** Godot checks a
    ///      suite-local before the global class registry (`SuiteNode::has_local` precedes
    ///      `is_global_class` in `resolve_datatype`), so the idiomatic `const Other = preload(...)`
    ///      used as `var x: Other` resolves to the CONST, not a same-named global class — even while
    ///      the global exists. A non-const local is rejected as "cannot be used as a type" at the
    ///      same precedence point, still without falling through to the global. Its `: NAME` is NEVER
    ///      a global reference, so editing it under a global-class rename is unconditionally wrong.
    ///      Suppression here is identity/error resolution.
    ///
    ///   2. **Same-file class-scope type member (`enum`/inner `class`/`const`) — a deliberate
    ///      FORWARD-REBIND POLICY (not identity resolution).** Counter-intuitively, while the global
    ///      `class_name name` still exists Godot resolves a class-scope `: name` to the GLOBAL class
    ///      (the global registry shadows a same-file class-scope `enum`/inner-`class` in
    ///      type-annotation position). So this occurrence DOES currently bind to the global. We
    ///      suppress it anyway because a global rename `name`→`new` REMOVES the global, after which the
    ///      class-scope `: name` REBINDS to the local type and the file compiles. Leaving it as `name`
    ///      (suppressing) is the intended refactoring — it silently RETYPES the annotated variable from
    ///      the (departing) global class to the in-file local type, which is the strictly-better
    ///      outcome vs. #166's blanket whole-rename refusal; editing it to `new` would instead point it
    ///      at the renamed global and is the wrong post-rename world for a name that has a local type.
    ///      This is a POLICY applied to an occurrence that currently resolves to the global — NOT a
    ///      claim that it already means the local type.
    ///
    /// Either way the rule is the same: a type-position `name` whose name is declared in a scope
    /// enclosing it is NOT collected for the global-class rename. When no enclosing scope declares it,
    /// the occurrence is a genuine global reference → `false`, and the caller (which has already
    /// confirmed a global `class_name name` exists) collects it.
    ///
    /// This is the TYPE-position analogue of [`Self::resolve_local_binding_at`]: same scope-walk
    /// discipline (innermost-first, the first declaring scope decides), reusing its suite-chain walk
    /// for the suite-local case and adding a CLASS-scope walk (nested by span containment in the
    /// arena — children are pushed after parents, so the smallest-span `Class` node containing `byte`
    /// is the innermost enclosing class). The walk is purely lexical/structural (no analyzer pass): a
    /// type-position class reference carries no `Binding::Use`, so this positional resolution is what a
    /// mutating consumer must use to tell an enclosing-scope `: Foo` from a top-level global `: Foo`
    /// per occurrence, instead of refusing the whole rename when both coexist in reach.
    ///
    /// KNOWN GAP (over-collect, never over-suppress): a `const NAME` INHERITED from a base class
    /// (`extends`-chain) is in scope in Godot but is invisible to this lexical/span walk, so such a
    /// `: NAME` would be (wrongly) collected. Narrow (base-class const alias + same-named global +
    /// a `: NAME` in the derived class); tracked in gdls#188. The suite-local and same-file
    /// class-scope cases — the ones that arise in the #167 collision matrix — are covered.
    pub fn type_name_shadowed_by_enclosing_scope(&self, byte: usize, name: &str) -> bool {
        // (1) Suite-local `NAME` in the enclosing suite chain. Godot's analyzer checks a
        // suite-local before the global class registry (`SuiteNode::has_local` precedes
        // `is_global_class`), so a `const Other = preload(...)` used as `: Other` resolves to the
        // const — editing it under a global-`class_name Other` rename is corruption. Unlike normal
        // expression-local lookup, Godot's `resolve_datatype` uses `SuiteNode::has_local` before the
        // global registry, so a same-named local declared later in the suite still shadows here. A
        // CONSTANT may be a valid type alias; any other local kind still shadows the global but
        // produces Godot's "Local ... cannot be used as a type." Either way, the type annotation is
        // not a global-class reference and must be suppressed under a global rename.
        let mut cur = self.innermost_suite_at(byte);
        while let Some(id) = cur {
            let NodeKind::Suite(s) = &self.get(id).kind else {
                break;
            };
            for local in &s.locals {
                if local.name == name {
                    return true;
                }
            }
            cur = s.parent_block;
        }
        // (2) A type member named `name` declared in any CLASS scope enclosing `byte`. Collect every
        // class scope whose span contains `byte`; the root class contains the whole file, so it is
        // always included when present. A type member declared in ANY enclosing scope means this
        // occurrence resolves locally after the global is renamed away (see the doc above), so it is
        // suppressed. (The boolean decision is "is it declared in any enclosing scope at all", so the
        // innermost-first ordering only documents the resolution semantics — one declaring scope
        // anywhere in the chain returns `true`.)
        let mut enclosing: Vec<(NodeId, u32)> = Vec::new();
        for id in self.iter_ids() {
            let node = self.get(id);
            if !matches!(node.kind, NodeKind::Class(_)) {
                continue;
            }
            if node.span.start <= byte && byte < node.span.end {
                enclosing.push((id, (node.span.end - node.span.start) as u32));
            }
        }
        enclosing.sort_by_key(|&(_, width)| width);
        enclosing
            .iter()
            .any(|&(class_id, _)| self.class_declares_type_named(class_id, name))
    }
}

impl std::ops::Index<NodeId> for ParseTree {
    type Output = Node;
    fn index(&self, id: NodeId) -> &Node {
        self.get(id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn innermost_node_picks_smallest_containing_span() {
        // Hand-built tree: an outer node spanning bytes 0..10, an inner identifier spanning 3..7.
        // A byte inside the inner identifier must resolve to the inner node, not the outer one.
        let mut tree = ParseTree::new();
        let outer = tree.push(Node {
            kind: NodeKind::None,
            span: ByteSpan { start: 0, end: 10 },
            loc: LineColRange::default(),
            annotations: Vec::new(),
            datatype: DataType::default(),
        });
        let inner = tree.push(Node {
            kind: NodeKind::Identifier(IdentifierNode {
                name: "x".to_string(),
            }),
            span: ByteSpan { start: 3, end: 7 },
            loc: LineColRange::default(),
            annotations: Vec::new(),
            datatype: DataType::default(),
        });

        assert_eq!(tree.innermost_node_at(0), Some(outer));
        assert_eq!(tree.innermost_node_at(5), Some(inner));
        assert_eq!(tree.innermost_node_at(6), Some(inner));
        // The end byte is exclusive: byte 7 is in the outer, not the inner.
        assert_eq!(tree.innermost_node_at(7), Some(outer));
        // Past the outer's end ⇒ no hit.
        assert_eq!(tree.innermost_node_at(10), None);
        assert_eq!(tree.innermost_node_at(11), None);
    }

    #[test]
    fn arena_push_and_index() {
        let mut tree = ParseTree::new();
        assert!(tree.is_empty());
        let id = tree.push(Node::new(NodeKind::Identifier(IdentifierNode {
            name: "x".to_string(),
        })));
        assert_eq!(id, NodeId(0));
        assert_eq!(tree.len(), 1);
        match &tree[id].kind {
            NodeKind::Identifier(ident) => assert_eq!(ident.name, "x"),
            _ => panic!("wrong kind"),
        }
        assert!(tree[id].kind.is_expression());
    }

    #[test]
    fn none_is_not_an_expression() {
        assert!(!NodeKind::None.is_expression());
        assert!(!NodeKind::Pass.is_expression());
    }

    // ===============================================================================
    // WP-R3 regression: ParseTree.eof_line follows the lexer's end-of-parse `line`
    // counter (including the synthetic EOF `newline(true)` bump for sources ending
    // in `\n`). Pinning this so a future refactor that drops the bump or stops
    // populating `eof_line` from `current.loc.end.line` fails CI rather than the
    // `match_with_subscript.gd` corpus fixture.
    // ===============================================================================

    #[test]
    fn eof_line_is_populated_by_parse() {
        // An empty source: the lexer's `line` starts at 1; the EOF newline bump takes it to 2.
        let tree = crate::parse("").tree;
        assert!(
            tree.eof_line >= 1,
            "eof_line must be set to >= 1, got {}",
            tree.eof_line
        );
    }

    #[test]
    fn eof_line_advances_past_newlines() {
        // A 3-line source must end with eof_line strictly greater than a 1-line source's.
        let one_line = crate::parse("var x = 0\n").tree;
        let three_line = crate::parse("var x = 0\nvar y = 1\nvar z = 2\n").tree;
        assert!(
            three_line.eof_line > one_line.eof_line,
            "longer source ⇒ larger eof_line; got one={} three={}",
            one_line.eof_line,
            three_line.eof_line
        );
    }

    #[test]
    fn eof_line_survives_trailing_newline_bump() {
        // Godot's tokenizer (gdscript_tokenizer.cpp:327-332, `newline(true)`) increments
        // `line` one more time at EOF for sources that end with `\n`. So a source ending in `\n`
        // has eof_line one HIGHER than the same source without the final newline.
        // Pinning this guards the `match_with_subscript.gd` post-EOF synthetic-line invariant.
        let with_nl = crate::parse("var x = 0\n").tree;
        let no_nl = crate::parse("var x = 0").tree;
        assert!(
            with_nl.eof_line >= no_nl.eof_line,
            "trailing-newline source has eof_line >= no-newline source; got with={} no={}",
            with_nl.eof_line,
            no_nl.eof_line
        );
    }

    /// Byte offset of the type-annotation `name` in `var <v>: <name>` (the first occurrence of
    /// `: name` after a `var`), used by the scope-resolution tests below.
    fn type_anno_byte(src: &str, decl: &str, name: &str) -> usize {
        let needle = format!("{decl}: {name}");
        src.find(&needle).expect("decl not found") + decl.len() + 2
    }

    #[test]
    fn type_name_class_scope_shadow_is_per_occurrence() {
        // An inner `enum Foo` makes the inner `: Foo` resolve locally (shadowed=true → suppress under a
        // global-class rename); a TOP-LEVEL `extends Foo` in the same file is NOT enclosed by `Inner`
        // and resolves to the global (shadowed=false → collect). This is the per-occurrence precision
        // the file-level guard could not express.
        let src = "extends Foo\n\nclass Inner:\n\tenum Foo { A }\n\tvar y: Foo = Foo.A\n";
        let tree = crate::parse(src).tree;
        // `extends Foo`: the `Foo` after "extends ".
        let extends_byte = src.find("extends Foo").unwrap() + "extends ".len();
        assert!(
            !tree.type_name_shadowed_by_enclosing_scope(extends_byte, "Foo"),
            "the top-level `extends Foo` is NOT inside `Inner`, so it is the GLOBAL class (unshadowed)"
        );
        // inner `var y: Foo`.
        let inner_anno = type_anno_byte(src, "var y", "Foo");
        assert!(
            tree.type_name_shadowed_by_enclosing_scope(inner_anno, "Foo"),
            "the inner `: Foo` is enclosed by `class Inner` (which declares `enum Foo`), so it is the \
             LOCAL type (shadowed)"
        );
    }

    #[test]
    fn type_name_func_local_const_alias_shadows_global() {
        // A function-local `const Foo = preload(...)` shadows a same-named global `class_name` in
        // type-annotation position (Godot checks suite locals before the global registry), so a
        // `var x: Foo` in that body must NOT be collected for a global-class rename. The class-scope
        // walk alone (NodeKind::Class only) would miss this — the suite-local branch covers it.
        let src = "extends Node\n\nfunc f() -> void:\n\tconst Foo = preload(\"res://other.gd\")\n\tvar x: Foo = null\n";
        let tree = crate::parse(src).tree;
        let anno = type_anno_byte(src, "var x", "Foo");
        assert!(
            tree.type_name_shadowed_by_enclosing_scope(anno, "Foo"),
            "the func-local `const Foo` shadows the global in `var x: Foo`; it must resolve LOCAL"
        );
    }

    #[test]
    fn type_name_func_local_const_declared_later_still_shadows_global() {
        // `resolve_datatype` checks `SuiteNode::has_local` before the global class registry without
        // declaration-order filtering. A later `const Foo` therefore still shadows `var x: Foo`
        // (Godot reports that the local constant is not resolved yet) and must be suppressed under a
        // global-class rename.
        let src = "extends Node\n\nfunc f() -> void:\n\tvar x: Foo = null\n\tconst Foo = preload(\"res://other.gd\")\n";
        let tree = crate::parse(src).tree;
        let anno = type_anno_byte(src, "var x", "Foo");
        assert!(
            tree.type_name_shadowed_by_enclosing_scope(anno, "Foo"),
            "a later func-local `const Foo` still shadows `var x: Foo` in Godot type resolution"
        );
    }

    #[test]
    fn type_name_unshadowed_resolves_global() {
        // No enclosing scope declares `Foo` → an `extends Foo` / `: Foo` is a genuine global reference
        // (shadowed=false → collect under a global-class rename).
        let src = "extends Foo\n\nfunc f() -> void:\n\tvar x: Foo = null\n";
        let tree = crate::parse(src).tree;
        let extends_byte = src.find("extends Foo").unwrap() + "extends ".len();
        let anno = type_anno_byte(src, "var x", "Foo");
        assert!(
            !tree.type_name_shadowed_by_enclosing_scope(extends_byte, "Foo"),
            "`extends Foo` with no local `Foo` is the global class"
        );
        assert!(
            !tree.type_name_shadowed_by_enclosing_scope(anno, "Foo"),
            "`var x: Foo` with no enclosing `Foo` declaration is the global class"
        );
    }

    #[test]
    fn type_name_func_local_non_const_shadows_global_as_error() {
        // A func-local `var Foo` (runtime variable, not a type) still shadows a same-named global in
        // Godot type resolution; the analyzer reports `Local variable "Foo" cannot be used as a
        // type.` instead of falling through to the global registry. A global-class rename must
        // suppress this already-erroring `: Foo`, not silently retarget it to the renamed global.
        let src = "extends Node\n\nfunc f() -> void:\n\tvar Foo = 1\n\tvar x: Foo = null\n";
        let tree = crate::parse(src).tree;
        let anno = type_anno_byte(src, "var x", "Foo");
        assert!(
            tree.type_name_shadowed_by_enclosing_scope(anno, "Foo"),
            "a func-local `var Foo` is an erroring local type shadow, not a global class reference"
        );
    }
}
