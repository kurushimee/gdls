//! Precise `$`/`%` typing for the NAVIGATION surfaces (#125) — hover, definition, typeDefinition,
//! completion, and signatureHelp.
//!
//! # Why this lives here and not in the analyzer
//!
//! Godot's analyzer types a valid `$Path` / `%Name` / `get_node("…")` as a hard bare `NATIVE Node`
//! (`gdscript_analyzer.cpp:3866-3886`) and never reads the `.tscn` for that type, so it TOLERATES
//! sibling/subtype downcasts off the access (`var c: Control = $Node2DChild`). `reduce_get_node`
//! reproduces that exactly. A `DataType` is used SYMMETRICALLY in the compatibility checks, so a
//! scene-precise type in the diagnostic path would turn those Godot-tolerated downcasts into false
//! positives — which is why the scene-resolution seam shipped dormant in M11 (`docs/02` §11).
//!
//! Navigation has no compatibility check to fail: "what is `$Health`?" is a read-only question, and
//! answering `Node2D` (or the script attached to that node) is strictly more useful than `Node`.
//! This module is that consumer, and the ONLY one — the type it builds is handed straight to the
//! hover / definition / completion renderers and never reaches an `AnalysisResult`, so the two
//! paths stay separate by construction.
//!
//! # Past the dot (#349)
//!
//! The precise type answers the node expression AND anything read off it. `$HUD/Label` knowing it
//! is a `Label` while `$HUD/Label.` one character later offers bare `Node`'s 314 members is the
//! same answer contradicting itself, and `$`-addressed nodes are the most common shape in a scene
//! script. [`scene_type_ending_at`] is the seam for that: a read surface asks it for the type of a
//! member-access BASE before falling back to the analyzer's own. It stays navigation-only for the
//! same reason the node hover does — nothing it returns is written back.
//!
//! # Conservative end to end
//!
//! The access shape must be one the scene index resolves soundly ([`node_path_query`]), and
//! [`crate::workspace::Workspace::scene_node_facts`] answers only when EVERY scene attaching this
//! script agrees on the target. Anything else — an absolute `$/root/…` path, a scene-less script, an
//! absent node, two scenes disagreeing — yields `None`, and the caller falls back to the analyzer's
//! bare `Node`. A missed precise type is a known limitation; a wrong one is a defect.

use gd_analyze::data_type::ScriptRef;
use gd_analyze::{DataType, DtKind, NodePathQuery, SceneNodeFacts, TypeSource, VariantType};
use gd_syntax::ast::{NodeId, NodeKind, ParseTree, SubscriptAccess};
use gd_syntax::token::Literal;
use lsp_types::Uri;

use crate::server::ServerState;

/// The scene-precise type of the `$`/`%`/`get_node("…")` access the cursor sits in, plus the access
/// node's own id (the caller uses its span as the hover range). `None` when the cursor is in no such
/// access, when the access shape isn't soundly resolvable, or when the scenes don't agree.
///
/// NAVIGATION ONLY — see the module docs. The returned [`DataType`] is a display/jump vehicle built
/// in the server; it is never written into an [`AnalysisResult`](gd_analyze::AnalysisResult).
pub(crate) fn scene_node_type_at(
    state: &ServerState,
    uri: &Uri,
    tree: &ParseTree,
    byte: usize,
) -> Option<(NodeId, DataType)> {
    let (node_id, query) = node_path_access_at(tree, byte)?;
    let path = crate::uri::uri_to_path(uri)?;
    let facts = state.workspace.scene_node_facts(&path, &query)?;
    Some((node_id, facts_to_nav_type(&facts)))
}

/// The scene-precise type of the `$`/`%`/`get_node("…")` access that ENDS at `end`, for a member
/// access whose base is that expression (`$HUD/Label.text`, `%Label.`). `None` when the expression
/// ending there is not such an access, or when [`scene_node_type_at`] declines it.
///
/// The end-anchored match is what keeps this exact: a `$X` buried inside a larger base expression
/// (`foo($X).bar`, `[$X][0].bar`) ends before that base does, so it never hijacks the base's own
/// type. NAVIGATION ONLY, exactly as [`scene_node_type_at`].
pub(crate) fn scene_type_ending_at(
    state: &ServerState,
    uri: &Uri,
    tree: &ParseTree,
    end: usize,
) -> Option<DataType> {
    let query = direct_query_ending_at(tree, end).or_else(|| identifier_hop_query(tree, end))?;
    let path = crate::uri::uri_to_path(uri)?;
    let facts = state.workspace.scene_node_facts(&path, &query)?;
    Some(facts_to_nav_type(&facts))
}

/// The query of a `$`/`%`/`get_node("…")` access written directly at the base position.
fn direct_query_ending_at(tree: &ParseTree, end: usize) -> Option<NodePathQuery> {
    let (access_id, query) = node_path_access_at(tree, end.checked_sub(1)?)?;
    (tree.get(access_id).span.end == end).then_some(query)
}

/// #458: the ONE hop through a variable that holds the access — `@onready var sp := $Sprite` and
/// then `sp.texture`. Godot's `_get_subscript_type` (`gdscript_editor.cpp:3234-3326`) does exactly
/// this: for a subscript whose base is an identifier, it reads what the identifier was DECLARED
/// from and, when that is a `GET_NODE`, resolves the path and stamps the base type from the node.
///
/// One hop only, off the declaration's own initializer. Upstream does no assignment-flow analysis
/// either (`// TODO`, `:3269`), and matching that is the point: a later `sp = something_else` is
/// invisible to both.
fn identifier_hop_query(tree: &ParseTree, end: usize) -> Option<NodePathQuery> {
    let ident_id = tree.iter_ids().find(|&id| {
        let node = tree.get(id);
        node.span.end == end && matches!(node.kind, NodeKind::Identifier(_))
    })?;
    // An identifier in ATTRIBUTE position (`a.sp.texture`) is a member of whatever `a` is, not a
    // bare name — resolving it as one would answer about a different symbol entirely.
    if is_attribute_identifier(tree, ident_id) {
        return None;
    }
    let NodeKind::Identifier(ident) = &tree.get(ident_id).kind else {
        return None;
    };
    declared_get_node_query(tree, ident_id, &ident.name)
}

/// Whether `ident_id` is the trailing identifier of a `base.attr` subscript.
fn is_attribute_identifier(tree: &ParseTree, ident_id: NodeId) -> bool {
    tree.iter_ids().any(|id| {
        matches!(
            &tree.get(id).kind,
            NodeKind::Subscript(sub)
                if sub.access == Some(SubscriptAccess::Attribute(Some(ident_id)))
        )
    })
}

/// The node-path query the declaration of `name` was initialized from, resolved in Godot's own
/// precedence: a local binding first, then a member VARIABLE of the innermost enclosing class.
///
/// A local wins outright. If `name` resolves to a parameter, a `for` variable, or a `match` bind,
/// the answer is `None` and the member is NOT consulted — that binding shadows the member, and
/// falling through would answer about a symbol the cursor is not on. Members are variables only:
/// upstream checks `Member::VARIABLE` (`:3258`) and reaches a constant's initializer only as a
/// LOCAL (`:3273`).
fn declared_get_node_query(
    tree: &ParseTree,
    ident_id: NodeId,
    name: &str,
) -> Option<NodePathQuery> {
    let use_at = tree.get(ident_id).span.start;
    if let Some(decl_ident) = tree.resolve_local_binding_at(use_at, name) {
        let decl_id = tree.iter_ids().find(|&id| match &tree.get(id).kind {
            NodeKind::Variable(v) => v.identifier == Some(decl_ident),
            NodeKind::Constant(c) => c.identifier == Some(decl_ident),
            _ => false,
        })?;
        return assignable_get_node_query(tree, decl_id);
    }
    let class_id = innermost_class_at(tree, use_at)?;
    let NodeKind::Class(class) = &tree.get(class_id).kind else {
        return None;
    };
    let index = *class.members_indices.get(name)?;
    let Some(gd_syntax::ast::Member::Variable(var_id)) = class.members.get(index) else {
        return None;
    };
    assignable_get_node_query(tree, *var_id)
}

/// The query a `var`/`const` declaration's initializer maps to, or `None` when the declaration is
/// annotated or its initializer is not a node access.
///
/// **The annotation refusal** is Godot's "Annotated type takes precedence" bail (`:3300-3303`),
/// expressed against what the parse tree carries: a `datatype_specifier` means the author wrote a
/// type, so the declaration's type is theirs and not the scene's. Upstream refuses only when the
/// annotation DIFFERS from the node's type, but the two agree through every consumer here: when it
/// matches, declining just hands the base back to the analyzer, whose answer for an annotated
/// declaration IS that annotation. Comparing them would need a resolved `DataType`, which is
/// analyzer work this module deliberately does not do.
fn assignable_get_node_query(tree: &ParseTree, decl_id: NodeId) -> Option<NodePathQuery> {
    let (specifier, initializer) = match &tree.get(decl_id).kind {
        NodeKind::Variable(v) => (v.datatype_specifier, v.initializer),
        NodeKind::Constant(c) => (c.datatype_specifier, c.initializer),
        _ => return None,
    };
    if specifier.is_some() {
        return None;
    }
    let init_id = initializer?;
    match &tree.get(init_id).kind {
        NodeKind::GetNode(g) => node_path_query(&g.full_path),
        NodeKind::Call(c) => call_node_path(tree, c),
        _ => None,
    }
}

/// The innermost `ClassNode` whose span covers `byte`. The implicit head class spans the whole
/// file, so this always answers for a parsed script; an inner class wins over it.
fn innermost_class_at(tree: &ParseTree, byte: usize) -> Option<NodeId> {
    let mut best: Option<(NodeId, usize)> = None;
    for id in tree.iter_ids() {
        let node = tree.get(id);
        if !matches!(node.kind, NodeKind::Class(_)) {
            continue;
        }
        if !(node.span.start <= byte && byte < node.span.end) {
            continue;
        }
        let width = node.span.end - node.span.start;
        if best.as_ref().is_none_or(|(_, w)| width < *w) {
            best = Some((id, width));
        }
    }
    best.map(|(id, _)| id)
}

/// [`scene_type_ending_at`] for a member access's base NODE — the common caller shape.
pub(crate) fn scene_type_of_base(
    state: &ServerState,
    uri: &Uri,
    tree: &ParseTree,
    base_id: NodeId,
) -> Option<DataType> {
    scene_type_ending_at(state, uri, tree, tree.get(base_id).span.end)
}

/// The innermost `$`/`%` access (a `GetNode` node) or `get_node("literal")` call containing `byte`,
/// with the scene query it maps to. Both spell the same access — Godot's parser desugars `$X` into
/// `get_node("X")` — so both resolve through the same query.
///
/// A call form is accepted only in its unambiguous shape: the callee is the bare name `get_node` /
/// `get_node_or_null` (never `other.get_node(…)`, whose receiver is some OTHER node) with exactly
/// one argument, a string literal.
fn node_path_access_at(tree: &ParseTree, byte: usize) -> Option<(NodeId, NodePathQuery)> {
    let mut best: Option<(NodeId, NodePathQuery, usize)> = None;
    for id in tree.iter_ids() {
        let node = tree.get(id);
        if !(node.span.start <= byte && byte < node.span.end) {
            continue;
        }
        let query = match &node.kind {
            NodeKind::GetNode(g) => node_path_query(&g.full_path),
            NodeKind::Call(c) => call_node_path(tree, c),
            _ => None,
        };
        let Some(query) = query else { continue };
        let width = node.span.end - node.span.start;
        if best.as_ref().is_none_or(|(_, _, w)| width < *w) {
            best = Some((id, query, width));
        }
    }
    best.map(|(id, query, _)| (id, query))
}

/// The query a `get_node("Rel/Path")` / `get_node_or_null("%Name")` call maps to, or `None` for any
/// other call.
fn call_node_path(tree: &ParseTree, call: &gd_syntax::ast::CallNode) -> Option<NodePathQuery> {
    if !matches!(call.function_name.as_str(), "get_node" | "get_node_or_null") {
        return None;
    }
    // A bare-name callee only: `other.get_node("X")` asks about a DIFFERENT node's subtree, which
    // this script's attachment point can't resolve.
    if !matches!(tree.get(call.callee?).kind, NodeKind::Identifier(_)) {
        return None;
    }
    let [arg] = call.arguments[..] else {
        return None;
    };
    let NodeKind::Literal(lit) = &tree.get(arg).kind else {
        return None;
    };
    match &lit.value {
        Literal::String(s) | Literal::StringName(s) | Literal::NodePath(s) => node_path_query(s),
        _ => None,
    }
}

/// The [`NodePathQuery`] a node-path string maps to, or `None` for a shape scene resolution does not
/// handle soundly. `full_path` is the parser's reconstruction (`$A/B` → `"A/B"`, `$/root/X` →
/// `"/root/X"`, `%Name` / `$%Name` → `"%Name"`), so the markers it preserves are what classify here:
///
/// * a leading `/` is an ABSOLUTE path — resolved against the running scene tree's root, which a
///   parsed `.tscn` cannot stand in for;
/// * a leading `%` with a single segment is an owner-scoped unique name;
/// * a `%` anywhere else (`A/%B`) mixes the two resolutions — declined;
/// * anything else is a root-relative path from the script's attachment node.
fn node_path_query(full_path: &str) -> Option<NodePathQuery> {
    if full_path.is_empty() || full_path.starts_with('/') {
        return None;
    }
    if let Some(unique) = full_path.strip_prefix('%') {
        if unique.is_empty() || unique.contains('/') || unique.contains('%') {
            return None;
        }
        return Some(NodePathQuery::UniqueName(unique.to_owned()));
    }
    if full_path.contains('%') {
        return None;
    }
    Some(NodePathQuery::RelativePath(full_path.to_owned()))
}

/// Project a [`SceneNodeFacts`] into the display/jump [`DataType`] the navigation renderers speak:
/// an attached script becomes a `Script` instance of that file (root class — a `.tscn` attaches a
/// FILE, never an inner class), a scriptless node its engine class. `AnnotatedExplicit` marks it a
/// hard type for the renderers; it is inert either way, since this value never enters analysis.
fn facts_to_nav_type(facts: &SceneNodeFacts) -> DataType {
    match facts {
        SceneNodeFacts::Script(file) => DataType {
            kind: DtKind::Script,
            type_source: TypeSource::AnnotatedExplicit,
            builtin_type: VariantType::Object,
            script_type: Some(ScriptRef {
                file: *file,
                inner: Vec::new(),
            }),
            ..Default::default()
        },
        SceneNodeFacts::Native(class) => DataType {
            kind: DtKind::Native,
            type_source: TypeSource::AnnotatedExplicit,
            builtin_type: VariantType::Object,
            native_type: class.clone(),
            ..Default::default()
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn node_path_query_classifies_each_access_shape() {
        assert_eq!(
            node_path_query("Health"),
            Some(NodePathQuery::RelativePath("Health".into()))
        );
        assert_eq!(
            node_path_query("A/B"),
            Some(NodePathQuery::RelativePath("A/B".into()))
        );
        assert_eq!(
            node_path_query("%Special"),
            Some(NodePathQuery::UniqueName("Special".into()))
        );
        // Absolute: the running tree's root, which a parsed `.tscn` cannot stand in for.
        assert_eq!(node_path_query("/root/Main"), None);
        // A `%` inside a relative path mixes the two resolutions.
        assert_eq!(node_path_query("A/%B"), None);
        assert_eq!(node_path_query("%A/B"), None);
        assert_eq!(node_path_query(""), None);
    }

    /// The query `identifier_hop_query` answers for the identifier ending at the LAST occurrence of
    /// `needle` in `src` — the base position of `needle.something`.
    fn hop(src: &str, needle: &str) -> Option<NodePathQuery> {
        let tree = gd_syntax::parse(src).tree;
        let end = src.rfind(needle).expect("needle in source") + needle.len();
        identifier_hop_query(&tree, end)
    }

    #[test]
    fn a_declaration_initialized_from_an_access_carries_its_query() {
        // A member variable, the `@onready var x := $Child` idiom.
        assert_eq!(
            hop(
                "extends Node2D\n@onready var sp := $Sprite\nfunc g():\n\tsp.flip_h\n",
                "sp"
            ),
            Some(NodePathQuery::RelativePath("Sprite".into()))
        );
        // A unique name.
        assert_eq!(
            hop(
                "extends Node2D\n@onready var lb := %Special\nfunc g():\n\tlb.x\n",
                "lb"
            ),
            Some(NodePathQuery::UniqueName("Special".into()))
        );
        // A local, and the `get_node("…")` call spelling of the same access.
        assert_eq!(
            hop(
                "extends Node2D\nfunc g():\n\tvar l = get_node(\"A/B\")\n\tl.x\n",
                "l"
            ),
            Some(NodePathQuery::RelativePath("A/B".into()))
        );
        // A local `const`, which upstream reads as a local (`gdscript_editor.cpp:3273`).
        assert_eq!(
            hop(
                "extends Node2D\nfunc g():\n\tconst C = $Sprite\n\tC.x\n",
                "C"
            ),
            Some(NodePathQuery::RelativePath("Sprite".into()))
        );
    }

    #[test]
    fn an_annotated_declaration_keeps_its_own_type() {
        // "Annotated type takes precedence" (`gdscript_editor.cpp:3300-3303`).
        assert_eq!(
            hop(
                "extends Node2D\nvar sp: Node2D = $Sprite\nfunc g():\n\tsp.x\n",
                "sp"
            ),
            None
        );
        assert_eq!(
            hop(
                "extends Node2D\nfunc g():\n\tvar l: Node = $Sprite\n\tl.x\n",
                "l"
            ),
            None
        );
    }

    #[test]
    fn a_shadowing_binding_blocks_the_member() {
        // A parameter of the same name is a different symbol; the member must not answer for it.
        assert_eq!(
            hop(
                "extends Node2D\n@onready var sp := $Sprite\nfunc g(sp: Node):\n\tsp.x\n",
                "sp"
            ),
            None
        );
        // Same for a `for` variable.
        assert_eq!(
            hop(
                "extends Node2D\n@onready var sp := $Sprite\nfunc g():\n\tfor sp in [1]:\n\t\tsp.x\n",
                "sp"
            ),
            None
        );
    }

    #[test]
    fn only_a_declared_access_hops() {
        // Not an access at all.
        assert_eq!(
            hop(
                "extends Node2D\nvar sp = Node2D.new()\nfunc g():\n\tsp.x\n",
                "sp"
            ),
            None
        );
        // Wrapped in a cast — upstream reads the initializer node itself, nothing inside it.
        assert_eq!(
            hop(
                "extends Node2D\nvar sp := $Sprite as Node2D\nfunc g():\n\tsp.x\n",
                "sp"
            ),
            None
        );
        // No initializer.
        assert_eq!(
            hop("extends Node2D\nvar sp\nfunc g():\n\tsp.x\n", "sp"),
            None
        );
        // One hop only: a variable holding another variable does not chain.
        assert_eq!(
            hop(
                "extends Node2D\n@onready var a := $Sprite\n@onready var b := a\nfunc g():\n\tb.x\n",
                "b"
            ),
            None
        );
    }

    #[test]
    fn an_attribute_identifier_never_hops() {
        // The `sp` in `other.sp` is a member of `other`, not the bare name — answering the scene
        // query for it would describe a different symbol.
        assert_eq!(
            hop(
                "extends Node2D\n@onready var sp := $Sprite\nfunc g(other: Node):\n\tother.sp.x\n",
                "sp"
            ),
            None
        );
    }

    /// A member of an OUTER class is not in an inner class's scope, and the innermost class is what
    /// answers — mirroring upstream's per-class `current_class->has_member`.
    #[test]
    fn an_inner_class_does_not_see_the_outer_members_access() {
        assert_eq!(
            hop(
                "extends Node2D\n@onready var sp := $Sprite\nclass Inner:\n\tfunc g():\n\t\tsp.x\n",
                "sp"
            ),
            None
        );
    }
}
