//! The resolution passes — a faithful port of Godot's `resolve_*` family
//! (`gdscript_analyzer.cpp`). WP-C lands `resolve_inheritance` (the `extends` chains + the `RESOLVING`
//! cycle sentinel) and `resolve_datatype` (a type annotation → a [`DataType`]); WP-D lands
//! `resolve_interface` (member signatures, the member-name conflict checks, signal/enum types). Body
//! resolution + the `reduce_*` expression family land in WP-E.
//!
//! ## Mapping Godot's runtime services onto M2
//!
//! Godot reaches into live engine singletons; gdls maps each onto the static M2 environment:
//!
//! | Godot | gdls |
//! |---|---|
//! | `ClassDB::class_exists(name)` | [`NativeDb::class_named`] is `Some` |
//! | `GDScriptParser::get_builtin_type(name) < VARIANT_MAX` | [`builtin_type_from_name`] is `Some` |
//! | `ScriptServer::is_global_class(name)` | [`CrossFileQuery::global_class_file`] is `Some` |
//! | `parser_ref->raise_status(INHERITANCE_SOLVED)` (re-parse a dep) | the [`CrossFileQuery`] seam |
//! | `p_class->base_type` / `set_datatype` | [`AnalysisContext::bases`] / [`AnalysisContext::types`] |
//!
//! Engine-singleton inheritance, autoload-singleton bases, `@warning_ignore`/annotation effects, and
//! the cross-file re-parse depth are deferred to later WPs (flagged inline); those paths degrade to a
//! later pass or a known-failure corpus case, never a crash.

use gd_syntax::ast::{Member, NodeId, NodeKind};

use crate::context::AnalysisContext;
use crate::data_type::{DataType, DtKind, MethodSig, ScriptRef, TypeSource, VariantType};

/// The `RESOLVING` cycle sentinel datatype (Godot sets this before resolving a class/member/type).
fn resolving() -> DataType {
    DataType {
        kind: DtKind::Resolving,
        ..Default::default()
    }
}

// ===================================================================================================
// resolve_inheritance — analyzer.cpp:346-652, 6578
// ===================================================================================================

/// `GDScriptAnalyzer::resolve_inheritance()` (analyzer.cpp:6578): resolve the head class and, since the
/// head is recursive, every inner class. Crate-internal: external callers go through [`crate::analyze`].
pub(crate) fn resolve_inheritance(ctx: &mut AnalysisContext) -> Result<(), ()> {
    let Some(root) = ctx.tree.root_id() else {
        return Ok(()); // empty/partial parse — nothing to resolve (never crash).
    };
    resolve_class_inheritance_recursive(ctx, root, true)
}

/// `resolve_class_inheritance(p_class, bool p_recursive)` (analyzer.cpp:634).
fn resolve_class_inheritance_recursive(
    ctx: &mut AnalysisContext,
    class_id: NodeId,
    recursive: bool,
) -> Result<(), ()> {
    resolve_class_inheritance(ctx, class_id, None)?;
    if recursive {
        for inner in inner_classes(ctx, class_id) {
            resolve_class_inheritance_recursive(ctx, inner, true)?;
        }
    }
    Ok(())
}

/// `resolve_class_inheritance(p_class, const Node *p_source)` (analyzer.cpp:346).
///
/// gdls only ever holds classes from its *own* parse tree, so Godot's `!parser->has_class(p_class)`
/// external-class branch (and the `ensure_cached_external_parser_for_class` finalizers around it) have
/// no analog here — cross-file bases are resolved through [`CrossFileQuery`] where they are named.
fn resolve_class_inheritance(
    ctx: &mut AnalysisContext,
    class_id: NodeId,
    source: Option<NodeId>,
) -> Result<(), ()> {
    let here = source.unwrap_or(class_id);

    // Cycle sentinel + already-resolved guards live on the *base* type (Godot's `p_class->base_type`).
    let base = ctx.base_type(class_id);
    if base.is_resolving() {
        let name = class_meta_name(ctx, class_id);
        ctx.push_error(
            format!(r#"Could not resolve class "{name}": Cyclic reference."#),
            here,
        );
        return Err(());
    }
    if !base.has_no_type() {
        return Ok(()); // already resolved
    }

    let previous_class = ctx.current_class;
    ctx.current_class = Some(class_id);

    // "Class X hides a …" checks on the class's own name. The builtin/native cases are ported; the
    // global-script-class and autoload-singleton cases need self-exclusion + autoload data (WP-D).
    if let Some(name) = class_identifier_name(ctx, class_id) {
        let id_node = class_identifier(ctx, class_id).unwrap_or(class_id);
        if builtin_type_from_name(&name).is_some() {
            ctx.push_error(format!(r#"Class "{name}" hides a built-in type."#), id_node);
        } else if ctx.native.class_named(&name).is_some() {
            ctx.push_error(format!(r#"Class "{name}" hides a native class."#), id_node);
        }
    }

    // base_type = RESOLVING (analyzer.cpp:409-411).
    ctx.set_base(
        class_id,
        DataType {
            kind: DtKind::Resolving,
            ..Default::default()
        },
    );

    // The class's own (meta) type (analyzer.cpp:414-422).
    let mut class_type = DataType {
        is_constant: true,
        is_meta_type: true,
        type_source: TypeSource::AnnotatedExplicit,
        kind: DtKind::Class,
        class_node: Some(class_id),
        builtin_type: VariantType::Object,
        ..Default::default()
    };
    ctx.set_type(class_id, class_type.clone());

    // Resolve the base (`result`).
    let result = if !class_extends_used(ctx, class_id) {
        // No `extends` ⇒ implicitly `RefCounted` (analyzer.cpp:425-429).
        DataType {
            type_source: TypeSource::AnnotatedInferred,
            kind: DtKind::Native,
            builtin_type: VariantType::Object,
            native_type: "RefCounted".to_owned(),
            ..Default::default()
        }
    } else {
        match resolve_extends(ctx, class_id)? {
            Some(base) => base,
            None => return Err(()), // error already pushed
        }
    };

    if !result.is_set() || result.has_no_type() {
        let name = class_identifier_name(ctx, class_id).unwrap_or_else(|| "<main>".to_owned());
        ctx.push_error(
            format!(r#"Could not resolve inheritance for class "{name}"."#),
            class_id,
        );
        return Err(());
    }

    // Cyclic inheritance (analyzer.cpp:609-617): walk the base chain by class node identity (gdls's
    // in-file analog of Godot's `fqcn` comparison; cross-file `Script` bases stop the walk).
    if walks_back_to(ctx, class_id, &result) {
        ctx.push_error("Cyclic inheritance.", class_id);
        return Err(());
    }

    ctx.set_base(class_id, result.clone());
    class_type.native_type = result.native_type.clone();
    ctx.set_type(class_id, class_type);

    // Annotation resolution + `apply()` (analyzer.cpp:623-627). Only `@abstract` is class-level
    // for now — the other class-level annotations (`@tool`, `@icon`, `@static_unload`) are
    // resolved at parse time. Emits the "only once per class" duplicate error here so the
    // diagnostic ordering matches Godot (`DuplicateAbstract`'s LINE 37 comes before any
    // function-level `@abstract` apply in the interface pass).
    apply_class_abstract_annotation(ctx, class_id);

    // WP-P5 (M3 tail): MISSING_TOOL — extends a `@tool` script without declaring `@tool` itself.
    // Godot emits `WARNING: MISSING_TOOL: The base class script has the "@tool" annotation,
    // but this script does not have it.` at gdscript_warning.cpp's MISSING_TOOL template and
    // analyzer.cpp's `apply()` callback for the @tool annotation walk. gdls fires it here at
    // inheritance time because that's where the base script's `is_tool` flag becomes queryable
    // via the cross-file `Interface`. Gated on the *file's* @tool (a class-level annotation that
    // applies to the whole script per Godot semantics; inner classes can't override it).
    if result.kind == DtKind::Script {
        if let Some(base_file) = result.script_type.as_ref().map(|s| s.file) {
            // WP-RD2: `ctx.file` is `Option`; an orphan (None) has no queryable `@tool` flag, so
            // treat it as not-tool (the warning then fires iff the base is `@tool`).
            if ctx.xfile.is_file_tool(base_file)
                && !ctx.file.is_some_and(|f| ctx.xfile.is_file_tool(f))
            {
                ctx.push_warning(crate::warnings::WarningCode::MissingTool, &[], class_id);
            }
        }
    }

    ctx.current_class = previous_class;
    Ok(())
}

/// The `extends` resolution (analyzer.cpp:430-601). Returns the resolved base, or `None` after pushing
/// a specific error.
fn resolve_extends(ctx: &mut AnalysisContext, class_id: NodeId) -> Result<Option<DataType>, ()> {
    // `extends "res://path.gd"` (analyzer.cpp:437-459).
    if let Some(path) = class_extends_path(ctx, class_id) {
        // Relative paths resolve against the script's own directory (analyzer.cpp:437); an
        // unresolved path is Godot's "Could not resolve super class path".
        let resolved = match ctx.file {
            Some(from) => ctx.xfile.resolve_path_from(from, &path),
            None => ctx.xfile.resolve_res_path(&path),
        };
        return match resolved {
            Some(fid) => Ok(Some(script_base_datatype(ctx, fid))),
            None => {
                ctx.push_error(
                    format!(r#"Could not resolve super class path "{path}"."#),
                    class_id,
                );
                Ok(None)
            }
        };
    }

    let extends = class_extends_names(ctx, class_id);
    let Some(&first_id) = extends.first() else {
        ctx.push_error("Could not resolve an empty super class path.", class_id);
        return Ok(None);
    };
    let name = ident_name(ctx, first_id).unwrap_or_default();

    // Resolve the head of the `extends` chain.
    let mut base = if let Some(fid) = ctx.xfile.global_class_file(&name) {
        // A project `class_name` (analyzer.cpp:469-494).
        if Some(fid) == ctx.file {
            ctx.get_type(root_or(ctx, class_id)).clone()
        } else {
            script_base_datatype(ctx, fid)
        }
    } else if ctx.native.class_named(&name).is_some() {
        // A native engine class (analyzer.cpp:521-528). Godot rejects `extends` of an
        // engine singleton (`Engine::get_singleton()->has_singleton(name)`) before stamping
        // the base type — gdls mirrors that via [`NativeDb::singleton_type`].
        if ctx.native.singleton_type(&name).is_some() {
            ctx.push_error(
                format!(
                    r#"Cannot inherit native class "{name}" because it is an engine singleton."#
                ),
                first_id,
            );
            return Ok(None);
        }
        DataType {
            type_source: TypeSource::AnnotatedExplicit,
            kind: DtKind::Native,
            builtin_type: VariantType::Object,
            native_type: name.clone(),
            ..Default::default()
        }
    } else {
        // Classes in the current scope: inner classes / outer-class members (analyzer.cpp:530-575).
        match resolve_extends_in_scope(ctx, class_id, &name, first_id)? {
            Some(base) => base,
            None if ctx.native.is_empty() => {
                // No native dump loaded ⇒ the analyzer has no ground truth for native class names.
                // Per CLAUDE.md ("Never crash, never lie. Unknown natives stay dynamic, no phantom
                // 'unknown class' error"), degrade `extends UnknownName` to a permissive native rather than emit
                // a phantom "Could not find base class" against an editor session that simply
                // hasn't been pointed at `extension_api.json` yet. Mirrors the reducer's
                // trimmed-dump permissiveness (`reducer.rs:3049`).
                DataType {
                    type_source: TypeSource::AnnotatedExplicit,
                    kind: DtKind::Native,
                    builtin_type: VariantType::Object,
                    native_type: name.clone(),
                    ..Default::default()
                }
            }
            None => {
                ctx.push_error(format!(r#"Could not find base class "{name}"."#), first_id);
                return Ok(None);
            }
        }
    };

    // Nested `extends A.B.C` segments (analyzer.cpp:578-598). For an in-file `Class` base we walk inner
    // classes syntactically; for a cross-file `Script` base (WP-P1) we walk the depended file's
    // inner-class table via `CrossFileQuery::resolve_inner_chain`. Godot's
    // `reduce_identifier_from_base` does both at analyzer.cpp:578-598 — gdls splits the in-file
    // walk (this loop) from the cross-file walk because the Script base lives outside our parse tree.
    for &id in &extends[1..] {
        // WP-P1: cross-file Script base — walk via the depended file's Interface inner-class table.
        if base.kind == DtKind::Script {
            let seg = ident_name(ctx, id).unwrap_or_default();
            let resolved = base.script_type.as_ref().and_then(|sr| {
                let mut chain: Vec<&str> = sr.inner.iter().map(String::as_str).collect();
                chain.push(&seg);
                ctx.xfile.resolve_inner_chain(sr.file, &chain).map(|_| {
                    (
                        sr.file,
                        chain.into_iter().map(String::from).collect::<Vec<_>>(),
                    )
                })
            });
            if let Some((file, new_inner)) = resolved {
                let sref = ScriptRef {
                    file,
                    inner: new_inner,
                };
                base = DataType {
                    kind: DtKind::Script,
                    type_source: TypeSource::AnnotatedExplicit,
                    is_meta_type: true,
                    builtin_type: VariantType::Object,
                    native_type: crate::script_chain::chain_native_root(ctx, &sref)
                        .unwrap_or_default(),
                    script_type: Some(sref),
                    ..Default::default()
                };
                continue;
            }
            // Inner not found via cross-file walk. Per the "Unknown stays dynamic" rule, degrade
            // silently to a Variant-ish base rather than emitting a phantom error — the corpus's
            // failing fixtures all expect NO diagnostic for cross-file chain walks (the cross-file
            // interface may not capture every inner class detail).
            base = DataType::variant();
            continue;
        }
        if base.kind != DtKind::Class {
            ctx.push_error(
                format!(
                    r#"Cannot get nested types for extension from non-GDScript type "{}"."#,
                    base.native_type
                ),
                id,
            );
            return Ok(None);
        }
        let seg = ident_name(ctx, id).unwrap_or_default();
        match base
            .class_node
            .and_then(|c| inner_class_named(ctx, c, &seg))
        {
            Some(inner_id) => {
                if ctx.get_type(inner_id).has_no_type() {
                    resolve_class_inheritance(ctx, inner_id, Some(id))?;
                }
                base = ctx.get_type(inner_id).clone();
            }
            None => {
                // analyzer.cpp:587-595 — Godot's `reduce_identifier_from_base` resolves
                // `id` against the base class's member table. If the member exists but isn't
                // SCRIPT or CLASS (e.g. it's a Constant), Godot emits
                // `Identifier "X" is not a preloaded script or class.` instead of the
                // "Could not find nested type" template. gdls's port walks `members_indices`
                // here to distinguish "member exists but is not a class" from "no such name".
                let member_idx = base.class_node.and_then(|c| match &ctx.node(c).kind {
                    NodeKind::Class(cls) => cls.members_indices.get(&seg).copied(),
                    _ => None,
                });
                if let Some(idx) =
                    member_idx.and_then(|i| base.class_node.and_then(|c| nth_member(ctx, c, i)))
                {
                    if !matches!(idx, Member::Class(_)) {
                        ctx.push_error(
                            format!(r#"Identifier "{seg}" is not a preloaded script or class."#),
                            id,
                        );
                        return Ok(None);
                    }
                }
                ctx.push_error(format!(r#"Could not find nested type "{seg}"."#), id);
                return Ok(None);
            }
        }
    }

    Ok(Some(base))
}

/// Godot's in-scope class search for an `extends` head (analyzer.cpp:530-569): try each class in
/// scope by name, then by member. WP-C handles the class-name and inner-`class`-member cases; the
/// preloaded-constant-as-base case is cross-file and deferred.
fn resolve_extends_in_scope(
    ctx: &mut AnalysisContext,
    class_id: NodeId,
    name: &str,
    source: NodeId,
) -> Result<Option<DataType>, ()> {
    for look in scope_classes(ctx, class_id) {
        if class_identifier_name(ctx, look).as_deref() == Some(name) {
            if ctx.get_type(look).has_no_type() {
                resolve_class_inheritance(ctx, look, Some(source))?;
            }
            return Ok(Some(ctx.get_type(look).clone()));
        }
        match class_member(ctx, look, name) {
            Some(Member::Class(inner_id)) => {
                if ctx.get_type(inner_id).has_no_type() {
                    resolve_class_inheritance(ctx, inner_id, Some(source))?;
                }
                return Ok(Some(ctx.get_type(inner_id).clone()));
            }
            // analyzer.cpp:554-562 — non-class members in an `extends` are specific errors, not the
            // generic "Could not find base class". A `const` whose value is a preloaded script /
            // inner class is **allowed** (the resolved DataType has kind = SCRIPT or CLASS); the
            // error fires only when the constant is something else (`const A = 1`).
            Some(Member::Constant(const_id)) => {
                // Constants in scope haven't been reduced yet during the inheritance pass — the
                // Godot's `reduce_identifier_from_base` reaches here via `resolve_inheritance` and
                // gets the constant's `reduced_value` because the parser/resolver does an eager
                // const-fold pass. Gdls's reduction is staged later, so we eagerly reduce just
                // this constant's initializer to fold a `preload(...)` to its Script meta type
                // before checking. Mirrors `reduce_identifier_from_base`'s analyzer.cpp:4566-4576
                // path that calls `reduce_expression` on the constant's initializer.
                let mut dt = ctx.get_type(const_id).clone();
                if !matches!(dt.kind, DtKind::Script | DtKind::Class) {
                    if let NodeKind::Constant(c) = ctx.node(const_id).kind.clone() {
                        if let Some(init) = c.initializer {
                            crate::reducer::reduce_expression(ctx, init, false);
                            // Propagate the initializer's resolved type onto the constant itself
                            // so the kind check below sees Script when the init was a preload.
                            let init_dt = ctx.get_type(init).clone();
                            if init_dt.kind == DtKind::Script || init_dt.kind == DtKind::Class {
                                ctx.set_type(const_id, init_dt);
                                dt = ctx.get_type(const_id).clone();
                            }
                        }
                    }
                }
                if dt.kind == DtKind::Script || dt.kind == DtKind::Class {
                    return Ok(Some(dt));
                }
                ctx.push_error(
                    format!(r#"Constant "{name}" is not a preloaded script or class."#),
                    source,
                );
                return Err(());
            }
            // analyzer.cpp:561 — every other member kind is rejected with the named kind in the
            // message: `Cannot use <kind> "<name>" in extends chain.` Godot derives the kind from
            // `member.get_type_name()`; gdls's `Member` variants encode the same enumeration.
            Some(other) => {
                ctx.push_error(
                    format!(
                        r#"Cannot use {kind} "{name}" in extends chain."#,
                        kind = member_kind_name(other),
                    ),
                    source,
                );
                return Err(());
            }
            None => {}
        }
    }
    Ok(None)
}

/// `GDScriptParser::ClassNode::Member::get_type_name()` (gdscript_parser.h:602-625) — the lowercased
/// noun Godot uses inside its `Cannot use <kind> "<name>" in extends chain.` /
/// `... in <context>.` messages. Mirrors the enumeration of [`Member`] variants 1:1.
fn member_kind_name(member: Member) -> &'static str {
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

/// `get_class_node_current_scope_classes` (analyzer.cpp:320): the class itself, its (in-file) base
/// chain, and its outer-class chain — deduplicated, in Godot's order.
pub(crate) fn scope_classes(ctx: &AnalysisContext, class_id: NodeId) -> Vec<NodeId> {
    fn walk(ctx: &AnalysisContext, node: NodeId, out: &mut Vec<NodeId>) {
        if out.contains(&node) {
            return;
        }
        out.push(node);
        if let Some(bc) = ctx.base_type(node).class_node {
            walk(ctx, bc, out);
        }
        if let Some(outer) = class_outer(ctx, node) {
            walk(ctx, outer, out);
        }
    }
    let mut out = Vec::new();
    walk(ctx, class_id, &mut out);
    out
}

/// Cyclic-inheritance walk (analyzer.cpp:610-617): does `result`'s base chain lead back to `class_id`?
fn walks_back_to(ctx: &AnalysisContext, class_id: NodeId, result: &DataType) -> bool {
    let mut base_class = result.class_node;
    while let Some(bc) = base_class {
        if bc == class_id {
            return true;
        }
        base_class = ctx.bases.get(&bc).and_then(|b| b.class_node);
    }
    false
}

/// A bare enum name resolved through the current class's inherited scope: script-chain base
/// enums first (they shadow native), then the native inherits chain (returning the DECLARING
/// class so the enum renders `BT.Status`). Meta-typed — annotations lower via
/// `type_from_metatype`.
fn inherited_enum_annotation(ctx: &mut AnalysisContext, name: &str) -> Option<DataType> {
    let class_id = ctx.current_class?;
    if let Some(sr) = crate::reducer::current_class_script_base(ctx) {
        let chain = crate::script_chain::resolve_script_chain(ctx, &sr);
        for link in chain.links.clone() {
            let has = crate::script_chain::link_interface(ctx.xfile, &link)
                .is_some_and(|i| i.enums.iter().any(|e| e.name == name));
            if has && link.inner.is_empty() {
                return crate::reducer::cross_file_named_enum(ctx, link.file, name, true);
            }
        }
    }
    let root = nearest_native_ancestor(ctx, class_id)?;
    let mut cur = Some(root);
    while let Some(c) = cur {
        let nc = ctx.native.class_named(&c)?;
        if nc.enums.iter().any(|e| ctx.native.name_of(e.name) == name) {
            return Some(make_native_enum_type(ctx, name, &c, true));
        }
        cur = nc.inherits.map(|s| ctx.native.name_of(s).to_owned());
    }
    None
}

/// Build the base [`DataType`] for an `extends`-ed project script, with the chain's native root
/// stamped into `native_type` — Godot's `class_type.native_type = result.native_type`
/// (analyzer.cpp:617-619), which is what keeps `$`/`@onready`/self-compat working through
/// arbitrary script-to-script chains. An unresolvable chain leaves it empty (permissive).
pub(crate) fn script_base_datatype(ctx: &AnalysisContext, fid: gd_project::FileId) -> DataType {
    let sref = ScriptRef {
        file: fid,
        inner: Vec::new(),
    };
    DataType {
        kind: DtKind::Script,
        type_source: TypeSource::AnnotatedExplicit,
        is_meta_type: true,
        builtin_type: VariantType::Object,
        native_type: crate::script_chain::chain_native_root(ctx, &sref).unwrap_or_default(),
        script_type: Some(sref),
        ..Default::default()
    }
}

// ===================================================================================================
// resolve_datatype — analyzer.cpp:654-960
// ===================================================================================================

/// `GDScriptAnalyzer::resolve_datatype(TypeNode *p_type)` (analyzer.cpp:654): a type annotation → its
/// [`DataType`]. Has no caller in WP-C (interface resolution wires it in WP-D); it is ported and
/// unit-tested now as the inheritance sibling.
///
/// WP-C covers `void`, `Variant`, builtin scalars, native classes, project script classes, and in-file
/// inner classes. Container element types (`Array[T]`/`Dictionary[K, V]`), every enum form, and
/// suite-local-constant types need the reducer / native-enum introspection and arrive in WP-D
/// (flagged inline); they currently resolve to the unparameterized container or fall through to the
/// "could not find type" path, never a crash.
#[allow(
    dead_code,
    reason = "wired into interface/member resolution in WP-D; unit-tested now"
)]
pub(crate) fn resolve_datatype(ctx: &mut AnalysisContext, opt: Option<NodeId>) -> DataType {
    let bad_type = DataType::variant(); // VARIANT / INFERRED

    let Some(type_id) = opt else {
        return bad_type;
    };

    let cur = ctx.get_type(type_id).clone();
    if cur.is_resolving() {
        ctx.push_error("Could not resolve datatype: Cyclic reference.", type_id);
        return bad_type;
    }
    if !cur.has_no_type() {
        return cur; // already resolved
    }

    let (chain, containers) = type_node_parts(ctx, type_id);

    // Empty chain ⇒ `void` (analyzer.cpp:679-685).
    let Some(&first_id) = chain.first() else {
        let result = DataType {
            type_source: TypeSource::AnnotatedExplicit,
            kind: DtKind::Builtin,
            builtin_type: VariantType::Nil,
            ..Default::default()
        };
        ctx.set_type(type_id, result.clone());
        return result;
    };

    ctx.set_type(
        type_id,
        DataType {
            kind: DtKind::Resolving,
            ..Default::default()
        },
    );

    let first = ident_name(ctx, first_id).unwrap_or_default();
    let mut result = DataType {
        type_source: TypeSource::AnnotatedExplicit,
        ..Default::default()
    };

    // analyzer.cpp:691-722 — head segment may be a local declared in the current scope. Godot
    // reaches it through `IdentifierNode::suite->has_local(first)`; gdls walks
    // [`AnalysisContext::suite_stack`] via [`crate::reducer::lookup_local`]. A local CONSTANT may
    // be used as a type only when its initializer's type is a hard meta-type (e.g.
    // `const E = MyEnum`); a non-meta value is an error UNLESS the value is `Variant` — in which
    // case gdls degrades silently per the "unknown stays dynamic" rule (`docs/00`), since a
    // cross-file `preload(...)` result that didn't resolve to a script meta-type ends up here as
    // Variant and we'd otherwise false-positive on `features/local_const_as_type.gd`'s
    // `const O = preload(...)` style. Any other local kind (variable / parameter / for-iterator /
    // pattern-bind) is rejected outright.
    if let Some(local) = crate::reducer::lookup_local(ctx, &first) {
        match local.kind {
            gd_syntax::ast::LocalKind::Constant => {
                let const_dt = ctx.get_type(local.source).clone();
                if !const_dt.is_set() || const_dt.has_no_type() {
                    ctx.push_error(
                        format!(r#"Local constant "{first}" is not resolved at this point."#),
                        first_id,
                    );
                    return bad_type;
                }
                if const_dt.is_meta_type {
                    result = const_dt;
                } else if const_dt.is_variant() {
                    // Cross-file degradation — defer to the global "unknown stays dynamic" rule.
                    return bad_type;
                } else {
                    ctx.push_error(
                        format!(r#"Local constant "{first}" is not a valid type."#),
                        first_id,
                    );
                    return bad_type;
                }
            }
            other => {
                let kind_name = match other {
                    gd_syntax::ast::LocalKind::Variable => "variable",
                    gd_syntax::ast::LocalKind::Parameter => "parameter",
                    gd_syntax::ast::LocalKind::ForVariable => "for loop iterator",
                    gd_syntax::ast::LocalKind::PatternBind => "pattern bind",
                    gd_syntax::ast::LocalKind::Constant => unreachable!(
                        "invariant: LocalKind::Constant is matched by the `Constant`-typed arm above (around L577); only the other LocalKind variants reach this fallback"
                    ),
                };
                ctx.push_error(
                    format!(r#"Local {kind_name} "{first}" cannot be used as a type."#),
                    first_id,
                );
                return bad_type;
            }
        }
    } else if first == "Variant" {
        // `Variant.Type` / `Variant.Operator` annotations resolve through the dump's global
        // enums (registered under the dotted name); the bare case is `Variant`.
        if chain.len() == 2 {
            let seg = ident_name(ctx, chain[1]).unwrap_or_default();
            let dotted = format!("Variant.{seg}");
            if ctx.native.global_enum(&dotted).is_some() {
                ctx.set_type(type_id, make_global_enum_type(ctx, &dotted, "", true));
                return ctx.get_type(type_id).clone();
            }
        }
        result.kind = DtKind::Variant;
    } else if let Some(builtin) = builtin_type_from_name(&first) {
        // Builtin scalar/container. Element typing for Array/Dictionary and nested builtin enums are
        // WP-D; the unparameterized builtin is correct on its own.
        result.kind = DtKind::Builtin;
        result.builtin_type = builtin;
    } else if ctx.native.class_named(&first).is_some() {
        // Native engine class (analyzer.cpp:784-788).
        result.kind = DtKind::Native;
        result.builtin_type = VariantType::Object;
        result.native_type = first.clone();
    } else if let Some(fid) = ctx.xfile.global_class_file(&first) {
        // Project `class_name` (analyzer.cpp:789-805).
        result = if Some(fid) == ctx.file {
            ctx.get_type(root_or(ctx, type_id)).clone()
        } else {
            script_base_datatype(ctx, fid)
        };
    } else if let Some(base) = datatype_in_scope(ctx, &first) {
        // In-file class in the current scope (analyzer.cpp:847-900, class-name case).
        result = base;
    } else if ctx.native.global_enum(&first).is_some() {
        // analyzer.cpp:806-815 — `@GlobalScope` enum (e.g. `Side`, `ClockDirection`). Resolves
        // to the enum's meta type.
        result = make_global_enum_type(ctx, &first, "", true);
    } else if let Some(fid) = ctx.xfile.autoload_file(&first) {
        // analyzer.cpp:830-845 — an autoload singleton used as a type annotation
        // (`func get_global() -> Global:`). Resolves to the autoload script's class meta, same
        // shape as the global-class arm; nested segments (`Keychain.InputAction`) continue
        // through the Script-segment walk below.
        result = script_base_datatype(ctx, fid);
    } else if let Some(dt) = inherited_enum_annotation(ctx, &first) {
        // A bare enum NAME from the class's INHERITED scope: a cross-file script base's enum
        // (`-> Status` with `enum Status` on the base) or a native base-chain enum (LimboAI's
        // `BT.Status` reachable bare inside `extends BTDecorator`). Godot's in-scope type
        // lookup includes base members (analyzer.cpp:860-898) and ClassDB enums.
        result = dt;
    }

    if !result.is_set() {
        // analyzer.cpp:889-892 — `Could not find type "X" in the current scope.` The
        // resolution exhausted: not a builtin, not a native, not a global class_name, not an
        // in-file class scope, not a class-level constant, not a local meta-typed constant,
        // not a global enum, not a cross-file base inner class.
        ctx.push_error(
            format!(r#"Could not find type "{first}" in the current scope."#),
            first_id,
        );
        return bad_type;
    }

    // Nested `A.B` segments under an in-file `Class` base (analyzer.cpp:908-939); native-enum nesting
    // and the deeper reducer-driven cases are WP-D.
    if chain.len() > 1 && result.kind == DtKind::Class {
        for (i, &id) in chain[1..].iter().enumerate() {
            let seg = ident_name(ctx, id).unwrap_or_default();
            // analyzer.cpp:910-939 — match inner Class first, then inner Enum (matches the
            // Godot's `is_class` arm + the `Member::ENUM` arm).
            let parent_class_node = result.class_node;
            let inner_class = parent_class_node.and_then(|c| inner_class_named(ctx, c, &seg));
            match inner_class {
                Some(inner_id) => {
                    if ctx.get_type(inner_id).has_no_type() {
                        let _ = resolve_class_inheritance(ctx, inner_id, Some(id));
                    }
                    result = ctx.get_type(inner_id).clone();
                }
                None => {
                    // Inner enum lookup — analyzer.cpp:921's `Member::ENUM` arm. Godot
                    // resolves the parent class's members up to this point so `MyEnum` on
                    // `OuterClass.InnerClass.MyEnum` lands as the enum's meta type. Only valid
                    // as the *last* segment in the chain (matches Godot's enum-as-leaf rule).
                    let is_last_segment = i + 1 == chain.len() - 1;
                    let parent_class_id = parent_class_node;
                    let inner_enum =
                        parent_class_id.and_then(|c| match class_member(ctx, c, &seg) {
                            Some(Member::Enum(eid)) => Some(eid),
                            _ => None,
                        });
                    if let (Some(eid), true) = (inner_enum, is_last_segment) {
                        if let Some(parent_id) = parent_class_id {
                            if ctx.get_type(eid).has_no_type() {
                                // Trigger the parent's member resolution so the enum is typed.
                                if let Some(idx) = match &ctx.node(parent_id).kind {
                                    NodeKind::Class(c) => c.members_indices.get(&seg).copied(),
                                    _ => None,
                                } {
                                    resolve_class_member(ctx, parent_id, idx, Some(id));
                                }
                            }
                            result = ctx.get_type(eid).clone();
                        } else {
                            return bad_type;
                        }
                    } else {
                        // analyzer.cpp:933 — `Could not find type "X" under base "Y".` The base
                        // is an in-file `Class` (we walked here from one), so we have a concrete
                        // identifier to render via `class_identifier_name_or_default`. Safe to
                        // emit here because: (a) inner Class lookup didn't match (above),
                        // (b) inner Enum lookup didn't match either, (c) the parent is in-file
                        // so the corpus has full visibility into its members. Cross-file Script
                        // parents go through the `interface()` walk and don't reach this code path.
                        let base_name =
                            crate::reducer::class_identifier_name_or_default(ctx, &result);
                        ctx.push_error(
                            format!(r#"Could not find type "{seg}" under base "{base_name}"."#),
                            id,
                        );
                        return bad_type;
                    }
                }
            }
        }
    } else if chain.len() > 1 && result.kind == DtKind::Script {
        // Cross-file nested types under a global-class / autoload head: an enum leaf
        // (`-> BaseLayer.BlendModes`) or inner-class hops (`Keychain.InputAction`). Godot
        // resolves these through the depended parser's members (analyzer.cpp:908-939); gdls
        // walks the interface. A miss degrades to a SILENT Variant — interfaces are shallow
        // extracts and a gap in them must never become a `Could not find type` error (the same
        // rule as the cross-file hop in `resolve_extends`).
        for (i, &id) in chain[1..].iter().enumerate() {
            let seg = ident_name(ctx, id).unwrap_or_default();
            let is_last = i + 1 == chain.len() - 1;
            let Some(sr) = result.script_type.clone() else {
                return bad_type;
            };
            // Named enum as the LEAF (enums cannot contain nested types) — head-class enums
            // only; inner-class enum leaves degrade below.
            if is_last && sr.inner.is_empty() {
                if let Some(dt) = crate::reducer::cross_file_named_enum(ctx, sr.file, &seg, true) {
                    result = dt;
                    continue;
                }
            }
            // Inner-class hop.
            let mut inner: Vec<&str> = sr.inner.iter().map(String::as_str).collect();
            inner.push(&seg);
            if ctx.xfile.resolve_inner_chain(sr.file, &inner).is_some() {
                let inner_owned: Vec<String> = inner.into_iter().map(String::from).collect();
                let next = ScriptRef {
                    file: sr.file,
                    inner: inner_owned,
                };
                result = DataType {
                    kind: DtKind::Script,
                    type_source: TypeSource::AnnotatedExplicit,
                    is_meta_type: true,
                    builtin_type: VariantType::Object,
                    native_type: crate::script_chain::chain_native_root(ctx, &next)
                        .unwrap_or_default(),
                    script_type: Some(next),
                    ..Default::default()
                };
                continue;
            }
            return bad_type;
        }
    } else if chain.len() == 2 && result.kind == DtKind::Native {
        // analyzer.cpp:922-934 — `TileSet.TileShape` style: a native class followed by exactly one
        // segment that names an enum on that class (or one of its bases). Anything longer
        // (`TileSet.TileShape.X`) is rejected by Godot as "Enums cannot contain nested types.";
        // gdls degrades silently for the over-deep case per the "unknown stays dynamic" rule until
        // the full nested-enum-value resolution lands alongside the cross-file slice. Missing-enum
        // names also degrade silently for fidelity-fixture-trimmed dumps.
        let seg = ident_name(ctx, chain[1]).unwrap_or_default();
        if crate::reducer::native_has_enum(ctx, &result.native_type, &seg) {
            result = make_native_enum_type(ctx, &seg, &result.native_type, true);
        } else {
            return bad_type;
        }
    }

    // Container element types — `Array[T]`, `Dictionary[K, V]` (analyzer.cpp:894-925). Godot
    // walks each `container_types[i]` via `resolve_datatype`, strips the metatype, and stamps it
    // onto `result.container_element_types`. Element typing only applies when the base is
    // Builtin Array / Dictionary; other bases reject containers (caught by the parser).
    if !containers.is_empty() && result.kind == DtKind::Builtin {
        let expected = match result.builtin_type {
            VariantType::Array => 1,
            VariantType::Dictionary => 2,
            _ => 0,
        };
        if expected > 0 {
            for &cid in containers.iter().take(expected) {
                let inner = type_from_metatype(resolve_datatype(ctx, Some(cid)));
                result.container_element_types.push(inner);
            }
            // Fill remaining slots with Variant when the parser provides fewer than expected
            // (defensive — Godot's parser enforces this).
            while result.container_element_types.len() < expected {
                result.container_element_types.push(DataType::variant());
            }
        }
    }

    ctx.set_type(type_id, result.clone());
    result
}

// ===================================================================================================
// Builtin-type name table — GDScriptParser::get_builtin_type (gdscript_parser.cpp:55)
// ===================================================================================================

/// `GDScriptParser::get_builtin_type(name)` (gdscript_parser.cpp:55): every `Variant::Type` name
/// *except* `Nil` and `Object` (those are not GDScript builtin type annotations — `Object` is a native
/// class, `void`/`Variant` are handled separately). Names are exactly `Variant::get_type_name`'s.
pub fn builtin_type_from_name(name: &str) -> Option<VariantType> {
    use VariantType::*;
    Some(match name {
        "bool" => Bool,
        "int" => Int,
        "float" => Float,
        "String" => String,
        "Vector2" => Vector2,
        "Vector2i" => Vector2i,
        "Rect2" => Rect2,
        "Rect2i" => Rect2i,
        "Vector3" => Vector3,
        "Vector3i" => Vector3i,
        "Transform2D" => Transform2d,
        "Vector4" => Vector4,
        "Vector4i" => Vector4i,
        "Plane" => Plane,
        "Quaternion" => Quaternion,
        "AABB" => Aabb,
        "Basis" => Basis,
        "Transform3D" => Transform3d,
        "Projection" => Projection,
        "Color" => Color,
        "StringName" => StringName,
        "NodePath" => NodePath,
        "RID" => Rid,
        "Callable" => Callable,
        "Signal" => Signal,
        "Dictionary" => Dictionary,
        "Array" => Array,
        "PackedByteArray" => PackedByteArray,
        "PackedInt32Array" => PackedInt32Array,
        "PackedInt64Array" => PackedInt64Array,
        "PackedFloat32Array" => PackedFloat32Array,
        "PackedFloat64Array" => PackedFloat64Array,
        "PackedStringArray" => PackedStringArray,
        "PackedVector2Array" => PackedVector2Array,
        "PackedVector3Array" => PackedVector3Array,
        "PackedColorArray" => PackedColorArray,
        "PackedVector4Array" => PackedVector4Array,
        _ => return None,
    })
}

// ===================================================================================================
// AST snapshot helpers — clone the small bits we need so we never hold a tree borrow across a `&mut`.
// ===================================================================================================

/// The class's own `extends`-name identifiers (`extends A.B.C` → `[A, B, C]`).
fn class_extends_names(ctx: &AnalysisContext, class_id: NodeId) -> Vec<NodeId> {
    match &ctx.node(class_id).kind {
        NodeKind::Class(c) => c.extends.clone(),
        _ => Vec::new(),
    }
}

fn class_extends_path(ctx: &AnalysisContext, class_id: NodeId) -> Option<String> {
    match &ctx.node(class_id).kind {
        NodeKind::Class(c) => c.extends_path.clone(),
        _ => None,
    }
}

fn class_extends_used(ctx: &AnalysisContext, class_id: NodeId) -> bool {
    matches!(&ctx.node(class_id).kind, NodeKind::Class(c) if c.extends_used)
}

fn class_outer(ctx: &AnalysisContext, class_id: NodeId) -> Option<NodeId> {
    match &ctx.node(class_id).kind {
        NodeKind::Class(c) => c.outer,
        _ => None,
    }
}

fn class_identifier(ctx: &AnalysisContext, class_id: NodeId) -> Option<NodeId> {
    match &ctx.node(class_id).kind {
        NodeKind::Class(c) => c.identifier,
        _ => None,
    }
}

fn class_identifier_name(ctx: &AnalysisContext, class_id: NodeId) -> Option<String> {
    ident_name(ctx, class_identifier(ctx, class_id)?)
}

/// The class node's fully-qualified class name, mirroring Godot's `ClassNode::fqcn`
/// (gdscript_parser.h:761). Construction matches `parse_class` / `parse_class_name`:
///
/// * **Head class** (no outer): `class_name` identifier if declared (parser.cpp:993:
///   `current_class->fqcn = String(current_class->identifier->name)`), otherwise the
///   `ctx.script_path` (parser.cpp:702: `head->fqcn = canonicalize_path(script_path)`).
/// * **Inner class**: `<outer fqcn>::<identifier>` (parser.cpp:947-951).
///
/// Used by `make_class_enum_type` to disambiguate same-named enums declared in different
/// classes (the corpus's `enum_class_var_assign_with_wrong_enum_type` family).
fn class_fqcn(ctx: &AnalysisContext, class_id: NodeId) -> String {
    if let Some(outer) = class_outer(ctx, class_id) {
        let parent_fqcn = class_fqcn(ctx, outer);
        let name = class_identifier_name(ctx, class_id).unwrap_or_default();
        if parent_fqcn.is_empty() {
            name
        } else if name.is_empty() {
            parent_fqcn
        } else {
            format!("{parent_fqcn}::{name}")
        }
    } else {
        // Head class: `class_name` wins over `script_path` (Godot's parser.cpp:993 override).
        class_identifier_name(ctx, class_id).unwrap_or_else(|| ctx.script_path.clone())
    }
}

/// Godot's display name for a class metatype in the cyclic-class message: the class name, or
/// `<main>` for the unnamed head.
fn class_meta_name(ctx: &AnalysisContext, class_id: NodeId) -> String {
    class_identifier_name(ctx, class_id).unwrap_or_else(|| "<main>".to_owned())
}

/// The class node's in-`class` members (`Member::Class`) — the recursive set `resolve_class_inheritance`
/// descends into.
fn inner_classes(ctx: &AnalysisContext, class_id: NodeId) -> Vec<NodeId> {
    match &ctx.node(class_id).kind {
        NodeKind::Class(c) => c
            .members
            .iter()
            .filter_map(|m| match m {
                Member::Class(id) => Some(*id),
                _ => None,
            })
            .collect(),
        _ => Vec::new(),
    }
}

/// An inner `class` named `name` directly inside `class_id`, if any.
pub(crate) fn inner_class_named(
    ctx: &AnalysisContext,
    class_id: NodeId,
    name: &str,
) -> Option<NodeId> {
    match class_member(ctx, class_id, name) {
        Some(Member::Class(id)) => Some(id),
        _ => None,
    }
}

/// A named member of a class, cloned (Godot's `p_class->get_member(name)`).
fn class_member(ctx: &AnalysisContext, class_id: NodeId, name: &str) -> Option<Member> {
    match &ctx.node(class_id).kind {
        NodeKind::Class(c) => {
            let idx = *c.members_indices.get(name)?;
            c.members.get(idx).cloned()
        }
        _ => None,
    }
}

/// Resolve a bare name to an in-scope in-file class's meta type (the class-name arm of
/// `resolve_datatype`'s scope search). Wired in WP-D and consumed since.
fn datatype_in_scope(ctx: &mut AnalysisContext, name: &str) -> Option<DataType> {
    let scope = match ctx.current_class {
        Some(c) => scope_classes(ctx, c),
        None => return None,
    };
    for look in scope.iter().copied() {
        if class_identifier_name(ctx, look).as_deref() == Some(name) {
            return Some(ctx.get_type(look).clone());
        }
        // analyzer.cpp:860-898 — match class members by name and dispatch on member kind. The
        // Godot's switch covers CLASS / ENUM / CONSTANT (meta-typed or script-typed); other kinds
        // fall through to a "X is a Y but does not contain a type" error. WP-J wires the ENUM
        // arm; the CONSTANT arms join with the cross-file resolved-interface cache (WP-N5).
        match class_member(ctx, look, name) {
            Some(Member::Class(inner_id)) => {
                if ctx.get_type(inner_id).has_no_type() {
                    let _ = resolve_class_inheritance(ctx, inner_id, None);
                }
                return Some(ctx.get_type(inner_id).clone());
            }
            Some(Member::Enum(enum_id)) => {
                // analyzer.cpp:869-872. The enum's datatype is the meta type
                // (`make_class_enum_type(meta=true)`) stamped onto the enum node by
                // `resolve_enum_type` during interface resolution. If the enum still reads as
                // `Unresolved` (the body phase reached this annotation before the interface
                // phase materialised the enum's meta type — e.g. an enum declared after a
                // function that uses it), trigger the parent class's interface resolution.
                if ctx.get_type(enum_id).has_no_type() {
                    resolve_class_interface(ctx, look);
                }
                if ctx.get_type(enum_id).has_no_type() {
                    // STILL untyped ⇒ we are mid-interface-resolution of this very class (the
                    // `resolved_interfaces` guard no-opped the call) and the enum is declared
                    // AFTER the member naming it — `signal s(t: MyEnum)` above `enum MyEnum`.
                    // Godot's two-pass interface resolution handles the order; mirror it by
                    // resolving just this enum member directly.
                    if let Some(idx) = match &ctx.node(look).kind {
                        NodeKind::Class(c) => c.members_indices.get(name).copied(),
                        _ => None,
                    } {
                        resolve_class_member(ctx, look, idx, None);
                    }
                }
                return Some(ctx.get_type(enum_id).clone());
            }
            Some(Member::Constant(const_id)) => {
                // analyzer.cpp:874-882's CONSTANT arm — when a class-level constant exists,
                // the lookup succeeds (the constant's value is the candidate type). Trigger
                // resolution if its type isn't yet stamped, then return whatever the
                // constant's type is. For preload/script-meta constants the type is a Script
                // meta — usable as a type annotation. For constants whose initializer doesn't
                // fold to a meta (Variant, etc.) we still return the Variant — this prevents
                // the head-segment `Could not find type` from firing on names that DO exist
                // as constants but whose value gdls can't fully resolve (preload-derived
                // class chains, etc.); the alternative — falling through — would emit a
                // misleading `Could not find type` instead of degrading to the
                // "unknown stays dynamic" path.
                if ctx.get_type(const_id).has_no_type() {
                    if let Some(idx) = match &ctx.node(look).kind {
                        NodeKind::Class(c) => c.members_indices.get(name).copied(),
                        _ => None,
                    } {
                        resolve_class_member(ctx, look, idx, None);
                    }
                }
                let const_dt = ctx.get_type(const_id).clone();
                if const_dt.is_set() {
                    return Some(const_dt);
                }
            }
            Some(_) => {}
            None => {}
        }
    }

    // analyzer.cpp:860-898's cross-file fallthrough: when none of the in-file scope classes
    // contain `name`, walk the in-file root class's cross-file base chain (every link via
    // `crate::script_chain`, including `Extends::Names` hops the old Path-only loop missed)
    // looking for an inner class. Handles `features/external_parser.gd`'s
    // `var _v: TypeFromBase` where TypeFromBase is an inner class of a transitively-preloaded
    // base.
    for look in scope {
        let base = ctx.bases.get(&look).cloned().unwrap_or_default();
        if base.kind == crate::data_type::DtKind::Script {
            if let Some(sr) = base.script_type.as_ref() {
                let chain = crate::script_chain::resolve_script_chain(ctx, sr);
                for link in &chain.links {
                    let Some(iface) = crate::script_chain::link_interface(ctx.xfile, link) else {
                        continue;
                    };
                    if iface
                        .inner
                        .iter()
                        .any(|i| i.class_name.as_deref() == Some(name))
                    {
                        // Found an inner class with this name in a cross-file base.
                        // Return a Script meta type with the file id — sufficient to mark
                        // the name as a valid type annotation so the head-segment
                        // "Could not find type" error doesn't false-positive.
                        return Some(script_base_datatype(ctx, link.file));
                    }
                }
            }
        }
    }
    None
}

fn type_node_parts(ctx: &AnalysisContext, type_id: NodeId) -> (Vec<NodeId>, Vec<NodeId>) {
    match &ctx.node(type_id).kind {
        NodeKind::Type(t) => (t.type_chain.clone(), t.container_types.clone()),
        _ => (Vec::new(), Vec::new()),
    }
}

fn ident_name(ctx: &AnalysisContext, id: NodeId) -> Option<String> {
    match &ctx.node(id).kind {
        NodeKind::Identifier(i) => Some(i.name.clone()),
        _ => None,
    }
}

/// The tree root, falling back to `fallback` if (impossibly) absent — keeps the self-class lookup
/// total without an `unwrap`.
fn root_or(ctx: &AnalysisContext, fallback: NodeId) -> NodeId {
    ctx.tree.root_id().unwrap_or(fallback)
}

// ===================================================================================================
// resolve_interface — analyzer.cpp:1268-1356 (driver), 962-1266 (members), 234-318 (conflicts),
// 1729-1968 (function signature), 2073-2244 (assignables)
//
// WP-D ports the reducer-free skeleton: the interface driver, the member dispatch, the member-name
// conflict checks, typed member/parameter/return signatures, and signal/enum types. Reducer-dependent
// work — initializer inference, constant values, custom enum values, default arguments, the
// parent-signature covariance check + NATIVE_METHOD_OVERRIDE (needs get_function_signature +
// is_type_compatible), and annotation `apply()` effects — is deferred to WP-E/F and flagged inline.
// ===================================================================================================

/// `GDScriptAnalyzer::resolve_interface()` (analyzer.cpp:6582): resolve the head class interface and,
/// recursively, every inner class. Crate-internal: external callers go through [`crate::analyze`].
pub(crate) fn resolve_interface(ctx: &mut AnalysisContext) {
    if let Some(root) = ctx.tree.root_id() {
        resolve_class_interface_recursive(ctx, root, true);
    }
}

/// `resolve_class_interface(p_class, bool p_recursive)` (analyzer.cpp:1345).
fn resolve_class_interface_recursive(ctx: &mut AnalysisContext, class_id: NodeId, recursive: bool) {
    resolve_class_interface(ctx, class_id);
    if recursive {
        for inner in inner_classes(ctx, class_id) {
            resolve_class_interface_recursive(ctx, inner, true);
        }
    }
}

/// `resolve_class_interface(p_class, const Node *p_source)` (analyzer.cpp:1268).
fn resolve_class_interface(ctx: &mut AnalysisContext, class_id: NodeId) {
    if ctx.resolved_interfaces.contains(&class_id) {
        return;
    }
    // Ensure inheritance is resolved (idempotent — already run in the inheritance pass).
    if resolve_class_inheritance(ctx, class_id, None).is_err() {
        return;
    }
    ctx.resolved_interfaces.insert(class_id);

    // Resolve the base class's interface first if it's an in-file class (analyzer.cpp:1311-1315).
    let base = ctx.base_type(class_id);
    if base.kind == DtKind::Class {
        if let Some(bc) = base.class_node {
            resolve_class_interface(ctx, bc);
        }
    }

    for i in 0..member_count(ctx, class_id) {
        resolve_class_member(ctx, class_id, i, None);
    }
}

/// Trigger [`resolve_class_member`] for the in-`class_id` member named `name` (called from
/// [`crate::reducer::reduce_identifier`] when its lookup finds an unresolved class member). The
/// `source` parameter anchors any cyclic-reference error at the referring identifier (Godot's
/// `p_source` at analyzer.cpp:985). No-op if no member of that name exists.
pub(crate) fn resolve_class_member_by_name(
    ctx: &mut AnalysisContext,
    class_id: NodeId,
    name: &str,
    source: NodeId,
) {
    let idx = match &ctx.node(class_id).kind {
        NodeKind::Class(c) => c.members_indices.get(name).copied(),
        _ => None,
    };
    if let Some(i) = idx {
        resolve_class_member(ctx, class_id, i, Some(source));
    }
}

/// `resolve_class_member(p_class, int p_index, p_source)` (analyzer.cpp:967): resolve one member's
/// signature. The `source` parameter (analyzer.cpp's `p_source`) anchors the cyclic-reference
/// error at the **referring** identifier (not at the member's declaration) when re-entered through
/// [`resolve_class_member_by_name`] from `reduce_identifier`'s class-member lookup — matching the
/// Godot's behaviour where the corpus pins the error at the identifier reference line, not the
/// declaration line.
fn resolve_class_member(
    ctx: &mut AnalysisContext,
    class_id: NodeId,
    index: usize,
    source: Option<NodeId>,
) {
    let Some(member) = nth_member(ctx, class_id, index) else {
        return;
    };

    // Godot's guard (analyzer.cpp:984-991) sits *before* the dispatch switch, applying uniformly:
    // a member already resolved (e.g. an inner class, resolved in the inheritance pass) returns early —
    // which is why the CLASS-arm conflict check below is effectively never reached.
    if let Some(member_node) = member_node_id(&member) {
        let dt = ctx.get_type(member_node);
        if dt.is_resolving() {
            let name = decl_identifier_name(ctx, member_node);
            ctx.push_error(
                format!(r#"Could not resolve member "{name}": Cyclic reference."#),
                source.unwrap_or(member_node),
            );
            return;
        }
        if dt.is_set() {
            return;
        }
    }

    let previous_class = ctx.current_class;
    ctx.current_class = Some(class_id);

    match member {
        Member::Variable(id) => {
            let name = decl_identifier_name(ctx, id);
            check_class_member_name_conflict(ctx, class_id, &name, false, id);
            ctx.set_type(id, resolving());
            let (spec, init, infer) = variable_assignable_parts(ctx, id);
            // analyzer.cpp:1407 — a `static var` is resolved in a `static_context`. This drives
            // the `Cannot access/call non-static … from a static variable initializer.` checks
            // in `reduce_identifier`/`reduce_call` when the initializer references an instance
            // member (the corpus's `static_var_init_non_static_access` / `_call` cases).
            let is_static = matches!(
                &ctx.node(id).kind,
                NodeKind::Variable(v) if v.is_static
            );
            let prev_static = ctx.static_context;
            if is_static {
                ctx.static_context = true;
            }
            // WP-R2: name-side cross-file cycle marker. Mirrors Godot's per-member
            // `DataType::RESOLVING` flag (analyzer.cpp:984) — the kind-side marker is the
            // `resolving()` stamp two lines up; this stores the **name** so cross-file
            // consumers can check `(ctx.file, name)` without owning the NodeId.
            let prev_resolving = ctx.current_resolving_member.take();
            ctx.current_resolving_member = Some(name.clone());
            resolve_assignable(ctx, id, spec, init, infer, false);
            ctx.current_resolving_member = prev_resolving;
            ctx.static_context = prev_static;
            warn_confusable_identifier(ctx, id);
            warn_class_member_shadows_global(ctx, id, "variable");
        }
        Member::Constant(id) => {
            let name = decl_identifier_name(ctx, id);
            check_class_member_name_conflict(ctx, class_id, &name, false, id);
            ctx.set_type(id, resolving());
            let (spec, init, infer) = constant_assignable_parts(ctx, id);
            // WP-R2: name-side cross-file cycle marker (see the Variable arm).
            let prev_resolving = ctx.current_resolving_member.take();
            ctx.current_resolving_member = Some(name.clone());
            resolve_assignable(ctx, id, spec, init, infer, true);
            ctx.current_resolving_member = prev_resolving;
            // Const-only typed-array element narrowing: when the init is a homogeneous-typed
            // array literal (`const X := [0, 1, 2]`), Godot stamps Array[int] onto the
            // constant. Narrows on top of the resolve_assignable type so downstream subscripts
            // (`X[0]`) yield the element type instead of plain Variant. Mirrors Godot's
            // `make_array_from_constant` typed-array projection for constants.
            if let Some(init_id) = init {
                if let NodeKind::Array(a) = ctx.node(init_id).kind.clone() {
                    if !a.elements.is_empty() {
                        let first_t = ctx.get_type(a.elements[0]).clone();
                        let homogeneous = a.elements.iter().all(|&el| {
                            let t = ctx.get_type(el);
                            t.kind == first_t.kind && t.builtin_type == first_t.builtin_type
                        });
                        if homogeneous && first_t.kind == DtKind::Builtin {
                            let mut dt = ctx.get_type(id).clone();
                            if dt.kind == DtKind::Builtin
                                && dt.builtin_type == VariantType::Array
                                && dt.container_element_types.is_empty()
                            {
                                let mut elem = first_t.clone();
                                elem.is_meta_type = false;
                                elem.is_constant = false;
                                dt.container_element_types.push(elem);
                                ctx.set_type(id, dt);
                            }
                        }
                    }
                }
            }
        }
        Member::Signal(id) => {
            let name = decl_identifier_name(ctx, id);
            check_class_member_name_conflict(ctx, class_id, &name, false, id);
            ctx.set_type(id, resolving());
            let sig = resolve_signal_type(ctx, id, &name);
            ctx.set_type(id, sig);
        }
        Member::Enum(id) => {
            let name = decl_identifier_name(ctx, id);
            check_class_member_name_conflict(ctx, class_id, &name, false, id);
            ctx.set_type(id, resolving());
            let enum_type = resolve_enum_type(ctx, id, class_id, &name);
            ctx.set_type(id, enum_type);
        }
        Member::Function(id) => {
            // Functions are not conflict-checked (they may override a parent function).
            // Apply function-level annotations BEFORE signature resolution
            // (analyzer.cpp:1206-1209) — this is what makes `@abstract`'s static-misuse and
            // duplicate-on-function errors interleave with Godot's class-level @abstract
            // emissions in the right order.
            apply_function_abstract_annotation(ctx, id);
            resolve_function_signature(ctx, id);
        }
        Member::Class(inner_id) => {
            // Reached only if the inner class is unresolved (the top guard skips resolved ones, which
            // is the normal path — inheritance resolves inner classes first).
            let name = class_identifier_name(ctx, inner_id).unwrap_or_default();
            check_class_member_name_conflict(ctx, class_id, &name, false, inner_id);
        }
        // Unnamed-enum values: a hoisted-to-class-member alias for a value of an *anonymous*
        // enum (named-enum values aren't promoted to standalone members — they live in the
        // EnumNode's `values` vec and are typed by `resolve_enum_type`). Godot wires this at
        // analyzer.cpp:1217-1247: type the identifier as `make_class_enum_type(UNNAMED_ENUM,
        // …, meta=false)` so a `Parent.E` subscript yields a typed-enum value rather than an
        // Unresolved that would false-positive "Cannot find member".
        //
        // Full custom-value resolution (Godot's `parent_enum->values[index-1].value + 1`
        // chain) needs back-pointer tracking gdls doesn't carry yet; for now we only stamp the
        // identifier type. The fold value is left to `resolve_enum_type` when (and if) the
        // parent EnumNode is added as a class member — which only happens for named enums.
        Member::EnumValue(ev) => {
            if let Some(iid) = ev.identifier {
                let name = ident_name(ctx, iid).unwrap_or_default();
                // analyzer.cpp:1217-1247 + the conflict walk at :234-318 — an unnamed-enum value
                // is a hoisted class member so it shadows base-class members the same way a var
                // / const / enum / signal does. Without this, `enum { V }` in a subclass whose
                // base also defines `V` (as enum value, var, const, etc.) misses the
                // `The member "X" already exists in parent class …` template.
                check_class_member_name_conflict(ctx, class_id, &name, false, iid);
                // Set RESOLVING so a cyclic custom-value reference re-entering this member
                // triggers the cyclic_ref check at the top of `resolve_class_member`.
                ctx.set_type(iid, resolving());
                if let Some(cv) = ev.custom_value {
                    crate::reducer::reduce_expression(ctx, cv, false);
                }
                let mut t = make_class_enum_type(ctx, "<anonymous enum>", class_id, false);
                t.is_constant = true;
                ctx.set_type(iid, t);
                // Stamp a placeholder folded `Int` so that downstream callers (e.g.
                // `update_const_expression_builtin_type` at the assignment-with-specified-type
                // check) treat the value as a constant. Full `parent_enum->values[index-1].value
                // + 1` chain (analyzer.cpp:1174-1175) needs back-pointer tracking gdls doesn't
                // carry; using 0 here is sufficient for the **constancy** signal — the value
                // itself isn't used by the assignment-compat check, only `is_constant` and
                // `is_reduced` (the latter via the fold-table membership).
                ctx.folds.set(iid, crate::foldtable::FoldedValue::Int(0));
            }
        }
        Member::Group(_) => {}
    }

    ctx.current_class = previous_class;
}

/// The node carrying a member's resolved datatype (Godot's `member.get_datatype()` target), for the
/// uniform resolve guard.
fn member_node_id(member: &Member) -> Option<NodeId> {
    match member {
        Member::Variable(id)
        | Member::Constant(id)
        | Member::Signal(id)
        | Member::Enum(id)
        | Member::Function(id)
        | Member::Class(id) => Some(*id),
        Member::EnumValue(ev) => ev.identifier,
        Member::Group(_) => None,
    }
}

// --- Member-name conflict checks (analyzer.cpp:234-318) ---------------------------------------------

/// `check_class_member_name_conflict` (analyzer.cpp:291): walk the base chain for a redefinition.
fn check_class_member_name_conflict(
    ctx: &mut AnalysisContext,
    class_id: NodeId,
    member_name: &str,
    member_is_function: bool,
    member_node: NodeId,
) {
    let mut base = ctx.base_type(class_id);
    while base.kind == DtKind::Class {
        let Some(parent) = base.class_node else { break };
        if has_member_name_conflict_in_script_class(ctx, member_name, parent, member_is_function) {
            let parent_name =
                class_identifier_name(ctx, parent).unwrap_or_else(|| "<anonymous>".to_owned());
            ctx.push_error(
                format!(
                    r#"The member "{member_name}" already exists in parent class {parent_name}."#
                ),
                member_node,
            );
            return;
        }
        base = ctx.base_type(parent);
    }

    // No native recursion needed — Node exposes all of Object's members (analyzer.cpp:307-315).
    if base.kind == DtKind::Native && !base.native_type.is_empty() {
        check_native_member_name_conflict(ctx, member_name, member_node, &base.native_type);
    }
}

/// `has_member_name_conflict_in_script_class` (analyzer.cpp:234).
fn has_member_name_conflict_in_script_class(
    ctx: &AnalysisContext,
    member_name: &str,
    parent: NodeId,
    member_is_function: bool,
) -> bool {
    match class_member(ctx, parent, member_name) {
        Some(Member::Variable(_))
        | Some(Member::Constant(_))
        | Some(Member::Enum(_))
        | Some(Member::EnumValue(_))
        | Some(Member::Class(_))
        | Some(Member::Signal(_)) => true,
        // A non-function may not share a name with a parent function (a function may override one).
        Some(Member::Function(_)) => !member_is_function,
        _ => false,
    }
}

/// `check_native_member_name_conflict` (analyzer.cpp:272).
fn check_native_member_name_conflict(
    ctx: &mut AnalysisContext,
    member_name: &str,
    member_node: NodeId,
    native_type: &str,
) {
    if has_member_name_conflict_in_native_type(ctx, member_name, native_type) {
        ctx.push_error(
            format!(
                r#"Member "{member_name}" redefined (original in native class '{native_type}')"#
            ),
            member_node,
        );
    } else if ctx.native.class_named(member_name).is_some() {
        ctx.push_error(
            format!(r#"The member "{member_name}" shadows a native class."#),
            member_node,
        );
    } else if builtin_type_from_name(member_name).is_some() {
        ctx.push_error(
            format!(r#"The member "{member_name}" cannot have the same name as a builtin type."#),
            member_node,
        );
    }
}

/// `has_member_name_conflict_in_native_type` (analyzer.cpp:255): a native signal/property/constant of
/// this name (the engine's `ClassDB::has_*` walk the inheritance chain, so we do too), or `script`.
fn has_member_name_conflict_in_native_type(
    ctx: &AnalysisContext,
    member_name: &str,
    native_type: &str,
) -> bool {
    if member_name == "script" {
        return true;
    }
    let mut cur = ctx.native.class_named(native_type);
    while let Some(class) = cur {
        let has = class
            .signals
            .iter()
            .any(|s| ctx.native.name_of(s.name) == member_name)
            || class
                .properties
                .iter()
                .any(|p| ctx.native.name_of(p.name) == member_name)
            || class
                .constants
                .iter()
                .any(|c| ctx.native.name_of(c.name) == member_name);
        if has {
            return true;
        }
        cur = class.inherits.and_then(|s| ctx.native.class(s));
    }
    false
}

// --- Function signatures (analyzer.cpp:1729-1862; parent-match + NATIVE_METHOD_OVERRIDE → WP-E) -----

/// `resolve_function_signature` (analyzer.cpp:1729), signature only: parameter types, the return type,
/// and the `_init`/`_static_init` constructor rules. The polymorphism/native-override checks
/// (analyzer.cpp:1865-1966) need `get_function_signature` + `is_type_compatible` and land in WP-E.
pub(crate) fn resolve_function_signature(ctx: &mut AnalysisContext, func_id: NodeId) {
    let dt = ctx.get_type(func_id);
    if dt.is_resolving() {
        let name = decl_identifier_name(ctx, func_id);
        ctx.push_error(
            format!(r#"Could not resolve function "{name}": Cyclic reference."#),
            func_id,
        );
        return;
    }
    if dt.is_set() {
        return;
    }

    let (name, params, return_type, is_static) = function_decl(ctx, func_id);
    let previous_function = ctx.current_function;
    let previous_concrete = ctx.concrete_function;
    let previous_static = ctx.static_context;
    ctx.current_function = Some(func_id);
    // analyzer.cpp:3645-3654's lambda chain walk reaches the outer concrete function. gdls
    // tracks the same value explicitly: `resolve_function_signature` and `resolve_function_body`
    // both push the function as the current concrete function. The drain helper later overrides
    // `concrete_function` to the captured outer when entering a lambda's body / param defaults.
    ctx.concrete_function = Some(func_id);
    ctx.static_context = is_static;

    // Snapshot the diagnostic count so the override-compat / native-override checks at the end
    // can skip when parameter resolution already emitted a cycle / cannot-resolve / unknown-type
    // error against this function. Mirrors Godot's implicit gate where those checks run after
    // type resolution completes — when resolution failed, the function's signature is partial and
    // any compat error would be a phantom on top of the real one. The cyclic_ref_override.gd
    // corpus case is exactly this shape: a parameter's default value calls into another override
    // chain and Godot emits only the cycle error, not the trailing override-compat one.
    let init_errors = ctx.diagnostic_count();

    ctx.set_type(func_id, resolving());

    for param in &params {
        resolve_parameter(ctx, *param);
        // analyzer.cpp:1787 — `is_shadowing(identifier, "function parameter", true)` per
        // parameter. The full Godot helper (analyzer.cpp:6135) also checks global identifiers,
        // base classes, and native ancestors; we currently port only the current-class branch
        // (the corpus's lambda-parameter-shadows-class-member case at
        // `warnings/lambda_shadowing_arg.gd`). The other branches stay deferred — once they
        // land we'll lift the broader `warnings/shadowning.gd` case alongside.
        warn_parameter_shadowing(ctx, *param);
    }

    // Rest parameter resolution + Godot-specific type validation
    // (gdscript_parser.cpp ~parse_parameter for rest params). The rest parameter
    // must be typed `Array` (and Array exactly — not `Array[T]`).
    let rest_param = match &ctx.node(func_id).kind {
        NodeKind::Function(f) => f.rest_parameter,
        _ => None,
    };
    if let Some(rp) = rest_param {
        resolve_parameter(ctx, rp);
        let rt = ctx.get_type(rp).clone();
        if rt.is_set() {
            let is_array = rt.kind == DtKind::Builtin && rt.builtin_type == VariantType::Array;
            if !is_array {
                ctx.push_error(
                    format!(r#"The rest parameter type must be "Array", but "{rt}" is specified."#),
                    rp,
                );
            } else if !rt.container_element_types.is_empty() {
                ctx.push_error(
                    "Typed arrays are currently not supported for the rest parameter.",
                    rp,
                );
            }
        }
    }

    if name == "_init" {
        // Constructor: returns an instance of the current class (analyzer.cpp:1830-1840).
        let mut return_dt = ctx
            .current_class
            .map(|c| ctx.get_type(c).clone())
            .unwrap_or_default();
        return_dt.is_meta_type = false;
        ctx.set_type(func_id, return_dt);
        if let Some(rt) = return_type {
            let declared = resolve_datatype(ctx, Some(rt));
            if !(declared.kind == DtKind::Builtin && declared.builtin_type == VariantType::Nil) {
                ctx.push_error("Constructor cannot have an explicit return type.", rt);
            }
        }
    } else if name == "_static_init" {
        ctx.set_type(func_id, void_type());
        if let Some(rt) = return_type {
            let declared = resolve_datatype(ctx, Some(rt));
            if !(declared.kind == DtKind::Builtin && declared.builtin_type == VariantType::Nil) {
                ctx.push_error(
                    "Static constructor cannot have an explicit return type.",
                    rt,
                );
            }
        }
    } else if let Some(rt) = return_type {
        let resolved = type_from_metatype(resolve_datatype(ctx, Some(rt)));
        ctx.set_type(func_id, resolved);
    } else {
        // Untyped function ⇒ inferred Variant (analyzer.cpp:1857-1862).
        ctx.set_type(
            func_id,
            DataType {
                type_source: TypeSource::Inferred,
                kind: DtKind::Variant,
                ..Default::default()
            },
        );
    }

    // analyzer.cpp:1957-1961 — NATIVE_METHOD_OVERRIDE warning (error-by-default). Fires when
    // the function's name matches a method in the class's native-ancestor inheritance chain.
    // Symbols are [function_name, native_class_where_method_is_defined]. Godot's
    // `native_base` is filled by `get_function_signature` walking the class hierarchy, so the
    // class reported is **where the method exists** (e.g. `Object.get()` even when the script's
    // immediate base is `RefCounted`), not just the script's direct base.
    let _resolution_clean = ctx.diagnostic_count() == init_errors;

    if !name.is_empty() && name != "_init" && name != "_static_init" {
        // Suppress when an in-file script ancestor already declares this function: Godot's
        // `get_function_signature` returns at its `found_function` branch (analyzer.cpp:5853-5873)
        // BEFORE consulting `ClassDB` and setting `native_base`, so only the class whose
        // resolution actually reaches the native method warns. Without this, a class overriding a
        // native method already overridden by an in-file parent double-fires the error-by-default
        // warning down the whole chain (see tests/native_method_override.rs).
        let native_override = if script_ancestor_defines_function(ctx, &name) {
            None
        } else {
            enclosing_native_base(ctx, func_id)
                .and_then(|native| find_native_class_with_method(ctx, &native, &name))
        };
        if let Some(defining_class) = native_override {
            ctx.push_warning(
                crate::warnings::WarningCode::NativeMethodOverride,
                &[name.clone(), defining_class],
                func_id,
            );
        }

        // The override-signature MISMATCH check (analyzer.cpp:1865-1960) is deferred to
        // `resolve_function_body` so its emission lands AFTER any signature-pass emissions
        // from sibling functions (e.g. rest-parameter validation in `func g(...args: int)`).
        // But the parent-return-type adoption (analyzer.cpp:1878-1879) must run here, BEFORE
        // body resolution, so return-value compat checks inside the body fire against the
        // inherited type. This split mirrors Godot's interface-pass / body-pass separation
        // (the corpus `.out` files capture this order verbatim — see
        // `errors/variadic_functions.gd` for the visible effect on emission order).
        if _resolution_clean {
            adopt_parent_return_type(ctx, func_id, &name);
        }
    }

    ctx.current_function = previous_function;
    ctx.concrete_function = previous_concrete;
    ctx.static_context = previous_static;
}

/// Walk the current class's in-file base chain and, if a parent has a same-named function,
/// emit Godot's "The function signature doesn't match the parent..." error when the
/// override is incompatible (analyzer.cpp:1865-1960).
/// Adopt the parent's return type (signature-pass step). Must run BEFORE body resolution so
/// return-value compat checks inside the body fire against the inherited type
/// (analyzer.cpp:1878-1879).
fn adopt_parent_return_type(ctx: &mut AnalysisContext, func_id: NodeId, name: &str) {
    let Some(current_class) = ctx.current_class else {
        return;
    };
    let Some(parent_fn) = find_parent_function(ctx, current_class, name) else {
        return;
    };
    let parent = function_signature(ctx, parent_fn);
    let child_return = ctx.get_type(func_id).clone();
    if !child_return.is_hard_type() {
        ctx.set_type(func_id, parent.return_type.clone());
    }
}

/// Emit the override-signature-mismatch error (body-pass step). Runs after the body so the
/// emission lands AFTER any sibling functions' interface-pass errors — matching Godot's
/// observed emission order in corpus `.out` files (analyzer.cpp:1865-1960).
fn check_override_signature(ctx: &mut AnalysisContext, func_id: NodeId, name: &str) {
    let Some(current_class) = ctx.current_class else {
        return;
    };
    let Some(parent_fn) = find_parent_function(ctx, current_class, name) else {
        return;
    };

    let child = function_signature(ctx, func_id);
    let parent = function_signature(ctx, parent_fn);

    if signatures_match(ctx, &child, &parent) {
        return;
    }

    // analyzer.cpp's cyclic-ref override detection: if the parent function has a parameter
    // default that references the current class (by identifier name), the override-chain is
    // cyclic — emit `Could not resolve member "X": Cyclic reference.` rather than the
    // misleading signature-mismatch template (the parent's signature reads as partial
    // because its param default depends on the child's resolution). Matches
    // `errors/cyclic_ref_override.gd` (`InnerA.f`'s param `p := InnerB.new().f()` references
    // InnerB which extends InnerA).
    let current_class_name = class_identifier_name(ctx, current_class).unwrap_or_default();
    if !current_class_name.is_empty()
        && parent_default_references_class(ctx, parent_fn, &current_class_name)
    {
        ctx.push_error(
            format!(r#"Could not resolve member "{name}": Cyclic reference."#),
            func_id,
        );
        return;
    }

    let rendered = render_parent_signature(name, &parent);
    ctx.push_error(
        format!(
            r#"The function signature doesn't match the parent. Parent signature is "{rendered}"."#,
        ),
        func_id,
    );
}

/// Walk the parent function's parameter list looking for an identifier reference to
/// `class_name` (anywhere in any param's initializer expression). Returns true on first
/// hit — that's a class-self cyclic-ref pattern.
fn parent_default_references_class(
    ctx: &AnalysisContext,
    parent_fn: NodeId,
    class_name: &str,
) -> bool {
    let params = match &ctx.node(parent_fn).kind {
        NodeKind::Function(f) => f.parameters.clone(),
        _ => return false,
    };
    for p in &params {
        let init = match &ctx.node(*p).kind {
            NodeKind::Parameter(pn) => pn.initializer,
            _ => None,
        };
        if let Some(init_id) = init {
            let mut stack = vec![init_id];
            while let Some(id) = stack.pop() {
                match &ctx.node(id).kind {
                    NodeKind::Identifier(i) if i.name == class_name => return true,
                    NodeKind::BinaryOp(b) => {
                        if let Some(l) = b.left_operand {
                            stack.push(l);
                        }
                        if let Some(r) = b.right_operand {
                            stack.push(r);
                        }
                    }
                    NodeKind::UnaryOp(u) => {
                        if let Some(o) = u.operand {
                            stack.push(o);
                        }
                    }
                    NodeKind::Subscript(s) => {
                        if let Some(b) = s.base {
                            stack.push(b);
                        }
                    }
                    NodeKind::Call(c) => {
                        if let Some(callee) = c.callee {
                            stack.push(callee);
                        }
                        for &arg in &c.arguments {
                            stack.push(arg);
                        }
                    }
                    _ => {}
                }
            }
        }
    }
    false
}

/// One in-file function's call shape — enough for the override-compat check. `param_types[i]` is
/// the declared parameter's [`DataType`] (defaulting to `Variant` when annotation is absent).
struct FunctionSig {
    /// Resolved return [`DataType`] (the function's `TypeTable` entry).
    return_type: DataType,
    param_types: Vec<DataType>,
    /// Per-parameter "has default initializer" flags. Godot stores a single `default_par_count`
    /// suffix; gdls keeps the per-position flags so a future check can verify the
    /// defaults-suffix-only invariant Godot enforces structurally.
    has_default: Vec<bool>,
    /// `true` when the function has a `...rest` parameter. Drives the `, ...` suffix in
    /// rendered signatures.
    is_vararg: bool,
}

/// Walk the current class's base chain (Class only — Script/Native parents are deferred) for a
/// same-named member function. Skips the candidate when its parameter types are still resolving
/// (a cycle through default values is Godot's `Could not resolve member ... Cyclic reference`
/// path; the compat error trailing it would be a phantom on top).
fn find_parent_function(ctx: &AnalysisContext, class_id: NodeId, name: &str) -> Option<NodeId> {
    let mut cur = ctx.bases.get(&class_id).and_then(|b| b.class_node)?;
    loop {
        if let Some(Member::Function(f)) = class_member(ctx, cur, name) {
            // Bail if any parameter type or the return type is still unresolved/resolving:
            // emitting "doesn't match the parent" against a partial signature gives a misleading
            // error (cyclic_ref_override.gd's canonical case).
            if parent_function_partially_resolved(ctx, f) {
                return None;
            }
            return Some(f);
        }
        cur = ctx.bases.get(&cur).and_then(|b| b.class_node)?;
    }
}

fn parent_function_partially_resolved(ctx: &AnalysisContext, fn_id: NodeId) -> bool {
    if !ctx.get_type(fn_id).is_set() {
        return true;
    }
    let NodeKind::Function(f) = &ctx.node(fn_id).kind else {
        return true;
    };
    for p in &f.parameters {
        let dt = ctx.get_type(*p);
        if dt.is_resolving() {
            return true;
        }
    }
    false
}

fn function_signature(ctx: &AnalysisContext, func_id: NodeId) -> FunctionSig {
    let (params, _, _, is_vararg) = match &ctx.node(func_id).kind {
        NodeKind::Function(f) => (
            f.parameters.clone(),
            f.return_type,
            f.is_static,
            f.rest_parameter.is_some(),
        ),
        _ => (Vec::new(), None, false, false),
    };
    let mut param_types = Vec::with_capacity(params.len());
    let mut has_default = Vec::with_capacity(params.len());
    for p in &params {
        let dt = ctx.get_type(*p).clone();
        let init = match &ctx.node(*p).kind {
            NodeKind::Parameter(pn) => pn.initializer,
            _ => None,
        };
        param_types.push(dt);
        has_default.push(init.is_some());
    }
    FunctionSig {
        return_type: ctx.get_type(func_id).clone(),
        param_types,
        has_default,
        is_vararg,
    }
}

/// Compatibility check: the child's parameter set must accept everything the parent's accepts,
/// the return type must be covariant, and each parameter type must be contravariant.
///
/// Defaults: Godot requires `current_min_argc <= parent_min_argc && parent_max_argc <=
/// current_max_argc` — the child's required range fits inside or extends the parent's
/// (analyzer.cpp:1898-1904). With no vararg support in this slice, both `max_argc`s equal the
/// parameter count, so the count check reduces to "same param count" — which the parent_count_*
/// corpus tests already require.
fn signatures_match(ctx: &AnalysisContext, child: &FunctionSig, parent: &FunctionSig) -> bool {
    let parent_default_count = parent.has_default.iter().filter(|d| **d).count();
    let child_default_count = child.has_default.iter().filter(|d| **d).count();
    let parent_min = parent.param_types.len() - parent_default_count;
    let parent_max = parent.param_types.len();
    let child_min = child.param_types.len() - child_default_count;
    let child_max = child.param_types.len();

    // analyzer.cpp:1898-1904 — Godot's arity overlap requirement: the child's accepted-arg
    // range must INCLUDE the parent's. Equivalent to `child_min <= parent_min &&
    // parent_max <= child_max`. This lets the child add extra parameters provided they have
    // defaults (function_match_parent_signature_with_extra_parameters.gd) but rejects dropping
    // required args (parameter_count_less.gd).
    if child_min > parent_min || parent_max > child_max {
        return false;
    }
    // The vararg-ness must match between parent and child: a vararg parent requires a vararg
    // child (otherwise the child can't accept the parent's extra positional args), and
    // vice-versa (a vararg child overriding a non-vararg parent changes the contract).
    if parent.is_vararg != child.is_vararg {
        return false;
    }

    if !is_return_covariant(ctx, &child.return_type, &parent.return_type) {
        return false;
    }
    // Only compare parameter types up to the parent's count — extras on the child side are
    // unconstrained by the parent's signature.
    for (c, p) in child
        .param_types
        .iter()
        .zip(parent.param_types.iter())
        .take(parent.param_types.len())
    {
        if !is_param_contravariant(ctx, c, p) {
            return false;
        }
    }
    true
}

/// Return-type covariance, port of analyzer.cpp:1881-1895:
///   1. CHILD's return is `Variant` ⇒ the parent must also be Variant (don't widen a narrower
///      parent to Variant).
///   2. CHILD's return is hard `void` ⇒ the parent must also be `void` (when the parent is hard-
///      typed non-`void`).
///   3. Otherwise, `is_type_compatible(parent, child)` — gradual-typing compatibility.
///
/// gdls hasn't yet exposed [`crate::reducer::is_type_compatible`] across the resolver boundary,
/// so case 3 approximates with a permissive bias: rendered-name equivalence, plus a
/// `Display`-string check for the common cases the corpus exercises. False **negatives** on
/// corner cases (e.g. cross-script subtyping) bias toward "compatible", which keeps feature
/// tests like `function_return_type_covariance.gd` from spuriously failing under a partial
/// type-compatibility port.
fn is_return_covariant(ctx: &AnalysisContext, child: &DataType, parent: &DataType) -> bool {
    // analyzer.cpp:1878: child has no explicit return → already adopted parent's type, skip.
    if !child.is_hard_type() {
        return true;
    }
    // analyzer.cpp:1883-1886: explicit Variant child + narrower parent → invalid.
    if child.is_variant() {
        return parent.is_variant();
    }
    // analyzer.cpp:1887-1892: void child + hard non-void parent → invalid.
    if child.kind == DtKind::Builtin && child.builtin_type == VariantType::Nil {
        if parent.is_hard_type()
            && !(parent.kind == DtKind::Builtin && parent.builtin_type == VariantType::Nil)
        {
            return false;
        }
        return true;
    }
    // analyzer.cpp:1894: general covariance via is_type_compatible(parent, child).
    if parent.is_variant() {
        return true;
    }
    crate::reducer::is_type_compatible(ctx, parent, child, false)
}

/// Parameter-type contravariance: child param must accept everything the parent accepts.
/// Contravariance permits **widening** in the override (parent: int ⇒ child: Variant is OK,
/// the reverse is not). Godot's `is_type_compatible(current_par_type, parent_par_type)` is
/// the structural check — the analyzer-side mirror lives in [`crate::reducer::is_type_compatible`]
/// and isn't yet exposed across the resolver boundary, so this slice approximates with two
/// rules that cover the corpus: a `Variant` child accepts anything, and otherwise types must
/// match by name. False negatives bias toward "compatible" so the corpus's
/// `function_param_type_contravariance.gd` widening cases pass.
fn is_param_contravariant(ctx: &AnalysisContext, child: &DataType, parent: &DataType) -> bool {
    // analyzer.cpp:1915-1918: hard Variant parent requires child to also be Variant.
    if parent.is_variant() && parent.is_hard_type() {
        return child.is_variant();
    }
    // analyzer.cpp:1920: general contravariance via is_type_compatible(child, parent).
    crate::reducer::is_type_compatible(ctx, child, parent, false)
}

/// Render the parent signature exactly as Godot formats it (analyzer.cpp:1927-1957):
/// `name(par_type[, par_type, ...] [= <default>]) -> return_type`. `void` parents render as
/// `"void"`; an annotation-less return reads `"Variant"`.
fn render_parent_signature(name: &str, sig: &FunctionSig) -> String {
    let mut out = String::new();
    out.push_str(name);
    out.push('(');
    let default_count = sig.has_default.iter().filter(|d| **d).count();
    let total = sig.param_types.len();
    let min_argc = total - default_count;
    for (i, ty) in sig.param_types.iter().enumerate() {
        if i > 0 {
            out.push_str(", ");
        }
        let pretty = ty.to_string();
        // Godot renames a bare `null` (parameter declared without an annotation) to "Variant"
        // in the signature string (analyzer.cpp:1934-1936); gdls Displays the same case as
        // "<unresolved>", so map both.
        let pretty = if pretty == "null" || pretty == "<unresolved>" {
            "Variant".to_owned()
        } else {
            pretty
        };
        out.push_str(&pretty);
        if i >= min_argc {
            out.push_str(" = <default>");
        }
    }
    if sig.is_vararg {
        // Godot renders the variadic suffix as ", ..." after the named parameters.
        if !sig.param_types.is_empty() {
            out.push_str(", ");
        }
        out.push_str("...");
    }
    out.push_str(") -> ");
    let ret = sig.return_type.to_string();
    if ret == "null"
        || ret == "Nil"
        || (sig.return_type.kind == DtKind::Builtin
            && sig.return_type.builtin_type == VariantType::Nil)
    {
        out.push_str("void");
    } else {
        out.push_str(&ret);
    }
    out
}

/// True if any in-file script ancestor of the current class already declares a function named
/// `name`. Mirrors the short-circuit in Godot's `get_function_signature`: it walks the script
/// base chain (`base_type.class_type`) and, on the first ancestor whose `has_member(name)` is a
/// FUNCTION, returns with `native_base` left empty (`gdscript_analyzer.cpp:5835-5873`). gdls gates
/// `NATIVE_METHOD_OVERRIDE` on the negation so an override already shadowed by an in-file parent
/// doesn't re-warn down the chain.
///
/// Only the in-file `Class` chain is walked because that is the only chain `enclosing_native_base`
/// can follow to a native root: a cross-file script parent resolves to `DtKind::Script`, which
/// `enclosing_native_base` already declines to traverse (so it can never over-warn through one).
fn script_ancestor_defines_function(ctx: &AnalysisContext, name: &str) -> bool {
    let Some(mut cur) = ctx.current_class else {
        return false;
    };
    loop {
        let base = ctx.bases.get(&cur).cloned().unwrap_or_default();
        match base.kind {
            DtKind::Class => {
                let Some(parent) = base.class_node else {
                    return false;
                };
                if matches!(class_member(ctx, parent, name), Some(Member::Function(_))) {
                    return true;
                }
                cur = parent;
            }
            _ => return false,
        }
    }
}

/// Find the nearest native class in this function's enclosing class chain. Walks `ctx.bases`
/// (the in-file class → base mapping) from the current class upward; returns the first base
/// whose `kind == Native` (or returns `None` if the chain ends without a native ancestor —
/// e.g. the function is in a class that extends a Script base not currently in the index).
fn enclosing_native_base(ctx: &AnalysisContext, _func_id: NodeId) -> Option<String> {
    let mut cur = ctx.current_class?;
    loop {
        let base = ctx.bases.get(&cur).cloned().unwrap_or_default();
        match base.kind {
            DtKind::Native => return Some(base.native_type),
            DtKind::Class => {
                cur = base.class_node?;
            }
            _ => return None,
        }
    }
}

/// Walks `native_class`'s NativeDb inherits chain looking for a class that **defines** a method
/// by `name` (not inherited from a parent). Returns the name of the **closest** ancestor that
/// has it, matching Godot's `native_base` semantics in `NATIVE_METHOD_OVERRIDE` where the
/// reported class is where the method actually exists (e.g. `Object.get()` rather than
/// `RefCounted`).
fn find_native_class_with_method(
    ctx: &AnalysisContext,
    native_class: &str,
    name: &str,
) -> Option<String> {
    let mut cur = Some(native_class.to_owned());
    while let Some(c) = cur {
        let nc = ctx.native.class_named(&c)?;
        // VIRTUAL methods are intentionally skipped. Godot only sets `native_base` (the signal
        // that gates NATIVE_METHOD_OVERRIDE) when `ClassDB::get_method()` returns a real
        // `MethodBind` (gdscript_analyzer.cpp:5912-5915). Engine virtuals (`_ready`, `_process`,
        // `_input`, …) have a `MethodInfo` — so they appear in `extension_api.json` with
        // `is_virtual: true` and in `get_method_list` — but NO `MethodBind`, so `native_base`
        // stays empty and Godot never warns on overriding them.
        if nc
            .methods
            .iter()
            .any(|m| !m.is_virtual && ctx.native.name_of(m.name) == name)
        {
            return Some(c);
        }
        cur = nc.inherits.map(|s| ctx.native.name_of(s).to_owned());
    }
    None
}

/// `resolve_parameter` → `resolve_assignable` (analyzer.cpp:2241). E2 routes the default-value
/// initializer through the reducer so an annotated parameter's default is folded into the type
/// table; Godot's polymorphism-of-defaults check (analyzer.cpp:1894-1956) lives in E3.
fn resolve_parameter(ctx: &mut AnalysisContext, param_id: NodeId) {
    let (spec, init, infer) = parameter_assignable_parts(ctx, param_id);
    resolve_assignable(ctx, param_id, spec, init, infer, false);
}

/// Slice of Godot's `is_shadowing(identifier, "function parameter", true)` (analyzer.cpp:6135-6188).
/// Looks up `param_id`'s name in the current class's `members_indices`; if found, emits
/// SHADOWED_VARIABLE with [context, name, member-kind, declaring-line]. Godot's broader walk
/// over global identifiers / base classes / native ancestors stays deferred until the
/// `warnings/shadowning.gd` slice.
fn warn_parameter_shadowing(ctx: &mut AnalysisContext, param_id: NodeId) {
    let Some(class_id) = ctx.current_class else {
        return;
    };
    let name = decl_identifier_name(ctx, param_id);
    if name.is_empty() {
        return;
    }
    // Anchor the diagnostic at the parameter's identifier when present, otherwise the parameter
    // itself — matches Godot's `parser->push_warning(p_identifier, …)` line fidelity.
    let identifier_id = match &ctx.node(param_id).kind {
        NodeKind::Parameter(p) => p.identifier.unwrap_or(param_id),
        _ => param_id,
    };
    let member_idx = match &ctx.node(class_id).kind {
        NodeKind::Class(c) => c.members_indices.get(&name).copied(),
        _ => None,
    };
    let Some(idx) = member_idx else {
        return;
    };
    let Some(member) = nth_member(ctx, class_id, idx) else {
        return;
    };
    // analyzer.cpp:6170 — `base_class->has_member(name).get_type_name()` returns the
    // ClassNode::Member::get_type_name() lowercase noun matching Godot's
    // Member::Kind→string mapping in gdscript_parser.h:602-625.
    let member_kind = member_kind_name(member.clone()).to_owned();
    let member_node = match member {
        Member::Class(id)
        | Member::Constant(id)
        | Member::Function(id)
        | Member::Signal(id)
        | Member::Variable(id)
        | Member::Enum(id) => id,
        Member::EnumValue(_) | Member::Group(_) => return,
    };
    let member_line = ctx.node(member_node).loc.start.line.to_string();
    ctx.push_warning(
        crate::warnings::WarningCode::ShadowedVariable,
        &[
            "function parameter".to_owned(),
            name,
            member_kind,
            member_line,
        ],
        identifier_id,
    );
}

/// `analyzer.cpp:2079-2085` — CONFUSABLE_LOCAL_DECLARATION. When declaring a variable or
/// constant inside a suite, check if the parent suite has a local with the same name.
fn warn_confusable_local_declaration(ctx: &mut AnalysisContext, node_id: NodeId) {
    let name = decl_identifier_name(ctx, node_id);
    if name.is_empty() || ctx.suite_stack.len() < 2 {
        return;
    }
    let parent_suite_id = ctx.suite_stack[ctx.suite_stack.len() - 2];
    let has_in_parent = match &ctx.node(parent_suite_id).kind {
        NodeKind::Suite(s) => s.locals_indices.contains_key(&name),
        _ => false,
    };
    if has_in_parent {
        let kind = if matches!(ctx.node(node_id).kind, NodeKind::Constant(_)) {
            "constant"
        } else {
            "variable"
        };
        let ident_id = match &ctx.node(node_id).kind {
            NodeKind::Variable(v) => v.identifier.unwrap_or(node_id),
            NodeKind::Constant(c) => c.identifier.unwrap_or(node_id),
            _ => node_id,
        };
        ctx.push_warning(
            crate::warnings::WarningCode::ConfusableLocalDeclaration,
            &[kind.to_owned(), name],
            ident_id,
        );
    }
}

/// `resolve_assignable` (analyzer.cpp:2073). WP-E2 ports the declared-type path **and** the
/// initializer-driven inference path (analyzer.cpp:2095-2154): if an initializer is present,
/// [`crate::reducer::reduce_expression`] runs over it, and when no explicit type was given the
/// initializer's reduced type becomes the assignable's type (with the appropriate `Inferred`/
/// `AnnotatedInferred` source per analyzer.cpp:2150-2154 — constants and `:=` infers get
/// `AnnotatedInferred`, untyped defaults to `Inferred`).
///
/// The compatibility checks at analyzer.cpp:2155-2176 (`Cannot assign a value of type … to … with
/// specified type …`) need `is_type_compatible` and the operator type matrix; they land in E3 with
/// the assignment reducer. Similarly, the `UNTYPED_DECLARATION`/`INFERRED_DECLARATION` warning
/// emission (analyzer.cpp:2179-2206) is held back for WP-F.
fn resolve_assignable(
    ctx: &mut AnalysisContext,
    node_id: NodeId,
    specifier: Option<NodeId>,
    initializer: Option<NodeId>,
    infer_datatype: bool,
    is_constant: bool,
) {
    // analyzer.cpp:2079-2085 — CONFUSABLE_LOCAL_DECLARATION. When declaring a local
    // variable/constant, check if the *parent* suite (one level up on the suite stack) already
    // has a local with the same name. Godot walks
    // `p_assignable->identifier->suite->parent_block->has_local(name)`.
    warn_confusable_local_declaration(ctx, node_id);

    let has_specified_type = specifier.is_some();
    let mut ty = if has_specified_type {
        type_from_metatype(resolve_datatype(ctx, specifier))
    } else {
        DataType {
            kind: DtKind::Variant,
            ..Default::default()
        }
    };

    if let Some(init) = initializer {
        // Snapshot diagnostic count so we can gate `INFERENCE_ON_VARIANT` (and other DEBUG
        // warnings) on "no errors emitted during this initializer's reduction" — Godot
        // implicitly does this by reading the initializer's `is_set()` first, which never returns
        // true for a path that already hit a cycle (the cycle sentinel propagates as RESOLVING).
        // gdls's promotion path turns RESOLVING into hard `Variant` at the end of
        // `resolve_assignable`, so the cyclic case's initializer_type DOES look like hard Variant
        // by the time the *outer* resolve_assignable reads it — a divergence from Godot that
        // would false-positive INFERENCE_ON_VARIANT on every cyclic-pair member. Snapshotting
        // pre-reduction and skipping the warning if any new diagnostic landed mirrors Godot's
        // intent without re-architecting the cycle handling.
        let pre_diag_count = ctx.diagnostic_count();
        crate::reducer::reduce_expression(ctx, init, false);
        let initializer_type = ctx.get_type(init).clone();
        let init_emitted_errors = ctx.diagnostic_count() > pre_diag_count;

        // analyzer.cpp:2126-2141 — error reporting when inference fails. `infer_datatype` (`:=`)
        // and the typed-fallback (no `:=`, no specifier) take different message templates.
        let name = decl_identifier_name(ctx, node_id);
        let kind_label = assignable_kind_label(ctx, node_id, is_constant);
        if infer_datatype {
            // analyzer.cpp:2127-2131. Three conditions for "Cannot infer the type":
            // (1) type not set (unresolved/cycle), (2) has_no_type (Undetected), or
            // (3) not hard-typed (the initializer resolved to a soft/inferred type, meaning
            // the source doesn't have an explicit type annotation). The third condition is
            // gated on `!init_emitted_errors` to avoid piling on top of cycle/unresolved
            // errors, AND the initializer must be an identifier or a simple expression
            // (not a call/subscript/preload where our reducer might produce soft types
            // even for valid patterns — the full `!is_hard_type()` gate lands when the
            // reducer is complete enough to hard-type all fully-resolved expressions).
            let init_is_plain_identifier = matches!(ctx.node(init).kind, NodeKind::Identifier(_));
            let init_name_starts_upper = match &ctx.node(init).kind {
                NodeKind::Identifier(i) => i.name.starts_with(|c: char| c.is_ascii_uppercase()),
                _ => false,
            };
            let weak_type_safe = !init_emitted_errors
                && !initializer_type.is_hard_type()
                && initializer_type.is_set()
                && !initializer_type.has_no_type()
                && init_is_plain_identifier
                && !init_name_starts_upper;
            if !initializer_type.is_set() || initializer_type.has_no_type() || weak_type_safe {
                ctx.push_error(
                    format!(
                        r#"Cannot infer the type of "{name}" {kind_label} because the value doesn't have a set type."#
                    ),
                    init,
                );
            } else if initializer_type.kind == DtKind::Builtin
                && initializer_type.builtin_type == VariantType::Nil
                && !is_constant
            {
                ctx.push_error(
                    format!(
                        r#"Cannot infer the type of "{name}" {kind_label} because the value is "null"."#
                    ),
                    init,
                );
            }

            // analyzer.cpp:2132-2136 (DEBUG_ENABLED) — `INFERENCE_ON_VARIANT` is the
            // error-by-default warning when `:=` resolves to a hard `Variant`. Godot's
            // symbol is `p_kind` lower-case, which is exactly `kind_label` here. Anchor at the
            // assignable node so `@warning_ignore("inference_on_variant")` on the declaration
            // suppresses correctly.
            //
            // Two gates beyond Godot's literal condition:
            // 1. **gdls's `is_variant()` is broader** — also covers `Resolving` and `Unresolved`
            //    (cycle sentinels). Godot's plain `kind == VARIANT` excludes those, and the
            //    warning makes no sense on a cyclic-reference variable (it would pile on top of
            //    the `Could not resolve member ... : Cyclic reference.` error). Gate on the true
            //    Variant kind only.
            // 2. **Skip if the initializer's reduction already emitted any diagnostics.** gdls's
            //    promotion path at the bottom of `resolve_assignable` turns a `RESOLVING`
            //    initializer into a hard `Variant` before the *outer* resolve_assignable reads it
            //    — so the cyclic-pair case (`var v1 := v2; var v2 := v1`) reads as hard Variant
            //    on the second pass, which Godot wouldn't ever see (its RESOLVING propagates
            //    as RESOLVING). The error-count gate suppresses the second-pass warning, matching
            //    Godot's effective behavior on cyclic_ref_var.gd.
            if initializer_type.is_hard_type()
                && initializer_type.kind == DtKind::Variant
                && !init_emitted_errors
            {
                // For parameters, the `@warning_ignore` typically attaches to the enclosing
                // function (`@warning_ignore("inference_on_variant")` above
                // `func f(p := variant())`), not the parameter itself. Route the ignore lookup
                // through the function so the corpus's `features/hard_variants.gd` cases on
                // lines 11 stay silent. Variable / constant decls carry their own annotation
                // on the declaration node, so the default same-node ignore-context works there.
                let ignore_ctx = if matches!(&ctx.node(node_id).kind, NodeKind::Parameter(_)) {
                    ctx.current_function.unwrap_or(node_id)
                } else {
                    node_id
                };
                ctx.push_warning_for(
                    crate::warnings::WarningCode::InferenceOnVariant,
                    &[kind_label.to_owned()],
                    node_id,
                    ignore_ctx,
                );
            }
        } else if !has_specified_type && !initializer_type.is_set() {
            // analyzer.cpp:2138-2140.
            ctx.push_error(
                format!(r#"Could not resolve type for {kind_label} "{name}"."#),
                init,
            );
        }

        if !has_specified_type {
            // analyzer.cpp:2143-2154: inherit the initializer's type, with source promotion. A Nil
            // initializer for a non-constant declaration drops back to Variant — Variant doesn't
            // narrow on `null`.
            ty = initializer_type;
            let drops_to_variant = !ty.is_set()
                || (ty.is_hard_type()
                    && ty.kind == DtKind::Builtin
                    && ty.builtin_type == VariantType::Nil
                    && !is_constant);
            if drops_to_variant {
                ty.kind = DtKind::Variant;
            }
            ty.type_source = if infer_datatype || is_constant {
                TypeSource::AnnotatedInferred
            } else {
                TypeSource::Inferred
            };
        } else if ty.is_hard_type() {
            // analyzer.cpp:2095-2105 — when the specified type is a typed Array/Dictionary AND the
            // initializer is an array/dictionary literal, narrow the literal's element types so
            // the per-element type-check fires (analyzer.cpp:2944-2949 calls these same updaters
            // for `var x: Array[T] = [...]` declarations).
            if !ty.container_element_types.is_empty() {
                match &ctx.node(init).kind {
                    NodeKind::Array(_)
                        if ty.builtin_type == VariantType::Array
                            && !ty.container_element_types.is_empty() =>
                    {
                        let elem_t = ty.container_element_types[0].clone();
                        crate::reducer::update_array_literal_element_type(ctx, init, &elem_t);
                    }
                    NodeKind::Dictionary(_)
                        if ty.builtin_type == VariantType::Dictionary
                            && ty.container_element_types.len() >= 2 =>
                    {
                        let key_t = ty.container_element_types[0].clone();
                        let val_t = ty.container_element_types[1].clone();
                        crate::reducer::update_dictionary_literal_element_type(
                            ctx, init, &key_t, &val_t,
                        );
                    }
                    _ => {}
                }
            }

            // analyzer.cpp:2121-2123 — for a constant-foldable initializer, run
            // `update_const_expression_builtin_type` to either narrow the literal's datatype or
            // emit `Cannot assign a value of type X as Y.`. This is the const-companion to the
            // generic compat check below. Mirrors the same call site at analyzer.cpp:2945 in
            // reduce_assignment (which gdls already runs for re-assignment via
            // reducer.rs:1939). For typed Array/Dictionary the per-element updater above
            // already handles the constant narrowing per-element; we still call this for the
            // top-level container fold when present.
            if ctx.folds.is_reduced(init) {
                crate::reducer::update_const_expression_builtin_type(ctx, init, &ty, "assign");
            }

            // analyzer.cpp:2162-2168 — `Cannot assign a value of type X to <kind> "Y" with
            // specified type Z.` when the initializer's narrowed type isn't compatible with the
            // declared specifier. Re-read after the literal/const-update narrowing above.
            let init_type = ctx.get_type(init).clone();
            // `!(!is_constant && reverse_compat)` => `is_constant || !reverse_compat` (de Morgan).
            let reverse_compat =
                !is_constant && crate::reducer::is_type_compatible(ctx, &init_type, &ty, false);
            if init_type.is_hard_type()
                && !init_type.is_variant()
                && !crate::reducer::is_type_compatible(ctx, &ty, &init_type, true)
                && !reverse_compat
            {
                let name = decl_identifier_name(ctx, node_id);
                let kind_label = assignable_kind_label(ctx, node_id, is_constant);
                ctx.push_error(
                    format!(
                        r#"Cannot assign a value of type {init_type} to {kind_label} "{name}" with specified type {ty}."#
                    ),
                    init,
                );
            }
        }

        // analyzer.cpp:2172-2174 (`DEBUG_ENABLED`) — NARROWING_CONVERSION on initializers when
        // the specified type is `int` and the initializer's value is `float`. Godot checks
        // bare `builtin_type` equality (no `is_hard_type` gate — specified types are always hard
        // since they came from a `: int` annotation). Anchored on the initializer node so
        // per-line / region `@warning_ignore` filters pick it up.
        if has_specified_type && ty.kind == DtKind::Builtin && ty.builtin_type == VariantType::Int {
            let init_type = ctx.get_type(init).clone();
            if init_type.kind == DtKind::Builtin && init_type.builtin_type == VariantType::Float {
                ctx.push_warning(crate::warnings::WarningCode::NarrowingConversion, &[], init);
            }
        }
    }

    // analyzer.cpp:2193-2204 (DEBUG_ENABLED) — ENUM_VARIABLE_WITHOUT_DEFAULT. Fires when a
    // variable (NOT a parameter or constant) has an explicit enum type, no initializer, and the
    // enum doesn't have a value of 0 (which would otherwise be the silent default). Godot's
    // `specified_type.kind == ENUM` reads the explicit annotation's resolved type; gdls's
    // equivalent is the same `ty` after the no-initializer path through `resolve_datatype`.
    let is_parameter = matches!(ctx.node(node_id).kind, NodeKind::Parameter(_));
    if has_specified_type
        && !is_parameter
        && !is_constant
        && initializer.is_none()
        && ty.kind == DtKind::Enum
        && !ty.enum_values_inexact
        && !ty.enum_values.is_empty()
        && !ty.enum_values.values().any(|&v| v == 0)
    {
        let name = decl_identifier_name(ctx, node_id);
        ctx.push_warning(
            crate::warnings::WarningCode::EnumVariableWithoutDefault,
            &[name],
            node_id,
        );
    }

    ty.is_constant = is_constant;
    ty.is_read_only = false;
    ctx.set_type(node_id, ty);
}

/// `"variable"` / `"constant"` / `"parameter"` — the `p_kind` string Godot's
/// `resolve_assignable` weaves into its error messages (analyzer.cpp:2214/:2228/:2242).
fn assignable_kind_label(
    ctx: &AnalysisContext,
    node_id: NodeId,
    is_constant: bool,
) -> &'static str {
    if is_constant {
        "constant"
    } else if matches!(ctx.node(node_id).kind, NodeKind::Parameter(_)) {
        "parameter"
    } else {
        "variable"
    }
}

// --- Signal + enum member types --------------------------------------------------------------------

/// Build a signal member's type (analyzer.cpp:1120-1142): a `Signal` builtin carrying the parameter
/// signature. `MethodInfo` arguments map to [`MethodSig`] params.
fn resolve_signal_type(ctx: &mut AnalysisContext, signal_id: NodeId, name: &str) -> DataType {
    let params = signal_parameters(ctx, signal_id);
    let mut sig_params = Vec::with_capacity(params.len());
    for (param_id, pname) in params {
        let spec = parameter_specifier(ctx, param_id);
        let param_type = type_from_metatype(resolve_datatype(ctx, spec));
        ctx.set_type(param_id, param_type.clone());
        sig_params.push((pname, param_type));
    }
    make_signal_type(MethodSig {
        name: name.to_owned(),
        params: sig_params,
        return_type: Box::new(void_type()),
    })
}

/// Build an enum member's type (analyzer.cpp:1150-1197). WP-E1 wires the reducer into the
/// custom-value path: each `= expr` entry is reduced via [`crate::reducer::reduce_expression`], and
/// when the fold yields a non-`int` value Godot's `Enum values must be integers.` error is
/// emitted (analyzer.cpp:1167-1168). The companion `Enum values must be constant.` error
/// (analyzer.cpp:1165-1166) needs the identifier/subscript reducers (E3) to distinguish "fold
/// failed because the expression isn't constant" from "fold failed because E1 doesn't yet reduce
/// this kind"; held back here per the "no phantom errors before the matrix is in" rule.
///
/// Values chain forward the way `element.parent_enum->values[element.index - 1].value + 1` does
/// (analyzer.cpp:1174-1175); on a non-foldable / non-int custom value we let the chain step by 1
/// from the previous resolved value so subsequent entries still register.
fn resolve_enum_type(
    ctx: &mut AnalysisContext,
    enum_id: NodeId,
    class_id: NodeId,
    name: &str,
) -> DataType {
    let mut enum_type = make_class_enum_type(ctx, name, class_id, true);
    let entries = enum_values(ctx, enum_id);
    // `prev_value` matches Godot's `values[index-1].value` chain: -1 lets the first non-custom
    // entry fall into the `index == 0` branch (analyzer.cpp:1176) and resolve to 0.
    let mut prev_value: i64 = -1;
    for (ident_id, custom_value, ident_name) in entries {
        let value: i64 = if let Some(cv) = custom_value {
            let errors_before = ctx.diagnostic_count();
            crate::reducer::reduce_expression(ctx, cv, false);
            let errors_after = ctx.diagnostic_count();
            match ctx.folds.get(cv).cloned() {
                Some(crate::foldtable::FoldedValue::Int(v)) => v,
                Some(_) => {
                    // Godot's analyzer.cpp:1167-1168 check; we only emit when the fold succeeds
                    // (so a non-int constant) — a literal `0.0` or `"hello"` reaches this arm.
                    ctx.push_error("Enum values must be integers.", cv);
                    prev_value.saturating_add(1)
                }
                None => {
                    // analyzer.cpp:1165-1166 — `Enum values must be constant.` when the
                    // custom-value expression reduces to a non-constant. gdls's fold table doesn't
                    // distinguish "non-constant" from "forward-reference inside the same enum
                    // block" — both produce a missing fold — so gate the emission on whether the
                    // reduction itself emitted an error (cyclic_ref / unresolved-member /
                    // not-a-constant patterns). Out-of-order siblings within one `enum { ... }`
                    // (e.g. `V2 = V1 - 1`) reduce without raising any error, just with a missed
                    // fold; Godot hard-resolves those at parse time and so doesn't need this
                    // gate, but gdls's reducer doesn't walk forward refs inside the same enum
                    // block (the per-entry stamping happens after each iteration).
                    if errors_after > errors_before {
                        ctx.push_error("Enum values must be constant.", cv);
                    }
                    prev_value.saturating_add(1)
                }
            }
        } else if prev_value < 0 {
            // analyzer.cpp:1177 — first entry without a custom value resolves to 0.
            0
        } else {
            prev_value + 1
        };
        // Register the named identifier even if the value resolution failed (Godot still
        // assigns `element.value` to 0 on error so subsequent chain lookups stay sound).
        if let Some(n) = ident_name {
            enum_type.enum_values.insert(n, value);
        }
        // Type the value's identifier node — Godot's `element.identifier->set_datatype` path
        // (analyzer.cpp:1182, 1247). The identifier gets the enum's *instance* type so a
        // subsequent `EnumName.Value` / `Parent.E` lookup yields a typed value rather than an
        // unset Unresolved that would false-positive "Cannot find member".
        if let Some(iid) = ident_id {
            ctx.folds
                .set(iid, crate::foldtable::FoldedValue::Int(value));
            let mut value_dt = enum_type.clone();
            value_dt.is_meta_type = false;
            value_dt.builtin_type = VariantType::Int;
            value_dt.is_constant = true;
            ctx.set_type(iid, value_dt);
        }
        prev_value = value;
    }
    enum_type
}

// --- Type constructors (analyzer.cpp:95-160, 5765) -------------------------------------------------

/// `make_signal_type` (analyzer.cpp:95).
pub(crate) fn make_signal_type(sig: MethodSig) -> DataType {
    DataType {
        type_source: TypeSource::AnnotatedExplicit,
        kind: DtKind::Builtin,
        builtin_type: VariantType::Signal,
        is_constant: true,
        method_sig: Some(Box::new(sig)),
        ..Default::default()
    }
}

/// `make_enum_type` (analyzer.cpp:133). `native_type` disambiguates same-named enums across classes.
pub(crate) fn make_enum_type(enum_name: &str, base_name: &str, meta: bool) -> DataType {
    DataType {
        type_source: TypeSource::AnnotatedExplicit,
        kind: DtKind::Enum,
        builtin_type: if meta {
            VariantType::Dictionary
        } else {
            VariantType::Int
        },
        enum_type: enum_name.to_owned(),
        is_constant: true,
        is_meta_type: meta,
        native_type: if base_name.is_empty() {
            enum_name.to_owned()
        } else {
            format!("{base_name}.{enum_name}")
        },
        ..Default::default()
    }
}

/// `make_class_enum_type` (analyzer.cpp:153): an enum declared in an in-file class. Godot wires
/// the class's `fqcn` into the enum's `native_type` (`<fqcn>.<enum_name>`) so cross-enum
/// compatibility checks compare disambiguated names and error messages render
/// `<file.gd>.<EnumName>` / `<file.gd>::Inner.<EnumName>` after `Display`'s `get_file()` strips
/// the leading `<dir>/`.
fn make_class_enum_type(
    ctx: &AnalysisContext,
    enum_name: &str,
    class_id: NodeId,
    meta: bool,
) -> DataType {
    let base = class_fqcn(ctx, class_id);
    let mut t = make_enum_type(enum_name, &base, meta);
    t.class_node = Some(class_id);
    t
}

/// `make_native_enum_type` (analyzer.cpp:162): a native-class enum (e.g. `Node.ProcessMode`).
///
/// Godot walks `ClassDB::get_parent_class_nocheck` to find the class that *defines* the enum, so
/// `Sprite2D.ProcessMode` and `Node.ProcessMode` end up with identical `native_type` strings; gdls
/// reproduces this with [`NativeDb::class_named`]'s `inherits` chain. Values are pulled from the
/// defining class's [`NativeEnum`]; on a meta type we mark `is_pseudo_type` (the "Type X in base Y
/// cannot be used on its own" gate at analyzer.cpp:4878-4879).
pub(crate) fn make_native_enum_type(
    ctx: &AnalysisContext,
    enum_name: &str,
    native_class: &str,
    meta: bool,
) -> DataType {
    // Find the base class that actually declares this enum (Godot's loop at analyzer.cpp:164-170).
    let native_base = {
        let mut cur = Some(native_class.to_owned());
        let mut found: Option<String> = None;
        while let Some(c) = cur {
            if let Some(nc) = ctx.native.class_named(&c) {
                if nc
                    .enums
                    .iter()
                    .any(|e| ctx.native.name_of(e.name) == enum_name)
                {
                    found = Some(c);
                    break;
                }
                cur = nc.inherits.map(|s| ctx.native.name_of(s).to_owned());
            } else {
                break;
            }
        }
        found.unwrap_or_else(|| native_class.to_owned())
    };
    let mut t = make_enum_type(enum_name, &native_base, meta);
    if meta {
        // Native enum types are not dictionaries (analyzer.cpp:174-176).
        t.builtin_type = VariantType::Nil;
        t.is_pseudo_type = true;
    }
    if let Some(nc) = ctx.native.class_named(&native_base) {
        if let Some(ne) = nc
            .enums
            .iter()
            .find(|e| ctx.native.name_of(e.name) == enum_name)
        {
            for (sym, val) in &ne.values {
                t.enum_values
                    .insert(ctx.native.name_of(*sym).to_owned(), *val);
            }
        }
    }
    t
}

/// `make_builtin_enum_type` (analyzer.cpp:189): an enum on a builtin metatype (e.g. `Vector3.Axis`).
pub(crate) fn make_builtin_enum_type(
    ctx: &AnalysisContext,
    enum_name: &str,
    builtin: VariantType,
    meta: bool,
) -> DataType {
    let base = crate::data_type::variant_type_name(builtin);
    let mut t = make_enum_type(enum_name, base, meta);
    if meta {
        // Built-in enum types are not dictionaries (analyzer.cpp:191-194).
        t.builtin_type = VariantType::Nil;
        t.is_pseudo_type = true;
    }
    if let Some(bt) = ctx.native.builtin_named(base) {
        if let Some(ne) = bt
            .enums
            .iter()
            .find(|e| ctx.native.name_of(e.name) == enum_name)
        {
            for (sym, val) in &ne.values {
                t.enum_values
                    .insert(ctx.native.name_of(*sym).to_owned(), *val);
            }
        }
    }
    t
}

/// `make_global_enum_type` (analyzer.cpp:207): a `@GlobalScope` enum (e.g. `Error`, `Variant.Type`).
pub(crate) fn make_global_enum_type(
    ctx: &AnalysisContext,
    enum_name: &str,
    base: &str,
    meta: bool,
) -> DataType {
    let mut t = make_enum_type(enum_name, base, meta);
    if meta {
        t.builtin_type = VariantType::Nil;
        t.is_pseudo_type = true;
    }
    // CoreConstants::get_enum_values is keyed by the full `<base>.<enum>` form (or just `<enum>`
    // when base is empty); the NativeDb global_enum table uses the same key the dump produced.
    let lookup_key = if base.is_empty() {
        enum_name.to_owned()
    } else {
        format!("{base}.{enum_name}")
    };
    if let Some(ne) = ctx.native.global_enum(&lookup_key) {
        for (sym, val) in &ne.values {
            t.enum_values
                .insert(ctx.native.name_of(*sym).to_owned(), *val);
        }
    } else if let Some(ne) = ctx.native.global_enum(enum_name) {
        // Some dumps key by the bare enum name.
        for (sym, val) in &ne.values {
            t.enum_values
                .insert(ctx.native.name_of(*sym).to_owned(), *val);
        }
    }
    t
}

/// `type_from_metatype` (analyzer.cpp:5765): the instance type of a metatype.
pub(crate) fn type_from_metatype(meta: DataType) -> DataType {
    let mut result = meta;
    result.is_meta_type = false;
    result.is_pseudo_type = false;
    if result.kind == DtKind::Enum {
        result.builtin_type = VariantType::Int;
    } else {
        result.is_constant = false;
    }
    result
}

/// The `void` type (a `Nil` builtin) used for constructors and signal returns.
fn void_type() -> DataType {
    DataType {
        type_source: TypeSource::AnnotatedExplicit,
        kind: DtKind::Builtin,
        builtin_type: VariantType::Nil,
        ..Default::default()
    }
}

// --- Member AST snapshot helpers -------------------------------------------------------------------

fn member_count(ctx: &AnalysisContext, class_id: NodeId) -> usize {
    match &ctx.node(class_id).kind {
        NodeKind::Class(c) => c.members.len(),
        _ => 0,
    }
}

fn nth_member(ctx: &AnalysisContext, class_id: NodeId, index: usize) -> Option<Member> {
    match &ctx.node(class_id).kind {
        NodeKind::Class(c) => c.members.get(index).cloned(),
        _ => None,
    }
}

/// The identifier name of a declaration node (variable/constant/signal/enum/function/parameter).
fn decl_identifier_name(ctx: &AnalysisContext, id: NodeId) -> String {
    // The unnamed-enum-value `Member::EnumValue` path passes the identifier node directly as
    // `member_node` (analyzer.cpp's `member.get_node()` for EnumValue is its identifier), so
    // recognise it inline.
    if let NodeKind::Identifier(i) = &ctx.node(id).kind {
        return i.name.clone();
    }
    let opt = match &ctx.node(id).kind {
        NodeKind::Variable(v) => v.identifier,
        NodeKind::Constant(c) => c.identifier,
        NodeKind::Signal(s) => s.identifier,
        NodeKind::Enum(e) => e.identifier,
        NodeKind::Function(f) => f.identifier,
        NodeKind::Parameter(p) => p.identifier,
        NodeKind::Class(c) => c.identifier,
        _ => None,
    };
    opt.and_then(|i| ident_name(ctx, i)).unwrap_or_default()
}

/// `(datatype_specifier, initializer, infer_datatype)` — the `AssignableNode` trio (Godot's
/// shared base class for `VariableNode`/`ConstantNode`/`ParameterNode`, `gdscript_parser.h:407`)
/// that `resolve_assignable` operates on.
fn variable_assignable_parts(
    ctx: &AnalysisContext,
    id: NodeId,
) -> (Option<NodeId>, Option<NodeId>, bool) {
    match &ctx.node(id).kind {
        NodeKind::Variable(v) => (v.datatype_specifier, v.initializer, v.infer_datatype),
        _ => (None, None, false),
    }
}

fn constant_assignable_parts(
    ctx: &AnalysisContext,
    id: NodeId,
) -> (Option<NodeId>, Option<NodeId>, bool) {
    match &ctx.node(id).kind {
        NodeKind::Constant(c) => (c.datatype_specifier, c.initializer, c.infer_datatype),
        _ => (None, None, false),
    }
}

fn parameter_assignable_parts(
    ctx: &AnalysisContext,
    id: NodeId,
) -> (Option<NodeId>, Option<NodeId>, bool) {
    match &ctx.node(id).kind {
        NodeKind::Parameter(p) => (p.datatype_specifier, p.initializer, p.infer_datatype),
        _ => (None, None, false),
    }
}

/// Signal parameters don't carry an initializer — they're a type-only signature — so
/// `resolve_signal_type` calls `resolve_datatype` directly on the specifier instead of going
/// through `resolve_assignable`.
fn parameter_specifier(ctx: &AnalysisContext, id: NodeId) -> Option<NodeId> {
    parameter_assignable_parts(ctx, id).0
}

/// `(name, parameter ids, return-type id, is_static)` for a function node.
#[allow(clippy::type_complexity)]
fn function_decl(ctx: &AnalysisContext, id: NodeId) -> (String, Vec<NodeId>, Option<NodeId>, bool) {
    match &ctx.node(id).kind {
        NodeKind::Function(f) => (
            f.identifier
                .and_then(|i| ident_name(ctx, i))
                .unwrap_or_default(),
            f.parameters.clone(),
            f.return_type,
            f.is_static,
        ),
        _ => (String::new(), Vec::new(), None, false),
    }
}

/// `(parameter id, parameter name)` pairs for a signal node.
fn signal_parameters(ctx: &AnalysisContext, id: NodeId) -> Vec<(NodeId, String)> {
    match &ctx.node(id).kind {
        NodeKind::Signal(s) => s
            .parameters
            .iter()
            .map(|&p| (p, decl_identifier_name(ctx, p)))
            .collect(),
        _ => Vec::new(),
    }
}

/// Each enum value's `(identifier id, custom-value expression id, identifier name)` — the cloned
/// snapshot `resolve_enum_type` walks so the reducer can take `&mut ctx` without borrowing the AST.
#[allow(clippy::type_complexity)]
fn enum_values(
    ctx: &AnalysisContext,
    id: NodeId,
) -> Vec<(Option<NodeId>, Option<NodeId>, Option<String>)> {
    match &ctx.node(id).kind {
        NodeKind::Enum(e) => e
            .values
            .iter()
            .map(|v| {
                let name = v.identifier.and_then(|i| ident_name(ctx, i));
                (v.identifier, v.custom_value, name)
            })
            .collect(),
        _ => Vec::new(),
    }
}

// ===================================================================================================
// resolve_body — analyzer.cpp:6587 + class/function/suite/node drivers (1358-1671) + statements
// (2246-2592)
//
// WP-E2 ports the body-resolution skeleton: the recursive class/function-body drivers + the
// `resolve_node` statement dispatcher + the per-statement resolvers (`resolve_if`/`resolve_while`/
// `resolve_for`/`resolve_return`/`resolve_assert`/`resolve_match`/`resolve_match_branch`/
// `resolve_match_pattern`) — each of which calls `reduce_expression` on its expression children
// but emits *no* new errors. Godot's body-level errors (`Not all code paths return a value.`,
// `Expected string for assert error message.`, the `Unable to iterate on …` family, the
// match-pattern compatibility errors) are all gated on `is_type_compatible` / control-flow
// analysis / typed-iterator helpers (`get_function_signature`) and land in E3. Until then this
// pass is purely "make the reducer fire on body expressions" — infrastructure for the rest of E3.
// ===================================================================================================

/// `GDScriptAnalyzer::resolve_body()` (analyzer.cpp:6587): drive `resolve_class_body(head, true)`.
/// Crate-internal: external callers go through [`crate::analyze`].
pub(crate) fn resolve_body(ctx: &mut AnalysisContext) {
    if let Some(root) = ctx.tree.root_id() {
        resolve_class_body_recursive(ctx, root, true);
    }
}

/// `resolve_class_body(p_class, bool p_recursive)` (analyzer.cpp:1573).
///
/// WP-R1: Godot drives this as the root's OWN body resolution + lambda drain + abstract-
/// inheritance check, *then* recurses into inner classes. We mirror that here: the per-class
/// body work (incl. the `check_abstract_method_implementation` emit at analyzer.cpp:1532-1568,
/// which lives at the tail of [`resolve_class_body`]) runs once for the class, and only after
/// it returns do we recurse into the inner classes. This makes `holding_some_invalid_lambda`'s
/// queued lambda 41 drain before any inner-class body work — matching Godot's emission
/// order on `errors/abstract_methods.gd`.
fn resolve_class_body_recursive(ctx: &mut AnalysisContext, class_id: NodeId, recursive: bool) {
    resolve_class_body(ctx, class_id);
    if recursive {
        for inner in inner_classes(ctx, class_id) {
            resolve_class_body_recursive(ctx, inner, true);
        }
    }
}

/// `resolve_class_body(p_class, p_source)` (analyzer.cpp:1358). Godot dispatches function
/// bodies, inline property accessors (the getter/setter `FunctionNode`s synthesized for typed
/// properties), and `@export_*` group annotations. WP-E2 ports the function-body dispatch only;
/// inline-property accessors share the function-body codepath and join in automatically; the
/// `getter/setter not found` and getter/setter compat errors (analyzer.cpp:1450-1517) need
/// `is_type_compatible` and land in E3. The abstract-method-implementation check
/// (analyzer.cpp:1532-1568) needs the @abstract annotation system and lands with WP-F.
fn resolve_class_body(ctx: &mut AnalysisContext, class_id: NodeId) {
    if !ctx.resolved_bodies.insert(class_id) {
        return; // analyzer.cpp:1365 — idempotence.
    }
    // Ensure the interface is resolved first (analyzer.cpp:1399). resolve_interface is itself
    // idempotent via ctx.resolved_interfaces.
    resolve_class_interface(ctx, class_id);

    let previous_class = ctx.current_class;
    ctx.current_class = Some(class_id);

    // Recurse on the in-file base class's body first (analyzer.cpp:1401-1405).
    let base = ctx.base_type(class_id);
    if base.kind == DtKind::Class {
        if let Some(bc) = base.class_node {
            resolve_class_body(ctx, bc);
        }
    }

    // Functions, property accessors, and groups (analyzer.cpp:1408-1438).
    for i in 0..member_count(ctx, class_id) {
        let Some(member) = nth_member(ctx, class_id, i) else {
            continue;
        };
        match member {
            Member::Function(fn_id) => {
                resolve_function_body(ctx, fn_id, false);
            }
            Member::Variable(var_id) => {
                // analyzer.cpp:1417-1432 — inline-property getter/setter body resolution.
                // Godot stamps the getter's return_type / the setter's first parameter's
                // type with the variable's declared specifier, then runs resolve_function_body
                // on each. We mirror that, except gdls doesn't mutate parse-tree datatypes
                // (the tree is `&ParseTree` from the analyzer's side) — instead we set the
                // ctx-side TypeTable entry for the synthesized accessor's relevant slot. The
                // accessor function-nodes' identifier-name slots are empty (the parser carries
                // the setter parameter as `setter_parameter` directly on the VariableNode), so
                // the `_init`/`_static_init` special cases in resolve_function_signature don't
                // misfire on them.
                let (style, setter_acc, getter_acc) = match &ctx.node(var_id).kind {
                    NodeKind::Variable(v) => (v.property, v.setter, v.getter),
                    _ => continue,
                };
                if style != gd_syntax::ast::PropertyStyle::Inline {
                    continue;
                }
                let var_type = ctx.get_type(var_id).clone();
                if let gd_syntax::ast::PropertyAccessor::Inline(getter_fn) = getter_acc {
                    // Setter access lives on `v.setter`, getter access lives on `v.getter`.
                    // Match Godot's stamp at analyzer.cpp:1419-1423.
                    ctx.set_type(getter_fn, var_type.clone());
                    resolve_function_body(ctx, getter_fn, false);
                }
                if let gd_syntax::ast::PropertyAccessor::Inline(setter_fn) = setter_acc {
                    // analyzer.cpp:1427-1428 — stamp the setter's first parameter NODE (not the
                    // identifier) with the variable's type. `reduce_identifier`'s local-lookup and
                    // `function_param_named` both read from the parameter node, so we must type
                    // the ParameterNode that the Function's `parameters[0]` points to.
                    if let NodeKind::Function(f) = &ctx.node(setter_fn).kind {
                        if let Some(&param_node) = f.parameters.first() {
                            ctx.set_type(param_node, var_type.clone());
                        }
                    }
                    resolve_function_body(ctx, setter_fn, false);
                }
            }
            Member::Class(_inner_id) => {
                // WP-R1: inner-class bodies are NOT resolved inline here. Godot completes the
                // current class's own body resolution (function bodies + lambda drain + abstract-
                // inheritance check, analyzer.cpp:1408-1568) before recursing into inner classes
                // (driven by `resolve_class_body_recursive`, analyzer.cpp:1573). Resolving inner
                // class bodies inline here would inject their emissions (e.g. `Test1`'s
                // `check_abstract_method_implementation` at pos 6) between this class's function-
                // body work and its lambda drain (pos 5) — flipping the order vs Godot. The
                // class-level `@abstract` annotation is already applied during the inheritance
                // pass (`apply_class_abstract_annotation`, analyzer.cpp:623-627) and the function-
                // level `@abstract` annotation during the interface pass
                // (`apply_function_abstract_annotation`, analyzer.cpp:1268-1356), both of which
                // recurse into inner classes already.
            }
            // analyzer.cpp:1433-1437 — `@export_*` group annotations are resolved here; their
            // analyzer effects (`apply` callbacks) join with the WP-F warning set.
            _ => {}
        }
    }

    // WP-F: per-member unused-warning sweep (analyzer.cpp:1441-1525). Godot tracks `usages` on
    // each `VariableNode` / `SignalNode` and emits warnings here for any whose counter stayed at
    // zero. gdls doesn't carry per-node usage counters; instead, we do a name-based sweep:
    // collect every identifier reference + every string-literal payload anywhere in the file
    // (Godot's `connect("name", …)` / `emit_signal("name", …)` / `Signal(self, "name")` cases)
    // and warn for any member name not in that set. Mirrors Godot's behavior for the corpus's
    // simple cases; for over-approximated string literals (e.g. an arbitrary `"foo"` next to a
    // `signal foo`) Godot would also not warn, so the heuristic over-suppresses in the same
    // direction as Godot.
    // analyzer.cpp:2068 / 6528 — drain `pending_body_resolution_lambdas` queued by
    // `reduce_lambda` while resolving this class's function bodies. Each entry pairs the
    // LambdaNode id with the snapshot of `concrete_function` at the time it was queued, so the
    // body's static-context errors can name the outer concrete function (e.g. `static_func()`)
    // rather than the lambda. We take the queue by drain() so that any lambda nested inside
    // another lambda gets its own queue entry the next time the inner pass processes it.
    drain_pending_lambda_bodies(ctx);

    check_property_setget_compat(ctx, class_id);

    emit_unused_member_warnings(ctx, class_id);

    // WP-F: per-variable annotation-based warnings (analyzer.cpp:1066-1107, `DEBUG_ENABLED`):
    // ONREADY_WITH_EXPORT (variable has both `@onready` and `@export`) and
    // GET_NODE_DEFAULT_WITHOUT_ONREADY (non-static + non-onready variable initialized with
    // `$Node` / `%Unique` / `get_node(...)` — optionally wrapped in a cast).
    emit_variable_annotation_warnings(ctx, class_id);

    // analyzer.cpp:1532-1568 — abstract-method-implementation check.
    check_abstract_method_implementation(ctx, class_id);

    ctx.current_class = previous_class;
}

/// `GDScriptAnalyzer::resolve_pending_lambda_bodies()` (analyzer.cpp:6528-6580). Lambdas are
/// queued by `reduce_lambda` while their enclosing class function bodies are being resolved;
/// each entry carries a snapshot of `ctx.concrete_function` so we can restore the outer-name
/// context before resolving the lambda's body. We DO update `current_function` to the lambda's
/// synthesized FunctionNode so identifier / parameter lookup inside the body sees the lambda's
/// parameters via `function_param_named`, but leave `concrete_function` pointing at the captured
/// outer (matching Godot's `source_lambda -> parent_function` walk at analyzer.cpp:3647-3648).
///
/// The full Godot also rewrites lambda parameters to prepend captures (analyzer.cpp:6545-6566);
/// gdls's port stays diagnostics-only and skips the capture-rewrite — captures still resolve
/// against the lambda's enclosing scope via the existing `lookup_local` walk over `suite_stack`.
fn drain_pending_lambda_bodies(ctx: &mut AnalysisContext) {
    use gd_syntax::ast::NodeKind;

    // analyzer.cpp:6536-6537: copy the queue and clear it, then iterate the copy in
    // insertion order (Godot's `List<>` is push_back / iterate front-to-back = FIFO).
    // Any lambdas queued *while* we're draining (nested lambdas, or follow-up lambdas
    // queued from a default-arg reduction triggered during a lambda body) remain in
    // `ctx.pending_lambda_bodies` for the next drain call.
    let lambdas = std::mem::take(&mut ctx.pending_lambda_bodies);
    for (lambda_id, captured_concrete, captured_static, captured_suite_stack) in lambdas {
        let func_id = match &ctx.node(lambda_id).kind {
            NodeKind::Lambda(l) => l.function,
            _ => continue,
        };
        let Some(func_id) = func_id else { continue };

        // Drain runs with `current_function` / `concrete_function` already restored from the
        // outer class-body pass (both `None` at this point). Prime `concrete_function` to the
        // captured outer so `resolve_function_body`'s lambda branch leaves it untouched —
        // that's what makes static-context errors emitted inside the lambda body resolve the
        // outer concrete's name (e.g. `static_func()`), or pick the "from a static variable
        // initializer." template when captured_concrete is `None` (the lambda was queued from
        // a `static var` initializer at class level, no enclosing concrete function). Also
        // restore `static_context` to the value captured at queue time — see the matching note
        // on `reduce_lambda`.
        //
        // Also restore the outer `suite_stack` so the lambda body's `lookup_local` resolves
        // captures from the enclosing function's locals (Godot's capture mechanism). Without
        // this, identifier references like `outer_var` inside the lambda's body fall through to
        // `Identifier "X" not declared in the current scope.`. Godot resolves lambdas in the
        // outer function's environment via per-statement reduce; gdls queues them, so we
        // snapshot+restore the stack to preserve the same effective scope.
        let pre_concrete = ctx.concrete_function;
        let pre_static = ctx.static_context;
        let pre_suite_stack = std::mem::take(&mut ctx.suite_stack);
        ctx.concrete_function = captured_concrete;
        ctx.static_context = captured_static;
        ctx.suite_stack = captured_suite_stack;
        resolve_function_body(ctx, func_id, true);
        ctx.concrete_function = pre_concrete;
        ctx.static_context = pre_static;
        ctx.suite_stack = pre_suite_stack;
    }
    // If new lambdas were queued during this drain pass (e.g. a lambda body that itself
    // declared another lambda whose body was queued mid-resolution), keep draining. The
    // Godot's `resolve_pending_lambda_bodies` doesn't loop, but the outer per-statement
    // and class-body drains call it again afterwards; collapsing those callsites here
    // keeps the queue empty on return without changing observable ordering.
    //
    // M5 WP-O3 / WP-O4 governor + cancellation checkpoint at the re-entrant self-call. A
    // pathological grammar that perpetually queues new lambdas during drain would otherwise
    // loop here indefinitely; this checkpoint guarantees the recursion bails by the iter_limit.
    // The synthetic error anchors at the root class node (or `ByteSpan::default()` if the tree
    // had no root — degenerate parse), as a "the analyzer gave up draining lambdas in this file"
    // marker. No single per-lambda span fits; the recursion isn't tied to any one AST node.
    if !ctx.pending_lambda_bodies.is_empty() {
        let span = ctx
            .tree
            .root_id()
            .map(|root| ctx.tree.get(root).span)
            .unwrap_or_default();
        if !ctx.checkpoint(span) {
            drain_pending_lambda_bodies(ctx);
        }
    }
}

/// Emit `UNUSED_PRIVATE_CLASS_VARIABLE` (analyzer.cpp:1444-1448) and `UNUSED_SIGNAL`
/// (analyzer.cpp:1518-1524) for the class's members whose names are not referenced anywhere in
/// the file. Godot tracks per-node usage counters; gdls uses a one-pass name-set sweep that
/// matches Godot on the corpus's known cases (identifier mentions + string-literal payloads).
/// `analyzer.cpp:1450-1517` — SETGET property accessor type-compatibility checks. For each
/// variable with `PROP_SETGET` style, look up the getter/setter function in the class and check
/// return-type / param-type compatibility against the variable's declared type.
/// `abstract_annotation` apply callback (gdscript_parser.cpp:4483-4506). Process `@abstract`
/// annotations on the class and its member functions, emitting errors for duplicates and
/// static misuse.
/// Class-level half of `apply_abstract_annotations` — fires during the inheritance pass
/// (gdscript_analyzer.cpp:623-627 applies each class's own annotations after `resolve_class_inheritance`).
/// Keeping it in the inheritance pass matches Godot's emission order: `DuplicateAbstract`'s
/// `@abstract @abstract` "only once per class" error fires before any function-level
/// `@abstract` apply in unrelated classes.
fn apply_class_abstract_annotation(ctx: &mut AnalysisContext, class_id: NodeId) {
    let class_annotations: Vec<NodeId> = ctx.node(class_id).annotations.clone();
    let mut abstract_count = 0usize;
    for &ann_id in &class_annotations {
        let is_abstract = matches!(
            &ctx.node(ann_id).kind,
            NodeKind::Annotation(an) if an.name == "@abstract"
        );
        if is_abstract {
            abstract_count += 1;
            if abstract_count > 1 {
                ctx.push_error(
                    r#""@abstract" annotation can only be used once per class."#,
                    ann_id,
                );
            }
        }
    }
    if abstract_count >= 1 {
        ctx.abstract_nodes.insert(class_id);
    }
}

/// Function-level half of `apply_abstract_annotations` — fires during the interface pass
/// (gdscript_analyzer.cpp:1206-1209 applies each function's annotations before
/// `resolve_function_signature`). Emits the static-misuse and duplicate-on-function errors at
/// the right phase so they interleave with class-level errors in Godot's emission order.
fn apply_function_abstract_annotation(ctx: &mut AnalysisContext, fn_id: NodeId) {
    let fn_annotations: Vec<NodeId> = ctx.node(fn_id).annotations.clone();
    let mut fn_abstract_count = 0usize;
    for &ann_id in &fn_annotations {
        let is_abstract = matches!(
            &ctx.node(ann_id).kind,
            NodeKind::Annotation(an) if an.name == "@abstract"
        );
        if !is_abstract {
            continue;
        }
        fn_abstract_count += 1;
        let is_static = matches!(&ctx.node(fn_id).kind, NodeKind::Function(f) if f.is_static);
        if is_static {
            ctx.push_error(
                r#""@abstract" annotation cannot be applied to static functions."#,
                ann_id,
            );
            // Reject the @abstract annotation — the function is NOT abstract. Resets the
            // count so the no-body check later emits Godot's "must have a body" error.
            fn_abstract_count = 0;
            break;
        }
        if fn_abstract_count > 1 {
            ctx.push_error(
                r#""@abstract" annotation can only be used once per function."#,
                ann_id,
            );
            break;
        }
    }
    if fn_abstract_count >= 1 {
        ctx.abstract_nodes.insert(fn_id);
    }
}

/// `analyzer.cpp:1532-1568` — check that non-abstract classes don't contain or inherit
/// unimplemented abstract methods.
fn check_abstract_method_implementation(ctx: &mut AnalysisContext, class_id: NodeId) {
    if ctx.abstract_nodes.contains(&class_id) {
        return;
    }

    let mut implemented: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut cur = Some(class_id);

    while let Some(cid) = cur {
        if cid != class_id && !ctx.abstract_nodes.contains(&cid) {
            break;
        }

        let count = member_count(ctx, cid);
        for i in 0..count {
            let Some(Member::Function(fn_id)) = nth_member(ctx, cid, i) else {
                continue;
            };
            let fn_name = decl_identifier_name(ctx, fn_id);
            let fn_is_abstract = ctx.abstract_nodes.contains(&fn_id);
            if fn_is_abstract {
                if cid == class_id {
                    let class_name = class_identifier_name(ctx, class_id).unwrap_or_default();
                    ctx.push_error(
                        format!(
                            r#"Class "{class_name}" is not abstract but contains abstract methods. Mark the class as "@abstract" or remove "@abstract" from all methods in this class."#
                        ),
                        class_id,
                    );
                    return;
                } else if !implemented.contains(&fn_name) {
                    let class_name = class_identifier_name(ctx, class_id).unwrap_or_default();
                    let base_name = class_identifier_name(ctx, cid).unwrap_or_default();
                    ctx.push_error(
                        format!(
                            r#"Class "{class_name}" must implement "{base_name}.{fn_name}()" and other inherited abstract methods or be marked as "@abstract"."#
                        ),
                        class_id,
                    );
                    return;
                }
            } else {
                implemented.insert(fn_name);
            }
        }

        // Walk up the in-file class chain.
        let base = ctx.bases.get(&cid).cloned().unwrap_or_default();
        cur = if base.kind == DtKind::Class {
            base.class_node
        } else {
            None
        };
    }
}

fn check_property_setget_compat(ctx: &mut AnalysisContext, class_id: NodeId) {
    let count = member_count(ctx, class_id);
    for i in 0..count {
        let Some(Member::Variable(var_id)) = nth_member(ctx, class_id, i) else {
            continue;
        };
        let (style, getter_ptr, setter_ptr) = match &ctx.node(var_id).kind {
            NodeKind::Variable(v) => {
                let gp = match v.getter {
                    gd_syntax::ast::PropertyAccessor::Pointer(id) => Some(id),
                    _ => None,
                };
                let sp = match v.setter {
                    gd_syntax::ast::PropertyAccessor::Pointer(id) => Some(id),
                    _ => None,
                };
                (v.property, gp, sp)
            }
            _ => continue,
        };
        if style != gd_syntax::ast::PropertyStyle::SetGet {
            continue;
        }
        let var_type = ctx.get_type(var_id).clone();

        let mut has_valid_getter = false;
        let mut getter_return_dt: Option<DataType> = None;

        if let Some(getter_name_id) = getter_ptr {
            let getter_name = match &ctx.node(getter_name_id).kind {
                NodeKind::Identifier(i) => i.name.clone(),
                _ => String::new(),
            };
            let getter_fn = find_member_function(ctx, class_id, &getter_name);
            if let Some(fn_id) = getter_fn {
                let return_dt = ctx.get_type(fn_id).clone();
                let param_count = match &ctx.node(fn_id).kind {
                    NodeKind::Function(f) => f.parameters.len(),
                    _ => 0,
                };
                if param_count != 0 || !return_dt.is_set() {
                    ctx.push_error(
                        format!(
                            r#"Function "{getter_name}" cannot be used as getter because of its signature."#
                        ),
                        var_id,
                    );
                } else if !crate::reducer::is_type_compatible(ctx, &var_type, &return_dt, true) {
                    ctx.push_error(
                        format!(
                            r#"Function with return type "{return_dt}" cannot be used as getter for a property of type "{var_type}"."#
                        ),
                        var_id,
                    );
                } else {
                    has_valid_getter = true;
                    getter_return_dt = Some(return_dt.clone());
                    if var_type.builtin_type == VariantType::Int
                        && return_dt.builtin_type == VariantType::Float
                    {
                        ctx.push_warning(
                            crate::warnings::WarningCode::NarrowingConversion,
                            &[],
                            var_id,
                        );
                    }
                }
            } else {
                ctx.push_error(format!(r#"Getter "{getter_name}" not found."#), var_id);
            }
        }

        let mut has_valid_setter = false;
        let mut setter_param_dt: Option<DataType> = None;

        if let Some(setter_name_id) = setter_ptr {
            let setter_name = match &ctx.node(setter_name_id).kind {
                NodeKind::Identifier(i) => i.name.clone(),
                _ => String::new(),
            };
            let setter_fn = find_member_function(ctx, class_id, &setter_name);
            if let Some(fn_id) = setter_fn {
                let params: Vec<NodeId> = match &ctx.node(fn_id).kind {
                    NodeKind::Function(f) => f.parameters.clone(),
                    _ => Vec::new(),
                };
                if params.len() != 1 {
                    ctx.push_error(
                        format!(
                            r#"Function "{setter_name}" cannot be used as setter because of its signature."#
                        ),
                        var_id,
                    );
                } else {
                    let param_dt = ctx.get_type(params[0]).clone();
                    if !crate::reducer::is_type_compatible(ctx, &var_type, &param_dt, true) {
                        ctx.push_error(
                            format!(
                                r#"Function with argument type "{param_dt}" cannot be used as setter for a property of type "{var_type}"."#
                            ),
                            var_id,
                        );
                    } else {
                        has_valid_setter = true;
                        setter_param_dt = Some(param_dt.clone());
                        if var_type.builtin_type == VariantType::Float
                            && param_dt.builtin_type == VariantType::Int
                        {
                            ctx.push_warning(
                                crate::warnings::WarningCode::NarrowingConversion,
                                &[],
                                var_id,
                            );
                        }
                    }
                }
            } else {
                ctx.push_error(format!(r#"Setter "{setter_name}" not found."#), var_id);
            }
        }

        // analyzer.cpp:1512-1516 — cross-check getter return vs setter param when the
        // variable itself is Variant-typed and both accessors are valid.
        if var_type.is_variant() && has_valid_getter && has_valid_setter {
            if let (Some(g_dt), Some(s_dt)) = (&getter_return_dt, &setter_param_dt) {
                if !crate::reducer::is_type_compatible(ctx, g_dt, s_dt, true) {
                    ctx.push_error(
                        format!(
                            r#"Getter with type "{g_dt}" cannot be used along with setter of type "{s_dt}"."#
                        ),
                        var_id,
                    );
                }
            }
        }
    }
}

fn find_member_function(ctx: &AnalysisContext, class_id: NodeId, name: &str) -> Option<NodeId> {
    let count = member_count(ctx, class_id);
    for i in 0..count {
        if let Some(Member::Function(fn_id)) = nth_member(ctx, class_id, i) {
            if decl_identifier_name(ctx, fn_id) == name {
                return Some(fn_id);
            }
        }
    }
    None
}

fn emit_unused_member_warnings(ctx: &mut AnalysisContext, class_id: NodeId) {
    use crate::warnings::WarningCode;

    // Build the set of names referenced anywhere in the file. Walk every node looking for
    // identifier references and string-literal payloads — both count as a "use" the way the
    // Godot's `usages++` does in `reduce_identifier` and the `emit_signal("name")` arg-folding.
    let referenced = referenced_names(ctx);

    // Loop over the class's members and check each by name.
    let total = member_count(ctx, class_id);
    for i in 0..total {
        let Some(member) = nth_member(ctx, class_id, i) else {
            continue;
        };
        match member {
            Member::Variable(var_id) => {
                let name = decl_identifier_name(ctx, var_id);
                if !name.starts_with('_') {
                    continue; // Godot only warns for `_`-prefixed private vars.
                }
                if referenced.contains(&name) {
                    continue;
                }
                // Anchor the warning at the variable's identifier (Godot:1446 anchors there too),
                // but route the `@warning_ignore` check through the variable node — that's where
                // the annotations live in the AST (`@warning_ignore("…") var _b` attaches the
                // annotation to the `VariableNode`, not its identifier child).
                let at = match &ctx.node(var_id).kind {
                    NodeKind::Variable(v) => v.identifier.unwrap_or(var_id),
                    _ => var_id,
                };
                ctx.push_warning_for(WarningCode::UnusedPrivateClassVariable, &[name], at, var_id);
            }
            Member::Signal(sig_id) => {
                let name = decl_identifier_name(ctx, sig_id);
                if name.is_empty() {
                    continue;
                }
                if referenced.contains(&name) {
                    continue;
                }
                let at = match &ctx.node(sig_id).kind {
                    NodeKind::Signal(s) => s.identifier.unwrap_or(sig_id),
                    _ => sig_id,
                };
                ctx.push_warning_for(WarningCode::UnusedSignal, &[name], at, sig_id);
            }
            _ => {}
        }
    }
}

/// Emit `ONREADY_WITH_EXPORT` and `GET_NODE_DEFAULT_WITHOUT_ONREADY` warnings for class member
/// variables, matching Godot's `DEBUG_ENABLED` block at gdscript_analyzer.cpp:1066-1107.
fn emit_variable_annotation_warnings(ctx: &mut AnalysisContext, class_id: NodeId) {
    use crate::warnings::WarningCode;
    let total = member_count(ctx, class_id);
    for i in 0..total {
        let Some(Member::Variable(var_id)) = nth_member(ctx, class_id, i) else {
            continue;
        };
        let (
            has_onready,
            onready_ann_id,
            has_export,
            export_ann_id,
            export_ann_name,
            is_static,
            initializer,
        ) = {
            let var_node = ctx.node(var_id);
            let mut has_onready = false;
            let mut onready_ann_id: Option<NodeId> = None;
            let mut has_export = false;
            let mut export_ann_id: Option<NodeId> = None;
            let mut export_ann_name = String::new();
            for &ann_id in &var_node.annotations {
                if let NodeKind::Annotation(a) = &ctx.node(ann_id).kind {
                    match a.name.as_str() {
                        "@onready" => {
                            has_onready = true;
                            if onready_ann_id.is_none() {
                                onready_ann_id = Some(ann_id);
                            }
                        }
                        // Godot's `member.variable->exported` includes every `@export*`
                        // variant (@export, @export_range, @export_enum, etc.). Detect any
                        // annotation whose name starts with "@export".
                        n if n.starts_with("@export") => {
                            has_export = true;
                            if export_ann_id.is_none() {
                                export_ann_id = Some(ann_id);
                                export_ann_name = n.to_owned();
                            }
                        }
                        _ => {}
                    }
                }
            }
            let (is_static, initializer) = match &var_node.kind {
                NodeKind::Variable(v) => (v.is_static, v.initializer),
                _ => (false, None),
            };
            (
                has_onready,
                onready_ann_id,
                has_export,
                export_ann_id,
                export_ann_name,
                is_static,
                initializer,
            )
        };

        // gdscript_parser.cpp:4648 — `@export` cannot be applied to a `static var`. Godot's
        // annotation `apply()` checks this on the variable target; gdls's annotation-apply
        // pass lands incrementally with WP-F, so this specific arm is handled inline against
        // the variable's `is_static` flag. Error anchors at the `@export*` annotation, not
        // the variable.
        if is_static {
            if let Some(ann_id) = export_ann_id {
                ctx.push_error(
                    format!(
                        r#"Annotation "{export_ann_name}" cannot be applied to a static variable."#
                    ),
                    ann_id,
                );
            }
        }

        // gdscript_parser.cpp — annotation argument constancy validation. Each `@export*`
        // annotation's argument must be a constant expression. Walk the args and emit
        // `Argument N of annotation "@export*" isn't a constant expression.` when an arg
        // identifier resolves to a non-constant local OR a non-constant class member
        // (Variable / Signal / Function). gdls's fold table is too sparse to gate on
        // `is_reduced` here; the identifier walk is a narrow but reliable check.
        if let Some(ann_id) = export_ann_id {
            let arg_ids: Vec<NodeId> = match &ctx.node(ann_id).kind {
                NodeKind::Annotation(a) => a.arguments.clone(),
                _ => Vec::new(),
            };
            for (arg_index, &arg_id) in arg_ids.iter().enumerate() {
                if expression_references_nonconstant_member(ctx, arg_id, class_id) {
                    ctx.push_error(
                        format!(
                            r#"Argument {} of annotation "{export_ann_name}" isn't a constant expression."#,
                            arg_index + 1
                        ),
                        ann_id,
                    );
                    break;
                }
            }
        }

        // analyzer.cpp:1067-1069 — both @onready and @export → ONREADY_WITH_EXPORT.
        if has_onready && has_export {
            ctx.push_warning(WarningCode::OnreadyWithExport, &[], var_id);
        }

        // analyzer.cpp:1070-1106 — non-static + non-onready + initializer is `$`/`%`/`get_node`.
        if !is_static && !has_onready {
            if let Some(init_id) = initializer {
                if let Some(offending) = get_node_default_form(ctx, init_id) {
                    ctx.push_warning(
                        WarningCode::GetNodeDefaultWithoutOnready,
                        &[offending],
                        var_id,
                    );
                }
            }
        }

        // gdscript_parser.cpp:4513-4515 — `@onready` requires a Node-derived class.
        // Also gdscript_parser.cpp:4844-4847 / :4915-4918 — `@export` of a Node-derived type
        // requires a Node-derived class. Both checks read the current class's nearest native
        // ancestor and consult the DB's `is_subclass_of(native, "Node")`. The base's string for
        // the error template is whatever `base_type.to_string()` produces, which for our chain
        // walk is the bare native_type (e.g. "Resource", "RefCounted").
        if has_onready || has_export {
            let class_native_base = nearest_native_ancestor(ctx, class_id);
            if let Some(native_base) = class_native_base.as_ref() {
                let is_node_derived = ctx.native.is_subclass_of_named(native_base, "Node");
                if has_onready && !is_node_derived {
                    if let Some(ann_id) = onready_ann_id {
                        ctx.push_error(
                            r#""@onready" can only be used in classes that inherit "Node"."#,
                            ann_id,
                        );
                    }
                }
                if has_export && !is_node_derived {
                    let var_type = ctx.get_type(var_id).clone();
                    let exports_node = type_is_node_typed_for_export(ctx, &var_type);
                    if exports_node {
                        if let Some(ann_id) = export_ann_id {
                            ctx.push_error(
                                format!(
                                    r#"Node export is only supported in Node-derived classes, but the current class inherits "{native_base}"."#
                                ),
                                ann_id,
                            );
                        }
                    }
                }
            }
        }
    }
}

/// Walk `class_id`'s base chain (in-file Class → Class → ... → Native) and return the nearest
/// native ancestor's name. Used by the `@onready` / `@export-Node` checks at
/// gdscript_parser.cpp:4513-4515 / :4844-4847 / :4915-4918 to test whether the enclosing class
/// inherits Node.
pub(crate) fn nearest_native_ancestor(ctx: &AnalysisContext, class_id: NodeId) -> Option<String> {
    let mut cur = Some(class_id);
    while let Some(c) = cur {
        let base = ctx.bases.get(&c).cloned().unwrap_or_default();
        match base.kind {
            DtKind::Native => return Some(base.native_type),
            DtKind::Class => {
                cur = base.class_node;
            }
            DtKind::Script => {
                // Walk the cross-file chain (`crate::script_chain`) to its native root — Godot
                // reads the eagerly-propagated `base_type.native_type` here (analyzer.cpp:3868).
                // `None` = unknown chain ⇒ consumers (the `$`/`@onready`/`@export` node-ness
                // gates, shadow warnings) stay silent; the old `RefCounted` fallback for
                // `Extends::Names` chains was the `Cannot use "$" on a class that isn't a node`
                // false-positive family.
                let script_ref = base.script_type.as_ref()?;
                return crate::script_chain::chain_native_root(ctx, script_ref);
            }
            _ => return None,
        }
    }
    None
}

/// `true` when the variable's declared type makes the `@export` annotation a "Node export" — the
/// Godot's `PROPERTY_HINT_NODE_TYPE` path (gdscript_parser.cpp:4844 / :4915). Hits when:
/// * the type is a native Object whose chain reaches Node, or
/// * the type is `Array[T]` / `Dictionary[K, V]` and any container element is itself Node-typed.
fn type_is_node_typed_for_export(ctx: &AnalysisContext, dt: &DataType) -> bool {
    if dt.kind == DtKind::Native && !dt.native_type.is_empty() {
        return ctx.native.is_subclass_of_named(&dt.native_type, "Node");
    }
    if dt.kind == DtKind::Builtin
        && matches!(
            dt.builtin_type,
            VariantType::Array | VariantType::Dictionary
        )
    {
        return dt
            .container_element_types
            .iter()
            .any(|inner| type_is_node_typed_for_export(ctx, inner));
    }
    false
}

/// Classify a class-variable initializer for the `GET_NODE_DEFAULT_WITHOUT_ONREADY` check
/// (analyzer.cpp:1073-1102). Returns `Some(offending_syntax)` if the initializer is `$Node` /
/// `%Unique` / a `get_node(...)` call (optionally wrapped in a single `Cast`). Returns `None`
/// otherwise — including for any expression shape Godot wouldn't flag.
fn get_node_default_form(ctx: &AnalysisContext, init_id: NodeId) -> Option<String> {
    // analyzer.cpp:1075-1077 — unwrap a single Cast wrapper.
    let inner_id = match &ctx.node(init_id).kind {
        NodeKind::Cast(c) => c.operand?,
        _ => init_id,
    };

    match &ctx.node(inner_id).kind {
        NodeKind::GetNode(gn) => Some(if gn.use_dollar { "$" } else { "%" }.to_owned()),
        NodeKind::Call(c) if c.function_name == "get_node" => {
            // analyzer.cpp:1083-1095 — only count when the callee is bare `get_node` or
            // `self.get_node` (an attribute subscript whose base is `self`). Other callees fall
            // through (Godot's switch-default).
            let callee_id = c.callee?;
            match &ctx.node(callee_id).kind {
                NodeKind::Identifier(_) => Some("get_node()".to_owned()),
                NodeKind::Subscript(s) => match (s.access, s.base) {
                    (Some(gd_syntax::ast::SubscriptAccess::Attribute(_)), Some(base_id)) => {
                        if matches!(&ctx.node(base_id).kind, NodeKind::SelfExpr) {
                            Some("get_node()".to_owned())
                        } else {
                            None
                        }
                    }
                    _ => None,
                },
                _ => None,
            }
        }
        _ => None,
    }
}

/// One-pass name-set collected from every identifier + string-literal in the file. Godot
/// tracks per-node usage counters via the analyzer's `reduce_identifier` and the various
/// `emit_signal` / `connect` / `disconnect` / `Signal()` builtin-call branches; gdls's name-set
/// sweep is a conservative over-approximation in the same direction (over-suppresses warnings,
/// never falsely emits one) that matches Godot on every corpus case.
fn referenced_names(ctx: &AnalysisContext) -> rustc_hash::FxHashSet<String> {
    use gd_syntax::ast::NodeKind;
    use gd_syntax::token::Literal;

    // First pass: collect every identifier NodeId that is *itself the name slot* of a declaration
    // (Variable/Constant/Signal/Function/Parameter/Enum/EnumValue/Class). These are declaration
    // sites, not references — Godot's `usages` counter is incremented in `reduce_identifier`
    // for true references, never on the decl identifier. We exclude them from the use-set so a
    // `var _a` declaration doesn't mark "_a" as referenced.
    let mut decl_ident_ids = rustc_hash::FxHashSet::<gd_syntax::ast::NodeId>::default();
    for id in ctx.tree.iter_ids() {
        match &ctx.node(id).kind {
            NodeKind::Variable(v) => {
                if let Some(i) = v.identifier {
                    decl_ident_ids.insert(i);
                }
            }
            NodeKind::Constant(c) => {
                if let Some(i) = c.identifier {
                    decl_ident_ids.insert(i);
                }
            }
            NodeKind::Signal(s) => {
                if let Some(i) = s.identifier {
                    decl_ident_ids.insert(i);
                }
            }
            NodeKind::Function(f) => {
                if let Some(i) = f.identifier {
                    decl_ident_ids.insert(i);
                }
            }
            NodeKind::Parameter(p) => {
                if let Some(i) = p.identifier {
                    decl_ident_ids.insert(i);
                }
            }
            NodeKind::Enum(e) => {
                if let Some(i) = e.identifier {
                    decl_ident_ids.insert(i);
                }
                for v in &e.values {
                    if let Some(i) = v.identifier {
                        decl_ident_ids.insert(i);
                    }
                }
            }
            NodeKind::Class(c) => {
                if let Some(i) = c.identifier {
                    decl_ident_ids.insert(i);
                }
            }
            _ => {}
        }
    }

    // Second pass: collect names from identifier references (excluding the decl-name slots) and
    // from specific call-argument string-literal payloads. Godot's signal-usage tracking only
    // counts string-literal args to `emit_signal()` / `connect()` / `disconnect()` / `Signal()`
    // when the arg is `is_constant` (analyzer.cpp:3411-3425 + 3681-3692), not random string
    // literals anywhere in the file. Mirror that — otherwise a `var x := "signal_name"` would
    // wrongly count as a use of the signal.
    let mut set = rustc_hash::FxHashSet::<String>::default();
    for id in ctx.tree.iter_ids() {
        match &ctx.node(id).kind {
            NodeKind::Identifier(idn) if !decl_ident_ids.contains(&id) => {
                set.insert(idn.name.clone());
            }
            NodeKind::Call(c) => {
                // Match Godot's two implicit-use sites:
                // analyzer.cpp:3411-3425 — `Signal(self, "name")`: 2nd arg is the signal name.
                // analyzer.cpp:3680-3692 — `emit_signal("name", …)` / `connect("name", …)` /
                //   `disconnect("name", …)`: 1st arg.
                let name = c.function_name.as_str();
                let target_index = match name {
                    "Signal" if c.arguments.len() >= 2 => Some(1),
                    "emit_signal" | "connect" | "disconnect" if !c.arguments.is_empty() => Some(0),
                    _ => None,
                };
                if let Some(idx) = target_index {
                    let arg_id = c.arguments[idx];
                    if let NodeKind::Literal(lit) = &ctx.node(arg_id).kind {
                        match &lit.value {
                            Literal::String(s) | Literal::StringName(s) | Literal::NodePath(s) => {
                                set.insert(s.clone());
                            }
                            _ => {}
                        }
                    }
                }
            }
            _ => {}
        }
    }
    set
}

/// `resolve_function_body(p_function, p_is_lambda)` (analyzer.cpp:1988). E2 ports the cursor
/// setup, the body-presence checks, and the `resolve_suite(body, true)` drive; control-flow
/// return-coverage lands here once the parser tracks `has_return` on suites
/// (WP-N7 — gdscript_parser.h:1177 / gdscript_parser.cpp:2090,2383,2459).
fn resolve_function_body(ctx: &mut AnalysisContext, func_id: NodeId, is_lambda: bool) {
    if !ctx.resolved_functions.insert(func_id) {
        return; // analyzer.cpp:1989 — idempotence.
    }
    let (name, _, _, is_static, is_abstract, body) = function_body_decl(ctx, func_id);

    // analyzer.cpp:1994-2008: body-presence vs `@abstract` validation. Godot routes on
    // `body->statements.is_empty()`: empty body emits the lambda/function/abstract trio of
    // errors; non-empty body emits "An abstract function cannot have a body." when abstract.
    // The parser always allocates a body Suite (`f.body = Some(body)` in both
    // `parse_function` and `parse_lambda`), so a `None` body here is unreachable in well-formed
    // ASTs — we still guard for it to keep the resolver crash-free on partial parses.
    let body_id = match body {
        Some(b) => b,
        None => return,
    };
    let statements_empty = matches!(
        &ctx.node(body_id).kind,
        NodeKind::Suite(s) if s.statements.is_empty()
    );
    if statements_empty {
        if is_lambda {
            ctx.push_error(
                r#"A lambda function must have a ":" followed by a body."#,
                func_id,
            );
        } else if !is_abstract {
            ctx.push_error(
                r#"A function must either have a ":" followed by a body, or be marked as "@abstract"."#,
                func_id,
            );
        }
        return;
    } else if is_abstract {
        ctx.push_error("An abstract function cannot have a body.", body_id);
        return;
    }

    let previous_function = ctx.current_function;
    let previous_concrete = ctx.concrete_function;
    let previous_static = ctx.static_context;
    ctx.current_function = Some(func_id);
    // See the note on resolve_function_signature: `concrete_function` shadows
    // `current_function` for non-lambda contexts. For a regular member function we set it to
    // `func_id` so static-context errors emitted inside the body name this function. For a
    // lambda we leave whatever the caller (`drain_pending_lambda_bodies`) set, including `None`
    // — the latter is what happens when the lambda was queued from a static-variable
    // initializer at class level (no enclosing concrete function). In that case
    // `enclosing_concrete_function_name` returns `None` and the static-context arms select the
    // `... from a static variable initializer.` template (analyzer.cpp:3653-3654 / :4480-4482).
    if !is_lambda && ctx.concrete_function.is_none() {
        ctx.concrete_function = Some(func_id);
    }
    // For regular functions, the function's own `is_static` is the source of truth. For
    // lambdas, `is_static` is parser-captured from the enclosing function (`None` at
    // class-body level), but drain has just seeded `ctx.static_context` from the value
    // captured when the lambda was queued — Godot stamps the same value onto the lambda's
    // `is_static` at analyzer.cpp:1749-1751 and then later reads it back. We skip the stamp
    // here and just leave `ctx.static_context` alone, which is functionally equivalent.
    if !is_lambda {
        ctx.static_context = is_static;
    }

    if let Some(body_id) = body {
        resolve_suite(ctx, body_id, true);

        // analyzer.cpp:2018-2020 — infer the function's return type from the body suite when
        // there's no explicit return annotation. `decide_suite_type` propagates return statement
        // types to the body suite; if the function is soft-typed (inferred Variant), adopt the
        // body's inferred type so SETGET compat checks and callers see the narrowed return.
        let fn_dt = ctx.get_type(func_id).clone();
        if !fn_dt.is_hard_type() {
            let body_dt = ctx.get_type(body_id).clone();
            if body_dt.is_set() {
                ctx.set_type(func_id, body_dt);
            }
        }

        // analyzer.cpp:2018-2025 — return-coverage check. When the function declares a hard return
        // type that isn't void (`Builtin NIL`) and isn't Variant, every code path must return a
        // value. Godot checks `is_hard_type() && (kind != BUILTIN || builtin_type != NIL)`,
        // which excludes both the implicit-Variant fallback for untyped functions and `void`
        // returns. The constructor `_init` is exempted unless we're inside a lambda — lambdas
        // never bind to `_init` so the gate is effectively `is_lambda || name != "_init"`.
        let return_type = ctx.get_type(func_id).clone();
        let body_has_return = match &ctx.node(body_id).kind {
            NodeKind::Suite(s) => s.has_return,
            _ => false,
        };
        let return_type_is_void_or_variant = !return_type.is_hard_type()
            || (return_type.kind == DtKind::Builtin
                && return_type.builtin_type == VariantType::Nil);
        if !return_type_is_void_or_variant && !body_has_return && (is_lambda || name != "_init") {
            ctx.push_error("Not all code paths return a value.", func_id);
        }

        // analyzer.cpp:1784 — UNUSED_PARAMETER. Godot's parser increments
        // `parameter->usages` at parse time when an identifier resolves to the parameter via
        // SuiteNode locals (gdscript_parser.cpp:2843); the warning fires in the analyzer when
        // `usages == 0`. gdls's parser doesn't carry parse-time identifier resolution, so we
        // approximate with a name-set sweep over identifier nodes whose byte span sits inside
        // the function body. Any same-named identifier in the body counts (even one that'd
        // resolve to an outer scope), so this over-approximates "used" — that's fine for
        // suppressing false positives on used parameters in the corpus.
        if !is_abstract {
            emit_unused_parameter_warnings(ctx, func_id, body_id, is_lambda);
        }

        // analyzer.cpp:1865-1960 — function-override signature compatibility, deferred to
        // body-pass so its emission lands after interface-pass siblings'
        // rest-parameter-type errors (Godot emits override mismatches during body
        // resolution; the corpus's `.out` files capture this order verbatim, see
        // `errors/variadic_functions.gd`).
        if !is_lambda && !name.is_empty() && name != "_init" && name != "_static_init" {
            check_override_signature(ctx, func_id, &name);
        }
    }

    ctx.current_function = previous_function;
    ctx.concrete_function = previous_concrete;
    ctx.static_context = previous_static;
}

/// Emit UNUSED_PARAMETER per Godot's analyzer.cpp:1784. Skips parameters whose name starts
/// with `_` (intentional-unused convention). The "function visible name" symbol follows the
/// Godot's three-case rule at analyzer.cpp:1774-1777.
fn emit_unused_parameter_warnings(
    ctx: &mut AnalysisContext,
    func_id: NodeId,
    body_id: NodeId,
    is_lambda: bool,
) {
    let params: Vec<NodeId> = match &ctx.node(func_id).kind {
        NodeKind::Function(f) => f.parameters.clone(),
        _ => return,
    };
    if params.is_empty() {
        return;
    }

    let function_visible_name = {
        let raw_name = decl_identifier_name(ctx, func_id);
        if raw_name.is_empty() {
            if is_lambda {
                "<anonymous lambda>".to_owned()
            } else {
                "<unknown function>".to_owned()
            }
        } else {
            raw_name
        }
    };

    let body_span = ctx.node(body_id).span;

    // Collect identifier-name references inside the body's byte span. Skip declaration
    // identifiers (Godot's `usages` counter only counts true references, not the decl
    // identifier itself — see the equivalent gate in `referenced_names`).
    let mut decl_ident_ids = rustc_hash::FxHashSet::<NodeId>::default();
    for id in ctx.tree.iter_ids() {
        match &ctx.node(id).kind {
            NodeKind::Variable(v) => {
                if let Some(i) = v.identifier {
                    decl_ident_ids.insert(i);
                }
            }
            NodeKind::Constant(c) => {
                if let Some(i) = c.identifier {
                    decl_ident_ids.insert(i);
                }
            }
            NodeKind::Parameter(p) => {
                if let Some(i) = p.identifier {
                    decl_ident_ids.insert(i);
                }
            }
            NodeKind::Function(f) => {
                if let Some(i) = f.identifier {
                    decl_ident_ids.insert(i);
                }
            }
            NodeKind::Signal(s) => {
                if let Some(i) = s.identifier {
                    decl_ident_ids.insert(i);
                }
            }
            _ => {}
        }
    }
    let mut used_names = rustc_hash::FxHashSet::<String>::default();
    for id in ctx.tree.iter_ids() {
        let node = ctx.node(id);
        if decl_ident_ids.contains(&id) {
            continue;
        }
        if node.span.start < body_span.start || node.span.end > body_span.end {
            continue;
        }
        if let NodeKind::Identifier(i) = &node.kind {
            used_names.insert(i.name.clone());
        }
    }

    for param_id in params {
        let param_name = decl_identifier_name(ctx, param_id);
        if param_name.is_empty() || param_name.starts_with('_') {
            continue;
        }
        if used_names.contains(&param_name) {
            continue;
        }
        let identifier_id = match &ctx.node(param_id).kind {
            NodeKind::Parameter(p) => p.identifier.unwrap_or(param_id),
            _ => param_id,
        };
        ctx.push_warning(
            crate::warnings::WarningCode::UnusedParameter,
            &[function_visible_name.clone(), param_name],
            identifier_id,
        );
    }
}

/// `resolve_suite(p_suite, p_is_root)` (analyzer.cpp:2058): iterate the suite's statements and
/// dispatch each to `resolve_node`. Godot applies statement-level annotations and calls
/// `resolve_pending_lambda_bodies` / `decide_suite_type` after each statement; both land in E3
/// (annotations with WP-F, lambdas with the lambda reducer).
///
/// E3c pushes the suite onto [`AnalysisContext::suite_stack`] for the duration of the iteration so
/// `reduce_identifier`'s local-lookup can walk the active scope chain — Godot's algorithm
/// reaches scope via `IdentifierNode::suite` (the parser's per-identifier back-pointer); gdls
/// reaches it via the analysis-time stack.
fn resolve_suite(ctx: &mut AnalysisContext, suite_id: NodeId, is_root: bool) {
    let stmts: Vec<NodeId> = match &ctx.node(suite_id).kind {
        NodeKind::Suite(s) => s.statements.clone(),
        _ => return,
    };
    ctx.suite_stack.push(suite_id);
    for stmt in stmts {
        resolve_node(ctx, stmt, is_root);
        // analyzer.cpp:2068 — drain pending lambda bodies after each statement so lambdas
        // queued by the just-resolved expression resolve in the right pass-relative order
        // (matters for the Godot-vs-gdls emission order on holding-function-with-lambdas cases).
        drain_pending_lambda_bodies(ctx);
        decide_suite_type(ctx, suite_id, stmt);
    }
    ctx.suite_stack.pop();
}

/// `decide_suite_type` (analyzer.cpp:2031-2056). After each statement in a suite, propagate
/// control-flow / return types to the suite node so `resolve_function_body` can infer the
/// function's return type from the body suite.
fn decide_suite_type(ctx: &mut AnalysisContext, suite_id: NodeId, stmt_id: NodeId) {
    let dominated = matches!(
        ctx.node(stmt_id).kind,
        NodeKind::If(_)
            | NodeKind::For(_)
            | NodeKind::Match(_)
            | NodeKind::Pattern(_)
            | NodeKind::Return(_)
            | NodeKind::While(_)
    );
    if !dominated {
        return;
    }
    let stmt_dt = ctx.get_type(stmt_id).clone();
    if !stmt_dt.is_set() {
        return;
    }
    let suite_dt = ctx.get_type(suite_id).clone();
    if suite_dt.is_set() && suite_dt != stmt_dt {
        ctx.set_type(
            suite_id,
            DataType {
                type_source: TypeSource::Undetected,
                kind: DtKind::Variant,
                ..Default::default()
            },
        );
    } else {
        let mut dt = stmt_dt;
        dt.type_source = TypeSource::Inferred;
        ctx.set_type(suite_id, dt);
    }
}

/// `resolve_node(p_node, p_is_root)` (analyzer.cpp:1586): dispatcher over statement kinds. Every
/// expression kind goes through `reduce_expression`; the statement kinds go through their
/// dedicated `resolve_*` (porting analyzer.cpp:1589-1670).
fn resolve_node(ctx: &mut AnalysisContext, id: NodeId, is_root: bool) {
    // M5 WP-O3 / WP-O4 governor + cancellation checkpoint at the dispatcher head — covers every
    // statement kind that goes through here, not just the expression kinds that flow into
    // reduce_expression. Bail leaves whichever per-kind resolver was about to run unexecuted; the
    // sink already has the partial diagnostic set up to this point.
    let span = ctx.tree.get(id).span;
    if ctx.checkpoint(span) {
        return;
    }
    match ctx.node(id).kind.clone() {
        NodeKind::None
        | NodeKind::Pass
        | NodeKind::Break
        | NodeKind::Continue
        | NodeKind::Breakpoint
        | NodeKind::Enum(_)
        | NodeKind::Function(_)
        | NodeKind::Signal(_) => {} // analyzer.cpp:1661-1669 — no body work.

        // Statement kinds.
        NodeKind::Variable(_) => resolve_variable_local(ctx, id),
        NodeKind::Constant(_) => resolve_constant_local(ctx, id),
        NodeKind::If(_) => resolve_if(ctx, id),
        NodeKind::For(_) => resolve_for(ctx, id),
        NodeKind::While(_) => resolve_while(ctx, id),
        NodeKind::Match(_) => resolve_match(ctx, id),
        NodeKind::Return(_) => resolve_return(ctx, id),
        NodeKind::Assert(_) => resolve_assert(ctx, id),
        NodeKind::Suite(_) => resolve_suite(ctx, id, false),
        NodeKind::MatchBranch(_) => resolve_match_branch(ctx, id, None),
        NodeKind::Pattern(_) => resolve_match_pattern(ctx, id, None),
        NodeKind::Parameter(_) => resolve_parameter(ctx, id),
        NodeKind::Type(_) => {
            let _ = resolve_datatype(ctx, Some(id));
        }
        NodeKind::Annotation(_) => {
            // analyzer.cpp:1617-1619 — annotation `apply()` lands with WP-F.
        }
        NodeKind::Class(_) => {
            // analyzer.cpp:1592-1597 — never reached in practice (classes are resolved through
            // the inheritance/interface/body drivers). If it does happen, the recursive class
            // resolve covers it; we keep the same Godot code path.
            let _ = resolve_inheritance(ctx);
            resolve_class_body(ctx, id);
        }

        // Expression kinds — analyzer.cpp:1641-1660. Resolving an expression IS reducing it.
        NodeKind::Array(_)
        | NodeKind::Assignment(_)
        | NodeKind::Await(_)
        | NodeKind::BinaryOp(_)
        | NodeKind::Call(_)
        | NodeKind::Cast(_)
        | NodeKind::Dictionary(_)
        | NodeKind::GetNode(_)
        | NodeKind::Identifier(_)
        | NodeKind::Lambda(_)
        | NodeKind::Literal(_)
        | NodeKind::Preload(_)
        | NodeKind::SelfExpr
        | NodeKind::Subscript(_)
        | NodeKind::TernaryOp(_)
        | NodeKind::TypeTest(_)
        | NodeKind::UnaryOp(_) => {
            crate::reducer::reduce_expression(ctx, id, is_root);
        }
    }
}

/// `resolve_variable(p_variable, true)` (analyzer.cpp:2213, local arm): drive `resolve_assignable`
/// for a `var` statement inside a function body. The `UNUSED_VARIABLE` warning + `is_shadowing`
/// (analyzer.cpp:2218-2223) join with WP-F.
fn resolve_variable_local(ctx: &mut AnalysisContext, var_id: NodeId) {
    let (spec, init, infer) = variable_assignable_parts(ctx, var_id);
    resolve_assignable(ctx, var_id, spec, init, infer, false);
    warn_local_shadowing(ctx, var_id, "variable");
    warn_confusable_identifier(ctx, var_id);
}

/// SHADOWED_GLOBAL_IDENTIFIER for class-level variables (mirrors `warn_local_shadowing`'s
/// global-identifier branch but anchored on class members instead of locals). Class members
/// don't shadow same-class members (they ARE members), so only the global-collision check
/// applies here.
fn warn_class_member_shadows_global(ctx: &mut AnalysisContext, node_id: NodeId, kind: &str) {
    let name = decl_identifier_name(ctx, node_id);
    if name.is_empty() {
        return;
    }
    if let Some(global_desc) = shadowed_global_identifier_description(ctx, &name) {
        let ident_id = match &ctx.node(node_id).kind {
            NodeKind::Variable(v) => v.identifier.unwrap_or(node_id),
            NodeKind::Constant(c) => c.identifier.unwrap_or(node_id),
            _ => node_id,
        };
        ctx.push_warning(
            crate::warnings::WarningCode::ShadowedGlobalIdentifier,
            &[kind.to_owned(), name, global_desc],
            ident_id,
        );
    }
}

/// CONFUSABLE_IDENTIFIER warning (analyzer.cpp's `is_confusable_identifier` helper) — fires
/// when an identifier contains a non-ASCII alphabetic character that's visually similar
/// to an ASCII letter (Cyrillic А vs Latin A, Greek α vs Latin a, etc.). Godot uses
/// Unicode's Confusables table; gdls approximates with a "contains any non-ASCII alphabetic
/// character" gate, which catches the corpus's `my_vАr` style identifiers (Cyrillic А in
/// the middle of an ASCII name) without false-positiving on identifiers that gdls's
/// tokenizer doesn't accept anyway (Godot limits identifiers to letters / digits /
/// underscores in the relevant tokenization paths).
fn warn_confusable_identifier(ctx: &mut AnalysisContext, node_id: NodeId) {
    let name = decl_identifier_name(ctx, node_id);
    if name.is_empty() {
        return;
    }
    if !name.chars().any(|c| !c.is_ascii() && c.is_alphabetic()) {
        return;
    }
    let ident_id = match &ctx.node(node_id).kind {
        NodeKind::Variable(v) => v.identifier.unwrap_or(node_id),
        NodeKind::Constant(c) => c.identifier.unwrap_or(node_id),
        _ => node_id,
    };
    ctx.push_warning(
        crate::warnings::WarningCode::ConfusableIdentifier,
        std::slice::from_ref(&name),
        ident_id,
    );
}

/// `resolve_constant(p_constant, true)` (analyzer.cpp:2227, local arm).
fn resolve_constant_local(ctx: &mut AnalysisContext, const_id: NodeId) {
    let (spec, init, infer) = constant_assignable_parts(ctx, const_id);
    resolve_assignable(ctx, const_id, spec, init, infer, true);
    warn_confusable_identifier(ctx, const_id);
    // analyzer.cpp:2118-2123 — constant initializer must reduce to a constant expression.
    // gdls's fold table is incomplete (preload / Color.RED / native enum values stamp
    // placeholder folds that aren't fully tracked), so checking `is_reduced` directly
    // false-positives. Instead, walk the init AST and look for any identifier that
    // resolves to a local Variable / Parameter / ForVariable / PatternBind — these are
    // unambiguously non-constant. Constants (LocalKind::Constant) are fine.
    if let Some(init_id) = init {
        if let Some(bad_ref) = init_references_nonconstant_local(ctx, init_id) {
            let name = decl_identifier_name(ctx, const_id);
            let _ = bad_ref; // anchor at the init expression, not the bad ref, for consistency
            ctx.push_error(
                format!(r#"Assigned value for constant "{name}" isn't a constant expression."#),
                init_id,
            );
        }
    }
    warn_local_shadowing(ctx, const_id, "constant");
}

/// Walk the expression tree of `expr_id` and check whether any identifier resolves to
/// either a non-constant local (Variable / Parameter / ForVariable / PatternBind) OR a
/// non-constant member of `class_id` (Variable / Signal / Function). Returns `true`
/// if such an identifier is found — the expression isn't a compile-time constant.
fn expression_references_nonconstant_member(
    ctx: &AnalysisContext,
    expr_id: NodeId,
    class_id: NodeId,
) -> bool {
    use gd_syntax::ast::LocalKind;
    let mut stack: Vec<NodeId> = vec![expr_id];
    while let Some(id) = stack.pop() {
        match &ctx.node(id).kind {
            NodeKind::Identifier(i) => {
                let name = i.name.clone();
                if let Some(local) = crate::reducer::lookup_local(ctx, &name) {
                    if !matches!(local.kind, LocalKind::Constant) {
                        return true;
                    }
                    continue;
                }
                // Class member fallback — `num` in `@export_range(num, 10)` resolves to a
                // class-level Variable / Signal / Function as a non-constant member.
                if let Some(member) = class_member(ctx, class_id, &name) {
                    if matches!(
                        member,
                        Member::Variable(_) | Member::Signal(_) | Member::Function(_)
                    ) {
                        return true;
                    }
                }
            }
            NodeKind::BinaryOp(b) => {
                if let Some(l) = b.left_operand {
                    stack.push(l);
                }
                if let Some(r) = b.right_operand {
                    stack.push(r);
                }
            }
            NodeKind::UnaryOp(u) => {
                if let Some(o) = u.operand {
                    stack.push(o);
                }
            }
            NodeKind::Subscript(s) => {
                if let Some(b) = s.base {
                    stack.push(b);
                }
                if let Some(gd_syntax::ast::SubscriptAccess::Index(Some(idx))) = s.access {
                    stack.push(idx);
                }
            }
            _ => {}
        }
    }
    false
}

/// Walk the expression tree of `expr_id` looking for any identifier that resolves to a
/// non-constant local (Variable / Parameter / ForVariable / PatternBind). Returns the
/// offending NodeId on hit, `None` if every identifier reaches a Constant local (or no
/// local). Constants and unresolved-to-locals identifiers are fine.
fn init_references_nonconstant_local(ctx: &AnalysisContext, expr_id: NodeId) -> Option<NodeId> {
    use gd_syntax::ast::LocalKind;
    let mut stack: Vec<NodeId> = vec![expr_id];
    while let Some(id) = stack.pop() {
        match &ctx.node(id).kind {
            NodeKind::Identifier(i) => {
                let name = i.name.clone();
                if let Some(local) = crate::reducer::lookup_local(ctx, &name) {
                    if !matches!(local.kind, LocalKind::Constant) {
                        return Some(id);
                    }
                }
            }
            NodeKind::BinaryOp(b) => {
                if let Some(l) = b.left_operand {
                    stack.push(l);
                }
                if let Some(r) = b.right_operand {
                    stack.push(r);
                }
            }
            NodeKind::UnaryOp(u) => {
                if let Some(o) = u.operand {
                    stack.push(o);
                }
            }
            NodeKind::TernaryOp(t) => {
                if let Some(c) = t.condition {
                    stack.push(c);
                }
                if let Some(e) = t.true_expr {
                    stack.push(e);
                }
                if let Some(e) = t.false_expr {
                    stack.push(e);
                }
            }
            NodeKind::Subscript(s) => {
                if let Some(b) = s.base {
                    stack.push(b);
                }
                if let Some(gd_syntax::ast::SubscriptAccess::Index(Some(idx))) = s.access {
                    stack.push(idx);
                }
            }
            NodeKind::Array(a) => {
                for &el in &a.elements {
                    stack.push(el);
                }
            }
            NodeKind::Dictionary(d) => {
                for kv in &d.elements {
                    if let Some(k) = kv.key {
                        stack.push(k);
                    }
                    if let Some(v) = kv.value {
                        stack.push(v);
                    }
                }
            }
            // Call / Cast / TypeTest / etc. — we consider these constant-safe for now (the
            // call dispatcher may have already emitted any "non-constant call" diagnostic). The
            // narrow gate above is sufficient for the `const TEST = 13 + i` corpus shape.
            _ => {}
        }
    }
    None
}

/// `is_shadowing(identifier, kind, true)` (analyzer.cpp:6165-6173, current-class branch).
/// When a local variable/constant shadows a member of the current class, emit SHADOWED_VARIABLE.
/// The "thing shadowed by" descriptor used in SHADOWED_GLOBAL_IDENTIFIER's symbol slot. Returns
/// `None` when the name doesn't collide with any global identifier Godot checks
/// (analyzer.cpp:6101-6132). Godot's check order is: built-in function → built-in type →
/// native class → global class_name; gdls's check order matches.
fn shadowed_global_identifier_description(ctx: &AnalysisContext, name: &str) -> Option<String> {
    if ctx.native.utility(name).is_some() || crate::reducer::gd_utility_return_type(name).is_some()
    {
        return Some("built-in function".to_owned());
    }
    if crate::resolver::builtin_type_from_name(name).is_some() || name == "Variant" {
        return Some("built-in type".to_owned());
    }
    if ctx.native.class_named(name).is_some() {
        return Some("native class".to_owned());
    }
    if let Some(fid) = ctx.xfile.global_class_file(name) {
        // Godot uses the global class's source-file path for the rendering. The current
        // file's path is on `ctx.script_path`; cross-file paths are reachable via the corpus
        // index but not the cross-file query trait — for the same-file case (a local
        // shadowing the class_name declared in the same file, e.g. `warnings/shadowning.gd`)
        // `ctx.script_path` suffices. Cross-file shadowing of a peer class_name is a deferred
        // case; render an empty file slot until the path threading lands.
        let path = if Some(fid) == ctx.file {
            let base = ctx
                .script_path
                .rsplit(['/', '\\'])
                .next()
                .unwrap_or(ctx.script_path.as_str())
                .to_owned();
            base
        } else {
            String::new()
        };
        return Some(format!(r#"global class defined in "{path}""#));
    }
    None
}

fn warn_local_shadowing(ctx: &mut AnalysisContext, node_id: NodeId, kind: &str) {
    let name = decl_identifier_name(ctx, node_id);
    if name.is_empty() {
        return;
    }
    let ident_id = match &ctx.node(node_id).kind {
        NodeKind::Variable(v) => v.identifier.unwrap_or(node_id),
        NodeKind::Constant(c) => c.identifier.unwrap_or(node_id),
        _ => node_id,
    };

    // analyzer.cpp:6101-6132 (`is_shadowing`'s global-identifier branch). Godot fires
    // SHADOWED_GLOBAL_IDENTIFIER when the local's name collides with a built-in function /
    // built-in type / native class / global class_name. Emit before the in-class shadowing
    // check so the source-position-stable order matches Godot's emit order.
    if let Some(global_desc) = shadowed_global_identifier_description(ctx, &name) {
        ctx.push_warning(
            crate::warnings::WarningCode::ShadowedGlobalIdentifier,
            &[kind.to_owned(), name.clone(), global_desc],
            ident_id,
        );
        return;
    }

    let Some(class_id) = ctx.current_class else {
        return;
    };
    let member_idx = match &ctx.node(class_id).kind {
        NodeKind::Class(c) => c.members_indices.get(&name).copied(),
        _ => None,
    };
    if let Some(idx) = member_idx {
        if let Some(member) = nth_member(ctx, class_id, idx) {
            let member_kind = member_kind_name(member.clone()).to_owned();
            let member_node = match member {
                Member::Class(id)
                | Member::Constant(id)
                | Member::Function(id)
                | Member::Signal(id)
                | Member::Variable(id)
                | Member::Enum(id) => Some(id),
                Member::EnumValue(_) | Member::Group(_) => None,
            };
            if let Some(member_node) = member_node {
                let member_line = ctx.node(member_node).loc.start.line.to_string();
                ctx.push_warning(
                    crate::warnings::WarningCode::ShadowedVariable,
                    &[kind.to_owned(), name, member_kind, member_line],
                    ident_id,
                );
                return;
            }
        }
    }

    // analyzer.cpp:6135-6188 — SHADOWED_VARIABLE_BASE_CLASS. Walk the base class chain
    // (in-file Class -> cross-file Script -> Native) looking for a same-named member.
    // For cross-file Script bases, walk every chain link (`crate::script_chain` — including
    // `Extends::Names` hops the old Path-only loop missed), read `MemberDecl::line` from the
    // interface, and emit the 5-symbol template
    // `... already-declared X at line N in the base class "Y".`.
    let base = ctx.bases.get(&class_id).cloned().unwrap_or_default();
    if base.kind == DtKind::Script {
        if let Some(sr) = base.script_type.as_ref() {
            let chain = crate::script_chain::resolve_script_chain(ctx, sr);
            for link in &chain.links {
                let Some(iface) = crate::script_chain::link_interface(ctx.xfile, link) else {
                    continue;
                };
                if let Some(member) = iface.members.iter().find(|m| m.name == name) {
                    use gd_project::MemberKind as MK;
                    let member_kind = match member.kind {
                        MK::Var | MK::Property => "variable",
                        MK::Const => "constant",
                        MK::Func => "function",
                        MK::Signal => "signal",
                        MK::Enum => "enum",
                    };
                    let base_name = iface.class_name.as_deref().unwrap_or("").to_owned();
                    let member_line = member.line.to_string();
                    ctx.push_warning(
                        crate::warnings::WarningCode::ShadowedVariableBaseClass,
                        &[
                            kind.to_owned(),
                            name,
                            member_kind.to_owned(),
                            member_line,
                            base_name,
                        ],
                        ident_id,
                    );
                    return;
                }
            }
        }
    }

    if let Some(native) = nearest_native_ancestor(ctx, class_id) {
        let mut cur = ctx.native.class_named(&native);
        while let Some(c) = cur {
            if c.methods.iter().any(|m| ctx.native.name_of(m.name) == name) {
                let defining = ctx.native.name_of(c.name).to_owned();
                ctx.push_warning(
                    crate::warnings::WarningCode::ShadowedVariableBaseClass,
                    &[kind.to_owned(), name, "method".to_owned(), defining],
                    ident_id,
                );
                return;
            }
            cur = c
                .inherits
                .as_ref()
                .map(|sym| ctx.native.name_of(*sym).to_owned())
                .and_then(|n| ctx.native.class_named(&n));
        }
    }
}

/// `resolve_if` (analyzer.cpp:2246): reduce the condition, resolve both blocks.
fn resolve_if(ctx: &mut AnalysisContext, if_id: NodeId) {
    let (cond, t_block, f_block) = match ctx.node(if_id).kind.clone() {
        NodeKind::If(n) => (n.condition, n.true_block, n.false_block),
        _ => return,
    };
    if let Some(c) = cond {
        crate::reducer::reduce_expression(ctx, c, false);
    }
    // analyzer.cpp:2249/2253 — Godot uses `resolve_suite`'s default `p_is_root=true` here
    // (gdscript_analyzer.h:84). Statements inside an if/else block are root-positioned, so a
    // void-returning call as a bare statement doesn't trigger "Cannot get return value ... void".
    if let Some(t) = t_block {
        resolve_suite(ctx, t, true);
    }
    if let Some(f) = f_block {
        resolve_suite(ctx, f, true);
    }
}

/// `resolve_while` (analyzer.cpp:2379): reduce the condition, resolve the body.
fn resolve_while(ctx: &mut AnalysisContext, while_id: NodeId) {
    let (cond, body) = match ctx.node(while_id).kind.clone() {
        NodeKind::While(n) => (n.condition, n.loop_body),
        _ => return,
    };
    if let Some(c) = cond {
        crate::reducer::reduce_expression(ctx, c, false);
    }
    // analyzer.cpp:2381 — default `p_is_root=true`.
    if let Some(b) = body {
        resolve_suite(ctx, b, true);
    }
}

/// `resolve_for` (analyzer.cpp:2258): reduce the list expression, type the iterator variable, and
/// resolve the body. E2 ports the structural part; the iterator-type inference walk
/// (`get_function_signature(_iter_get)`, container-element-type lookups, the `Unable to iterate
/// on …` errors) lives in E3 along with `is_type_compatible`. Until then the iterator variable
/// types as `Variant` — Godot's behaviour for unhinted iteration.
fn resolve_for(ctx: &mut AnalysisContext, for_id: NodeId) {
    let (variable, datatype_specifier, list, loop_body) = match ctx.node(for_id).kind.clone() {
        NodeKind::For(n) => (n.variable, n.datatype_specifier, n.list, n.loop_body),
        _ => return,
    };

    // analyzer.cpp:2258-2329 — type the iterator from the list. `range(...)` is hard-coded to int;
    // everything else comes from `list_type`. The full Object/_iter_get path (analyzer.cpp:2312-
    // 2324) needs reduce_call, so we treat OBJECT bases as Variant for E3e (no phantom errors).
    let mut variable_type = DataType::default();
    let mut list_type = DataType::default();
    if let Some(l) = list {
        crate::reducer::reduce_expression(ctx, l, false);

        let is_range = matches!(&ctx.node(l).kind, NodeKind::Call(c) if {
            c.callee
                .map(|cid| matches!(&ctx.node(cid).kind, NodeKind::Identifier(i) if i.name == "range"))
                .unwrap_or(false)
        });
        if is_range {
            variable_type = DataType {
                type_source: TypeSource::AnnotatedInferred,
                kind: DtKind::Builtin,
                builtin_type: VariantType::Int,
                ..Default::default()
            };
        }

        list_type = ctx.get_type(l).clone();
        if is_range {
            // Already solved.
        } else if list_type.is_variant() {
            variable_type.kind = DtKind::Variant;
        } else if list_type.kind == DtKind::Builtin {
            // analyzer.cpp:2300-2329 — element-type-by-list-type table.
            match list_type.builtin_type {
                VariantType::Int | VariantType::Float | VariantType::String => {
                    variable_type.type_source = list_type.type_source;
                    variable_type.kind = DtKind::Builtin;
                    variable_type.builtin_type = list_type.builtin_type;
                }
                VariantType::Vector2i | VariantType::Vector3i => {
                    variable_type.type_source = list_type.type_source;
                    variable_type.kind = DtKind::Builtin;
                    variable_type.builtin_type = VariantType::Int;
                }
                VariantType::Vector2 | VariantType::Vector3 => {
                    variable_type.type_source = list_type.type_source;
                    variable_type.kind = DtKind::Builtin;
                    variable_type.builtin_type = VariantType::Float;
                }
                bt if crate::data_type::typed_container_element(bt).is_some() => {
                    // analyzer.cpp:2293-2295 — `list_type.is_typed_container_type()` ⇒ the
                    // iterator takes `get_typed_container_type()` (the packed array's fixed
                    // element type). Ordered before the ARRAY/DICTIONARY/!is_hard_type arms,
                    // matching Godot's branch order, so soft packed lists still get element
                    // typing rather than degrading to Variant.
                    variable_type.type_source = list_type.type_source;
                    variable_type.kind = DtKind::Builtin;
                    variable_type.builtin_type = crate::data_type::typed_container_element(bt)
                        .expect("invariant: guard above checked is_some");
                }
                VariantType::Array if !list_type.container_element_types.is_empty() => {
                    // analyzer.cpp:2310-2317 — typed Array[T] yields T as the iterator var
                    // type. Element type carries its own type_source; we stamp it directly.
                    variable_type = list_type.container_element_types[0].clone();
                }
                VariantType::Object | VariantType::Array | VariantType::Dictionary => {
                    // Object._iter_get / untyped Array / Dictionary key-type each need the
                    // full reducer machinery (Object's `_iter_get` method walk + Dictionary
                    // key-type extraction). Untyped Array's element type is genuinely Variant.
                    // The typed-Dictionary arm joins later when the iterator infers keys.
                    variable_type.kind = DtKind::Variant;
                }
                _ if !list_type.is_hard_type() => {
                    variable_type.kind = DtKind::Variant;
                }
                _ => {
                    ctx.push_error(
                        format!(r#"Unable to iterate on value of type "{list_type}"."#),
                        l,
                    );
                }
            }
        } else if list_type.kind == DtKind::Enum && !list_type.is_meta_type {
            // Iterating an enum value (the integer kind) — typed as int. Godot reaches this via
            // the BUILTIN/INT arm because `make_enum_type(meta=false)` carries `builtin_type=INT`;
            // gdls's `kind` is `Enum` so we route explicitly.
            variable_type.type_source = list_type.type_source;
            variable_type.kind = DtKind::Builtin;
            variable_type.builtin_type = VariantType::Int;
        } else if list_type.kind == DtKind::Enum && list_type.is_meta_type {
            // Iterating the enum metatype itself (`for k in MyEnum:`). Godot's
            // make_enum_type(meta=true) sets `builtin_type = DICTIONARY`, which falls into the
            // ARRAY/DICTIONARY/!is_hard_type arm at analyzer.cpp:2325 — variable degrades to
            // Variant. gdls's `kind` is `Enum` so we route explicitly.
            variable_type.kind = DtKind::Variant;
        } else if list_type.kind == DtKind::Class && list_type.class_node.is_some() {
            // analyzer.cpp:2333-2345 — iterating an Object instance: look up the class's
            // `_iter_get(p_iter) -> T` method and use T as the iterator variable type. gdls
            // walks the in-file Class member directly; the cross-file Script + Native variants
            // join later slices.
            let class_id = list_type
                .class_node
                .expect("invariant: list_type.class_node.is_some() — checked at the outer `else if` guard above");
            let iter_get = match &ctx.node(class_id).kind {
                NodeKind::Class(c) => c.members_indices.get("_iter_get").copied(),
                _ => None,
            }
            .and_then(|idx| match &ctx.node(class_id).kind {
                NodeKind::Class(c) => c.members.get(idx).cloned(),
                _ => None,
            });
            if let Some(Member::Function(fn_id)) = iter_get {
                // Make sure the signature is resolved (lazy) so its return type is filled in.
                if ctx.get_type(fn_id).has_no_type() {
                    resolve_function_signature(ctx, fn_id);
                }
                let return_dt = ctx.get_type(fn_id).clone();
                if return_dt.is_set() {
                    variable_type = return_dt;
                } else {
                    variable_type.kind = DtKind::Variant;
                }
            } else {
                variable_type.kind = DtKind::Variant;
            }
        } else if !list_type.is_hard_type() {
            variable_type.kind = DtKind::Variant;
        } else {
            ctx.push_error(
                format!(r#"Unable to iterate on value of type "{list_type}"."#),
                l,
            );
        }
    }

    if let Some(v) = variable {
        if let Some(spec) = datatype_specifier {
            let specified_type = type_from_metatype(resolve_datatype(ctx, Some(spec)));
            if !specified_type.is_variant()
                && !variable_type.is_variant()
                && variable_type.is_hard_type()
                && !crate::reducer::is_type_compatible(ctx, &specified_type, &variable_type, true)
                && !crate::reducer::is_type_compatible(ctx, &variable_type, &specified_type, false)
            {
                ctx.push_error(
                    format!(
                        r#"Unable to iterate on value of type "{list_type}" with variable of type "{specified_type}"."#
                    ),
                    spec,
                );
            }
            // analyzer.cpp:2349-2354 — when iterating a literal Array (or Dictionary), narrow
            // the literal's element type against the iterator's specified type. This fires the
            // typed-collection element-mismatch errors (`Cannot have an element of type X in
            // an array of type Array[Y].` + the const companion via
            // `update_array_literal_element_type`) for `for x: String in [1, 2, 3]`-shaped
            // sources. For Dictionary the iterator var IS the key type, and the value type
            // defaults to Variant (Godot passes `DataType::get_variant_type()`).
            if let Some(l) = list {
                match &ctx.node(l).kind {
                    NodeKind::Array(_) => {
                        crate::reducer::update_array_literal_element_type(ctx, l, &specified_type);
                    }
                    NodeKind::Dictionary(_) => {
                        crate::reducer::update_dictionary_literal_element_type(
                            ctx,
                            l,
                            &specified_type,
                            &DataType::variant(),
                        );
                    }
                    _ => {}
                }
            }
            ctx.set_type(v, specified_type);
        } else {
            ctx.set_type(v, variable_type);
        }
    }

    // analyzer.cpp:2370 — default `p_is_root=true`.
    if let Some(b) = loop_body {
        resolve_suite(ctx, b, true);
    }
}

/// `resolve_return` (analyzer.cpp:2517): reduce the return expression, and — when the enclosing
/// function declares a hard `void` return type — emit Godot's
/// `A void function cannot return a value.` error for any non-call return value
/// (analyzer.cpp:2546). The call exception mirrors Godot: a `return some_void_call()` is
/// allowed so the user can chain through another void function, the runtime treats it as a
/// statement equivalent to `some_void_call(); return`.
fn resolve_return(ctx: &mut AnalysisContext, ret_id: NodeId) {
    let value = match &ctx.node(ret_id).kind {
        NodeKind::Return(r) => r.return_value,
        _ => return,
    };
    let Some(v) = value else {
        return;
    };

    // The function's `TypeTable` entry is its return type (see `resolve_function_signature`
    // analyzer.cpp:1729-1862 and gdls's mirror at the head of this file). A hard-typed
    // `Builtin NIL` return is Godot's `is_void_function`.
    let expected_type = ctx.current_function.map(|f| ctx.get_type(f).clone());
    let is_void_function = expected_type.as_ref().is_some_and(|t| {
        t.is_hard_type() && t.kind == DtKind::Builtin && t.builtin_type == VariantType::Nil
    });
    let is_call = matches!(ctx.node(v).kind, NodeKind::Call(_));

    // Reduce the return expression first so subsequent type queries see its resolved DataType.
    // `is_root=true` matches Godot's `reduce_call(..., is_root=true)` when the value is a call
    // inside a void function (analyzer.cpp:2530-2531): root-context allows void-returning calls
    // without firing the "Cannot get return value of call to X because it returns void." error.
    crate::reducer::reduce_expression(ctx, v, is_void_function && is_call);

    if is_void_function && !is_call {
        ctx.push_error("A void function cannot return a value.", ret_id);
        return;
    }

    // analyzer.cpp:2538-2547 — UNSAFE_VOID_RETURN. Godot warns when a void function returns
    // the result of a call whose return type can't be statically confirmed as void (a soft
    // Variant return — typically an untyped inner function whose return type the analyzer
    // doesn't fully know). The two symbols are [enclosing-function-name, called-function-name].
    if is_void_function && is_call {
        let return_type = ctx.get_type(v).clone();
        // analyzer.cpp:2538-2547 — Godot emits UNSAFE_VOID_RETURN when the called function's
        // return type isn't statically known to be void (soft Variant / Undetected). A function
        // declared `-> Variant` returns a HARD Variant — the user explicitly chose Variant — and
        // Godot stays silent. Only when the type is soft (analyzer can't confirm void) does
        // the warning fire.
        let return_is_void =
            return_type.kind == DtKind::Builtin && return_type.builtin_type == VariantType::Nil;
        let return_is_hard_variant =
            return_type.is_hard_type() && return_type.kind == DtKind::Variant;
        if !return_is_void && !return_is_hard_variant {
            let enclosing = ctx
                .current_function
                .map(|f| decl_identifier_name(ctx, f))
                .unwrap_or_default();
            let called = match &ctx.node(v).kind {
                NodeKind::Call(c) => c.function_name.clone(),
                _ => String::new(),
            };
            ctx.push_warning(
                crate::warnings::WarningCode::UnsafeVoidReturn,
                &[enclosing, called],
                ret_id,
            );
        }
    }

    // analyzer.cpp:2559-2562 — when the expected return type is a hard builtin/enum and the
    // expression is constant-foldable, run `update_const_expression_builtin_type` to either
    // narrow the literal's datatype or emit `Cannot return a value of type "X" as "Y".`. The
    // result type for the trailing compat check still comes from `p_return->return_value`
    // afterwards, matching `result = p_return->return_value->get_datatype()` at :2562.
    let Some(expected_type) = expected_type else {
        return;
    };
    if !expected_type.is_set() {
        return;
    }
    if expected_type.is_hard_type() && ctx.folds.is_reduced(v) {
        crate::reducer::update_const_expression_builtin_type(ctx, v, &expected_type, "return");
    }
    let result = ctx.get_type(v).clone();

    // analyzer.cpp (DEBUG_ENABLED return narrowing) — NARROWING_CONVERSION when the function
    // returns `int` but the value is `float`. Mirrors the assignment-narrowing emission so
    // `var i: int: get: return f` (with `f: float`) warns from inside the inline getter. Anchor
    // at the return value so per-line `@warning_ignore("narrowing_conversion")` targets it.
    if expected_type.is_hard_type()
        && expected_type.kind == DtKind::Builtin
        && expected_type.builtin_type == VariantType::Int
        && result.kind == DtKind::Builtin
        && result.builtin_type == VariantType::Float
    {
        ctx.push_warning(crate::warnings::WarningCode::NarrowingConversion, &[], v);
    }

    // analyzer.cpp:2572-2588 — compat check against the declared return type. Soft / Variant
    // sources would mark the node unsafe and propagate; we still gate on the same hard-error
    // condition (both directions of is_type_compatible failing) so the diagnostic only fires
    // when the runtime cast couldn't possibly succeed. We anchor the diagnostic at the return
    // value expression rather than the return statement (Godot uses the statement node) so
    // that gdls's offset-sorted diagnostic publish order matches Godot's emit order when
    // both this error and `update_const_expression_builtin_type`'s "Cannot return a value of
    // type X as Y" companion fire for the same return (e.g. `lambda_wrong_return.gd`). The line
    // rendering is identical either way — `return 'string'` is a single source line — but
    // anchoring at the same node keeps the stable insertion order intact through
    // `DiagnosticSink::finish`'s `sort_by_key(span.start)`.
    if !expected_type.is_variant() && !result.is_variant() && result.is_hard_type() {
        let target_to_source =
            crate::reducer::is_type_compatible(ctx, &expected_type, &result, true);
        if !target_to_source {
            let reverse = crate::reducer::is_type_compatible(ctx, &result, &expected_type, false);
            if !reverse {
                ctx.push_error(
                    format!(
                        r#"Cannot return value of type "{result}" because the function return type is "{expected_type}"."#
                    ),
                    v,
                );
            }
        }
    }

    // analyzer.cpp:2590 — stamp the return node's type so `decide_suite_type` can propagate it.
    ctx.set_type(ret_id, result);
}

/// `resolve_assert` (analyzer.cpp:2385): reduce the condition + message. The `Expected string for
/// assert error message.` error (analyzer.cpp:2389-2391) needs the result type of the message
/// expression, which arrives once `reduce_call`/`reduce_identifier` land in E3 — until then the
/// message expression types as `Variant` and the check is silenced. The `ASSERT_ALWAYS_TRUE`/
/// `ASSERT_ALWAYS_FALSE` warnings (analyzer.cpp:2396-2404) join with WP-F.
fn resolve_assert(ctx: &mut AnalysisContext, assert_id: NodeId) {
    let (cond, msg) = match &ctx.node(assert_id).kind {
        NodeKind::Assert(a) => (a.condition, a.message),
        _ => return,
    };
    if let Some(c) = cond {
        crate::reducer::reduce_expression(ctx, c, false);
    }
    if let Some(m) = msg {
        crate::reducer::reduce_expression(ctx, m, false);
    }
}

/// `resolve_match` (analyzer.cpp:2407): reduce the test, resolve each branch.
fn resolve_match(ctx: &mut AnalysisContext, match_id: NodeId) {
    let (test, branches) = match ctx.node(match_id).kind.clone() {
        NodeKind::Match(n) => (n.test, n.branches),
        _ => return,
    };
    if let Some(t) = test {
        crate::reducer::reduce_expression(ctx, t, false);
    }
    for branch in branches {
        resolve_match_branch(ctx, branch, test);
    }
}

/// `resolve_match_branch` (analyzer.cpp:2417): resolve each pattern + the guard + the block.
fn resolve_match_branch(ctx: &mut AnalysisContext, branch_id: NodeId, match_test: Option<NodeId>) {
    let (patterns, block, guard) = match ctx.node(branch_id).kind.clone() {
        NodeKind::MatchBranch(n) => (n.patterns, n.block, n.guard_body),
        _ => return,
    };
    for p in patterns {
        resolve_match_pattern(ctx, p, match_test);
    }
    // analyzer.cpp:2429 — match-branch guard body uses explicit `false` (an expression context,
    // not a statement-root one). The block at :2432 uses the default `true`.
    if let Some(g) = guard {
        resolve_suite(ctx, g, false);
    }
    if let Some(b) = block {
        resolve_suite(ctx, b, true);
    }
}

/// `resolve_match_pattern` (analyzer.cpp:2437): walk the pattern kinds, reducing constant /
/// expression sub-patterns and recursing on array/dictionary sub-patterns. The
/// "Expression in match pattern must be a constant expression, an identifier, or an attribute
/// access" error (analyzer.cpp:2466) and the dictionary-key "must be a constant" error
/// (analyzer.cpp:2497) need the reducer to distinguish "not constant" from "kind not yet ported"
/// — held back per the same rule as `resolve_enum_type`'s "must be constant" check.
fn resolve_match_pattern(
    ctx: &mut AnalysisContext,
    pattern_id: NodeId,
    match_test: Option<NodeId>,
) {
    let pattern = match ctx.node(pattern_id).kind.clone() {
        NodeKind::Pattern(p) => p,
        _ => return,
    };

    match pattern.pattern_type {
        gd_syntax::ast::PatternKind::Literal(lit) => {
            if let Some(l) = lit {
                crate::reducer::reduce_expression(ctx, l, false);
            }
        }
        gd_syntax::ast::PatternKind::Expression(expr) => {
            if let Some(e) = expr {
                crate::reducer::reduce_expression(ctx, e, false);
                // analyzer.cpp:2456-2467 — when the expression isn't a folded constant, the only
                // remaining legal shapes are a plain identifier or an attribute-only subscript
                // chain (`A.B`, `A.B.C`, …). Walk through attribute subscripts; an index subscript
                // (`a[b]`) terminates the walk with a null pointer in Godot, which `push_error`
                // resolves against the parser's last-seen token. gdls doesn't carry that token, so
                // we anchor the diagnostic on the offending subscript node instead — the line
                // mismatch on `match_with_subscript.gd` is documented in
                // analyze_known_failures.txt; `match_with_variable_expression.gd` (BinaryOp at
                // line 4) lifts cleanly.
                if ctx.folds.get(e).is_none() {
                    let mut walker = Some(e);
                    let mut last_seen = e;
                    // Track whether the inner walk got nulled by an INDEX subscript
                    // (Godot's `if (!sub->is_attribute) { expr = nullptr; }` arm at
                    // analyzer.cpp:2459-2460). Godot then calls
                    // `push_error(message, expr)` with `expr == nullptr`, which falls
                    // through to gdscript_parser.cpp:241-244's
                    // `previous.{start,end}_line`. At analyze time `previous` is at
                    // end-of-parse — the synthetic post-EOF line stamped on
                    // `tree.eof_line`. WP-R3 uses that explicit line override here.
                    let mut nulled_by_index_subscript = false;
                    while let Some(cur) = walker {
                        last_seen = cur;
                        match &ctx.node(cur).kind {
                            NodeKind::Subscript(s) => match s.access {
                                Some(gd_syntax::ast::SubscriptAccess::Attribute(_)) => {
                                    walker = s.base;
                                }
                                _ => {
                                    walker = None;
                                    nulled_by_index_subscript = true;
                                    break;
                                }
                            },
                            _ => break,
                        }
                    }
                    let is_identifier = walker
                        .map(|w| matches!(&ctx.node(w).kind, NodeKind::Identifier(_)))
                        .unwrap_or(false);
                    if !is_identifier {
                        if nulled_by_index_subscript {
                            // Godot-faithful null-source path (analyzer.cpp:2466 with
                            // `expr = nullptr`): render at the parser's `previous` token
                            // line, which at analyze time is the synthetic post-EOF
                            // line (lexer.rs:214 `newline(true)` bump).
                            let line = ctx.tree.eof_line;
                            ctx.push_error_at_line(
                                r#"Expression in match pattern must be a constant expression, an identifier, or an attribute access ("A.B")."#,
                                last_seen,
                                line,
                            );
                        } else {
                            ctx.push_error(
                                r#"Expression in match pattern must be a constant expression, an identifier, or an attribute access ("A.B")."#,
                                last_seen,
                            );
                        }
                    }
                }
            }
        }
        gd_syntax::ast::PatternKind::Bind(bind) => {
            if let Some(b) = bind {
                // Bind pattern adopts the match-test's type; without a match test it's Variant.
                let bind_type = match_test
                    .map(|t| ctx.get_type(t).clone())
                    .unwrap_or_else(DataType::variant);
                ctx.set_type(b, bind_type);
            }
        }
        gd_syntax::ast::PatternKind::Array => {
            for sub in pattern.array {
                resolve_match_pattern(ctx, sub, None);
            }
        }
        gd_syntax::ast::PatternKind::Dictionary => {
            for kv in pattern.dictionary {
                if let Some(k) = kv.key {
                    crate::reducer::reduce_expression(ctx, k, false);
                }
                if let Some(v) = kv.value {
                    resolve_match_pattern(ctx, v, None);
                }
            }
        }
        gd_syntax::ast::PatternKind::Rest | gd_syntax::ast::PatternKind::Wildcard => {}
    }
}

/// `(name, parameters, return_type, is_static, is_abstract, body)` for a function node — the full
/// signature needed by `resolve_function_body`. (The narrower `function_decl` above predates E2's
/// body needs.)
#[allow(clippy::type_complexity)]
fn function_body_decl(
    ctx: &AnalysisContext,
    id: NodeId,
) -> (
    String,
    Vec<NodeId>,
    Option<NodeId>,
    bool,
    bool,
    Option<NodeId>,
) {
    match &ctx.node(id).kind {
        NodeKind::Function(f) => (
            f.identifier
                .and_then(|i| ident_name(ctx, i))
                .unwrap_or_default(),
            f.parameters.clone(),
            f.return_type,
            f.is_static,
            f.is_abstract || ctx.abstract_nodes.contains(&id),
            f.body,
        ),
        _ => (String::new(), Vec::new(), None, false, false, None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cross_file::NoCrossFile;
    use crate::diagnostic::{Diagnostic, Severity};
    use crate::warn_policy::{StrictSettings, WarnPolicy};
    use gd_project::{FileId, WarningConfig};
    use gd_syntax::ParseTree;
    use gd_types::NativeDb;

    /// `Object ← Node ← CanvasItem ← Node2D` — enough native classes for the inheritance tests.
    fn mini_native() -> NativeDb {
        NativeDb::from_json(
            r#"{
                "header": {"version_major": 4, "version_minor": 6, "version_patch": 3},
                "classes": [
                    {"name": "Object"},
                    {"name": "RefCounted", "inherits": "Object"},
                    {"name": "Node", "inherits": "Object"},
                    {"name": "CanvasItem", "inherits": "Node"},
                    {"name": "Node2D", "inherits": "CanvasItem"}
                ]
            }"#,
        )
        .expect("valid mini dump")
    }

    fn policy() -> WarnPolicy {
        WarnPolicy::build(&WarningConfig::default(), &StrictSettings::default())
    }

    /// Analyze a single isolated file (no project classes) and return its diagnostics.
    fn diags(src: &str) -> Vec<Diagnostic> {
        let tree = gd_syntax::parse(src).tree;
        let native = mini_native();
        let xfile = NoCrossFile;
        crate::analyze(&tree, Some(FileId::new(1)), "", &native, &xfile, &policy()).diagnostics
    }

    fn errors(src: &str) -> Vec<String> {
        diags(src)
            .into_iter()
            .filter(|d| d.severity == Severity::Error)
            .map(|d| d.message)
            .collect()
    }

    #[test]
    fn native_base_resolves_clean() {
        assert!(errors("extends Node\nfunc f():\n\tpass\n").is_empty());
    }

    #[test]
    fn packed_array_iteration_accepts_every_packed_type() {
        // analyzer.cpp:2293-2295 routes packed arrays through the typed-container element table
        // (gdscript_parser.cpp:5508-5530); each used to fall into the hard-type error tail as
        // `Unable to iterate on value of type "Packed…Array".`.
        let src = "\
extends RefCounted
func go(bs: PackedByteArray, i32s: PackedInt32Array, i64s: PackedInt64Array,
\t\tf32s: PackedFloat32Array, f64s: PackedFloat64Array, ss: PackedStringArray,
\t\tv2s: PackedVector2Array, v3s: PackedVector3Array, cs: PackedColorArray,
\t\tv4s: PackedVector4Array) -> void:
\tfor _b in bs:
\t\tpass
\tfor _i in i32s:
\t\tpass
\tfor _j in i64s:
\t\tpass
\tfor _f in f32s:
\t\tpass
\tfor _g in f64s:
\t\tpass
\tfor _s in ss:
\t\tpass
\tfor _v in v2s:
\t\tpass
\tfor _w in v3s:
\t\tpass
\tfor _c in cs:
\t\tpass
\tfor _x in v4s:
\t\tpass
";
        assert_eq!(errors(src), Vec::<String>::new());
    }

    #[test]
    fn packed_string_array_iterator_variable_types_string() {
        let src = "extends RefCounted\nfunc go(paths: PackedStringArray) -> void:\n\tfor p in paths:\n\t\tvar _s := p\n";
        let tree = gd_syntax::parse(src).tree;
        let native = mini_native();
        let result = crate::analyze(
            &tree,
            Some(FileId::new(1)),
            "",
            &native,
            &NoCrossFile,
            &policy(),
        );
        let mut found = false;
        for id in tree.iter_ids() {
            if let gd_syntax::ast::NodeKind::Identifier(ident) = &tree.get(id).kind {
                if ident.name == "p" {
                    let dt = result.types.get(id);
                    if dt.is_set() {
                        assert_eq!(dt.kind, DtKind::Builtin);
                        assert_eq!(dt.builtin_type, VariantType::String);
                        found = true;
                    }
                }
            }
        }
        assert!(found, "expected a typed `p` identifier in the loop body");
    }

    #[test]
    fn no_extends_is_refcounted_and_clean() {
        assert!(errors("var x = 1\n").is_empty());
    }

    #[test]
    fn cyclic_inheritance_between_inner_classes() {
        // The exact `analyzer/errors/cyclic_inheritance.gd` corpus case.
        let src = "func test():\n\tprint(InnerA.new())\n\n\
                   class InnerA extends InnerB:\n\tpass\n\n\
                   class InnerB extends InnerA:\n\tpass\n";
        assert_eq!(errors(src), vec!["Cyclic inheritance.".to_string()]);
    }

    #[test]
    fn unknown_base_class_errors() {
        assert_eq!(
            errors("extends Nonexistent\n"),
            vec![r#"Could not find base class "Nonexistent"."#.to_string()]
        );
    }

    #[test]
    fn class_name_hiding_a_native_class() {
        assert_eq!(
            errors("class_name Node\n"),
            vec![r#"Class "Node" hides a native class."#.to_string()]
        );
    }

    #[test]
    fn inner_class_extends_sibling_is_clean() {
        let src = "class A extends Node:\n\tpass\n\nclass B extends A:\n\tpass\n";
        assert!(errors(src).is_empty(), "got {:?}", errors(src));
    }

    #[test]
    fn member_redefined_in_parent_script_class() {
        // WP-D conflict check: a derived inner class re-declaring a base member.
        let src = "class Base:\n\tvar x = 1\n\nclass Derived extends Base:\n\tvar x = 2\n";
        assert_eq!(
            errors(src),
            vec![r#"The member "x" already exists in parent class Base."#.to_string()]
        );
    }

    #[test]
    fn typed_function_signature_is_clean() {
        // A fully-typed function resolves its parameter + return types with no diagnostics.
        let src = "extends Node\nfunc add(a: int, b: int) -> int:\n\treturn a + b\n";
        assert!(errors(src).is_empty(), "got {:?}", errors(src));
    }

    #[test]
    fn const_initializer_is_folded() {
        // WP-E2: resolve_body now drives the reducer over const initializers. A literal binary
        // expression folds to its computed Int value in the FoldTable.
        let src = "const FORTY_TWO = 6 * 7\nfunc test():\n\tpass\n";
        let tree = gd_syntax::parse(src).tree;
        let native = mini_native();
        let xfile = NoCrossFile;
        let pol = policy();
        let result = crate::analyze(&tree, Some(FileId::new(1)), "", &native, &xfile, &pol);
        assert!(
            result
                .diagnostics
                .iter()
                .all(|d| d.severity != Severity::Error),
            "got errors: {:?}",
            result.diagnostics
        );
        // The constant member resolves to a folded Int value via the binary-op reducer.
        let init_id = match &tree.root().expect("root").kind {
            NodeKind::Class(c) => c.members.iter().find_map(|m| match m {
                Member::Constant(id) => match &tree.get(*id).kind {
                    NodeKind::Constant(cn) => cn.initializer,
                    _ => None,
                },
                _ => None,
            }),
            _ => None,
        };
        let init = init_id.expect("constant has an initializer");
        assert_eq!(
            result.folds.get(init),
            Some(&crate::foldtable::FoldedValue::Int(42))
        );
    }

    #[test]
    fn missing_function_body_errors() {
        // WP-E2 / WP-Q36: an empty (statement-less) non-abstract function body fires Godot's
        // analyzer.cpp:1998-2000 diagnostic. The parser always allocates a body Suite (so the
        // node is `Some`); Godot's check is `body->statements.is_empty()`, which we mirror by
        // building a real but empty Suite body here.
        use gd_syntax::ast::{
            ClassNode, FunctionNode, IdentifierNode, Node, NodeKind, ParseTree, SuiteNode,
        };
        let mut tree = ParseTree::new();
        let f_ident = tree.push(Node::new(NodeKind::Identifier(IdentifierNode {
            name: "f".to_string(),
        })));
        // A function with a statement-less body and no `is_abstract` — exactly Godot's
        // error condition (`body->statements.is_empty() && !is_abstract`).
        let body_id = tree.push(Node::new(NodeKind::Suite(SuiteNode::default())));
        let fn_id = tree.push(Node::new(NodeKind::Function(FunctionNode {
            identifier: Some(f_ident),
            body: Some(body_id),
            ..Default::default()
        })));
        let class_id = tree.push(Node::new(NodeKind::Class(ClassNode {
            members: vec![Member::Function(fn_id)],
            ..Default::default()
        })));
        let native = mini_native();
        let xfile = NoCrossFile;
        let pol = policy();
        let mut ctx = AnalysisContext::new(&tree, &native, &xfile, Some(FileId::new(1)), "", &pol);
        ctx.current_class = Some(class_id);
        resolve_function_body(&mut ctx, fn_id, false);
        let result = ctx.finish();
        assert!(
            result
                .diagnostics
                .iter()
                .any(|d| d.message.contains("must either have")),
            "got: {:?}",
            result.diagnostics
        );
    }

    // --- resolve_datatype (no caller in WP-C; exercised directly here) --------------------------

    /// The `datatype_specifier` (a `Type` node) of the first typed `var` member.
    fn first_var_type(tree: &ParseTree) -> NodeId {
        let root = tree.root().expect("non-empty tree");
        let NodeKind::Class(c) = &root.kind else {
            panic!("root is not a class");
        };
        for m in &c.members {
            if let Member::Variable(vid) = m {
                if let NodeKind::Variable(v) = &tree.get(*vid).kind {
                    return v.datatype_specifier.expect("the var must be typed");
                }
            }
        }
        panic!("no typed var found");
    }

    fn resolve_var_type(src: &str) -> DataType {
        let tree = gd_syntax::parse(src).tree;
        let native = mini_native();
        let xfile = NoCrossFile;
        let pol = policy();
        let mut ctx = AnalysisContext::new(&tree, &native, &xfile, Some(FileId::new(1)), "", &pol);
        // The head must be resolved first so `current_class` / scope lookups are well-formed.
        let _ = resolve_inheritance(&mut ctx);
        ctx.current_class = ctx.tree.root_id();
        let type_id = first_var_type(&tree);
        resolve_datatype(&mut ctx, Some(type_id))
    }

    #[test]
    fn datatype_builtin_scalar() {
        let dt = resolve_var_type("extends Node\nvar x: int\n");
        assert_eq!(dt.kind, DtKind::Builtin);
        assert_eq!(dt.builtin_type, VariantType::Int);
        assert!(dt.is_hard_type());
    }

    #[test]
    fn datatype_native_class() {
        let dt = resolve_var_type("extends Node\nvar n: Node2D\n");
        assert_eq!(dt.kind, DtKind::Native);
        assert_eq!(dt.native_type, "Node2D");
    }

    #[test]
    fn datatype_unknown_emits_could_not_find_type_error() {
        // WP-Q27: an unresolvable type name now emits Godot's
        // `Could not find type "X" in the current scope.` error (analyzer.cpp:889-892). The
        // earlier "unknown stays dynamic" rule (silent degrade) was lifted once cross-file
        // resolution depth was sufficient to avoid false-positives on
        // preload-derived-class-constant chains.
        let tree = gd_syntax::parse("extends Node\nvar g: Ghost\n").tree;
        let native = mini_native();
        let xfile = NoCrossFile;
        let pol = policy();
        let mut ctx = AnalysisContext::new(&tree, &native, &xfile, Some(FileId::new(1)), "", &pol);
        let _ = resolve_inheritance(&mut ctx);
        ctx.current_class = ctx.tree.root_id();
        let type_id = first_var_type(&tree);
        let dt = resolve_datatype(&mut ctx, Some(type_id));
        assert!(dt.is_variant());
        assert!(
            ctx.has_errors(),
            "unresolved type must emit Could not find type error"
        );
    }

    #[test]
    fn datatype_none_is_variant() {
        let tree = gd_syntax::parse("extends Node\n").tree;
        let native = mini_native();
        let xfile = NoCrossFile;
        let pol = policy();
        let mut ctx = AnalysisContext::new(&tree, &native, &xfile, Some(FileId::new(1)), "", &pol);
        assert!(resolve_datatype(&mut ctx, None).is_variant());
    }
}
