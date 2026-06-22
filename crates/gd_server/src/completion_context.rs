//! Cursor → completion-context classifier (M8, issue #64 — Phase 2 spine).
//!
//! Given a parsed tree, the source's token stream, and a mid-edit byte offset, [`classify`]
//! determines **which** completion context applies and extracts the load-bearing payload (the base
//! expression for member access, the callee + argument index for a call site, the typed prefix).
//! No rendering happens here and no LSP handler is wired in this phase — this is the pure,
//! exhaustively-tested engine that Phase 3 (handler), Phase 4 (call-arg completion), and Phase 5
//! (`signatureHelp`) all consume.
//!
//! # Why two layers (token-frame primary, AST secondary)
//!
//! gdls always returns a partial AST — it does **not** port Godot's separate `for_completion` parse
//! mode (`parser.rs:18-20`), and #65 forbids a raw-text backward bracket scan. An empirical probe
//! over every incomplete mid-edit form established that the AST survives for only about half the
//! contexts: trailing whitespace collapses `speed = ` to a bare `Suite`, `print(` loses its `Call`
//! node, top-level `local.` recovers as a `Class`, and an empty call argument vanishes from the
//! argument array. So classification is organized around the **token stream**:
//!
//! - The **anchor token** = the last token whose `span.end <= byte` (the just-typed character).
//!   Because spans are half-open, `innermost_node_at(byte)` is `None` at end-of-input, so the AST
//!   probe (when used) is taken at `byte` *and* `byte - 1`.
//! - The **nearest enclosing unclosed bracket** is found by a bracket-depth-aware backward scan over
//!   tokens. Since the lexer already makes each string literal a single token, a `)`/`,` inside a
//!   string never confuses the scan (#65).
//!
//! The AST is consulted only to recover a base `NodeId` (member access) or to confirm a type
//! position where the node survived. The two parent edges needed (an `Identifier`'s enclosing
//! `Subscript` or `Type`) are found with [`smallest_node_strictly_containing`] rather than a full
//! child→parent map, since only two lookups need it.
//!
//! # Precedence (decided up front; order is load-bearing)
//!
//! 1. **Deferred** (`$`/`%`/`get_node` node paths, `load`/`preload` file paths) — M11; checked first
//!    so they are never misread as a member/identifier.
//! 2. **Member access** — anchor is `.` (or an identifier immediately preceded by `.`). Wins even
//!    inside a call: `print(foo.` is `Attribute`, not a call argument. `super.` ⇒ `SuperMethod`.
//! 3. **Punctuation-anchored** — `[` (subscript), `=`/`+=`/… (assign), `@…` (annotation), an
//!    identifier in a type position (`var x: Vec`, `-> `, `Array[`), `extends <name>`, or
//!    `func <name>` at class-body statement start (override).
//! 4. **Call / identifier** — otherwise consult the enclosing unclosed `(`: a **call** paren (the
//!    token before it is a callee, not a `func` declaration, not an annotation, not grouping) ⇒
//!    `CallArguments` (arg index = depth-0 commas from the `(` to the cursor); else a bare/partial
//!    identifier ⇒ `Identifier`, or `None`.

use gd_syntax::ast::{NodeId, NodeKind, ParseTree, SubscriptAccess};
use gd_syntax::token::{Token, TokenKind};
use gd_syntax::ByteSpan;

/// The completion context at a cursor: which kind applies plus the extracted payload. Covers the
/// full M8 taxonomy (classification only — later phases render). Some kinds are deliberately
/// *coarse*: `Attribute` covers both instance member access and builtin-type static access (the
/// instance-vs-static split needs base-type resolution, a render-time concern), and `Identifier`
/// covers the bare-call `METHOD` case. See the module docs and the per-variant notes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CompletionContext {
    pub kind: CompletionKind,
    /// The identifier already typed at the cursor that the completion should filter by, if any
    /// (e.g. the `x` in `local.x`, the `spe` in `print(spe`). Byte span into the source; empty when
    /// the cursor sits at a position with no partial word (a trailing `.`, `(`, `,`, `=`).
    pub prefix: Option<ByteSpan>,
}

/// The taxonomy of completion contexts gdls recognizes (the in-scope subset of Godot's
/// `complete_code` — scene/file-path contexts are [`CompletionKind::Deferred`], handled in M11).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CompletionKind {
    /// Bare identifier position: locals/params, class members, utilities, types, keywords, … The
    /// `METHOD` (bare-name call) context folds in here — it is rendered as identifiers restricted
    /// to callables, a render-time distinction.
    Identifier,

    /// Member access on a base expression: `base.` or `base.partial`. `base` is the base
    /// expression's node id when the AST preserved it (`Option` because top-level `local.` recovers
    /// as a `Class` with no `Subscript` — the base name is then read from the token stream by the
    /// renderer). Covers both instance members and builtin-type static members (`Color.`); the
    /// instance-vs-static split is a render-time decision needing base-type resolution.
    Attribute { base: Option<NodeId> },

    /// Inside a call's argument list: `callee(`, `callee(arg, `, `callee(p`. `callee_name` is the
    /// callee identifier's text when the token before the `(` is a simple name (the common case);
    /// `callee` is its node id when the AST preserved the call. `arg_index` is the **0-based** index
    /// of the argument the cursor sits in, derived by counting depth-0 commas between the `(` and
    /// the cursor — *not* the AST argument-array length, so an empty `max(1, , 2)` slot reports 1.
    CallArguments {
        callee: Option<NodeId>,
        callee_name: Option<String>,
        arg_index: usize,
    },

    /// Annotation name: `@expo`, or the bare `@` (the lexer emits `@` as an empty-name `Annotation`
    /// token). The prefix carries the typed text after the `@`.
    Annotation,

    /// Annotation argument list: `@export_range(`, `@export(p`. Distinct from a call so the renderer
    /// can offer annotation-specific argument hints. Same `arg_index` semantics as
    /// [`CompletionKind::CallArguments`].
    AnnotationArguments {
        annotation_name: Option<String>,
        arg_index: usize,
    },

    /// Type position in a variable/constant/parameter hint or container element: `var t: Vec`,
    /// `Array[`, a cast `x as T`. Identifier names of types are wanted here.
    TypeName,

    /// Inline type-or-accessor position of a **class-body** `var` declaration: `var x: <cursor>`
    /// (no trailing accessor newline) — Godot's `COMPLETION_PROPERTY_DECLARATION_OR_TYPE`
    /// (`gdscript_parser.cpp:1241`), which offers the available types **plus** the `get`/`set`
    /// accessor keywords (`gdscript_editor.cpp:3535`). Distinct from [`CompletionKind::TypeName`]
    /// (the same slot in a function-local `var`, a `const`, a parameter, a cast `x as T`, or an
    /// `Array[` element — all type-only, since `parse_property` is unreachable there).
    PropertyDeclarationOrType,

    /// Member access **on a type** in type position: `var x: Foo.` — the nested types, enums, and
    /// constants of `Foo` are wanted, NOT its instance members. Distinct from
    /// [`CompletionKind::Attribute`] (which is instance member access) so the renderer offers the
    /// type-scoped set. `base` is the base type's node id when the AST preserved it.
    TypeAttribute { base: Option<NodeId> },

    /// The class an `extends` clause names: `extends Nod`.
    InheritType,

    /// The return-type position after `-> `: `func f() -> `.
    TypeNameOrVoid,

    /// Index subscript: `base[` or `base[partial`. Distinct from member access; the base is offered
    /// keys/indices semantics at render time.
    Subscript,

    /// Right-hand side of an assignment: `speed = `, `x += `. Identifiers (or enum members when the
    /// assignee is enum-typed — a render-time refinement) are wanted.
    Assign,

    /// Method access through `super`: `super.`, `super.partial`. Restricted to the parent class's
    /// methods at render time.
    SuperMethod,

    /// A `func <name>` at class-body statement start — completing a virtual-method override stub.
    OverrideMethod,

    /// The method-name side of a property accessor: `var x: int:\n\tget = |` / `set = |`. The class's
    /// methods are wanted (the accessor binds a getter/setter by name), NOT an arbitrary expression —
    /// so this is distinct from [`CompletionKind::Assign`].
    PropertyMethod,

    /// The bare accessor-keyword position of an inline property block: `var x: int:\n\t|` /
    /// `var x: int:\n\tg|` — Godot's `COMPLETION_PROPERTY_DECLARATION` (`gdscript_parser.cpp:1288`),
    /// which offers exactly the plain-text keywords `get`/`set` (`gdscript_editor.cpp:3543`). Distinct
    /// from [`CompletionKind::PropertyMethod`] (the `get = <method>` binding side) and from a generic
    /// [`CompletionKind::Identifier`] in an ordinary class/function body.
    PropertyAccessor,

    /// A context gdls deliberately does not serve in v1: `$`/`%`/`get_node(...)` node paths and
    /// `load`/`preload` file paths (the scene/resource index lands in M11). Carried explicitly so a
    /// handler returns an empty list here instead of misclassifying it as a member or identifier.
    Deferred(DeferredReason),

    /// No completion context applies at this offset (e.g. inside whitespace at class scope with no
    /// preceding trigger, or a position the engine cannot resolve). A handler returns an empty list.
    None,
}

/// Which deferred (M11) context was detected, for diagnostics/telemetry and so a handler can branch
/// later without re-deriving it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeferredReason {
    /// `$path` / `get_node("…")` — a scene node path.
    NodePath,
    /// `%UniqueName` — a scene unique-node path.
    UniqueNodePath,
    /// `load("…")` / `preload("…")` — a resource file path.
    ResourcePath,
}

impl CompletionContext {
    fn new(kind: CompletionKind, prefix: Option<ByteSpan>) -> Self {
        Self { kind, prefix }
    }

    fn bare(kind: CompletionKind) -> Self {
        Self { kind, prefix: None }
    }
}

/// Classify the completion context at byte offset `byte`. Pure and panic-free for any
/// `(tree, tokens, byte)` — out-of-range offsets and partial/mid-edit token streams all resolve to
/// a well-defined variant (worst case [`CompletionKind::None`]). `tokens` must be the standalone
/// [`gd_syntax::tokenize`] output for the same source the tree was parsed from.
#[must_use]
pub fn classify(tree: &ParseTree, tokens: &[Token], byte: usize) -> CompletionContext {
    // The token index of the anchor = the last token that ends at or before the cursor. Whitespace
    // is not a token, so for `speed = <space>` the anchor is `=`, regardless of trailing spaces.
    let anchor = anchor_index(tokens, byte);

    // (1) Deferred contexts first — never let a `$`/`%`/path read as a member/identifier.
    if let Some(ctx) = classify_deferred(tokens, anchor, byte) {
        return ctx;
    }

    // (2) Member access — `.`-anchored. Wins over an enclosing call (`print(foo.` is Attribute).
    if let Some(ctx) = classify_member(tree, tokens, anchor, byte) {
        return ctx;
    }

    // (3) Punctuation- / keyword-anchored single-token contexts.
    if let Some(ctx) = classify_anchored(tree, tokens, anchor, byte) {
        return ctx;
    }

    // (4) Enclosing call / annotation arguments, else bare identifier.
    classify_call_or_identifier(tree, tokens, anchor, byte)
}

// ---------------------------------------------------------------------------------------------------
// Token-frame primitives.
// ---------------------------------------------------------------------------------------------------

/// Tokens the bracket-depth scan and anchor logic treat as "not really there": layout tokens the
/// standalone lexer emits (it runs `multiline_mode = false`, so newlines/indents appear inside
/// brackets) plus `Eof`. They never change bracket depth and are skipped when looking for a
/// meaningful neighbor.
fn is_layout(kind: TokenKind) -> bool {
    matches!(
        kind,
        TokenKind::Newline | TokenKind::Indent | TokenKind::Dedent | TokenKind::Eof
    )
}

/// Tokens skipped when picking the anchor or a meaningful neighbor: the layout tokens **plus**
/// `Error`. The standalone lexer emits an `Error` token as a *diagnostic marker* — often
/// co-located with a real token (a bare `@` lexes as `[Annotation, Error]` sharing the same span).
/// It is never the token the cursor is anchored to, so letting it steal the anchor would misread
/// `@<cursor>` (the `Error` wins the `span.end <= byte` tie) as a no-context position. Skipping it
/// here keeps the *real* token (the `Annotation`, the identifier, …) as the anchor; `is_layout`
/// stays unchanged for the bracket-depth / line-start logic that legitimately ignores only layout.
fn is_anchor_skippable(kind: TokenKind) -> bool {
    is_layout(kind) || kind == TokenKind::Error
}

/// Index of the anchor token: the last token whose `span.end <= byte`, skipping layout / error
/// tokens. `None` when no such token exists (cursor before the first real token).
fn anchor_index(tokens: &[Token], byte: usize) -> Option<usize> {
    tokens
        .iter()
        .enumerate()
        .rev()
        .find(|(_, t)| !is_anchor_skippable(t.kind) && t.span.end <= byte)
        .map(|(i, _)| i)
}

/// The non-layout token immediately before index `i` (skipping layout / error tokens), if any.
fn prev_meaningful(tokens: &[Token], i: usize) -> Option<usize> {
    tokens[..i]
        .iter()
        .enumerate()
        .rev()
        .find(|(_, t)| !is_anchor_skippable(t.kind))
        .map(|(j, _)| j)
}

/// Whether the cursor is sitting in a partial identifier: the anchor token is an identifier-like
/// token whose span actually reaches the cursor (`span.end == byte`, i.e. the cursor is glued to its
/// end). Returns that token's span as the completion prefix.
fn prefix_at(tokens: &[Token], anchor: Option<usize>, byte: usize) -> Option<ByteSpan> {
    let i = anchor?;
    let t = &tokens[i];
    if t.span.end == byte && is_word_token(t.kind) {
        Some(t.span)
    } else {
        None
    }
}

/// Whether a token kind is a "word" the user could be mid-typing as an identifier/keyword. Used to
/// recover a completion prefix. Keywords count because a half-typed keyword lexes as a keyword
/// token (e.g. `re` is `Identifier`, but `ret` could lex toward `return`).
fn is_word_token(kind: TokenKind) -> bool {
    // Identifiers, contextual keywords, and the named constants are all valid prefixes.
    kind.is_identifier() || is_keyword_token(kind)
}

/// Whether a token is a reserved keyword (anything in the keyword span of the token table). Used to
/// allow a half-typed keyword as a prefix and to recognize keyword-anchored contexts.
fn is_keyword_token(kind: TokenKind) -> bool {
    use TokenKind::*;
    matches!(
        kind,
        If | Elif
            | Else
            | For
            | While
            | Break
            | Continue
            | Pass
            | Return
            | Match
            | When
            | As
            | Assert
            | Await
            | Breakpoint
            | Class
            | ClassName
            | Const
            | Enum
            | Extends
            | Func
            | In
            | Is
            | Namespace
            | Preload
            | SelfKw
            | Signal
            | Static
            | Super
            | Trait
            | Var
            | Void
            | Yield
    )
}

/// Find the nearest enclosing **unclosed** opening bracket of any kind by scanning tokens backward
/// from the anchor, tracking depth so already-closed pairs are skipped. Returns the token index of
/// the unclosed opener and which bracket it is, or `None` if the cursor is not inside any bracket.
/// String literals are single tokens, so brackets inside strings are invisible to this scan (#65).
///
/// `pub(crate)` so `signatureHelp` (M8 #65) reuses the exact same call-site scan as call-argument
/// completion — "which call am I in" is this primitive, not a re-implemented bracket scan.
pub(crate) fn enclosing_open_bracket(
    tokens: &[Token],
    anchor: Option<usize>,
) -> Option<(usize, TokenKind)> {
    use TokenKind::*;
    let start = anchor?;
    // Independent depth counters per bracket family so `([)` mismatches don't cross-cancel.
    let mut paren = 0i32;
    let mut bracket = 0i32;
    let mut brace = 0i32;
    // Inclusive of the anchor: `foo(` has the `(` as the anchor and it is the enclosing opener.
    let mut i = start as isize;
    while i >= 0 {
        match tokens[i as usize].kind {
            ParenthesisClose => paren += 1,
            ParenthesisOpen => {
                if paren == 0 {
                    return Some((i as usize, ParenthesisOpen));
                }
                paren -= 1;
            }
            BracketClose => bracket += 1,
            BracketOpen => {
                if bracket == 0 {
                    return Some((i as usize, BracketOpen));
                }
                bracket -= 1;
            }
            BraceClose => brace += 1,
            BraceOpen => {
                if brace == 0 {
                    return Some((i as usize, BraceOpen));
                }
                brace -= 1;
            }
            _ => {}
        }
        i -= 1;
    }
    None
}

/// Count depth-0 commas between the token at `open_idx` (an opening bracket, exclusive) and the
/// cursor — the argument index. Nested brackets and any commas inside them are skipped; layout
/// tokens are ignored; string literals are single tokens so in-string commas never count (#65).
///
/// `pub(crate)` so `signatureHelp` (M8 #65) derives `activeParameter` from the same comma count as
/// call-argument completion.
pub(crate) fn arg_index_after(tokens: &[Token], open_idx: usize, byte: usize) -> usize {
    use TokenKind::*;
    let mut commas = 0usize;
    let mut depth = 0i32;
    for t in &tokens[open_idx + 1..] {
        if t.span.start >= byte {
            break;
        }
        match t.kind {
            ParenthesisOpen | BracketOpen | BraceOpen => depth += 1,
            ParenthesisClose | BracketClose | BraceClose => depth -= 1,
            Comma if depth == 0 => commas += 1,
            _ => {}
        }
    }
    commas
}

/// The smallest node whose span **strictly** contains `inner`'s span (a proper superset), i.e. the
/// nearest enclosing parent of a node — used for the two parent edges classification needs
/// (`Identifier` → enclosing `Subscript`, `Identifier` → enclosing `Type`) without building a full
/// child→parent map. Ties on identical width are broken toward the latest-emitted node, mirroring
/// `innermost_node_at`'s convention (a child is pushed after its parent, so this picks the closest
/// ancestor). `None` when nothing strictly contains it.
///
/// M9 (#70): `pub(crate)` so `handlers::selection_range` reuses this exact "nearest strictly-
/// enclosing ancestor" step to build its `SelectionRange` ancestor chain (repeated calls walk
/// innermost → root). Because it excludes equal-span nodes, the chain is strictly increasing — no
/// duplicate or looping range.
pub(crate) fn smallest_node_strictly_containing(tree: &ParseTree, inner: NodeId) -> Option<NodeId> {
    let target = tree.get(inner).span;
    let mut best: Option<(NodeId, usize)> = None;
    for id in tree.iter_ids() {
        if id == inner {
            continue;
        }
        let s = tree.get(id).span;
        let strictly_contains = s.start <= target.start
            && target.end <= s.end
            && (s.start, s.end) != (target.start, target.end);
        if !strictly_contains {
            continue;
        }
        let width = s.end - s.start;
        match best {
            Some((_, bw)) if width > bw => {}
            _ => best = Some((id, width)),
        }
    }
    best.map(|(id, _)| id)
}

// ---------------------------------------------------------------------------------------------------
// Layer dispatch.
// ---------------------------------------------------------------------------------------------------

/// (1) `$`/`%` node paths and `load`/`preload` file paths — out of scope until M11.
fn classify_deferred(
    tokens: &[Token],
    anchor: Option<usize>,
    byte: usize,
) -> Option<CompletionContext> {
    use TokenKind::*;
    let i = anchor?;

    // Walk backward over the contiguous node-path segment (`$A/B/C…`). A node path is a run of
    // names/slashes/`%`/keywords-as-node-names rooted at a `$` or a `%`. If we hit such a root
    // before any token that cannot be part of a node path, the cursor is in a node path.
    let mut j = i as isize;
    while j >= 0 {
        let k = tokens[j as usize].kind;
        match k {
            Dollar => {
                let prefix = bare_node_path_prefix(tokens, anchor, byte, DeferredReason::NodePath);
                return Some(CompletionContext::new(
                    CompletionKind::Deferred(DeferredReason::NodePath),
                    prefix,
                ));
            }
            Percent => {
                // `%` is a unique-node sigil only in prefix/expression-boundary position. When the
                // token before it produces a value (`x % yy`), the `%` is infix modulo, not a node
                // path — fall through to the normal identifier/expression context rather than
                // firing an empty `%unique` completion. (`%=` is a distinct token, never reached
                // here.) `Token::can_precede_bin_op` is exactly Godot's value-producing test.
                let is_modulo = prev_meaningful(tokens, j as usize)
                    .is_some_and(|p| tokens[p].kind.can_precede_bin_op());
                if is_modulo {
                    break;
                }
                let prefix =
                    bare_node_path_prefix(tokens, anchor, byte, DeferredReason::UniqueNodePath);
                return Some(CompletionContext::new(
                    CompletionKind::Deferred(DeferredReason::UniqueNodePath),
                    prefix,
                ));
            }
            // Tokens that can appear inside a `$`/`%` path: a name segment, a `/` separator, or a
            // quoted segment (`$"a/b"`). Keep scanning left.
            Slash | Literal => {}
            _ if k.is_node_name() => {}
            // Anything else ends the potential path without a `$`/`%` root.
            _ => break,
        }
        j -= 1;
    }

    // String-argument deferred contexts: the cursor is inside a string literal that is the first
    // argument of a resource-loader (`load`/`preload`) or a node-path call (`get_node`/
    // `get_node_or_null`/`NodePath`). Detect the enclosing `(`, check the callee, and require the
    // cursor to sit inside the *first* argument's string literal.
    if let Some((open_idx, ParenthesisOpen)) = enclosing_open_bracket(tokens, anchor) {
        if let Some(callee_idx) = prev_meaningful(tokens, open_idx) {
            let callee = &tokens[callee_idx];
            let reason = match callee.kind {
                // `preload(` is its own keyword; `load`/`get_node`/`get_node_or_null`/`NodePath`
                // are plain identifiers in GDScript (no dedicated tokens).
                Preload => Some(DeferredReason::ResourcePath),
                Identifier => match &*callee.source {
                    "load" => Some(DeferredReason::ResourcePath),
                    // `get_node`/`get_node_or_null` take a node-PATH string; `NodePath("…")` is the
                    // same shape (a node path written as a `NodePath` value).
                    "get_node" | "get_node_or_null" | "NodePath" => Some(DeferredReason::NodePath),
                    _ => None,
                },
                _ => None,
            };
            if let Some(reason) = reason {
                // The path is always the FIRST argument of these calls (`get_node(path)`,
                // `load(path)`, …). Only fire in arg slot 0 — a `get_node(foo, "Bar"|)` 2nd arg is
                // not a path position, so it falls through to the normal call-argument context.
                if arg_index_after(tokens, open_idx, byte) == 0 {
                    // A node-path call (`get_node`/`get_node_or_null`/`NodePath`) only completes a
                    // path when arg 0 is a STRING-literal context: the cursor inside the string
                    // (`get_node("Sp|")`) or just past a complete one (`get_node("x"|)` → an empty
                    // post-quote list). A bare-identifier arg (`get_node(pa|)`, passing a `NodePath`/
                    // `String` variable) is a normal expression position → fall through to identifier
                    // completion, not an (empty) node-path list that suppresses it. (`load`/`preload`
                    // keep their established whole-arg behavior.)
                    if matches!(reason, DeferredReason::NodePath)
                        && !first_arg_has_string_literal(tokens, open_idx, byte)
                    {
                        return None;
                    }
                    // The path text lives INSIDE one string-literal token (the lexer makes a string a
                    // single token), so `prefix_at` can't recover it — compute the in-string prefix
                    // span (the last segment up to the cursor) directly so the edit replaces exactly
                    // the typed segment, never the whole path or the surrounding quotes. The segment
                    // split is reason-specific (it must match the renderer's committed-dir split).
                    let prefix = string_arg_prefix(tokens, byte, reason);
                    return Some(CompletionContext::new(
                        CompletionKind::Deferred(reason),
                        prefix,
                    ));
                }
            }
        }
    }

    None
}

// ---------------------------------------------------------------------------------------------------
// Deferred (M11 node-path / resource-path) prefix extraction.
//
// These are `pub(crate)` so the M11 Phase-3 completion renderer (`completion.rs`) reuses the SAME
// token/string walk the classifier uses, rather than re-implementing a path scan (#65: the bracket /
// string framing lives here, never in the handler). A deferred context's *committed directory* (the
// path up to the last `/`) tells the renderer which node's children to enumerate; the *last segment*
// is the already-captured `CompletionContext::prefix` (the edit/filter span), so it is NOT recomputed
// here.
// ---------------------------------------------------------------------------------------------------

/// The committed directory of a **bare** `$`/`%` node-path access at the cursor: the path segments
/// before the last `/`, joined with `/`. For `$A/B/Sp|` it is `"A/B"`; for `$A/|` it is `"A"`; for a
/// rootless `$|` / `$Sp|` it is `""` (the access node's own children). `None` when the cursor is not
/// in a bare `$`/`%` node path (e.g. it is a `get_node("…")` string form — use
/// [`string_node_path_committed_dir`] there).
///
/// Returned alongside the sigil so the renderer can branch `$` (relative path) vs `%` (unique name)
/// without re-scanning. Segments are read from the token stream (names / `/` separators / quoted
/// `$"a/b"` segments), mirroring [`classify_deferred`]'s own backward walk.
#[must_use]
pub(crate) fn bare_node_path_committed_dir(
    tokens: &[Token],
    byte: usize,
) -> Option<(NodePathSigil, String)> {
    use TokenKind::*;
    let anchor = anchor_index(tokens, byte)?;

    // Collect the path segment tokens (names + separators) walking left to the `$`/`%` root, the
    // same set `classify_deferred` accepts. Stop the moment a non-path token appears.
    let mut segs_rev: Vec<&str> = Vec::new();
    let mut j = anchor as isize;
    let sigil;
    loop {
        if j < 0 {
            return None; // ran off the front without a sigil → not a bare node path
        }
        let t = &tokens[j as usize];
        match t.kind {
            Dollar => {
                sigil = NodePathSigil::Relative;
                break;
            }
            Percent => {
                // Modulo `x % y`, not a unique-node sigil — not a bare node path.
                let is_modulo = prev_meaningful(tokens, j as usize)
                    .is_some_and(|p| tokens[p].kind.can_precede_bin_op());
                if is_modulo {
                    return None;
                }
                sigil = NodePathSigil::Unique;
                break;
            }
            Slash => segs_rev.push("/"),
            // A quoted segment (`$"a/b"`) — take its decoded/source text verbatim as one segment.
            Literal => segs_rev.push(literal_segment_text(t)),
            _ if t.kind.is_node_name() => segs_rev.push(&t.source),
            _ => return None,
        }
        j -= 1;
    }

    // Reassemble the path text in source order, then drop the trailing partial segment (the part
    // after the last `/`, which is the captured `prefix`). What remains is the committed directory.
    segs_rev.reverse();
    let joined: String = segs_rev.concat();
    let committed = match joined.rfind('/') {
        Some(slash) => joined[..slash].to_string(),
        None => String::new(), // no `/` → the partial is a first segment → dir is the access root
    };
    Some((sigil, committed))
}

/// Which sigil a node-path access uses: `$` (a tree-relative path) or `%` (an owner-unique name).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum NodePathSigil {
    /// `$Rel/Path` — resolved relative to the access node.
    Relative,
    /// `%UniqueName` — resolved owner-wide by unique name.
    Unique,
}

/// The committed directory of a **string-form** node path (`get_node("A/B/Sp|")` /
/// `NodePath("A/B/|")`) at the cursor: the in-string path up to the last `/`. For
/// `get_node("A/B/Sp|")` it is `"A/B"`; for `get_node("|")` it is `""`. `None` when the cursor is
/// not inside such a call's string argument. Leading `./` is preserved verbatim (Godot resolves it),
/// and a `%`-rooted string (`get_node("%Unique")`) is reported via the returned leading-`%` flag so
/// the renderer can treat it as a unique-name access.
#[must_use]
pub(crate) fn string_node_path_committed_dir(
    tokens: &[Token],
    byte: usize,
) -> Option<StringNodePath> {
    let content = string_arg_content_before_cursor(tokens, byte)?;
    // A `%Name`-rooted string is a unique-name access written as a path string.
    if let Some(rest) = content.strip_prefix('%') {
        // `%Unique/child` (a slash after the unique name) is a deeper traversal we defer; only the
        // bare `%Unique` form lists unique names.
        if rest.contains('/') {
            return Some(StringNodePath {
                unique: true,
                committed_dir: rest[..rest
                    .rfind('/')
                    .expect("invariant: rest.contains('/') checked immediately above")]
                    .to_string(),
                deeper_unique: true,
            });
        }
        return Some(StringNodePath {
            unique: true,
            committed_dir: String::new(),
            deeper_unique: false,
        });
    }
    let committed_dir = match content.rfind('/') {
        Some(slash) => content[..slash].to_string(),
        None => String::new(),
    };
    Some(StringNodePath {
        unique: false,
        committed_dir,
        deeper_unique: false,
    })
}

/// A parsed string-form node path access (`get_node("…")`): whether it is `%`-rooted (unique) and the
/// committed directory before the cursor's last `/`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct StringNodePath {
    /// `true` for a `%Name`-rooted string (a unique-name access).
    pub unique: bool,
    /// The path up to the last `/` (the children of this directory are the suggestions).
    pub committed_dir: String,
    /// `true` for the deferred `%Unique/child` deeper-traversal form (renderer emits nothing).
    pub deeper_unique: bool,
}

/// The committed `res://` directory of a `load`/`preload` string argument at the cursor: the typed
/// path up to the last `/`. For `load("res://a/b/c|")` it is `"res://a/b"`; for `load("|")` it is
/// `""`. `None` when the cursor is not inside a resource-loader string. (The last segment is the
/// captured `prefix`; this is everything before it.)
#[must_use]
pub(crate) fn resource_path_committed_dir(tokens: &[Token], byte: usize) -> Option<String> {
    let content = string_arg_content_before_cursor(tokens, byte)?;
    Some(match content.rfind('/') {
        Some(slash) => content[..=slash].to_string(), // keep the trailing `/` (a directory prefix)
        None => String::new(),
    })
}

/// True iff the first argument of the call opened at `open_idx` is (or begins as) a string-literal
/// context before `byte` — a quote-starting `Literal` at arg-0 depth. Distinguishes a node-path
/// string (`get_node("…"|)` — keep classifying so the renderer offers paths, or an empty post-quote
/// list) from a bare identifier (`get_node(pa|)` — fall through to identifier completion).
fn first_arg_has_string_literal(tokens: &[Token], open_idx: usize, byte: usize) -> bool {
    let mut depth = 0i32;
    for t in tokens.iter().skip(open_idx + 1) {
        if t.span.start >= byte {
            break;
        }
        match t.kind {
            TokenKind::ParenthesisOpen | TokenKind::BracketOpen | TokenKind::BraceOpen => {
                depth += 1;
            }
            TokenKind::ParenthesisClose | TokenKind::BracketClose | TokenKind::BraceClose => {
                if depth == 0 {
                    break;
                }
                depth -= 1;
            }
            TokenKind::Comma if depth == 0 => break,
            TokenKind::Literal if depth == 0 && t.source.starts_with(['"', '\'']) => return true,
            _ => {}
        }
    }
    false
}

/// The raw source text of a string literal's content from the opening quote to the cursor, for the
/// string-argument deferred contexts. The cursor must sit STRICTLY inside a string-literal token —
/// after the opening quote and *before* the closing quote (`start < cursor < end`). A cursor AT the
/// closing quote (`cursor == end`, i.e. just past it) is NOT inside the string: returning content
/// there would let the edit span swallow the closing quote (an unterminated-string corruption).
/// `None` for any non-in-string position. Works on the SOURCE bytes (not the decoded value) so the
/// offsets line up with the cursor; an escape sequence in the typed prefix is not un-escaped (a
/// documented limit — node names / res paths are plain text).
fn string_arg_content_before_cursor(tokens: &[Token], byte: usize) -> Option<&str> {
    // The string token whose span STRICTLY contains the cursor. The lexer makes a string a single
    // `Literal` token whose `span` covers BOTH quotes; `byte < span.end` keeps the cursor before the
    // closing quote (so the prefix span never includes it). A `Literal` is always terminated (an
    // unterminated string lexes as an `Error`, never reaching here), so this is the right bound.
    let tok = tokens
        .iter()
        .find(|t| t.kind == TokenKind::Literal && t.span.start < byte && byte < t.span.end)?;
    // Content starts after the opening quote (1 byte: `"` / `'`). Guard a malformed/short token.
    let content_start = tok.span.start + 1;
    if byte < content_start {
        return None;
    }
    // Slice the token's own source text (which spans the quotes) from after the quote to the cursor.
    let rel_start = content_start - tok.span.start;
    let rel_end = byte - tok.span.start;
    tok.source.get(rel_start..rel_end)
}

/// The in-string prefix span the edit replaces for a string-argument deferred context — chosen so
/// accepting an item never doubles a prefix and never drops a required scheme. Returns a source-byte
/// [`ByteSpan`]; `None` when the cursor is not strictly inside the string.
///
/// The span model is REASON-SPECIFIC:
///
/// * **Node paths** ([`DeferredReason::NodePath`] / [`UniqueNodePath`](DeferredReason::UniqueNodePath)):
///   the LAST segment (after the last `/`) — the renderer inserts a bare node name, which fills
///   exactly that segment (`get_node("A/B/Sp|")` accept `Sprite` → `get_node("A/B/Sprite")`). A
///   leading `%` (a `%Name`-rooted string) is the root sigil and is kept; a mid-path `%` is NOT a
///   boundary (it can't legally appear mid-segment) — matching the committed-dir `/`-only split.
/// * **Resource paths** ([`DeferredReason::ResourcePath`]): the WHOLE typed content from the opening
///   quote to the cursor. A `res://…` literal has a mandatory `res://` scheme the renderer inserts as
///   part of the full path, so the edit must cover the entire (possibly partial) scheme+path — else
///   `load("re|")` accepting `res://src/` would leave `load("src/")` (scheme dropped) or
///   `load("reres://src/")` (doubled). Replacing the whole content with the full path is correct for
///   any amount of typed prefix. `%` is just a filename byte here, never a boundary.
fn string_arg_prefix(tokens: &[Token], byte: usize, reason: DeferredReason) -> Option<ByteSpan> {
    let content = string_arg_content_before_cursor(tokens, byte)?;
    let seg_len = match reason {
        // Node path: the last `/`-delimited segment. `%` is a boundary ONLY as the leading root
        // sigil of a `%Name`-rooted string (it can't legally appear mid-segment), never mid-path —
        // stripping a leading `%` then splitting on `/` matches `string_node_path_committed_dir`'s
        // `/`-only split, so accepting an item replaces exactly the segment whose children the
        // committed dir lists (no stale `Bar%` fragment for an already-invalid `Foo/Bar%Baz`).
        DeferredReason::NodePath | DeferredReason::UniqueNodePath => {
            let segment = content.strip_prefix('%').unwrap_or(content);
            match segment.rfind('/') {
                Some(pos) => segment.len() - pos - 1,
                None => segment.len(),
            }
        }
        // Resource path: the entire typed content (the renderer inserts the full `res://` path).
        DeferredReason::ResourcePath => content.len(),
    };
    Some(ByteSpan::new(byte - seg_len, byte))
}

/// The completion prefix span for a BARE `$`/`%` node path. The usual case is a partial word token
/// glued to the cursor (`$A/B/Sp|` → `prefix_at` returns the `Sp` span). But a quoted segment
/// (`$"Sp|"` / `$Player/"Sp|"`) puts the cursor INSIDE a string-literal token, where `prefix_at`
/// returns `None` (the anchor is the `$`/`%`, not the still-open literal) — which would give a
/// zero-width edit while the item carries the bare child name, DOUBLING it (`$"SpSprite"`). When the
/// cursor is strictly inside such a literal, use the in-string segment span instead.
fn bare_node_path_prefix(
    tokens: &[Token],
    anchor: Option<usize>,
    byte: usize,
    reason: DeferredReason,
) -> Option<ByteSpan> {
    // Cursor strictly inside a quoted segment → the in-string segment span (handles `$"Sp|"`).
    if let Some(span) = string_arg_prefix(tokens, byte, reason) {
        return Some(span);
    }
    // Otherwise the ordinary partial-word prefix (`$A/B/Sp|`, `%Hea|`).
    prefix_at(tokens, anchor, byte)
}

/// The text of a quoted node-path segment token (`$"a/b"`), as it should join into the path string —
/// the decoded string value when available (so `$"weird name"` keeps its spaces), else the raw
/// source. Used only for assembling the committed directory of a bare `$`/`%` path.
fn literal_segment_text(tok: &Token) -> &str {
    use gd_syntax::token::Literal;
    match &tok.literal {
        Some(Literal::String(s) | Literal::NodePath(s) | Literal::StringName(s)) => s,
        _ => &tok.source,
    }
}

/// (2) Member access (`.`-anchored). `super.` ⇒ `SuperMethod`; otherwise `Attribute`, recovering the
/// base node id from the AST when it survived. Wins over an enclosing call.
fn classify_member(
    tree: &ParseTree,
    tokens: &[Token],
    anchor: Option<usize>,
    byte: usize,
) -> Option<CompletionContext> {
    use TokenKind::*;
    let i = anchor?;
    let anchor_kind = tokens[i].kind;

    // Two shapes put the cursor in member-access position:
    //   `base.`         → anchor is `.`               (prefix empty)
    //   `base.partial`  → anchor is the partial ident, the token before it is `.`
    let dot_idx = if anchor_kind == Period {
        Some(i)
    } else if is_word_token(anchor_kind) && tokens[i].span.end == byte {
        match prev_meaningful(tokens, i) {
            Some(p) if tokens[p].kind == Period => Some(p),
            _ => None,
        }
    } else {
        None
    };
    let dot_idx = dot_idx?;

    // `..`/`...` are range/spread, not member access. The lexer emits those as their own kinds, so a
    // `Period` token here is genuinely a single `.`.

    // The base token sits immediately before the `.`.
    let base_idx = prev_meaningful(tokens, dot_idx)?;
    let base_kind = tokens[base_idx].kind;

    let prefix = if anchor_kind == Period {
        None
    } else {
        prefix_at(tokens, anchor, byte)
    };

    // `super.` → SuperMethod (the AST may show a Call node, but the token is authoritative).
    if base_kind == Super {
        return Some(CompletionContext::new(CompletionKind::SuperMethod, prefix));
    }

    // `var x: Foo.` — member access on a *type* (the dot sits inside a `Type` node). The nested
    // types/enums/constants of `Foo` are wanted, not its instance members, so this is TypeAttribute,
    // not Attribute. The parser keeps the `Type` node at the dot (and the partial `Foo.Ba` identifier
    // is parented by it), so `ast_is_type_position` is the discriminator.
    if ast_is_type_position(tree, byte) {
        let base = recover_attribute_base(tree, tokens, dot_idx, byte);
        return Some(CompletionContext::new(
            CompletionKind::TypeAttribute { base },
            prefix,
        ));
    }

    // Recover the base node id from the AST when the `Subscript{access: Attribute(..)}` survived.
    let base = recover_attribute_base(tree, tokens, dot_idx, byte);
    Some(CompletionContext::new(
        CompletionKind::Attribute { base },
        prefix,
    ))
}

/// Best-effort recovery of the member-access base node id from the AST. The probe established two
/// shapes: a trailing `base.` yields `Subscript{access: Attribute(None)}` directly at `byte-1`,
/// while `base.partial` yields the partial `Identifier` whose enclosing parent is that `Subscript`.
/// Top-level `local.` recovers only as a `Class` (no `Subscript`), so this returns `None` there —
/// the renderer falls back to the base token text.
fn recover_attribute_base(
    tree: &ParseTree,
    tokens: &[Token],
    dot_idx: usize,
    byte: usize,
) -> Option<NodeId> {
    if tree.is_empty() {
        return None;
    }
    // Probe at the dot and just inside it.
    let dot_span = tokens[dot_idx].span;
    for probe in [byte.saturating_sub(1), dot_span.start, dot_span.end] {
        if let Some(id) = tree.innermost_node_at(probe) {
            // Direct hit: the Subscript itself (trailing-dot case).
            if let NodeKind::Subscript(s) = &tree.get(id).kind {
                if matches!(s.access, Some(SubscriptAccess::Attribute(_))) {
                    return s.base;
                }
            }
            // Mid-name case: the cursor node is the partial Identifier; its enclosing parent is the
            // Subscript whose base we want.
            if let Some(parent) = smallest_node_strictly_containing(tree, id) {
                if let NodeKind::Subscript(s) = &tree.get(parent).kind {
                    if matches!(s.access, Some(SubscriptAccess::Attribute(_))) {
                        return s.base;
                    }
                }
            }
        }
    }
    None
}

/// (3) Single-token punctuation/keyword-anchored contexts: subscript `[`, assign `=`, annotation
/// `@`, type positions, `extends`, and class-body `func` override.
fn classify_anchored(
    tree: &ParseTree,
    tokens: &[Token],
    anchor: Option<usize>,
    byte: usize,
) -> Option<CompletionContext> {
    use TokenKind::*;
    let i = anchor?;
    let anchor_kind = tokens[i].kind;
    let prefix = prefix_at(tokens, anchor, byte);

    // Annotation: the lexer emits `@name` (and a bare `@`) as a single `Annotation` token. If the
    // cursor is glued to it, the user is typing the annotation name; the whole token (`@expo`) is
    // the prefix (it is not a "word" token, so `prefix_at` does not capture it).
    if anchor_kind == Annotation && tokens[i].span.end == byte {
        return Some(CompletionContext::new(
            CompletionKind::Annotation,
            Some(tokens[i].span),
        ));
    }

    // Subscript: anchor is `[`. Disambiguate a *type* container (`Array[`, `Dictionary[`) from an
    // index subscript by the AST — a surviving `Type` node at the cursor means we are in a type.
    if anchor_kind == BracketOpen {
        if ast_is_type_position(tree, byte) {
            return Some(CompletionContext::new(CompletionKind::TypeName, prefix));
        }
        return Some(CompletionContext::new(CompletionKind::Subscript, prefix));
    }

    // Property accessor: `get = ` / `set = ` binds a getter/setter to a *method name*, so the class's
    // methods are wanted, not an arbitrary expression. `get`/`set` are contextual identifiers; this
    // is a property accessor when the `get`/`set` identifier opens a line (preceded by layout) — i.e.
    // it is at accessor-block statement start, not `x = get(...)`.
    if anchor_kind == Equal {
        if let Some(p) = prev_meaningful(tokens, i) {
            let pt = &tokens[p];
            let is_get_set = pt.kind == Identifier && matches!(&*pt.source, "get" | "set");
            // `get`/`set` opens the accessor line ⇒ its raw predecessor is layout (or none). This
            // excludes `x = get(...)` where `get` follows an expression token.
            let at_line_start = p
                .checked_sub(1)
                .is_none_or(|prev| is_layout(tokens[prev].kind));
            if is_get_set && at_line_start {
                return Some(CompletionContext::new(
                    CompletionKind::PropertyMethod,
                    prefix,
                ));
            }
        }
    }

    // Assignment RHS: anchor is `=` or a compound-assign operator. (`==`/`!=`/`<=`/`>=` are
    // comparisons, separate token kinds, so they are correctly excluded.)
    if is_assign_op(anchor_kind) {
        return Some(CompletionContext::new(CompletionKind::Assign, prefix));
    }

    // Return type: `-> ` (anchor is `->`) or a partial type after it.
    if anchor_kind == ForwardArrow {
        return Some(CompletionContext::new(
            CompletionKind::TypeNameOrVoid,
            prefix,
        ));
    }
    if is_word_token(anchor_kind) && tokens[i].span.end == byte {
        if let Some(p) = prev_meaningful(tokens, i) {
            match tokens[p].kind {
                ForwardArrow => {
                    return Some(CompletionContext::new(
                        CompletionKind::TypeNameOrVoid,
                        prefix,
                    ));
                }
                Extends => {
                    return Some(CompletionContext::new(CompletionKind::InheritType, prefix));
                }
                // `func <name>` at class-body statement start → override-method completion.
                Func if is_class_body_func_position(tokens, p) => {
                    return Some(CompletionContext::new(
                        CompletionKind::OverrideMethod,
                        prefix,
                    ));
                }
                _ => {}
            }
        }
        // A partial word at the start of an inline property-accessor line (`var x: int:\n\tg|`) →
        // the bare `get`/`set` keyword completion. Gated on the word opening the line (its raw
        // predecessor is layout, so an in-body expression `get:\n\t\tprin|` is excluded) AND the AST
        // showing a property-style `Variable` enclosing the cursor.
        if i.checked_sub(1)
            .is_none_or(|prev| is_layout(tokens[prev].kind))
            && ast_is_property_accessor_position(tree, byte)
        {
            return Some(CompletionContext::new(
                CompletionKind::PropertyAccessor,
                prefix,
            ));
        }
    }
    // `extends ` with a trailing space (anchor is the `extends` keyword itself).
    if anchor_kind == Extends {
        return Some(CompletionContext::new(CompletionKind::InheritType, None));
    }
    // `func ` with a trailing space at class-body statement start.
    if anchor_kind == Func && is_class_body_func_position(tokens, i) {
        return Some(CompletionContext::new(CompletionKind::OverrideMethod, None));
    }

    // Empty bare accessor-keyword position (`var x: int:\n\t|`, or a second blank accessor line after
    // a completed `get:` body). There is no word token, so the partial-word branch above cannot see
    // it; the AST signal still proves we are in the property declaration context, not an ordinary
    // class/function body.
    if prefix.is_none()
        && (ast_is_property_accessor_position(tree, byte)
            || (anchor_kind == Colon
                && is_property_block_colon(tokens, i)
                && no_meaningful_token_between(tokens, i, byte))
            || token_is_property_accessor_position(tokens, byte))
    {
        return Some(CompletionContext::new(
            CompletionKind::PropertyAccessor,
            None,
        ));
    }

    // Type hint after a `:` in a declaration (`var t: Vec`, `var t: `). The AST is the reliable
    // signal — a `Type` node (under a Variable/Constant/Parameter `datatype_specifier`) survives
    // even mid-name. Token-level: anchor is `:` or a word whose enclosing AST node is a `Type`.
    //
    // A class-body `var x: <cursor>` is Godot's `COMPLETION_PROPERTY_DECLARATION_OR_TYPE`
    // (`gdscript_parser.cpp:1241`, reached only under `p_allow_property`): types PLUS `get`/`set`.
    // Every other type-only slot (function-local `var`, `const`, parameter, cast, `Array[`) keeps
    // plain `TypeName` — `parse_property` is unreachable there.
    if anchor_kind == Colon && is_declaration_colon(tokens, i) {
        let kind = if is_class_body_var_colon(tokens, i) {
            CompletionKind::PropertyDeclarationOrType
        } else {
            CompletionKind::TypeName
        };
        return Some(CompletionContext::new(kind, prefix));
    }
    if is_word_token(anchor_kind) && tokens[i].span.end == byte && ast_is_type_position(tree, byte)
    {
        let kind = match decl_colon_before(tokens, i) {
            Some(colon) if is_class_body_var_colon(tokens, colon) => {
                CompletionKind::PropertyDeclarationOrType
            }
            _ => CompletionKind::TypeName,
        };
        return Some(CompletionContext::new(kind, prefix));
    }

    None
}

/// Index of the declaration type-hint colon (`var x:` / `const x:` / param `p:`) that opens the type
/// position the word at index `i` sits in, if any — the nearest preceding depth-0 `:` on the same
/// logical line whose [`is_declaration_colon`] holds. `None` when no such colon precedes `i` on the
/// line (e.g. a cast `x as T` or an `Array[` element, which have no declaration colon).
fn decl_colon_before(tokens: &[Token], i: usize) -> Option<usize> {
    use TokenKind::*;
    let mut depth: i32 = 0;
    let mut j = i as isize - 1;
    while j >= 0 {
        let k = tokens[j as usize].kind;
        match k {
            Newline | Indent | Dedent => break,
            ParenthesisClose | BracketClose | BraceClose => depth += 1,
            ParenthesisOpen | BracketOpen | BraceOpen => depth -= 1,
            Colon if depth == 0 && is_declaration_colon(tokens, j as usize) => {
                return Some(j as usize);
            }
            _ => {}
        }
        j -= 1;
    }
    None
}

/// Whether the declaration colon at index `i` belongs to a **class-body** `var` declaration — Godot's
/// `parse_variable(_, p_allow_property = true)` path, the only one that reaches
/// `COMPLETION_PROPERTY_DECLARATION_OR_TYPE`. Two conditions, both token-primary (the AST collapses on
/// mid-edit `var x: <cursor>`, so this never trusts a surviving node):
/// 1. The colon's logical line declares with `var` (not `const`, and not a bare parameter colon).
/// 2. The declaration is not nested inside a function body — class-body scope. A `func` opens a suite
///    at a deeper indent; the colon is function-local while such a suite is still open.
fn is_class_body_var_colon(tokens: &[Token], i: usize) -> bool {
    use TokenKind::*;
    // (1) The colon's line opens with `var` at depth 0 (excludes `const x:` and parameter `p:`).
    let line_start = tokens[..i]
        .iter()
        .rposition(|t| matches!(t.kind, Newline | Indent | Dedent))
        .map_or(0, |idx| idx + 1);
    let mut depth: i32 = 0;
    let mut saw_var = false;
    for t in &tokens[line_start..i] {
        match t.kind {
            ParenthesisOpen | BracketOpen | BraceOpen => depth += 1,
            ParenthesisClose | BracketClose | BraceClose => depth -= 1,
            Var if depth == 0 => saw_var = true,
            Const if depth == 0 => return false,
            _ => {}
        }
    }
    if !saw_var {
        return false;
    }

    // (2) No enclosing `func` suite is open at the colon. Track indentation up to the colon and keep a
    // stack of the indent depths at which `func` declarations opened; a `func` is still open while the
    // indentation stays deeper than its own. The colon is class-body iff that stack is empty.
    let mut indent_depth: i32 = 0;
    let mut func_indents: Vec<i32> = Vec::new();
    let mut line_open_kind: Option<TokenKind> = None;
    let mut bracket_depth: i32 = 0;
    for t in &tokens[..i] {
        match t.kind {
            Indent => {
                indent_depth += 1;
                line_open_kind = None;
            }
            Dedent => {
                indent_depth -= 1;
                while func_indents.last().is_some_and(|&fi| indent_depth <= fi) {
                    func_indents.pop();
                }
                line_open_kind = None;
            }
            Newline => line_open_kind = None,
            ParenthesisOpen | BracketOpen | BraceOpen => bracket_depth += 1,
            ParenthesisClose | BracketClose | BraceClose => bracket_depth -= 1,
            Func if bracket_depth == 0 && line_open_kind.is_none() => {
                func_indents.push(indent_depth);
                line_open_kind = Some(Func);
            }
            // A leading `static` is transparent: it does not claim the line-open slot, so `static func`
            // and `static var` keep `func`/`var` as the line-opening keyword (Godot's
            // `parse_class_member` consumes `static` first) — a `static func` body's vars are still
            // seen as function-local.
            Static if bracket_depth == 0 && line_open_kind.is_none() => {}
            _ if bracket_depth == 0 && line_open_kind.is_none() => line_open_kind = Some(t.kind),
            _ => {}
        }
    }
    func_indents.is_empty()
}

/// (4) Enclosing call / annotation arguments, else a bare/partial identifier or `None`.
fn classify_call_or_identifier(
    tree: &ParseTree,
    tokens: &[Token],
    anchor: Option<usize>,
    byte: usize,
) -> CompletionContext {
    use TokenKind::*;
    let prefix = prefix_at(tokens, anchor, byte);

    if let Some((open_idx, open_kind)) = enclosing_open_bracket(tokens, anchor) {
        match open_kind {
            ParenthesisOpen => {
                if let Some(callee_idx) = prev_meaningful(tokens, open_idx) {
                    let callee_kind = tokens[callee_idx].kind;

                    // Annotation argument list: `@export_range(...`.
                    if callee_kind == Annotation {
                        let arg_index = arg_index_after(tokens, open_idx, byte);
                        let annotation_name = Some(tokens[callee_idx].source.to_string());
                        return CompletionContext::new(
                            CompletionKind::AnnotationArguments {
                                annotation_name,
                                arg_index,
                            },
                            prefix,
                        );
                    }

                    // A `func <name>(` parameter list is NOT a call. Detect it: the callee is an
                    // identifier and the token before it is `func`.
                    let is_func_params = callee_kind == Identifier
                        && prev_meaningful(tokens, callee_idx)
                            .map(|p| tokens[p].kind == Func)
                            .unwrap_or(false);

                    // A callee must be something a call can be applied to: an identifier, a `)`/`]`
                    // (call on a call/subscript result), or `super`. A `(` right after another `(`
                    // or after `=`/`,`/etc. is a grouping paren, not a call.
                    let is_callee = matches!(
                        callee_kind,
                        Identifier | ParenthesisClose | BracketClose | Super
                    ) || callee_kind.is_identifier();

                    if is_callee && !is_func_params {
                        let arg_index = arg_index_after(tokens, open_idx, byte);
                        let (callee, callee_name) = recover_callee(tree, tokens, callee_idx);
                        return CompletionContext::new(
                            CompletionKind::CallArguments {
                                callee,
                                callee_name,
                                arg_index,
                            },
                            prefix,
                        );
                    }
                    // Grouping paren / param list — fall through to identifier handling below.
                }
            }
            // `[` index reached here only when not anchored directly on it (rare); treat as subscript.
            BracketOpen => {
                return CompletionContext::new(CompletionKind::Subscript, prefix);
            }
            // `{` dictionary/set literal — a key/value position; identifiers are valid there.
            BraceOpen => {}
            // `enclosing_open_bracket` only ever returns an opening bracket kind.
            _ => {}
        }
    }

    // Bare identifier position: the anchor is a word glued to the cursor, or the cursor sits where
    // an expression could begin (after a statement separator / at block start). If we have a prefix,
    // it is an identifier completion; otherwise still offer identifiers when the position plausibly
    // starts an expression, else None.
    if prefix.is_some() {
        return CompletionContext::new(CompletionKind::Identifier, prefix);
    }
    if starts_expression(tokens, anchor) {
        return CompletionContext::bare(CompletionKind::Identifier);
    }
    CompletionContext::bare(CompletionKind::None)
}

// ---------------------------------------------------------------------------------------------------
// Small predicates.
// ---------------------------------------------------------------------------------------------------

/// `=` or a compound assignment operator (`+=`, `-=`, `*=`, `**=`, `/=`, `%=`, `<<=`, `>>=`, `&=`,
/// `|=`, `^=`). Comparison operators are distinct token kinds and excluded.
fn is_assign_op(kind: TokenKind) -> bool {
    use TokenKind::*;
    matches!(
        kind,
        Equal
            | PlusEqual
            | MinusEqual
            | StarEqual
            | StarStarEqual
            | SlashEqual
            | PercentEqual
            | LessLessEqual
            | GreaterGreaterEqual
            | AmpersandEqual
            | PipeEqual
            | CaretEqual
    )
}

/// Whether a `func` token at index `i` begins a method **declaration** whose name position the
/// cursor is completing (override) — as opposed to a *named lambda* in expression position
/// (`var g = func na…`, `arr.map(func …)`), which is NOT an override. The discriminator is whether
/// `func` opens a statement: a declaration `func` is at line start, so its raw predecessor token is
/// a layout boundary (`Newline`/`Indent`/`Dedent`) or there is no predecessor. A lambda `func`
/// follows an expression token (`=`, `(`, `,`, `return`, …), which is never layout.
fn is_class_body_func_position(tokens: &[Token], i: usize) -> bool {
    match i.checked_sub(1) {
        // `func` is the very first token → a top-level declaration.
        None => true,
        // Raw predecessor is layout ⇒ `func` opens a line ⇒ declaration. Otherwise it follows an
        // expression token ⇒ a named lambda, not an override.
        Some(prev) => is_layout(tokens[prev].kind),
    }
}

/// Whether the `:` token at index `i` introduces a declaration's type annotation (`var x:`,
/// `const x:`, a parameter `p:`), as opposed to a block-opening `:` (`if c:`), a dictionary entry,
/// or a slice. Heuristic over the token stream: scan left on the current logical line; a `var`/
/// `const` keyword (with no intervening `=`/`(`) marks a declaration colon.
fn is_declaration_colon(tokens: &[Token], i: usize) -> bool {
    use TokenKind::*;
    let mut j = i as isize - 1;
    while j >= 0 {
        match tokens[j as usize].kind {
            Newline | Indent | Dedent => break,
            Var | Const => return true,
            // An `=`/`(`/`[`/`{`/`,`/`:` before the colon means this colon is not a declaration
            // type colon (it is a slice, a nested entry, or a second colon).
            Equal | ParenthesisOpen | BracketOpen | BraceOpen | Colon => return false,
            _ => {}
        }
        j -= 1;
    }
    false
}

/// Token/layout fallback for an EMPTY accessor-keyword line. The parser does not always stretch the
/// property `Variable` node far enough to cover `var x: int:\n\t|` before the `get`/`set` word exists,
/// so the AST-only signal above misses the exact bare position Godot's
/// `COMPLETION_PROPERTY_DECLARATION` is made for. Track indentation up to the cursor and remember a
/// depth-0 `var ...:` property-block colon; the cursor must be on a still-empty line exactly one
/// indent deeper than that colon.
fn token_is_property_accessor_position(tokens: &[Token], byte: usize) -> bool {
    use TokenKind::*;
    let mut indent_depth: i32 = 0;
    let mut property_depth: Option<i32> = None;
    let mut line_has_meaningful = false;

    for (i, t) in tokens.iter().enumerate() {
        if t.span.end > byte {
            break;
        }
        match t.kind {
            Newline => line_has_meaningful = false,
            Indent => {
                indent_depth += 1;
                line_has_meaningful = false;
            }
            Dedent => {
                if t.span.end == byte
                    && !line_has_meaningful
                    && property_depth.is_some_and(|depth| indent_depth.saturating_sub(1) <= depth)
                {
                    continue;
                }
                indent_depth = indent_depth.saturating_sub(1);
                if property_depth.is_some_and(|depth| indent_depth <= depth) {
                    property_depth = None;
                }
                line_has_meaningful = false;
            }
            Eof | Error => {}
            Colon if is_property_block_colon(tokens, i) => {
                property_depth = Some(indent_depth);
                line_has_meaningful = true;
            }
            _ => line_has_meaningful = true,
        }
    }

    !line_has_meaningful
        && property_depth
            .map(|depth| indent_depth == depth + 1)
            .unwrap_or(false)
}

/// Whether `tokens[i]` is the depth-0 block-opening colon in a property declaration line
/// (`var x: int:` / `var x = 1:`), not the type-hint colon or a colon inside a default/dict literal.
fn is_property_block_colon(tokens: &[Token], i: usize) -> bool {
    use TokenKind::*;
    if tokens.get(i).map(|t| t.kind) != Some(Colon) {
        return false;
    }
    let line_start = tokens[..i]
        .iter()
        .rposition(|t| matches!(t.kind, Newline | Indent | Dedent))
        .map_or(0, |idx| idx + 1);
    let mut depth: i32 = 0;
    let mut saw_var = false;
    let mut saw_value_or_type = false;
    for t in &tokens[line_start..i] {
        match t.kind {
            ParenthesisOpen | BracketOpen | BraceOpen => depth += 1,
            ParenthesisClose | BracketClose | BraceClose => depth -= 1,
            Var if depth == 0 => saw_var = true,
            Colon | Equal if depth == 0 => saw_value_or_type = true,
            _ => {}
        }
    }
    depth == 0 && saw_var && saw_value_or_type
}

fn no_meaningful_token_between(tokens: &[Token], i: usize, byte: usize) -> bool {
    tokens[i + 1..]
        .iter()
        .take_while(|t| t.span.end <= byte)
        .all(|t| is_anchor_skippable(t.kind))
}

/// Whether the cursor sits at the bare accessor-keyword position of an inline property block
/// (`var x: T:\n\t<cursor>`, `var x: T:\n\tget:\n\t\t…\n\t<cursor>`) — where Godot offers the `get`/
/// `set` keywords (`COMPLETION_PROPERTY_DECLARATION`). The reliable signal is the AST: a `Variable`
/// node whose `property` is a property style (`Inline`/`SetGet`, never `None`) **encloses** the
/// cursor (probing `byte` and `byte-1`, since the partial accessor identifier sits at the node's
/// trailing edge). This distinguishes the accessor block from an ordinary class body (where the
/// enclosing node is the `Class`) or a function body (a `Suite`), which the property-style Variable
/// never covers. The caller additionally gates on the word being at line start (the accessor-name
/// position), so an in-accessor-body expression (`get:\n\t\tprin|`) does not match.
fn ast_is_property_accessor_position(tree: &ParseTree, byte: usize) -> bool {
    if tree.is_empty() {
        return false;
    }
    for probe in [byte, byte.saturating_sub(1)] {
        if let Some(id) = tree.innermost_node_at(probe) {
            // Walk from the cursor's innermost node outward. The accessor-KEYWORD position reaches a
            // property `Variable` directly (chain `Identifier → Variable`). A position INSIDE an
            // accessor BODY (`get:\n\t\tprin|`) instead reaches the body's `Function`/`Suite` first
            // (chain `Identifier → Suite → Function → Variable`) — so a `Function`/`Suite` between
            // the cursor and the property `Variable` means we are in an accessor body, NOT at the
            // keyword position. Reject the moment one is seen.
            let mut cur = Some(id);
            while let Some(n) = cur {
                match &tree.get(n).kind {
                    NodeKind::Function(_) | NodeKind::Suite(_) => break,
                    NodeKind::Variable(v) if v.property != gd_syntax::ast::PropertyStyle::None => {
                        return true
                    }
                    _ => {}
                }
                cur = smallest_node_strictly_containing(tree, n);
            }
        }
    }
    false
}

/// Whether the AST has a surviving `Type` node covering the cursor (probing `byte` and `byte-1`),
/// the reliable signal for a type position (`var x: T`, `Array[`, a cast). The parser keeps the
/// `Type` node even mid-name, so this catches `var t: Vec` where the token-only heuristic cannot
/// tell a type identifier from a value identifier.
fn ast_is_type_position(tree: &ParseTree, byte: usize) -> bool {
    if tree.is_empty() {
        return false;
    }
    for probe in [byte, byte.saturating_sub(1)] {
        if let Some(id) = tree.innermost_node_at(probe) {
            if matches!(tree.get(id).kind, NodeKind::Type(_)) {
                return true;
            }
            // The cursor node may be the partial Identifier directly under a Type.
            if let Some(parent) = smallest_node_strictly_containing(tree, id) {
                if matches!(tree.get(parent).kind, NodeKind::Type(_)) {
                    return true;
                }
            }
        }
    }
    false
}

/// Recover a call's callee node id and/or name. When the callee token is a simple identifier we
/// always have the name; the node id comes from the AST when the `Call` survived (it does not for
/// `print(` with an empty arg list, but does for `max(1, , 2)`).
fn recover_callee(
    tree: &ParseTree,
    tokens: &[Token],
    callee_idx: usize,
) -> (Option<NodeId>, Option<String>) {
    let tok = &tokens[callee_idx];
    let name = if tok.kind == TokenKind::Identifier || tok.kind.is_identifier() {
        Some(tok.source.to_string())
    } else {
        None
    };
    let node = if tree.is_empty() {
        None
    } else {
        tree.innermost_node_at(tok.span.start)
            .filter(|&id| matches!(tree.get(id).kind, NodeKind::Identifier(_)))
    };
    (node, name)
}

/// Whether the cursor (with the given anchor) plausibly begins an expression, so a bare identifier
/// completion is appropriate even without a typed prefix. True at the very start, right after a
/// statement boundary, or after an operator/opening bracket/keyword that expects an expression.
fn starts_expression(tokens: &[Token], anchor: Option<usize>) -> bool {
    use TokenKind::*;
    match anchor {
        // No token before the cursor: top of file / blank line → an identifier could begin here.
        None => true,
        Some(i) => matches!(
            tokens[i].kind,
            // After these, an expression is expected next.
            Equal
                | PlusEqual
                | MinusEqual
                | StarEqual
                | SlashEqual
                | PercentEqual
                | Comma
                | ParenthesisOpen
                | BracketOpen
                | BraceOpen
                | Return
                | Colon
                | And
                | Or
                | Not
                | Plus
                | Minus
                | Star
                | Slash
                | If
                | Elif
                | While
                | In
        ),
    }
}

#[cfg(test)]
mod tests;
