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
    /// `func` members: whether the declaration has a rest parameter (`func f(a, ...rest)`). A
    /// vararg method accepts any number of trailing arguments, so a cross-file caller must
    /// suppress the too-many arity check — the in-file path reads the same bit via
    /// `FunctionNode::rest_parameter`. Hashed: gaining/losing varargs changes call compatibility.
    pub is_vararg: bool,
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
    /// `func` members: how many parameters have NO default value (the call-site arity minimum;
    /// `mirror_array(arr, callable := …)` requires 1). Equals `params.len()` for everything
    /// else. Hashed — a default added/removed changes call compatibility.
    pub required_params: usize,
    pub flags: MemberFlags,
    /// Byte range of the declaration. **Excluded from [`Interface::signature_hash`]** so that a
    /// body-only edit (which shifts later members' spans) does not look like an interface change.
    pub span: ByteSpan,
    /// Byte range of the declaration's NAME identifier — narrower than [`Self::span`], which
    /// covers the whole declaration node. Anchors `workspace/symbol` results and cross-file
    /// `definition` jumps on the name token instead of the full declaration. Extraction always
    /// records the identifier node's span (a member without an identifier is never extracted);
    /// zero-width only in defensively-constructed values, so consumers must validate against the
    /// live text and fall back to [`Self::span`]. **Excluded from [`Interface::signature_hash`]**
    /// like [`Self::span`].
    pub name_span: ByteSpan,
    /// M7 (#62): the member's `##` doc comment, when present. **Excluded from
    /// [`Interface::signature_hash`]** like the spans: a doc-only edit re-analyzes the file
    /// itself (the epoch bump) but never invalidates dependents — they read the live
    /// `Interface` for hover prose, so docs stay fresh without reverse-dependency churn.
    pub doc: Option<Box<gd_syntax::doc_comments::MemberDoc>>,
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
    pub values: Vec<EnumValueDecl>,
}

/// One value of a named enum: its identifier plus the integer it is syntactically known to hold.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct EnumValueDecl {
    pub name: String,
    /// M7 (#62): the value's `##` doc comment. Excluded from `signature_hash` (docs are not
    /// interface-relevant for invalidation).
    pub doc: Option<Box<gd_syntax::doc_comments::MemberDoc>>,
    /// The value's integer when the extractor can read it without evaluation: an int literal, a
    /// negated int literal, or the implicit previous-value-plus-one chain (Godot resolves these
    /// in the analyzer, `gdscript_analyzer.cpp:1150-1197`; this parser-only pass follows the same
    /// chain for literal assignments). `None` when the assigned expression needs evaluation
    /// (`A = compute()`, `B = FLAG | 2`) — and every later implicit value in the same enum is then
    /// also unknown. Consumers must degrade permissively on `None`: suppress value-dependent
    /// diagnostics, never guess.
    pub value: Option<i64>,
}

/// The shallow interface of one class: what it exposes, with no types resolved.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Interface {
    /// `class_name X` for the top-level class, or the declared name of an inner `class X:`.
    pub class_name: Option<String>,
    /// Where the `class_name` identifier sits: 1-based source line + its byte span. Lets the
    /// registry anchor `workspace/symbol` results and `definition` jumps at the declaration
    /// without re-parsing the file (#33). **Excluded from [`Self::signature_hash`]** like
    /// [`MemberDecl::span`]: an edit that only shifts the declaration line must not look like an
    /// interface change to dependents.
    pub class_name_loc: Option<(u32, ByteSpan)>,
    pub extends: Extends,
    pub is_abstract: bool,
    /// `@tool` annotation on the class. Godot's `ClassNode::is_tool` (set from the parser's
    /// `@tool` annotation walk at `gdscript_parser.cpp` annotation table). Cross-file consumer:
    /// MISSING_TOOL warning (see `gdscript_warning.cpp::get_message` for MISSING_TOOL —
    /// `"The base class script has the @tool annotation, but this script does not have it."`).
    pub is_tool: bool,
    pub icon_path: Option<String>,
    pub members: Vec<MemberDecl>,
    /// M7 (#62): the class's `##` doc comment (brief/description/tutorials). Excluded from
    /// [`Self::signature_hash`] — see [`MemberDecl::doc`].
    pub doc: Option<Box<gd_syntax::doc_comments::ClassDoc>>,
    /// Inner classes, recursively (reachable as `Outer.Inner`).
    pub inner: Vec<Interface>,
    /// Named enums + their value identifiers. Reachable as `Self.<EnumName>.<value>` or
    /// (cross-file) `<preload_const>.<EnumName>.<value>`.
    pub enums: Vec<EnumDecl>,
    /// Names of constants hoisted from *unnamed* `enum { … }` blocks. The hoisted members appear
    /// in [`Self::members`] as ordinary `MemberKind::Const` entries (Godot hoists them the same
    /// way); this list is what lets a cross-file consumer tell an anonymous-enum value apart from
    /// a regular `const` — typing a regular const as an enum value is exactly the
    /// `Cannot get property from enum value.` false-positive family.
    pub unnamed_enum_values: Vec<String>,
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
    /// #255: every identifier this file *references* anywhere — function bodies included — with
    /// attribute segments (`d.Dep`) and Lua-style dictionary keys (`{ Dep = 1 }`) excluded, since
    /// neither names a symbol in scope (the same two exclusions the rename firewall applies,
    /// #181). Sorted and deduped.
    ///
    /// The `Interface` is otherwise the eager-shallow record of what a file *exposes*, and
    /// [`referenced_names`](crate::index) reads only that: `extends`, member annotations, parameter
    /// types. So a class used ONLY inside a body — `var d := Dep.new()` — produced no `DepGraph`
    /// edge, and editing `Dep` never invalidated the file that uses it. That is a real dependency
    /// (its call sites type-check against `Dep`'s members), so `Index::recompute_edges` resolves
    /// these through the `class_name` registry and adds the surviving ones as edges. Everything
    /// that does not name a project class is dropped there, so the over-capture costs nothing
    /// downstream.
    ///
    /// **Excluded from [`Interface::signature_hash`]** for the same reason as [`Self::preload_deps`]:
    /// this is *what this file depends on*, not *what it exposes*. It is deliberately NOT fed into
    /// the `name_referencers` index either — that set is the `references`/`rename` candidate
    /// fast-path, and filling it with every local's name would turn a cursor on an unresolvable
    /// identifier into a project-wide analysis.
    pub body_refs: Vec<String>,
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
        // self.class_name_loc is intentionally NOT hashed (a span, like MemberDecl::span).
        self.extends.hash(h);
        self.is_abstract.hash(h);
        self.is_tool.hash(h);
        self.icon_path.hash(h);
        for m in &self.members {
            m.name.hash(h);
            m.kind.hash(h);
            m.ty.hash(h);
            m.params.hash(h);
            m.required_params.hash(h);
            m.flags.hash(h);
            // m.span / m.name_span are intentionally NOT hashed.
        }
        for inner in &self.inner {
            inner.signature_hash().hash(h);
        }
        for e in &self.enums {
            // EnumValueDecl::value participates deliberately: explicit enum-value edits shift the
            // value-dependent diagnostics of dependents (INT_AS_ENUM_WITHOUT_MATCH,
            // ENUM_VARIABLE_WITHOUT_DEFAULT), so they must re-analyze.
            e.hash(h);
        }
        self.unnamed_enum_values.hash(h);
    }
}

/// Extract the interface of a parsed source. A partial/empty AST yields a default (empty) interface —
/// the parser always returns *something*, so extraction never fails (`docs/00`: never crash).
pub fn extract(tree: &ParseTree) -> Interface {
    let Some(root_id) = tree.root_id() else {
        return Interface::default();
    };
    let root = tree.get(root_id);
    let NodeKind::Class(class) = &root.kind else {
        return Interface::default();
    };
    let mut head = extract_class(tree, root_id, class, &root.annotations);
    // WP-RD12: capture this file's `preload`/`load` `res://` targets on the head interface so
    // `Index::recompute_edges` can turn them into `DepGraph` edges (the preload-const cross-file
    // cycle case). Walked once over the whole tree (a file-wide over-approximation of the const
    // initializers the WP-R2 cycle reaches through — additive edges, so over-capturing a
    // body-level preload only ever invalidates a consumer slightly more eagerly, never less).
    head.preload_deps = collect_preload_deps(tree);
    // #255: the body-level reference scan, likewise on the head interface only (the `DepGraph` is
    // per-file, so an inner class's references roll up).
    head.body_refs = collect_body_refs(tree);
    head
}

/// #255: every identifier name the file references, minus the two positions that only *look* like
/// references — the trailing ident of an attribute chain (`obj.x`, a member of some other type) and
/// a Lua-style dictionary key (`{ x = v }`, folded to a string literal). Those are exactly the
/// exclusions `ParseTree::ident_is_non_local_position` applies for rename/highlight (#181); this
/// runs them as two linear arena passes instead of that helper's per-candidate pass, because
/// extraction visits every identifier, not a handful.
///
/// Declaration identifiers (a `var`/`func`/parameter's own name) and local uses are kept: telling
/// them apart from a class reference needs scope resolution, which is the analyzer's job, not the
/// shallow pass's. `Index::recompute_edges` filters the result through the `class_name` registry,
/// so a name that isn't a project class is discarded without ever reaching an edge.
fn collect_body_refs(tree: &ParseTree) -> Vec<String> {
    use gd_syntax::ast::{DictStyle, SubscriptAccess};
    let mut excluded: rustc_hash::FxHashSet<NodeId> = rustc_hash::FxHashSet::default();
    for id in tree.iter_ids() {
        match &tree.get(id).kind {
            NodeKind::Subscript(s) => {
                if let Some(SubscriptAccess::Attribute(Some(aid))) = s.access {
                    excluded.insert(aid);
                }
            }
            // `style == None` is the single-element ambiguous case, parsed Lua-style.
            NodeKind::Dictionary(d) if matches!(d.style, Some(DictStyle::LuaTable) | None) => {
                excluded.extend(d.elements.iter().filter_map(|kv| kv.key));
            }
            _ => {}
        }
    }
    let mut names: Vec<String> = tree
        .iter_ids()
        .filter(|id| !excluded.contains(id))
        .filter_map(|id| match &tree.get(id).kind {
            NodeKind::Identifier(i) => Some(i.name.clone()),
            _ => None,
        })
        .collect();
    names.sort_unstable();
    names.dedup();
    names
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
fn extract_class(
    tree: &ParseTree,
    class_id: NodeId,
    class: &ClassNode,
    annotations: &[NodeId],
) -> Interface {
    let mut members = Vec::new();
    let mut inner = Vec::new();
    let mut enums = Vec::new();
    let mut unnamed_enum_values = Vec::new();
    for member in &class.members {
        match member {
            Member::Class(id) => {
                let node = tree.get(*id);
                if let NodeKind::Class(c) = &node.kind {
                    inner.push(extract_class(tree, *id, c, &node.annotations));
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
            // A value of an *unnamed* enum is hoisted to a class constant; remember its name so
            // cross-file consumers can tell it apart from a regular `const`.
            Member::EnumValue(value) => {
                if let Some(m) = enum_value_member(tree, value) {
                    unnamed_enum_values.push(m.name.clone());
                    members.push(m);
                }
            }
            // `@export_group`/category/subgroup — presentation only, not an exposed name.
            Member::Group(_) => {}
        }
    }

    Interface {
        class_name: ident_name(tree, class.identifier),
        class_name_loc: class.identifier.map(|id| {
            let n = tree.get(id);
            (n.loc.start.line, n.span)
        }),
        extends: extends_of(tree, class),
        is_abstract: has_annotation(tree, annotations, |n| n == "@abstract"),
        is_tool: has_annotation(tree, annotations, |n| n == "@tool"),
        icon_path: class.icon_path.clone(),
        members,
        doc: tree.docs.class_docs.get(&class_id).cloned().map(Box::new),
        inner,
        enums,
        unnamed_enum_values,
        // WP-RD12: populated only on the head interface by `extract` (the DepGraph is per-file);
        // inner classes' preloads roll up there.
        preload_deps: Vec::new(),
        // #255: likewise head-interface-only, populated by `extract`.
        body_refs: Vec::new(),
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

/// M7 (#62): the associated `##` doc for a declaration node, boxed for the common no-doc case.
fn member_doc(tree: &ParseTree, id: NodeId) -> Option<Box<gd_syntax::doc_comments::MemberDoc>> {
    tree.docs.member_docs.get(&id).cloned().map(Box::new)
}

fn var_member(tree: &ParseTree, id: NodeId) -> Option<MemberDecl> {
    let node = tree.get(id);
    let NodeKind::Variable(v) = &node.kind else {
        return None;
    };
    let ident_id = v.identifier?;
    let name = ident_name(tree, Some(ident_id))?;
    let kind = if v.property != PropertyStyle::None {
        MemberKind::Property
    } else {
        MemberKind::Var
    };
    let mut ty = type_expr(tree, v.datatype_specifier);
    if matches!(ty, TypeExpr::None) {
        // `var x := <literal/constructor/builtin-constant>` — capture the syntactically-obvious
        // type so cross-file consumers see `int`/`Color`/… instead of soft Variant (Godot's
        // full analysis infers these; the shallow interface can read the simple shapes).
        ty = initializer_type_expr(tree, v.initializer);
    }
    Some(MemberDecl {
        name,
        kind,
        ty,
        params: Vec::new(),
        param_names: Vec::new(),
        required_params: 0,
        flags: MemberFlags {
            is_static: v.is_static,
            exported: has_annotation(tree, &node.annotations, |n| n.starts_with("@export")),
            onready: has_annotation(tree, &node.annotations, |n| n == "@onready"),
            ..MemberFlags::default()
        },
        span: node.span,
        name_span: tree.get(ident_id).span,
        line: node.loc.start.line,
        doc: member_doc(tree, id),
    })
}

fn const_member(tree: &ParseTree, id: NodeId) -> Option<MemberDecl> {
    let node = tree.get(id);
    let NodeKind::Constant(c) = &node.kind else {
        return None;
    };
    let ident_id = c.identifier?;
    let name = ident_name(tree, Some(ident_id))?;
    let mut ty = type_expr(tree, c.datatype_specifier);
    if matches!(ty, TypeExpr::None) {
        ty = initializer_type_expr(tree, c.initializer);
    }
    Some(MemberDecl {
        name,
        kind: MemberKind::Const,
        ty,
        params: Vec::new(),
        param_names: Vec::new(),
        required_params: 0,
        flags: MemberFlags::default(),
        span: node.span,
        name_span: tree.get(ident_id).span,
        line: node.loc.start.line,
        doc: member_doc(tree, id),
    })
}

fn func_member(tree: &ParseTree, id: NodeId) -> Option<MemberDecl> {
    let node = tree.get(id);
    let NodeKind::Function(f) = &node.kind else {
        return None;
    };
    let ident_id = f.identifier?;
    let name = ident_name(tree, Some(ident_id))?;
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
    let defaulted = f
        .parameters
        .iter()
        .filter(|&&p| match &tree.get(p).kind {
            NodeKind::Parameter(pn) => pn.initializer.is_some(),
            _ => false,
        })
        .count();
    let required_params = params.len().saturating_sub(defaulted);
    Some(MemberDecl {
        name,
        kind: MemberKind::Func,
        ty: type_expr(tree, f.return_type),
        params,
        param_names,
        required_params,
        flags: MemberFlags {
            is_static: f.is_static,
            is_abstract: has_annotation(tree, &node.annotations, |n| n == "@abstract"),
            is_coroutine: f.is_coroutine,
            is_vararg: f.rest_parameter.is_some(),
            ..MemberFlags::default()
        },
        span: node.span,
        name_span: tree.get(ident_id).span,
        line: node.loc.start.line,
        doc: member_doc(tree, id),
    })
}

fn signal_member(tree: &ParseTree, id: NodeId) -> Option<MemberDecl> {
    let node = tree.get(id);
    let NodeKind::Signal(s) = &node.kind else {
        return None;
    };
    let ident_id = s.identifier?;
    let name = ident_name(tree, Some(ident_id))?;
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
    let required_params = params.len();
    Some(MemberDecl {
        name,
        kind: MemberKind::Signal,
        ty: TypeExpr::None,
        params,
        param_names,
        required_params,
        flags: MemberFlags::default(),
        span: node.span,
        name_span: tree.get(ident_id).span,
        line: node.loc.start.line,
        doc: member_doc(tree, id),
    })
}

fn enum_member(tree: &ParseTree, id: NodeId) -> Option<MemberDecl> {
    let node = tree.get(id);
    let NodeKind::Enum(e) = &node.kind else {
        return None;
    };
    // A nameless `enum { … }` is hoisted by the parser to individual `EnumValue` members instead, so
    // a `Member::Enum` always carries a name.
    let ident_id = e.identifier?;
    let name = ident_name(tree, Some(ident_id))?;
    Some(MemberDecl {
        name,
        kind: MemberKind::Enum,
        ty: TypeExpr::None,
        params: Vec::new(),
        param_names: Vec::new(),
        required_params: 0,
        flags: MemberFlags::default(),
        span: node.span,
        name_span: tree.get(ident_id).span,
        line: node.loc.start.line,
        doc: member_doc(tree, id),
    })
}

/// Build the [`EnumDecl`] sidecar for a *named* enum member — the enum name plus every value's
/// identifier and (when syntactically readable) its integer. Mirrors Godot's
/// `EnumNode::values[i]` walk: implicit values are previous + 1 (`gdscript_analyzer.cpp:
/// 1174-1177`); a custom value that is an int literal (optionally negated) is read directly;
/// anything needing evaluation yields `None` and poisons every later implicit value in the chain.
fn enum_decl(tree: &ParseTree, id: NodeId) -> Option<EnumDecl> {
    let NodeKind::Enum(e) = &tree.get(id).kind else {
        return None;
    };
    let name = ident_name(tree, e.identifier)?;
    let mut values = Vec::with_capacity(e.values.len());
    let mut prev: Option<i64> = Some(-1);
    for (i, v) in e.values.iter().enumerate() {
        let Some(value_name) = ident_name(tree, v.identifier) else {
            continue;
        };
        let value = match v.custom_value {
            None => prev.map(|p| p.wrapping_add(1)),
            Some(cv) => int_literal_value(tree, cv),
        };
        prev = value;
        values.push(EnumValueDecl {
            name: value_name,
            doc: tree
                .docs
                .enum_value_docs
                .get(&(id, i))
                .cloned()
                .map(Box::new),
            value,
        });
    }
    Some(EnumDecl { name, values })
}

/// Read an int literal (optionally under a single unary minus) without evaluation; anything else
/// is `None` — the extractor never folds expressions (that's the analyzer's job).
fn int_literal_value(tree: &ParseTree, id: NodeId) -> Option<i64> {
    use gd_syntax::ast::UnaryOp;
    use gd_syntax::token::Literal;
    match &tree.get(id).kind {
        NodeKind::Literal(l) => match l.value {
            Literal::Int(v) => Some(v),
            _ => None,
        },
        NodeKind::UnaryOp(u) if u.operation == UnaryOp::Negative => {
            match &tree.get(u.operand?).kind {
                NodeKind::Literal(l) => match l.value {
                    Literal::Int(v) => Some(v.wrapping_neg()),
                    _ => None,
                },
                _ => None,
            }
        }
        _ => None,
    }
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
        required_params: 0,
        flags: MemberFlags::default(),
        // A hoisted enum value IS its identifier — declaration span and name span coincide.
        span: node.span,
        name_span: node.span,
        line: node.loc.start.line,
        doc: member_doc(tree, id),
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

/// The syntactically-obvious type of a `:=` initializer, for members with no annotation: a
/// literal, an Array/Dictionary literal, a builtin constructor call (`Color(…)`), or a
/// builtin-class constant (`Color.PURPLE` — captured as the two-segment path so the analyzer can
/// consult the dump for the constant's REAL declared type; `Vector3.AXIS_X` is `int`). Anything
/// needing evaluation stays `TypeExpr::None` (soft Variant downstream). Godot's full analysis
/// infers all of these; the shallow interface reads only the unambiguous shapes.
fn initializer_type_expr(tree: &ParseTree, init: Option<NodeId>) -> TypeExpr {
    use gd_syntax::token::Literal;
    let named = |s: &str| TypeExpr::Named {
        path: vec![s.to_owned()],
        args: Vec::new(),
    };
    let Some(id) = init else {
        return TypeExpr::None;
    };
    match &tree.get(id).kind {
        NodeKind::Literal(l) => match l.value {
            Literal::Int(_) => named("int"),
            Literal::Float(_) => named("float"),
            Literal::Bool(_) => named("bool"),
            Literal::String(_) => named("String"),
            Literal::StringName(_) => named("StringName"),
            Literal::NodePath(_) => named("NodePath"),
            Literal::Null => TypeExpr::None,
        },
        NodeKind::Array(_) => named("Array"),
        NodeKind::Dictionary(_) => named("Dictionary"),
        NodeKind::Call(c) => {
            if is_builtin_type_name(&c.function_name) {
                named(&c.function_name)
            } else if c.function_name == "new" {
                // `X.new()` constructs an X — the everyday `var map := SelectionMap.new()`
                // member idiom. The callee is `X.new` (a Subscript over an identifier base).
                let base_name = c.callee.and_then(|cid| match &tree.get(cid).kind {
                    NodeKind::Subscript(sub) => sub.base.and_then(|b| match &tree.get(b).kind {
                        NodeKind::Identifier(i) => Some(i.name.clone()),
                        _ => None,
                    }),
                    _ => None,
                });
                match base_name {
                    Some(b) => named(&b),
                    None => TypeExpr::None,
                }
            } else {
                TypeExpr::None
            }
        }
        NodeKind::Subscript(sub) => {
            // `Builtin.CONSTANT` — record both segments; the analyzer resolves the constant's
            // declared type from the dump.
            let base_name = sub.base.and_then(|b| match &tree.get(b).kind {
                NodeKind::Identifier(i) => Some(i.name.clone()),
                _ => None,
            });
            let attr_name = match sub.access {
                Some(gd_syntax::ast::SubscriptAccess::Attribute(Some(a))) => {
                    match &tree.get(a).kind {
                        NodeKind::Identifier(i) => Some(i.name.clone()),
                        _ => None,
                    }
                }
                _ => None,
            };
            match (base_name, attr_name) {
                (Some(b), Some(a)) if is_builtin_type_name(&b) => TypeExpr::Named {
                    path: vec![b, a],
                    args: Vec::new(),
                },
                _ => TypeExpr::None,
            }
        }
        _ => TypeExpr::None,
    }
}

/// GDScript's builtin type-name set (`GDScriptParser::get_builtin_type`, minus `Nil`/`Object`).
/// Duplicated from the analyzer's table because gd_project must stay engine-free; the list is
/// frozen by the language.
fn is_builtin_type_name(name: &str) -> bool {
    matches!(
        name,
        "bool"
            | "int"
            | "float"
            | "String"
            | "Vector2"
            | "Vector2i"
            | "Rect2"
            | "Rect2i"
            | "Vector3"
            | "Vector3i"
            | "Transform2D"
            | "Vector4"
            | "Vector4i"
            | "Plane"
            | "Quaternion"
            | "AABB"
            | "Basis"
            | "Transform3D"
            | "Projection"
            | "Color"
            | "StringName"
            | "NodePath"
            | "RID"
            | "Callable"
            | "Signal"
            | "Dictionary"
            | "Array"
            | "PackedByteArray"
            | "PackedInt32Array"
            | "PackedInt64Array"
            | "PackedFloat32Array"
            | "PackedFloat64Array"
            | "PackedStringArray"
            | "PackedVector2Array"
            | "PackedVector3Array"
            | "PackedColorArray"
            | "PackedVector4Array"
    )
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
    fn enum_values_follow_literal_chain_and_poison_on_expressions() {
        let i = iface(
            "enum Mode { A, B = 5, C, D = -2, E }\nenum Hard { X = 1 << 3, Y, Z = 9 }\nenum { LOOSE, FREE }\nconst PLAIN := 1\n",
        );
        let mode = i.enums.iter().find(|e| e.name == "Mode").expect("Mode");
        let vals: Vec<(String, Option<i64>)> = mode
            .values
            .iter()
            .map(|v| (v.name.clone(), v.value))
            .collect();
        assert_eq!(
            vals,
            vec![
                ("A".into(), Some(0)),
                ("B".into(), Some(5)),
                ("C".into(), Some(6)),
                ("D".into(), Some(-2)),
                ("E".into(), Some(-1)),
            ]
        );
        // `1 << 3` needs evaluation — unknown, and it poisons the implicit `Y`; the explicit
        // literal `Z` recovers.
        let hard = i.enums.iter().find(|e| e.name == "Hard").expect("Hard");
        let vals: Vec<Option<i64>> = hard.values.iter().map(|v| v.value).collect();
        assert_eq!(vals, vec![None, None, Some(9)]);
        // Unnamed-enum hoists are tracked by name; a regular const is not.
        assert_eq!(i.unnamed_enum_values, vec!["LOOSE", "FREE"]);
        assert!(i.members.iter().any(|m| m.name == "PLAIN"));
    }

    #[test]
    fn class_name_and_extends_name() {
        let i = iface("@abstract\nclass_name Hero\nextends Node2D\n");
        assert_eq!(i.class_name.as_deref(), Some("Hero"));
        assert_eq!(i.extends, Extends::Names(vec!["Node2D".into()]));
        assert!(i.is_abstract);
    }

    #[test]
    fn class_name_loc_records_identifier_line_and_span() {
        // The common `extends`-first shape (#33): `class_name` sits on line 2, and the recorded
        // span covers exactly the `Hero` identifier bytes.
        let src = "extends Node2D\nclass_name Hero\n";
        let i = iface(src);
        let (line, span) = i.class_name_loc.expect("named class records its location");
        assert_eq!(line, 2);
        assert_eq!(&src[span.start..span.end], "Hero");
        // An anonymous script records none.
        assert!(iface("extends Node\n").class_name_loc.is_none());
    }

    #[test]
    fn class_name_loc_is_excluded_from_signature_hash() {
        // Shifting the declaration down a line moves the loc but must not look like an interface
        // change to dependents (the MemberDecl::span rule).
        let a = iface("class_name Hero\nextends Node2D\n");
        let b = iface("# moved\n\nclass_name Hero\nextends Node2D\n");
        assert_ne!(a.class_name_loc, b.class_name_loc);
        assert_eq!(a.signature_hash(), b.signature_hash());
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
    fn member_name_spans_cover_their_identifiers() {
        let src = "extends Node\n\
                   const MAX := 10\n\
                   var speed: float = 1.0\n\
                   var hp: int: get = _get_hp\n\
                   signal hit(amount: int)\n\
                   func move(dir: Vector2) -> void:\n\tpass\n\
                   enum State { IDLE, RUN }\n\
                   enum { LOOSE }\n\
                   class Inner extends Resource:\n\tvar x: int\n";
        let i = iface(src);
        assert!(!i.members.is_empty());
        for m in i.members.iter().chain(i.inner[0].members.iter()) {
            assert_eq!(
                &src[m.name_span.start..m.name_span.end],
                m.name,
                "name_span of `{}` must slice exactly its identifier",
                m.name
            );
            assert!(
                m.span.start <= m.name_span.start && m.name_span.end <= m.span.end,
                "name_span of `{}` must sit inside the declaration span",
                m.name
            );
        }
    }

    #[test]
    fn name_span_is_excluded_from_signature_hash() {
        // Shifting a member down a line moves its name_span but must not look like an interface
        // change to dependents (the MemberDecl::span rule).
        let a = iface("extends Node\nvar hp := 1\n");
        let b = iface("extends Node\n# moved\n\nvar hp := 1\n");
        assert_ne!(a.members[0].name_span, b.members[0].name_span);
        assert_eq!(a.signature_hash(), b.signature_hash());
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
    fn doc_only_edit_keeps_signature_hash() {
        // M7 (#62): docs are deliberately excluded from the hash — a doc edit re-analyzes only
        // the file itself (epoch bump) and never invalidates dependents, which read the live
        // Interface for hover prose anyway.
        let a = iface("## Old doc.\nvar speed := 1.0\n");
        let b = iface("## Completely rewritten doc.\nvar speed := 1.0\n");
        assert_ne!(a.members[0].doc, b.members[0].doc, "docs extracted");
        assert_eq!(a.signature_hash(), b.signature_hash());
    }

    #[test]
    fn extraction_populates_class_member_and_enum_value_docs() {
        let i = iface(
            "## The class brief.\nclass_name Doc\nextends Node\n\n## Member doc.\nvar x := 1\n\nenum E {\n\t## Value doc.\n\tA,\n}\n",
        );
        assert_eq!(i.doc.as_ref().expect("class doc").brief, "The class brief.");
        let member = i.members.iter().find(|m| m.name == "x").expect("x");
        assert_eq!(
            member.doc.as_ref().expect("member doc").description,
            "Member doc."
        );
        let e = i.enums.iter().find(|e| e.name == "E").expect("enum E");
        assert_eq!(
            e.values[0].doc.as_ref().expect("value doc").description,
            "Value doc."
        );
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
