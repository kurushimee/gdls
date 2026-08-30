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
use gd_syntax::ast::{NodeId, NodeKind, ParseTree};
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
    let (access_id, dt) = scene_node_type_at(state, uri, tree, end.checked_sub(1)?)?;
    (tree.get(access_id).span.end == end).then_some(dt)
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
}
