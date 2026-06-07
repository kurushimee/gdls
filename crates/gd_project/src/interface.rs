//! The eager shallow extractor: walk a parsed `.gd`'s root `ClassNode` to capture what it *exposes*.
//!
//! This is the parser-only half of Godot's class discovery (`GDScriptLanguage::get_global_class_name`
//! / the shallow `GDScriptCache` pass): it reads the already-parsed M1 AST and records `class_name`,
//! `extends`, the member signatures, and inner classes — **no type analysis** (that is M3). The
//! resulting [`Interface`] is the unit the registry, the dependency graph, and (M3) the analyzer all
//! consume; closed files keep only their `Interface` and re-parse on demand (`docs/03` §5).
//!
//! Member *types* are captured syntactically as a [`TypeExpr`] (the name(s) as written), not resolved
//! to the type lattice: native DB + syntactic type refs in M2, lattice in M3.

use std::hash::{Hash, Hasher};

use gd_syntax::ast::{ClassNode, EnumValue, Member, NodeId, NodeKind, PropertyStyle};
use gd_syntax::{ByteSpan, ParseTree};
use rustc_hash::FxHasher;
use serde::{Deserialize, Serialize};

/// What a class's `extends` clause names, captured syntactically.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Extends {
    /// No `extends` (Godot implies `RefCounted`; M2 leaves that to the analyzer).
    #[default]
    None,
    /// `extends "res://path.gd"` — a path literal, verbatim.
    Path(String),
    /// `extends Foo` / `extends A.B.C` — an identifier chain.
    Names(Vec<String>),
}

/// The kind of an exposed member. Inner classes are not members — they live in [`Interface::inner`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum MemberKind {
    Const,
    Var,
    /// A `var` with a getter/setter.
    Property,
    Func,
    Signal,
    /// A *named* `enum E { … }` (its values are reachable as `E.A`).
    Enum,
}

/// A member's declared type, as written — an unresolved syntactic reference (decision 3). `Array[T]`
/// / `Dictionary[K, V]` keep their container args; an attribute chain (`A.B`) keeps every segment.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum TypeExpr {
    /// No annotation (untyped, inferred, or `void`).
    None,
    /// A named type: the identifier chain plus any container type arguments.
    Named {
        path: Vec<String>,
        args: Vec<TypeExpr>,
    },
}

impl TypeExpr {
    /// The leading identifier of the type (`A` in `A.B`, `Array` in `Array[int]`), if any. This is the
    /// name that participates in cross-file resolution / dependency edges.
    pub fn head(&self) -> Option<&str> {
        match self {
            TypeExpr::None => None,
            TypeExpr::Named { path, .. } => path.first().map(String::as_str),
        }
    }
}

/// Declaration flags that are part of a member's *interface* (they change how callers may use it).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct MemberFlags {
    pub is_static: bool,
    pub exported: bool,
    pub onready: bool,
    pub is_abstract: bool,
    pub is_coroutine: bool,
}

/// One exposed member of a class.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemberDecl {
    pub name: String,
    pub kind: MemberKind,
    /// The declared type (a `var`/`const`'s annotation, or a `func`'s return type).
    pub ty: TypeExpr,
    /// Parameter types for `func`/`signal` members; empty otherwise.
    pub params: Vec<TypeExpr>,
    /// Parameter identifier names for `func`/`signal` members, parallel to `params`. Empty for
    /// non-func/signal members, and empty for parameters without identifiers (rare, defensive).
    /// Not included in `signature_hash` — param renames don't change call compatibility in
    /// GDScript's positional-call model, so they aren't interface-relevant for invalidation.
    pub param_names: Vec<String>,
    pub flags: MemberFlags,
    /// Byte range of the declaration. **Excluded from [`Interface::signature_hash`]** so that a
    /// body-only edit (which shifts later members' spans) does not look like an interface change.
    pub span: ByteSpan,
    /// 1-based source line of the declaration. Drives diagnostics like
    /// SHADOWED_VARIABLE_BASE_CLASS that include the member's line in the message
    /// (`"already-declared variable at line N"`).
    pub line: u32,
}

/// A *named* enum and its value identifiers. Godot's `EnumNode::values[i].identifier->name`
/// chain — used by cross-file enum-value attribute walks (e.g. `P.Named.VALUE_A` where `P` is
/// a preloaded script). Values without identifiers (computed at the parser, e.g. raw int
/// expressions outside an enum declaration) are not collected — Godot ignores them too.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct EnumDecl {
    pub name: String,
    pub values: Vec<String>,
}

/// The shallow interface of one class: what it exposes, with no types resolved.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Interface {
    /// `class_name X` for the top-level class, or the declared name of an inner `class X:`.
    pub class_name: Option<String>,
    pub extends: Extends,
    pub is_abstract: bool,
    /// `@tool` annotation on the class. Godot's `ClassNode::is_tool` (set from the parser's
    /// `@tool` annotation walk at `gdscript_parser.cpp` annotation table). Cross-file consumer:
    /// MISSING_TOOL warning (see `gdscript_warning.cpp::get_message` for MISSING_TOOL —
    /// `"The base class script has the @tool annotation, but this script does not have it."`).
    pub is_tool: bool,
    pub icon_path: Option<String>,
    pub members: Vec<MemberDecl>,
    /// Inner classes, recursively (reachable as `Outer.Inner`).
    pub inner: Vec<Interface>,
    /// Named enums + their value identifiers. Reachable as `Self.<EnumName>.<value>` or
    /// (cross-file) `<preload_const>.<EnumName>.<value>`.
    pub enums: Vec<EnumDecl>,
    /// WP-RD12: `res://` paths this file `preload(...)`s / `load(...)`s. M2 deliberately excluded
    /// these as "body-level" edges, but a cross-file member-initializer cycle (WP-R2) reaches its
    /// target THROUGH a `const X = preload("res://b.gd")` — a const has no type annotation, so it
    /// was never a `referenced_names` / path-`extends` edge, and editing the dependency never
    /// re-invalidated the consumer (the missing-diagnostic gap the WP-RD8 xfile freshness-gate
    /// comment calls out). `Index::recompute_edges` now resolves these to `DepGraph` edges so the
    /// existing reverse-closure invalidation carries the rest. **Excluded from
    /// [`Interface::signature_hash`]**: this is *what this file depends on*, not *what it exposes*,
    /// so changing it must re-link THIS file's forward edges (which `on_file_changed` always does)
    /// but must not look like an interface change to this file's own consumers. Populated only on
    /// the head interface (the `DepGraph` is per-file, so inner-class preloads roll up to it).
    pub preload_deps: Vec<String>,
}

impl Interface {
    /// The signature hash: every interface-relevant field **except source spans** (see
    /// [`MemberDecl::span`]). Computed on demand so it can never drift from the contents — equal
    /// hashes ⇒ the interface is unchanged ⇒ a body-only edit ⇒ dependents need not be re-analyzed
    /// (WP-E, `docs/03` §5). Spans are excluded so a body edit (which shifts later members' offsets)
    /// doesn't look like an interface change.
    pub fn signature_hash(&self) -> u64 {
        let mut h = FxHasher::default();
        self.hash_into(&mut h);
        h.finish()
    }

    fn hash_into(&self, h: &mut FxHasher) {
        self.class_name.hash(h);
        self.extends.hash(h);
        self.is_abstract.hash(h);
        self.is_tool.hash(h);
        self.icon_path.hash(h);
        for m in &self.members {
            m.name.hash(h);
            m.kind.hash(h);
            m.ty.hash(h);
            m.params.hash(h);
            m.flags.hash(h);
            // m.span is intentionally NOT hashed.
        }
        for inner in &self.inner {
            inner.signature_hash().hash(h);
        }
        for e in &self.enums {
            e.hash(h);
        }
    }
}

/// Extract the interface of a parsed source. A partial/empty AST yields a default (empty) interface —
/// the parser always returns *something*, so extraction never fails (`docs/00`: never crash).
pub fn extract(tree: &ParseTree) -> Interface {
    let Some(root) = tree.root() else {
        return Interface::default();
    };
    let NodeKind::Class(class) = &root.kind else {
        return Interface::default();
    };
    let mut head = extract_class(tree, class, &root.annotations);
    // WP-RD12: capture this file's `preload`/`load` `res://` targets on the head interface so
    // `Index::recompute_edges` can turn them into `DepGraph` edges (the preload-const cross-file
    // cycle case). Walked once over the whole tree (a file-wide over-approximation of the const
    // initializers the WP-R2 cycle reaches through — additive edges, so over-capturing a
    // body-level preload only ever invalidates a consumer slightly more eagerly, never less).
    head.preload_deps = collect_preload_deps(tree);
    head
}

/// WP-RD12: every `res://` path the file `preload(...)`s (a dedicated `PreloadNode`) or
/// `load("res://…")`s (a `Call` to the `load` utility). Dedup-free — `recompute_edges` collects
/// into a set. Skips non-`res://` literals (engine resources, user:// paths) since only project
/// scripts participate in the cross-file dependency graph.
fn collect_preload_deps(tree: &ParseTree) -> Vec<String> {
    use gd_syntax::token::Literal;
    let mut deps = Vec::new();
    for id in tree.iter_ids() {
        let path_node = match &tree.get(id).kind {
            NodeKind::Preload(p) => p.path,
            NodeKind::Call(c) if c.function_name == "load" => c.arguments.first().copied(),
            _ => None,
        };
        let Some(pid) = path_node else {
            continue;
        };
        if let NodeKind::Literal(lit) = &tree.get(pid).kind {
            if let Literal::String(s) | Literal::StringName(s) | Literal::NodePath(s) = &lit.value {
                if s.starts_with("res://") {
                    deps.push(s.clone());
                }
            }
        }
    }
    deps
}

/// `annotations` are the declaration-level annotations attached to *this* class node — the M1 parser
/// records `@abstract`/`@export`/`@onready` as annotations rather than setting the corresponding bool
/// fields (those are populated by the analyzer's annotation callbacks in M3), so M2 reads the flags
/// off the attached annotations.
fn extract_class(tree: &ParseTree, class: &ClassNode, annotations: &[NodeId]) -> Interface {
    let mut members = Vec::new();
    let mut inner = Vec::new();
    let mut enums = Vec::new();
    for member in &class.members {
        match member {
            Member::Class(id) => {
                let node = tree.get(*id);
                if let NodeKind::Class(c) = &node.kind {
                    inner.push(extract_class(tree, c, &node.annotations));
                }
            }
            Member::Variable(id) => members.extend(var_member(tree, *id)),
            Member::Constant(id) => members.extend(const_member(tree, *id)),
            Member::Function(id) => members.extend(func_member(tree, *id)),
            Member::Signal(id) => members.extend(signal_member(tree, *id)),
            Member::Enum(id) => {
                members.extend(enum_member(tree, *id));
                enums.extend(enum_decl(tree, *id));
            }
            // A value of an *unnamed* enum is hoisted to a class constant.
            Member::EnumValue(value) => members.extend(enum_value_member(tree, value)),
            // `@export_group`/category/subgroup — presentation only, not an exposed name.
            Member::Group(_) => {}
        }
    }

    Interface {
        class_name: ident_name(tree, class.identifier),
        extends: extends_of(tree, class),
        is_abstract: has_annotation(tree, annotations, |n| n == "@abstract"),
        is_tool: has_annotation(tree, annotations, |n| n == "@tool"),
        icon_path: class.icon_path.clone(),
        members,
        inner,
        enums,
        // WP-RD12: populated only on the head interface by `extract` (the DepGraph is per-file);
        // inner classes' preloads roll up there.
        preload_deps: Vec::new(),
    }
}

fn extends_of(tree: &ParseTree, class: &ClassNode) -> Extends {
    if let Some(path) = &class.extends_path {
        return Extends::Path(path.clone());
    }
    let names: Vec<String> = class
        .extends
        .iter()
        .filter_map(|&id| ident_name(tree, Some(id)))
        .collect();
    if names.is_empty() {
        Extends::None
    } else {
        Extends::Names(names)
    }
}

fn var_member(tree: &ParseTree, id: NodeId) -> Option<MemberDecl> {
    let node = tree.get(id);
    let NodeKind::Variable(v) = &node.kind else {
        return None;
    };
    let name = ident_name(tree, v.identifier)?;
    let kind = if v.property != PropertyStyle::None {
        MemberKind::Property
    } else {
        MemberKind::Var
    };
    Some(MemberDecl {
        name,
        kind,
        ty: type_expr(tree, v.datatype_specifier),
        params: Vec::new(),
        param_names: Vec::new(),
        flags: MemberFlags {
            is_static: v.is_static,
            exported: has_annotation(tree, &node.annotations, |n| n.starts_with("@export")),
            onready: has_annotation(tree, &node.annotations, |n| n == "@onready"),
            ..MemberFlags::default()
        },
        span: node.span,
        line: node.loc.start.line,
    })
}

fn const_member(tree: &ParseTree, id: NodeId) -> Option<MemberDecl> {
    let node = tree.get(id);
    let NodeKind::Constant(c) = &node.kind else {
        return None;
    };
    let name = ident_name(tree, c.identifier)?;
    Some(MemberDecl {
        name,
        kind: MemberKind::Const,
        ty: type_expr(tree, c.datatype_specifier),
        params: Vec::new(),
        param_names: Vec::new(),
        flags: MemberFlags::default(),
        span: node.span,
        line: node.loc.start.line,
    })
}

fn func_member(tree: &ParseTree, id: NodeId) -> Option<MemberDecl> {
    let node = tree.get(id);
    let NodeKind::Function(f) = &node.kind else {
        return None;
    };
    let name = ident_name(tree, f.identifier)?;
    let (params, param_names): (Vec<TypeExpr>, Vec<String>) = f
        .parameters
        .iter()
        .map(|&p| match &tree.get(p).kind {
            NodeKind::Parameter(pn) => (
                type_expr(tree, pn.datatype_specifier),
                ident_name(tree, pn.identifier)
                    .map(|n| n.to_owned())
                    .unwrap_or_default(),
            ),
            _ => (TypeExpr::None, String::new()),
        })
        .unzip();
    Some(MemberDecl {
        name,
        kind: MemberKind::Func,
        ty: type_expr(tree, f.return_type),
        params,
        param_names,
        flags: MemberFlags {
            is_static: f.is_static,
            is_abstract: has_annotation(tree, &node.annotations, |n| n == "@abstract"),
            is_coroutine: f.is_coroutine,
            ..MemberFlags::default()
        },
        span: node.span,
        line: node.loc.start.line,
    })
}

fn signal_member(tree: &ParseTree, id: NodeId) -> Option<MemberDecl> {
    let node = tree.get(id);
    let NodeKind::Signal(s) = &node.kind else {
        return None;
    };
    let name = ident_name(tree, s.identifier)?;
    let (params, param_names): (Vec<TypeExpr>, Vec<String>) = s
        .parameters
        .iter()
        .map(|&p| match &tree.get(p).kind {
            NodeKind::Parameter(pn) => (
                type_expr(tree, pn.datatype_specifier),
                ident_name(tree, pn.identifier)
                    .map(|n| n.to_owned())
                    .unwrap_or_default(),
            ),
            _ => (TypeExpr::None, String::new()),
        })
        .unzip();
    Some(MemberDecl {
        name,
        kind: MemberKind::Signal,
        ty: TypeExpr::None,
        params,
        param_names,
        flags: MemberFlags::default(),
        span: node.span,
        line: node.loc.start.line,
    })
}

fn enum_member(tree: &ParseTree, id: NodeId) -> Option<MemberDecl> {
    let node = tree.get(id);
    let NodeKind::Enum(e) = &node.kind else {
        return None;
    };
    // A nameless `enum { … }` is hoisted by the parser to individual `EnumValue` members instead, so
    // a `Member::Enum` always carries a name.
    let name = ident_name(tree, e.identifier)?;
    Some(MemberDecl {
        name,
        kind: MemberKind::Enum,
        ty: TypeExpr::None,
        params: Vec::new(),
        param_names: Vec::new(),
        flags: MemberFlags::default(),
        span: node.span,
        line: node.loc.start.line,
    })
}

/// Build the [`EnumDecl`] sidecar for a *named* enum member — the enum name plus the identifier
/// of every value inside it. Mirrors Godot's `EnumNode::values[i].identifier->name` walk
/// (parsed from `gdscript_parser.cpp::parse_enum`). The values' integer assignments are not
/// captured here — only their names, since the consumer set (cross-file enum-value attribute
/// resolution, e.g. `preload_enum_error.gd`'s `P.Named.VALUE_A`) only needs membership.
fn enum_decl(tree: &ParseTree, id: NodeId) -> Option<EnumDecl> {
    let NodeKind::Enum(e) = &tree.get(id).kind else {
        return None;
    };
    let name = ident_name(tree, e.identifier)?;
    let values = e
        .values
        .iter()
        .filter_map(|v| ident_name(tree, v.identifier))
        .collect();
    Some(EnumDecl { name, values })
}

fn enum_value_member(tree: &ParseTree, value: &EnumValue) -> Option<MemberDecl> {
    let id = value.identifier?;
    let name = ident_name(tree, Some(id))?;
    let node = tree.get(id);
    Some(MemberDecl {
        name,
        kind: MemberKind::Const,
        ty: TypeExpr::None,
        params: Vec::new(),
        param_names: Vec::new(),
        flags: MemberFlags::default(),
        span: node.span,
        line: node.loc.start.line,
    })
}

/// Decode a `datatype_specifier` / `return_type` (a [`NodeKind::Type`] node) into a [`TypeExpr`].
fn type_expr(tree: &ParseTree, opt: Option<NodeId>) -> TypeExpr {
    let Some(id) = opt else {
        return TypeExpr::None;
    };
    let NodeKind::Type(t) = &tree.get(id).kind else {
        return TypeExpr::None;
    };
    let path: Vec<String> = t
        .type_chain
        .iter()
        .filter_map(|&n| ident_name(tree, Some(n)))
        .collect();
    let args: Vec<TypeExpr> = t
        .container_types
        .iter()
        .map(|&n| type_expr(tree, Some(n)))
        .collect();
    if path.is_empty() && args.is_empty() {
        // An empty type node is a `void` return — nothing nameable for M2 resolution.
        TypeExpr::None
    } else {
        TypeExpr::Named { path, args }
    }
}

fn ident_name(tree: &ParseTree, opt: Option<NodeId>) -> Option<String> {
    let id = opt?;
    match &tree.get(id).kind {
        NodeKind::Identifier(i) => Some(i.name.clone()),
        _ => None,
    }
}

/// Whether any of `annotations` (a node's attached `@…` annotations) has a name satisfying `pred`.
/// The name is stored with its leading `@` (e.g. `"@export"`, `"@onready"`, `"@abstract"`).
fn has_annotation(tree: &ParseTree, annotations: &[NodeId], pred: impl Fn(&str) -> bool) -> bool {
    annotations
        .iter()
        .any(|&a| matches!(&tree.get(a).kind, NodeKind::Annotation(an) if pred(&an.name)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn iface(src: &str) -> Interface {
        extract(&gd_syntax::parse(src).tree)
    }

    #[test]
    fn class_name_and_extends_name() {
        let i = iface("@abstract\nclass_name Hero\nextends Node2D\n");
        assert_eq!(i.class_name.as_deref(), Some("Hero"));
        assert_eq!(i.extends, Extends::Names(vec!["Node2D".into()]));
        assert!(i.is_abstract);
    }

    #[test]
    fn extends_path_literal() {
        let i = iface("extends \"res://base.gd\"\n");
        assert_eq!(i.extends, Extends::Path("res://base.gd".into()));
        assert!(i.class_name.is_none());
    }

    #[test]
    fn extends_attribute_chain() {
        let i = iface("extends Outer.Inner\n");
        assert_eq!(
            i.extends,
            Extends::Names(vec!["Outer".into(), "Inner".into()])
        );
    }

    #[test]
    fn members_captured_with_kinds_and_types() {
        let src = "extends Node\n\
                   const MAX := 10\n\
                   var speed: float = 1.0\n\
                   var hp: int: get = _get_hp\n\
                   @export var name: String\n\
                   signal hit(amount: int)\n\
                   func move(dir: Vector2) -> void:\n\tpass\n\
                   enum State { IDLE, RUN }\n";
        let i = iface(src);
        let by = |n: &str| i.members.iter().find(|m| m.name == n).unwrap();

        assert_eq!(by("MAX").kind, MemberKind::Const);
        assert_eq!(by("speed").kind, MemberKind::Var);
        assert_eq!(by("speed").ty.head(), Some("float"));
        assert_eq!(by("hp").kind, MemberKind::Property); // has a getter
        assert!(by("name").flags.exported);
        assert_eq!(by("hit").kind, MemberKind::Signal);
        assert_eq!(
            by("hit").params.first().and_then(TypeExpr::head),
            Some("int")
        );
        assert_eq!(by("move").kind, MemberKind::Func);
        assert_eq!(
            by("move").params.first().and_then(TypeExpr::head),
            Some("Vector2")
        );
        assert_eq!(by("State").kind, MemberKind::Enum);
        // The named enum's values are reachable as `State.IDLE`, not as standalone members.
        assert!(i.members.iter().all(|m| m.name != "IDLE"));
    }

    #[test]
    fn unnamed_enum_values_become_constants() {
        let i = iface("extends Node\nenum { A, B, C }\n");
        for v in ["A", "B", "C"] {
            let m = i.members.iter().find(|m| m.name == v).unwrap();
            assert_eq!(m.kind, MemberKind::Const);
        }
    }

    #[test]
    fn typed_collection_keeps_container_arg() {
        let i = iface("extends Node\nvar items: Array[Enemy]\n");
        let items = &i.members[0];
        match &items.ty {
            TypeExpr::Named { path, args } => {
                assert_eq!(path, &["Array".to_string()]);
                assert_eq!(args.first().and_then(TypeExpr::head), Some("Enemy"));
            }
            TypeExpr::None => panic!("expected a typed array"),
        }
    }

    #[test]
    fn inner_class_captured_recursively() {
        let src = "extends Node\nclass Inner extends Resource:\n\tvar x: int\n";
        let i = iface(src);
        assert_eq!(i.inner.len(), 1);
        assert_eq!(i.inner[0].class_name.as_deref(), Some("Inner"));
        assert_eq!(i.inner[0].extends, Extends::Names(vec!["Resource".into()]));
        assert_eq!(i.inner[0].members[0].name, "x");
    }

    #[test]
    fn body_only_edit_keeps_signature_hash() {
        // Same signatures, different function bodies ⇒ identical signature_hash (the WP-E body-only
        // case). The two sources differ only inside `move`.
        let a = iface("extends Node\nfunc move() -> void:\n\tpass\n");
        let b = iface("extends Node\nfunc move() -> void:\n\tprint(\"moved a lot\")\n\treturn\n");
        assert_eq!(a.signature_hash(), b.signature_hash());
    }

    #[test]
    fn signature_change_changes_hash() {
        let a = iface("extends Node\nfunc move(x: int) -> void:\n\tpass\n");
        let b = iface("extends Node\nfunc move(x: float) -> void:\n\tpass\n"); // param type changed
        assert_ne!(a.signature_hash(), b.signature_hash());
    }

    #[test]
    fn empty_or_partial_tree_is_default() {
        assert_eq!(extract(&ParseTree::default()), Interface::default());
    }
}
