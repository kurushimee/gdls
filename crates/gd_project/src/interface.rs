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
    /// `extends "res://path.gd"` — a path literal, verbatim, plus the name segments after it.
    /// `extends "res://x.gd".Inner` is legal GDScript and names the inner class, not the file's
    /// head class, so the segments have to ride along or every consumer resolves the wrong one
    /// (#388). Empty for the common bare-path form.
    Path { path: String, segments: Vec<String> },
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

    /// The type as written: `Array`, `Array[Entry]`, `Dictionary[String, int]`, `Outer.Inner`.
    /// `None` for [`TypeExpr::None`], so a caller can pick its own word for "no annotation"
    /// (`void` at a return position, nothing at a declaration).
    ///
    /// #307: every consumer used to render `path.join(".")`, which silently dropped `args` — so a
    /// declaration hovered as `var entries: Array` while a USE of the same variable hovered as
    /// `Array[Entry]`, and `documentSymbol` showed the lossy one. There is one renderer now,
    /// because the element type is exactly the part a reader needs and the part that was easiest
    /// to lose one call site at a time.
    #[must_use]
    pub fn render(&self) -> Option<String> {
        let TypeExpr::Named { path, args } = self else {
            return None;
        };
        let base = path.join(".");
        if args.is_empty() {
            return Some(base);
        }
        // An arg with no annotation of its own can only come from a malformed `Array[]`; render it
        // as `Variant`, the type Godot gives an unannotated element.
        let rendered: Vec<String> = args
            .iter()
            .map(|a| a.render().unwrap_or_else(|| "Variant".to_owned()))
            .collect();
        Some(format!("{base}[{}]", rendered.join(", ")))
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
    /// `var` members whose [`MemberDecl::ty`] was read off a plain `=` initializer rather than an
    /// annotation or a `:=`. Godot gives those a SOFT type — `INFERRED`, not `ANNOTATED_INFERRED`
    /// (`gdscript_analyzer.cpp` `resolve_assignable`, the `!has_specified_type` arm) — and a soft
    /// type is excused from the checks a hard one has to pass: no `Cannot assign a value of type
    /// X`, no `UNSAFE_PROPERTY_ACCESS` on a member miss. Without this bit a cross-file reader
    /// hardens every inferred member and reports things Godot does not. `const` never sets it:
    /// a constant is `ANNOTATED_INFERRED` whether or not it was written with `:=`.
    pub ty_is_soft: bool,
}

/// What an untyped member's initializer names, when [`initializer_type_expr`] could not decide a
/// type from the syntax alone. The shallow pass has no analyzer under it, so it cannot evaluate
/// `make()` or `E.A` — but it can record *what was written*, and the reading file's analyzer can
/// resolve that against the declaring class the same lazy way it resolves an annotation.
///
/// Only shapes with a single reading are captured. Anything that could mean two things — an
/// index, an argument-dependent call, a chain through a value — is left out, because a wrong
/// cross-file type is worse than no cross-file type (#431).
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum InitShape {
    /// Segments read as a value. With no `base` this is a dotted identifier chain —
    /// `SOME_CONST`, `E.A`, `Other.KONST`, `SomeAutoload.level`. With a `base` the segments are
    /// read off whatever that shape resolves to: `preload("x.gd").KONST`.
    Read {
        base: Option<Box<InitShape>>,
        path: Vec<String>,
    },
    /// A call. The last segment of `path` is the method; earlier ones are reads leading to it.
    /// With no `base` the chain starts at a name — `make()`, `Other.make()`. With a `base` the
    /// call is addressed off another shape's result: `OS.get_data_dir().path_join(x)`. The
    /// arguments are never captured — the type comes from the function's declared return.
    Call {
        base: Option<Box<InitShape>>,
        path: Vec<String>,
    },
    /// `preload("x.gd")`, and `preload("x.gd").new()` when `construct` is set. The path is the
    /// literal as written — `res://` or relative to the declaring file, the same two forms
    /// `CrossFileQuery::resolve_path_from` resolves and `preload_deps` carries an edge for.
    Preload { path: String, construct: bool },
}

/// How deep a nested shape may go before the whole capture is refused.
///
/// The bound has to sit at capture, not just at resolve: extraction runs eagerly at startup on
/// every `.gd`, so a generated `a().b().c()…` chain would otherwise walk the tree unboundedly.
/// Refusing the WHOLE shape rather than truncating it is the load-bearing half — a shape missing
/// its root resolves off the wrong thing, which is the one outcome worse than no shape at all.
/// The cap is also far under serde's own recursion limit, so a captured shape always round-trips
/// through the cache.
pub const INIT_SHAPE_MAX_DEPTH: usize = 16;

/// How a `func` parameter got its type — what a [`TypeExpr`] alone cannot say. Godot routes a
/// parameter through `resolve_assignable` (`gdscript_analyzer.cpp:2255-2258`), whose
/// no-specified-type arm stamps `ANNOTATED_INFERRED` when the declaration used `:=` and plain
/// `INFERRED` otherwise. Hardness gates the `Invalid argument` error and `UNSAFE_CALL_ARGUMENT`'s
/// second arm, so it has to cross the seam; so does the difference between a parameter that is
/// genuinely untyped and one whose default the shallow pass simply could not read.
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize,
)]
pub enum ParamTyping {
    /// `a: String` — a written annotation. Hard.
    Annotated,
    /// `a := ""` — inferred from the default, `ANNOTATED_INFERRED`. Hard.
    InferredHard,
    /// `a = ""` — inferred from the default, plain `INFERRED`. Soft.
    InferredSoft,
    /// `a` — no annotation and no default. A soft `Variant`, and Godot says so: a call passing
    /// anything into it draws `requires the subtype "Variant"`.
    #[default]
    Untyped,
    /// A default the shallow pass could not decode (`a := TileSet.TILE_SHAPE_SQUARE`). The shape
    /// of that default rides along in [`MemberDecl::param_inits`] for the analyzer to resolve at
    /// the seam (#528); `hard` is the `:=`-versus-`=` split the resolved type needs, since the
    /// `TypeExpr` that would otherwise carry it is `None` here. A slot that still resolves to
    /// nothing degrades to "no type" rather than to `Variant` — claiming `Variant` would render
    /// the wrong name in a warning that is otherwise correct to fire.
    Unknown { hard: bool },
}

/// One exposed member of a class.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemberDecl {
    pub name: String,
    pub kind: MemberKind,
    /// The declared type (a `var`/`const`'s annotation, or a `func`'s return type).
    pub ty: TypeExpr,
    /// Parameter types for `func`/`signal` members; empty otherwise. A parameter with no
    /// annotation but a default value carries what the shallow pass could read off that default
    /// ([`initializer_type_expr`], the same decode `var_member` uses), so `f(a := "")` crosses as
    /// `String` rather than as nothing.
    pub params: Vec<TypeExpr>,
    /// Parallel to [`Self::params`]: how that parameter got its type, which a `TypeExpr` alone
    /// cannot say. Empty for non-func/signal members.
    pub params_typing: Vec<ParamTyping>,
    /// Parallel to [`Self::params`]: the default's SHAPE, for the slots where the shallow decode
    /// read nothing. `None` everywhere else, and empty for non-func members — a signal parameter
    /// cannot carry a default. Members took this road first ([`Self::init`]); parameters follow it
    /// so every shape the analyzer's seam resolves reaches a defaulted parameter too (#528).
    pub param_inits: Vec<Option<Box<InitShape>>>,
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
    /// What the initializer named, for a `var`/`const` whose [`Self::ty`] came out
    /// [`TypeExpr::None`] — the reader resolves it lazily (#431). `None` whenever `ty` already
    /// has an answer, and boxed because almost every member has neither. **Hashed**: swapping
    /// `make()` for `other()` changes what dependents compute, and nothing else in the interface
    /// would show it.
    pub init: Option<Box<InitShape>>,
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
    /// 1-based source line of the value's identifier, and the identifier's own byte span. The same
    /// anchor pair [`MemberDecl`] carries, and for the same reason: `workspace/symbol` reports an
    /// enum value at its own declaration rather than at the enum's (#305). **Excluded from
    /// [`Interface::signature_hash`]**, like every other span — an edit that only shifts a line
    /// must not read as an interface change to dependents.
    pub line: u32,
    /// See [`Self::line`].
    pub name_span: ByteSpan,
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
    /// WP-RD12: the literal paths this file `preload(...)`s / `load(...)`s — `res://` and
    /// relative alike, resolved by `Index::recompute_edges` against the file that wrote them. M2
    /// deliberately excluded these as "body-level" edges, but a cross-file member-initializer cycle (WP-R2) reaches its
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
    /// identifier into a project-wide analysis. `Index::relink_referencers` does SCAN it (#519), so
    /// a `class_name` appearing still reaches a file that only used the name in a body, without that
    /// file joining the rename fast-path. The scan binary-searches, so keep this sorted and deduped.
    pub body_refs: Vec<String>,
    /// `true` when the parse this interface was extracted from reported no syntax errors, so the
    /// member list is the complete set of declarations the source has.
    ///
    /// A broken parse extracts a **truncated** interface — the declarations after the error are
    /// simply gone — which is fine for every consumer that reads what IS here, and fatal for one
    /// that reads absence as proof. `gd_analyze`'s chain walk carries this into its
    /// "the name exists nowhere on this chain" claim; nothing else reads it.
    ///
    /// Defaults to `false`, the safe answer for a negative claim, so a defensively-constructed
    /// interface never grants evidence it does not have. Set on the head **and** on every `inner`
    /// interface: one file, one parse. Hashed — a base flipping broken↔clean changes what its
    /// dependents may report, so they must re-analyze.
    pub parse_clean: bool,
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
            m.params_typing.hash(h);
            m.param_inits.hash(h);
            m.required_params.hash(h);
            m.flags.hash(h);
            m.init.hash(h);
            // m.span / m.name_span are intentionally NOT hashed.
        }
        for inner in &self.inner {
            inner.signature_hash().hash(h);
        }
        for e in &self.enums {
            e.name.hash(h);
            for v in &e.values {
                v.name.hash(h);
                // EnumValueDecl::value participates deliberately: explicit enum-value edits shift
                // the value-dependent diagnostics of dependents (INT_AS_ENUM_WITHOUT_MATCH,
                // ENUM_VARIABLE_WITHOUT_DEFAULT), so they must re-analyze.
                v.value.hash(h);
                // v.line / v.name_span are intentionally NOT hashed (spans, like MemberDecl::span);
                // neither is v.doc (docs are not interface-relevant for invalidation).
            }
        }
        self.unnamed_enum_values.hash(h);
        self.parse_clean.hash(h);
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
    // One file, one parse: the head and every inner class share the same completeness.
    stamp_parse_clean(&mut head, !tree.had_parse_errors);
    head
}

/// Set [`Interface::parse_clean`] on `iface` and, recursively, on its inner classes.
fn stamp_parse_clean(iface: &mut Interface, clean: bool) {
    iface.parse_clean = clean;
    for inner in &mut iface.inner {
        stamp_parse_clean(inner, clean);
    }
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
                // Both forms: `res://…` and a path relative to the referring file. The index
                // resolves each against the file that wrote it, and a path that resolves to
                // nothing simply carries no edge.
                deps.push(s.clone());
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
        // Stamped for the whole file by `extract`, head and inner alike.
        parse_clean: false,
    }
}

fn extends_of(tree: &ParseTree, class: &ClassNode) -> Extends {
    let names: Vec<String> = class
        .extends
        .iter()
        .filter_map(|&id| ident_name(tree, Some(id)))
        .collect();
    if let Some(path) = &class.extends_path {
        // The parser stores the whole chain in `extends` regardless of the head's shape, so with
        // a path head every name in it is a segment hanging off the loaded script.
        return Extends::Path {
            path: path.clone(),
            segments: names,
        };
    }
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
    // `var x = expr` reads the initializer the same way `var x := expr` does, but the answer is
    // soft: Godot only hardens the inferred type when `:=` asked for it.
    let ty_is_soft = v.datatype_specifier.is_none() && !v.infer_datatype;
    let init = matches!(ty, TypeExpr::None)
        .then(|| capture_init_shape(tree, v.initializer))
        .flatten();
    Some(MemberDecl {
        name,
        kind,
        ty,
        params: Vec::new(),
        params_typing: Vec::new(),
        param_inits: Vec::new(),
        param_names: Vec::new(),
        required_params: 0,
        flags: MemberFlags {
            is_static: v.is_static,
            exported: has_annotation(tree, &node.annotations, |n| n.starts_with("@export")),
            onready: has_annotation(tree, &node.annotations, |n| n == "@onready"),
            ty_is_soft,
            ..MemberFlags::default()
        },
        span: node.span,
        name_span: tree.get(ident_id).span,
        line: node.loc.start.line,
        doc: member_doc(tree, id),
        init,
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
    let init = matches!(ty, TypeExpr::None)
        .then(|| capture_init_shape(tree, c.initializer))
        .flatten();
    Some(MemberDecl {
        name,
        kind: MemberKind::Const,
        ty,
        params: Vec::new(),
        params_typing: Vec::new(),
        param_inits: Vec::new(),
        param_names: Vec::new(),
        required_params: 0,
        flags: MemberFlags::default(),
        span: node.span,
        name_span: tree.get(ident_id).span,
        line: node.loc.start.line,
        doc: member_doc(tree, id),
        init,
    })
}

fn func_member(tree: &ParseTree, id: NodeId) -> Option<MemberDecl> {
    let node = tree.get(id);
    let NodeKind::Function(f) = &node.kind else {
        return None;
    };
    let ident_id = f.identifier?;
    let name = ident_name(tree, Some(ident_id))?;
    // A parameter with no annotation still has a type when it has a default: Godot resolves it
    // through `resolve_assignable`, exactly as it resolves an untyped `var`'s (#451). Read the
    // default with the same shallow decode the member path uses, and record whether `:=` or a
    // plain `=` wrote it — the first is `ANNOTATED_INFERRED` (hard), the second `INFERRED` (soft).
    let mut params: Vec<TypeExpr> = Vec::with_capacity(f.parameters.len());
    let mut param_names: Vec<String> = Vec::with_capacity(f.parameters.len());
    let mut params_typing: Vec<ParamTyping> = Vec::with_capacity(f.parameters.len());
    // Only the slots the shallow decode could not read carry a shape; every other slot already has
    // its answer in `params`, and capturing one there would be a second source of truth.
    let mut param_inits: Vec<Option<Box<InitShape>>> = Vec::with_capacity(f.parameters.len());
    for &p in &f.parameters {
        let (ty, name, typing, init) = match &tree.get(p).kind {
            NodeKind::Parameter(pn) => {
                let annotated = type_expr(tree, pn.datatype_specifier);
                let (ty, typing) = if annotated != TypeExpr::None {
                    (annotated, ParamTyping::Annotated)
                } else if pn.initializer.is_some() {
                    match initializer_type_expr(tree, pn.initializer) {
                        // `f(a = null)` is not an unread type — Godot resolves `null` to a plain
                        // soft `Variant`, the same answer a bare `f(a)` gets, and the argument
                        // check names it that way.
                        TypeExpr::None if is_null_literal(tree, pn.initializer) => {
                            (TypeExpr::None, ParamTyping::Untyped)
                        }
                        TypeExpr::None => (
                            TypeExpr::None,
                            ParamTyping::Unknown {
                                hard: pn.infer_datatype,
                            },
                        ),
                        t if pn.infer_datatype => (t, ParamTyping::InferredHard),
                        t => (t, ParamTyping::InferredSoft),
                    }
                } else {
                    (TypeExpr::None, ParamTyping::Untyped)
                };
                let init = matches!(typing, ParamTyping::Unknown { .. })
                    .then(|| capture_init_shape(tree, pn.initializer))
                    .flatten();
                (
                    ty,
                    ident_name(tree, pn.identifier)
                        .map(|n| n.to_owned())
                        .unwrap_or_default(),
                    typing,
                    init,
                )
            }
            _ => (TypeExpr::None, String::new(), ParamTyping::Untyped, None),
        };
        params.push(ty);
        param_names.push(name);
        params_typing.push(typing);
        param_inits.push(init);
    }
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
        params_typing,
        param_inits,
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
        init: None,
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
        // A signal parameter cannot carry a default, so it is annotated or nothing — and it
        // carries no shape either.
        param_inits: Vec::new(),
        params_typing: params
            .iter()
            .map(|t| {
                if *t == TypeExpr::None {
                    ParamTyping::Untyped
                } else {
                    ParamTyping::Annotated
                }
            })
            .collect(),
        params,
        param_names,
        required_params,
        flags: MemberFlags::default(),
        span: node.span,
        name_span: tree.get(ident_id).span,
        line: node.loc.start.line,
        doc: member_doc(tree, id),
        init: None,
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
        params_typing: Vec::new(),
        param_inits: Vec::new(),
        param_names: Vec::new(),
        required_params: 0,
        flags: MemberFlags::default(),
        span: node.span,
        name_span: tree.get(ident_id).span,
        line: node.loc.start.line,
        doc: member_doc(tree, id),
        init: None,
    })
}

/// Build the [`EnumDecl`] sidecar for a *named* enum member — the enum name plus every value's
/// identifier and (when syntactically readable) its integer. Mirrors Godot's
/// `EnumNode::values[i]` walk: implicit values are previous + 1 (`gdscript_analyzer.cpp:
/// 1174-1177`); a custom value that is an int literal (optionally negated) is read directly;
/// anything needing evaluation yields `None` and poisons every later implicit value in the chain.
///
/// Public because hover reads it straight off the tree of the file being edited, where the indexed
/// interface may be a revision behind the buffer.
pub fn enum_decl(tree: &ParseTree, id: NodeId) -> Option<EnumDecl> {
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
        // `ident_name` above already proved the identifier resolves, so this cannot fail.
        let ident = v.identifier.map(|i| tree.get(i));
        values.push(EnumValueDecl {
            name: value_name,
            doc: tree
                .docs
                .enum_value_docs
                .get(&(id, i))
                .cloned()
                .map(Box::new),
            value,
            line: ident.map(|n| n.loc.start.line).unwrap_or_default(),
            name_span: ident.map(|n| n.span).unwrap_or_default(),
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
        params_typing: Vec::new(),
        param_inits: Vec::new(),
        param_names: Vec::new(),
        required_params: 0,
        flags: MemberFlags::default(),
        // A hoisted enum value IS its identifier — declaration span and name span coincide.
        span: node.span,
        name_span: node.span,
        line: node.loc.start.line,
        doc: member_doc(tree, id),
        init: None,
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
/// literal, an Array/Dictionary literal, a builtin constructor call (`Color(…)`), a
/// builtin-class constant (`Color.PURPLE` — captured as the two-segment path so the analyzer can
/// consult the dump for the constant's REAL declared type; `Vector3.AXIS_X` is `int`), a
/// constructor call (`A.new()`, `A.B.new()` — the whole attribute chain, so an inner class keeps
/// its path), a cast (`x as T` — the cast's own type node, which is as declared as an
/// annotation), or a node lookup (`$Path` / `%Unique` — a bare `Node`, per the `$`/`%` convention
/// in `docs/02` §11). Anything needing evaluation stays `TypeExpr::None` (soft Variant
/// downstream). Godot's full analysis infers all of these; the shallow interface reads only the
/// unambiguous shapes.
///
/// #431: this is the floor a cross-file member's type falls to. Every shape missing here reads as
/// `Variant` from another file while Godot has a real type, which silences the member access on
/// it and then everything downstream of that.
/// Whether `init` is the literal `null`. [`initializer_type_expr`] answers `TypeExpr::None` for it
/// the same way it does for anything it cannot read, but the two mean different things to a
/// parameter: `null` really is a soft `Variant`, while an unread initializer is a type the
/// declaring file has and gdls does not.
fn is_null_literal(tree: &ParseTree, init: Option<NodeId>) -> bool {
    init.is_some_and(|id| {
        matches!(&tree.get(id).kind, NodeKind::Literal(l) if l.value == gd_syntax::token::Literal::Null)
    })
}

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
                // member idiom. The callee is `X.new` (a Subscript over the class), and the class
                // itself may be a dotted chain (`Outer.Inner.new()`), which the analyzer resolves
                // segment by segment the same way it resolves the annotation `Outer.Inner`.
                let path = c
                    .callee
                    .and_then(|cid| match &tree.get(cid).kind {
                        NodeKind::Subscript(sub) => sub.base,
                        _ => None,
                    })
                    .and_then(|b| attribute_path(tree, b));
                match path {
                    Some(path) => TypeExpr::Named {
                        path,
                        args: Vec::new(),
                    },
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
        // `x as T` — the cast names its type outright, so it is worth exactly what an annotation
        // is worth. The operand never has to be understood.
        NodeKind::Cast(c) => type_expr(tree, c.cast_type),
        // `$Path` / `%Unique` type as a bare `Node`, the same hard floor the analyzer gives them
        // (`docs/02` §11). The precise scene-derived type stays out of this on purpose: it is
        // navigation-only, and an interface row feeds the diagnostic path.
        NodeKind::GetNode(_) => named("Node"),
        _ => TypeExpr::None,
    }
}

/// What an initializer NAMES, for the members [`initializer_type_expr`] could not type. The
/// answers are deliberately few (#431): the shallow pass has no analyzer, so it records the shape
/// and the reader resolves it. A shape with more than one reading is not recorded at all — an
/// index, a call through a value, a call whose result depends on its arguments — because a wrong
/// cross-file type is worse than none.
fn capture_init_shape(tree: &ParseTree, init: Option<NodeId>) -> Option<Box<InitShape>> {
    Some(Box::new(capture_shape(tree, init?, INIT_SHAPE_MAX_DEPTH)?))
}

/// One level of [`capture_init_shape`]. `depth` bounds the nesting; hitting zero refuses the
/// whole shape rather than truncating it.
fn capture_shape(tree: &ParseTree, id: NodeId, depth: usize) -> Option<InitShape> {
    if depth == 0 {
        return None;
    }
    match &tree.get(id).kind {
        NodeKind::Identifier(i) => Some(InitShape::Read {
            base: None,
            path: vec![i.name.clone()],
        }),
        NodeKind::Subscript(_) => {
            let (base, path) = split_chain_base(tree, id, depth)?;
            Some(InitShape::Read { base, path })
        }
        NodeKind::Preload(pl) => preload_res_path(tree, pl.path).map(|path| InitShape::Preload {
            path,
            construct: false,
        }),
        NodeKind::Call(_) => capture_call_shape(tree, id, depth),
        // A literal, an operator, a ternary, a lambda, an `await`: either the shape has no single
        // reading, or the reader has no arm for its root. Recording it would mistype the member.
        _ => None,
    }
}

/// A call, as either a `Preload { construct }` or a [`InitShape::Call`].
fn capture_call_shape(tree: &ParseTree, id: NodeId, depth: usize) -> Option<InitShape> {
    let NodeKind::Call(c) = &tree.get(id).kind else {
        return None;
    };
    let callee = c.callee?;
    if c.function_name == "new" {
        // `preload("res://x.gd").new()` — the callee is `<preload>.new`, so the base of the
        // attribute is the preload rather than a nameable chain.
        if let NodeKind::Subscript(sub) = &tree.get(callee).kind {
            if let Some(base) = sub.base {
                if let NodeKind::Preload(pl) = &tree.get(base).kind {
                    return preload_res_path(tree, pl.path).map(|path| InitShape::Preload {
                        path,
                        construct: true,
                    });
                }
            }
        }
        // A nameable `A.new()` / `A.B.new()` is already a type, handled by
        // `initializer_type_expr`; anything else naming `new` has no single reading.
        return None;
    }
    match &tree.get(callee).kind {
        NodeKind::Identifier(i) => Some(InitShape::Call {
            base: None,
            path: vec![i.name.clone()],
        }),
        NodeKind::Subscript(_) => {
            let (base, path) = split_chain_base(tree, callee, depth)?;
            Some(InitShape::Call { base, path })
        }
        _ => None,
    }
}

/// Peel an attribute chain and decide what it hangs off: `(base, path)`.
///
/// A plain identifier root stays the head of the chain itself, so `A.B.C` comes back as
/// `(None, [A, B, C])` — the flat form every shape had before nesting existed. A call or a
/// preload root becomes a nested shape and the peeled segments are read off its result. Any
/// other root refuses the whole capture.
#[allow(clippy::type_complexity)]
fn split_chain_base(
    tree: &ParseTree,
    id: NodeId,
    depth: usize,
) -> Option<(Option<Box<InitShape>>, Vec<String>)> {
    let (root, mut path) = split_attribute_chain(tree, id)?;
    match &tree.get(root).kind {
        NodeKind::Identifier(i) => {
            path.insert(0, i.name.clone());
            Some((None, path))
        }
        NodeKind::Call(_) | NodeKind::Preload(_) => {
            Some((Some(Box::new(capture_shape(tree, root, depth - 1)?)), path))
        }
        _ => None,
    }
}

/// The string literal a `preload` was given, when it is a plain literal. A non-literal path is
/// refused — the shallow pass cannot fold it, and guessing is the one thing this must not do.
fn preload_res_path(tree: &ParseTree, path: Option<NodeId>) -> Option<String> {
    let NodeKind::Literal(l) = &tree.get(path?).kind else {
        return None;
    };
    let gd_syntax::token::Literal::String(text) = &l.value else {
        return None;
    };
    Some(text.clone())
}

/// Peel attribute-over-identifier links off `id`, returning the innermost node that is not one
/// and the segment names in source order: `A.B.C` → `(A, [B, C])`.
///
/// `None` if any link is something other than a plain attribute over an identifier — a `[]`
/// subscript, an attribute whose name slot is missing. The whole chain is refused in that case,
/// never truncated. Iterative rather than recursive: a generated ten-thousand-segment chain must
/// not blow the extraction stack, and extraction runs on every `.gd` at startup.
fn split_attribute_chain(tree: &ParseTree, id: NodeId) -> Option<(NodeId, Vec<String>)> {
    let mut segments = Vec::new();
    let mut cur = id;
    loop {
        let NodeKind::Subscript(sub) = &tree.get(cur).kind else {
            segments.reverse();
            return Some((cur, segments));
        };
        let Some(gd_syntax::ast::SubscriptAccess::Attribute(Some(a))) = sub.access else {
            return None;
        };
        let NodeKind::Identifier(attr) = &tree.get(a).kind else {
            return None;
        };
        segments.push(attr.name.clone());
        cur = sub.base?;
    }
}

/// The dotted identifier chain under an attribute expression: `A` → `[A]`, `A.B.C` → `[A, B, C]`.
/// `None` if the chain does not bottom out at a plain identifier.
fn attribute_path(tree: &ParseTree, id: NodeId) -> Option<Vec<String>> {
    let (root, mut path) = split_attribute_chain(tree, id)?;
    let NodeKind::Identifier(i) = &tree.get(root).kind else {
        return None;
    };
    path.insert(0, i.name.clone());
    Some(path)
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
        assert_eq!(
            i.extends,
            Extends::Path {
                path: "res://base.gd".into(),
                segments: Vec::new()
            }
        );
        assert!(i.class_name.is_none());
    }

    /// #388: `extends "res://x.gd".Inner` names the inner class. The segments used to be dropped
    /// here, which made every consumer answer the file's HEAD class — and `class_parent` feeds
    /// `rename`'s override grouping, so the wrong answer reached a mutating surface.
    #[test]
    fn extends_path_literal_keeps_its_segments() {
        let i = iface("extends \"res://base.gd\".Inner\n");
        assert_eq!(
            i.extends,
            Extends::Path {
                path: "res://base.gd".into(),
                segments: vec!["Inner".to_owned()]
            }
        );
        let deep = iface("extends \"res://base.gd\".Mid.Leaf\n");
        assert_eq!(
            deep.extends,
            Extends::Path {
                path: "res://base.gd".into(),
                segments: vec!["Mid".to_owned(), "Leaf".to_owned()]
            }
        );
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
    fn an_untyped_initializer_reads_every_unambiguous_shape() {
        // #431: each of these is a member another file can read off. Anything that stays
        // `TypeExpr::None` here reads as Variant from across the project, so the shapes gdls can
        // decode without evaluating anything are pinned.
        let src = "extends Node
                   var count := 3
                   var tint := Color(1, 0, 0)
                   var purple := Color.PURPLE
                   var map := SelectionMap.new()
                   var cell := Outer.Inner.new()
                   var canvas := get_parent() as CanvasItem
                   @onready var timer := $Timer
                   @onready var label := %Score
                   var opaque := compute()
";
        let i = iface(src);
        let ty = |n: &str| i.members.iter().find(|m| m.name == n).unwrap().ty.clone();

        assert_eq!(ty("count").head(), Some("int"));
        assert_eq!(ty("tint").head(), Some("Color"));
        assert_eq!(
            ty("purple"),
            TypeExpr::Named {
                path: vec!["Color".into(), "PURPLE".into()],
                args: Vec::new()
            }
        );
        assert_eq!(ty("map").head(), Some("SelectionMap"));
        // A dotted constructor keeps every segment, so the analyzer walks to the inner class
        // rather than stopping at the outer one.
        assert_eq!(
            ty("cell"),
            TypeExpr::Named {
                path: vec!["Outer".into(), "Inner".into()],
                args: Vec::new()
            }
        );
        // A cast names its type outright; the operand never has to be understood.
        assert_eq!(ty("canvas").head(), Some("CanvasItem"));
        // `$` and `%` are a bare `Node`, the analyzer's own hard floor for them.
        assert_eq!(ty("timer").head(), Some("Node"));
        assert_eq!(ty("label").head(), Some("Node"));
        // A call that needs evaluating still has no answer, and says so.
        assert_eq!(ty("opaque"), TypeExpr::None);
    }

    #[test]
    fn a_typed_collection_cast_keeps_its_element_type() {
        let i = iface(
            "extends Node
var rows := load_rows() as Array[Entry]
",
        );
        assert_eq!(
            i.members.iter().find(|m| m.name == "rows").unwrap().ty,
            TypeExpr::Named {
                path: vec!["Array".into()],
                args: vec![TypeExpr::Named {
                    path: vec!["Entry".into()],
                    args: Vec::new()
                }]
            }
        );
    }

    #[test]
    fn a_constructor_over_something_that_is_not_a_name_stays_unknown() {
        // `arr[0].new()` and `get_class().new()` name nothing decodable — an answer here would be
        // a guess, and a wrong member type is worse than no member type.
        let i = iface(
            "extends Node
var a := kinds[0].new()
var b := pick().new()
",
        );
        assert_eq!(
            i.members.iter().find(|m| m.name == "a").unwrap().ty,
            TypeExpr::None
        );
        assert_eq!(
            i.members.iter().find(|m| m.name == "b").unwrap().ty,
            TypeExpr::None
        );
    }

    #[test]
    fn a_parameters_default_gives_it_a_type_and_a_hardness() {
        let iface = iface(
            "extends Node\n\
             func f(bare, annotated: String, hard := \"\", soft = 1, unknown := TileSet.TILE_SHAPE_SQUARE) -> void:\n\
             \tpass\n",
        );
        let m = iface
            .members
            .iter()
            .find(|m| m.name == "f")
            .expect("func member");
        let named = |n: &str| TypeExpr::Named {
            path: vec![n.to_owned()],
            args: vec![],
        };
        assert_eq!(
            m.params,
            vec![
                TypeExpr::None,
                named("String"),
                named("String"),
                named("int"),
                // A dotted native enum constant is not something the shallow pass can decode.
                TypeExpr::None,
            ]
        );
        assert_eq!(
            m.params_typing,
            vec![
                ParamTyping::Untyped,
                ParamTyping::Annotated,
                ParamTyping::InferredHard,
                ParamTyping::InferredSoft,
                ParamTyping::Unknown { hard: true },
            ]
        );
        // #528: only the undecodable slot carries a shape — every other slot already has its
        // answer in `params`, and a second source of truth there is how the two drift.
        assert_eq!(
            m.param_inits,
            vec![
                None,
                None,
                None,
                None,
                Some(Box::new(InitShape::Read {
                    base: None,
                    path: vec!["TileSet".to_owned(), "TILE_SHAPE_SQUARE".to_owned()],
                })),
            ]
        );
    }

    /// A `=` default records the same shape but the other hardness, since the `TypeExpr` that
    /// carries that split for a decoded slot is `None` here.
    #[test]
    fn an_undecodable_default_records_its_writing() {
        let iface = iface("extends Node\nfunc f(a = TileSet.TILE_SHAPE_SQUARE) -> void:\n\tpass\n");
        let m = iface.members.iter().find(|m| m.name == "f").expect("func");
        assert_eq!(m.params_typing, vec![ParamTyping::Unknown { hard: false }]);
        assert!(m.param_inits[0].is_some());
    }

    /// A shape the capture itself refuses stays refused, and the slot behaves as it did before.
    #[test]
    fn a_default_the_capture_refuses_carries_no_shape() {
        let iface = iface("extends Node\nfunc f(a := [1, 2][0]) -> void:\n\tpass\n");
        let m = iface.members.iter().find(|m| m.name == "f").expect("func");
        assert!(matches!(m.params_typing[0], ParamTyping::Unknown { .. }));
        assert_eq!(m.param_inits, vec![None]);
    }

    /// A signal parameter cannot carry a default, so it carries no shape either.
    #[test]
    fn a_signal_parameter_carries_no_shape() {
        let iface = iface("extends Node\nsignal fired(a: int, b)\n");
        let m = iface
            .members
            .iter()
            .find(|m| m.name == "fired")
            .expect("signal");
        assert!(m.param_inits.is_empty());
    }

    /// Swapping only the default's SHAPE moves the hash: a caller checking arguments against the
    /// resolved type has to be re-run.
    #[test]
    fn a_default_shape_change_moves_the_hash() {
        let a = iface("extends Node\nfunc f(x := TileSet.TILE_SHAPE_SQUARE) -> void:\n\tpass\n");
        let b = iface("extends Node\nfunc f(x := TileSet.TILE_SHAPE_ISOMETRIC) -> void:\n\tpass\n");
        assert_ne!(a.signature_hash(), b.signature_hash());
    }

    #[test]
    fn a_parameters_default_changes_the_hash() {
        // Nothing else in the interface moves when `a := ""` becomes `a := 0`, so without the
        // parameter type in the hash a caller would keep checking against the old one.
        let a = iface("extends Node\nfunc f(a := \"\") -> void:\n\tpass\n");
        let b = iface("extends Node\nfunc f(a := 0) -> void:\n\tpass\n");
        assert_ne!(a.signature_hash(), b.signature_hash());
        // And `:=` versus `=` is a hardness change the caller must see too.
        let c = iface("extends Node\nfunc f(a = \"\") -> void:\n\tpass\n");
        assert_ne!(a.signature_hash(), c.signature_hash());
    }

    #[test]
    fn an_untypeable_initializer_records_what_it_names() {
        let i = iface(
            "extends Node\n\
             const K := OTHER\n\
             var chain := Other.KONST\n\
             var enum_val := E.A\n\
             var called := make()\n\
             var dotted := Other.make()\n\
             var pre := preload(\"res://lib.gd\")\n\
             var pre_new := preload(\"res://lib.gd\").new()\n\
             var indexed := rows[0]\n\
             var through_value := holder.thing.compute()\n\
             var relative := preload(\"lib.gd\")\n\
             var typed := 3\n",
        );
        let init = |n: &str| {
            i.members
                .iter()
                .find(|m| m.name == n)
                .unwrap()
                .init
                .clone()
                .map(|b| *b)
        };
        let chain = |v: &[&str]| {
            Some(InitShape::Read {
                base: None,
                path: v.iter().map(|s| (*s).to_owned()).collect(),
            })
        };
        let call = |v: &[&str]| {
            Some(InitShape::Call {
                base: None,
                path: v.iter().map(|s| (*s).to_owned()).collect(),
            })
        };
        assert_eq!(init("K"), chain(&["OTHER"]));
        assert_eq!(init("chain"), chain(&["Other", "KONST"]));
        assert_eq!(init("enum_val"), chain(&["E", "A"]));
        assert_eq!(init("called"), call(&["make"]));
        assert_eq!(init("dotted"), call(&["Other", "make"]));
        assert_eq!(
            init("pre"),
            Some(InitShape::Preload {
                path: "res://lib.gd".to_owned(),
                construct: false
            })
        );
        assert_eq!(
            init("pre_new"),
            Some(InitShape::Preload {
                path: "res://lib.gd".to_owned(),
                construct: true
            })
        );
        // A longer chain is still one reading — whether it resolves to anything is the reader's
        // problem, not extraction's.
        assert_eq!(init("through_value"), call(&["holder", "thing", "compute"]));
        // An index has no single reading, so nothing is recorded.
        assert_eq!(init("indexed"), None);
        // A relative preload is recorded verbatim; the index joins it against the reading file's
        // own directory when it walks the edges, so the dependency still exists.
        assert_eq!(
            init("relative"),
            Some(InitShape::Preload {
                path: "lib.gd".to_owned(),
                construct: false
            })
        );
        // A member the interface can already type records no shape.
        assert_eq!(init("typed"), None);
    }

    #[test]
    fn a_shape_over_a_call_or_a_preload_nests() {
        let i = iface(
            "extends Node\n\
             var joined := OS.get_temp_dir().path_join(\"x\")\n\
             var read_off_call := make().field\n\
             var read_off_preload := preload(\"res://lib.gd\").KONST\n\
             var call_off_new := preload(\"res://lib.gd\").new().make()\n\
             var through_index := rows[0].compute()\n",
        );
        let init = |n: &str| {
            i.members
                .iter()
                .find(|m| m.name == n)
                .unwrap()
                .init
                .clone()
                .map(|b| *b)
        };
        let path = |v: &[&str]| v.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>();
        assert_eq!(
            init("joined"),
            Some(InitShape::Call {
                base: Some(Box::new(InitShape::Call {
                    base: None,
                    path: path(&["OS", "get_temp_dir"]),
                })),
                path: path(&["path_join"]),
            })
        );
        assert_eq!(
            init("read_off_call"),
            Some(InitShape::Read {
                base: Some(Box::new(InitShape::Call {
                    base: None,
                    path: path(&["make"]),
                })),
                path: path(&["field"]),
            })
        );
        assert_eq!(
            init("read_off_preload"),
            Some(InitShape::Read {
                base: Some(Box::new(InitShape::Preload {
                    path: "res://lib.gd".to_owned(),
                    construct: false,
                })),
                path: path(&["KONST"]),
            })
        );
        assert_eq!(
            init("call_off_new"),
            Some(InitShape::Call {
                base: Some(Box::new(InitShape::Preload {
                    path: "res://lib.gd".to_owned(),
                    construct: true,
                })),
                path: path(&["make"]),
            })
        );
        // An index anywhere in the chain still has more than one reading, and nesting does not
        // make it recordable.
        assert_eq!(init("through_index"), None);
    }

    #[test]
    fn a_shape_deeper_than_the_cap_is_refused_whole() {
        let init = |src: &str| {
            iface(src)
                .members
                .iter()
                .find(|m| m.name == "x")
                .unwrap()
                .init
                .clone()
        };
        // Each `.g()` link is one more nesting level, and the innermost `f()` is the first.
        let chain = |n: usize| {
            let mut src = String::from("extends Node\nvar x := f()");
            for _ in 1..n {
                src.push_str(".g()");
            }
            src.push('\n');
            src
        };
        assert!(init(&chain(INIT_SHAPE_MAX_DEPTH)).is_some());
        assert!(init(&chain(INIT_SHAPE_MAX_DEPTH + 1)).is_none());
    }

    #[test]
    fn an_initializer_shape_change_changes_the_hash() {
        // Nothing else in the interface moves when `make()` becomes `other()`, so without the
        // shape in the hash a dependent would keep serving the old type.
        let a = iface("extends Node\nvar x := make()\n");
        let b = iface("extends Node\nvar x := other()\n");
        assert_ne!(a.signature_hash(), b.signature_hash());
        // And a body-only edit around it still does not.
        let c = iface("extends Node\nvar x := make()\nfunc f() -> void:\n\tpass\n");
        let d = iface("extends Node\nvar x := make()\nfunc f() -> void:\n\tprint(1)\n");
        assert_eq!(c.signature_hash(), d.signature_hash());
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

    /// #406: `parse_clean` rides on the interface so a cross-file consumer can tell a real
    /// absence from a recovery hole. One file means one parse, so the head and every inner class
    /// carry the same answer.
    #[test]
    fn parse_clean_is_stamped_on_the_head_and_every_inner_class() {
        let clean = iface("extends Node\nclass Inner:\n\tvar x: int\n");
        assert!(clean.parse_clean);
        assert!(clean.inner[0].parse_clean);

        let broken = iface("extends Node\nclass Inner:\n\tvar x: int\nfunc (( -> :\n");
        assert!(!broken.parse_clean);
        assert!(!broken.inner[0].parse_clean);
    }

    /// The flag is part of what the file exposes: a file that starts parsing cleanly newly proves
    /// its own absences, so dependents must re-analyze.
    #[test]
    fn parse_cleanliness_change_changes_hash() {
        let a = iface("extends Node\nfunc move() -> void:\n\tpass\n");
        let b = iface("extends Node\nfunc move() -> void:\n\tpass\nfunc (( -> :\n");
        assert!(a.parse_clean && !b.parse_clean);
        assert_ne!(a.signature_hash(), b.signature_hash());
    }

    #[test]
    fn empty_or_partial_tree_is_default() {
        assert_eq!(extract(&ParseTree::default()), Interface::default());
    }
}
